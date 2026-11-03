//! Home Assistant: one hub on the LAN that drives every brand it has an
//! integration for. Home talks to its REST API to drive things, and to its
//! WebSocket API once per listing, to read the entity registry.
//!
//! Connecting takes a long-lived access token (`/home ha [URL] TOKEN`),
//! made under the HA user's profile, Security tab. The hub is kept in the
//! settings, the one place its token is kept: a scan lists its entities
//! again from it, and home hands it to each entity it drives. Home talks
//! to one Home Assistant at a time.
//! Each entity is a backend of its category: an HA light is driven through
//! the same `light_set` as a Wiz bulb.
//!
//! `http://` and `https://` both work. Over https, HA's certificate has to
//! be one this machine trusts.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;

use super::driver::{backend_of, Found};
use super::light::scene_key;
use super::{Device, State};

pub(super) mod connect;
#[cfg(test)]
pub(super) mod fake;
mod lights;
mod switches;
mod tvs;

pub use lights::HaLight;
pub use switches::HaSwitch;
pub use tvs::HaTv;
use lights::describe_light;
use switches::describe_switch;
use tvs::describe_tv;

/// Where HA listens unless told otherwise.
const PORT: u16 = 8123;
/// How long to wait to connect at all, and for HA to answer a question
/// about what something is doing.
const WAIT: Duration = Duration::from_secs(5);
/// How long to wait for HA to answer a call that drives something. HA
/// answers once the integration has done it, and one that goes through
/// a cloud can take well over five seconds.
const DRIVE: Duration = Duration::from_secs(20);
/// What a call that ran out of time says.
const SLOW: &str = "Home Assistant took too long to answer";
/// The longest effect or source name passed on to the model; a longer one
/// is nothing anyone would say, and would only fill the prompt.
const MAX_LABEL: usize = 48;
/// The most sources named back when the one asked for isn't there.
const MAX_SOURCES_SAID: usize = 20;
/// How long a listing may take: a big home's states and entity registry
/// run to megabytes, which HA takes a while to put together and send.
const LISTING: Duration = Duration::from_secs(60);
/// How long to look for an HA announcing itself.
const FIND: Duration = Duration::from_secs(3);
const SERVICE: &str = "_home-assistant._tcp.local.";
/// Integrations home drives itself: their entities would be the same
/// device twice.
const NATIVE: &[&str] = &["wiz", "webostv", "androidtv_remote"];
/// The most of an answer to read. A big home's `/api/states` runs to
/// megabytes, past the HTTP client's own 10 MB default.
const STILL_ON_IT: &str = "Home Assistant is still on it";
const TOOK_TOO_LONG: &str = "Home Assistant took too long to say whether it worked";
const MAX_ANSWER: u64 = 64 * 1024 * 1024;
/// `LightEntityFeature::FLASH`.
const LIGHT_FLASH: u64 = 8;
/// `MediaPlayerEntityFeature::VOLUME_SET`.
const VOLUME_SET: u64 = 4;
/// What one `volume_up` moves by, as a percentage, when an integration
/// doesn't say otherwise.
const VOLUME_STEP: u8 = 10;

/// A Home Assistant to talk to.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Hub {
    pub url: String,
    pub token: String,
}

/// Never prints the token.
impl fmt::Debug for Hub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hub").field("url", &self.url).field("token", &"<redacted>").finish()
    }
}

/// What a Home Assistant lists, and a warning when it couldn't be filtered.
pub struct Listing {
    pub found: Vec<Found>,
    pub warning: Option<String>,
}

/// The `http://host:port` or `https://host:port` someone typed, with the
/// scheme and port filled in when left out.
pub fn url(typed: &str) -> Result<String, String> {
    let typed = typed.trim().trim_end_matches('/');
    let (scheme, rest) = match typed.split_once("://") {
        Some((scheme @ ("http" | "https"), rest)) => (scheme, rest),
        Some((other, _)) => return Err(format!("{other}:// isn't how to reach Home Assistant; use http:// or https://")),
        None => ("http", typed),
    };
    // A path, query or login would be dropped or misread; HA is at the root.
    if rest.is_empty() || rest.contains(['/', '?', '#', '@']) {
        return Err(format!("{typed} isn't an address like http://homeassistant.local:8123"));
    }
    // Unbracketed, an IPv6 address's last group would read as a port.
    if !rest.starts_with('[') && rest.matches(':').count() > 1 {
        return Err(format!("put an IPv6 address in brackets, like http://[{rest}]:8123"));
    }
    let has_port = rest.rsplit_once(':').is_some_and(|(_, p)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    // HA itself listens on 8123 over http. Over https it is behind a proxy
    // or Nabu Casa, on https's own 443, so the scheme's default is left be.
    Ok(if has_port || scheme == "https" { format!("{scheme}://{rest}") } else { format!("{scheme}://{rest}:{PORT}") })
}

/// The URL of a Home Assistant announcing itself on the LAN, if one does
/// within a few seconds. Only said back to the person, never connected to
/// with their token: anything on the LAN can announce itself as HA, and
/// the token mustn't go to an address home only heard of.
pub fn find() -> Option<String> {
    let daemon = mdns_sd::ServiceDaemon::new().ok()?;
    let browse = daemon.browse(SERVICE).ok()?;
    let until = Instant::now() + FIND;
    let mut found = None;
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        match browse.recv_timeout(left) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(service)) => {
                let said = ["internal_url", "base_url"]
                    .iter()
                    .filter_map(|k| service.get_property_val_str(k))
                    .find_map(|u| url(u).ok());
                let seen = service
                    .get_addresses_v4()
                    .into_iter()
                    .next()
                    .map(|ip| format!("http://{ip}:{}", service.get_port()));
                found = announced(seen, said);
                if found.is_some() {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    found
}

/// The address to tell the person of, given where an announcement came
/// from (`seen`) and the URL its TXT record claims (`said`). The address
/// it came from wins: the TXT record is whatever the announcer wrote,
/// and home mustn't put an address off the LAN in front of someone about
/// to type their token after it. A claimed URL stands only when nothing
/// was seen and it points at this LAN.
fn announced(seen: Option<String>, said: Option<String>) -> Option<String> {
    seen.or_else(|| said.filter(|u| on_lan(u)))
}

/// Whether a URL's host is on this LAN: a private or link-local IPv4
/// address, or a `.local` name.
fn on_lan(url: &str) -> bool {
    let host = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = host.rsplit_once(':').map_or(host, |(host, _)| host);
    match host.parse::<Ipv4Addr>() {
        Ok(ip) => ip.is_private() || ip.is_link_local(),
        Err(_) => host.to_ascii_lowercase().ends_with(".local"),
    }
}

impl Hub {
    /// The hub home handed a device to reach it through.
    fn of(d: &Device) -> Result<Hub, String> {
        d.hub.clone().ok_or_else(|| "home isn't connected to Home Assistant; connect it with /home ha".to_string())
    }

    /// `host:port`, without the scheme, with https's 443 filled in when the
    /// URL leaves it to the scheme.
    fn host(&self) -> String {
        let host = self.url.split_once("://").map_or(self.url.as_str(), |(_, host)| host);
        let has_port = host.rsplit_once(':').is_some_and(|(_, p)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        match (has_port, self.secure()) {
            (true, _) => host.to_string(),
            (false, true) => format!("{host}:443"),
            (false, false) => format!("{host}:{PORT}"),
        }
    }

    fn secure(&self) -> bool {
        self.url.starts_with("https://")
    }

    /// One client for every call, so its connections to HA are kept and
    /// reused rather than opened per request.
    fn agent() -> &'static ureq::Agent {
        static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
        AGENT.get_or_init(|| {
            let tls = ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build();
            ureq::Agent::config_builder()
                .timeout_connect(Some(WAIT))
                .timeout_global(Some(WAIT))
                .tls_config(tls)
                // An HTTP error from HA says why in its body; keep it.
                .http_status_as_error(false)
                .build()
                .new_agent()
        })
    }

    /// The JSON HA answered, or why not. What goes wrong driving a device
    /// is said in the group that asked, so it never names the hub's
    /// address on the LAN.
    fn answer(&self, sent: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Value, String> {
        let mut response = sent.map_err(unreached)?;
        let code = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_ANSWER)
            .read_to_string()
            .map_err(|e| format!("couldn't read Home Assistant's answer: {e}"))?;
        outcome(code, &body)
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        self.get_within(path, WAIT)
    }

    /// GETs `path`, allowing `wait` for the whole answer to arrive.
    fn get_within(&self, path: &str, wait: Duration) -> Result<Value, String> {
        let sent = Self::agent()
            .get(format!("{}{path}", self.url))
            .header("Authorization", format!("Bearer {}", self.token))
            .config()
            .timeout_global(Some(wait))
            .build()
            .call();
        self.answer(sent)
    }

    /// Calls a service, such as `light.turn_on`, on one entity, and returns
    /// the entity's new state if HA's answer has it. HA answers with every
    /// state the call changed; one that changes later, as a polled or
    /// cloud device may, isn't in it.
    fn call(&self, service: &str, entity: &str, body: Value) -> Result<Option<Value>, String> {
        self.call_within(service, entity, body, DRIVE)
    }

    fn call_within(&self, service: &str, entity: &str, mut body: Value, wait: Duration) -> Result<Option<Value>, String> {
        let (domain, service) = service.split_once('.').unwrap_or((service, ""));
        body["entity_id"] = json!(entity);
        let sent = Self::agent()
            .post(format!("{}/api/services/{domain}/{service}", self.url))
            .header("Authorization", format!("Bearer {}", self.token))
            .config()
            .timeout_global(Some(wait))
            .build()
            .send_json(&body);
        Ok(changed(&self.answer(sent)?, entity))
    }

    /// Calls a service on one entity, and says what it is doing now when
    /// HA's answer has it. When HA takes too long to answer, the call was
    /// still sent and HA carries on with it, so this never says it failed:
    /// it says what the entity is doing so far, and that it may yet change.
    fn drive(&self, service: &str, entity: &str, body: Value, describe: Says) -> Result<Option<String>, String> {
        self.drive_within(service, entity, body, describe, DRIVE)
    }

    fn drive_within(&self, service: &str, entity: &str, body: Value, describe: Says, wait: Duration) -> Result<Option<String>, String> {
        match self.call_within(service, entity, body, wait) {
            Ok(now) => Ok(now.as_ref().map(describe)),
            Err(e) if e == SLOW => Err(match self.state(entity) {
                Ok(state) => format!("{STILL_ON_IT}; so far it is {}", describe(&state)),
                Err(_) => format!("{TOOK_TOO_LONG}; it may still happen"),
            }),
            Err(e) => Err(e),
        }
    }

    /// One entity's state and attributes.
    fn state(&self, entity: &str) -> Result<Value, String> {
        self.get(&format!("/api/states/{entity}")).map_err(|e| {
            if e.ends_with("HTTP 404") || e.contains("Entity not found") { format!("Home Assistant has no {entity}") } else { e }
        })
    }

    /// The name of the home it runs, as a check that the URL and token work
    /// and that what answers is a Home Assistant.
    pub fn check(&self) -> Result<String, String> {
        home_name(&self.get("/api/config")?).ok_or_else(|| format!("{} answers, but not as Home Assistant", self.url))
    }

    /// The address devices from it are listed at in the panel.
    fn ip(&self) -> Ipv4Addr {
        self.host()
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| {
                addrs.find_map(|a| match a {
                    SocketAddr::V4(v4) => Some(*v4.ip()),
                    SocketAddr::V6(_) => None,
                })
            })
            .unwrap_or(Ipv4Addr::UNSPECIFIED)
    }

    /// Every entity home can drive, as found devices, each with its state.
    pub fn entities(&self) -> Result<Listing, String> {
        // Neither needs the other, and each can take a while on a big home.
        let (states, registry) = std::thread::scope(|s| {
            let registry = s.spawn(|| self.registry());
            let states = self.get_within("/api/states", LISTING);
            (states, registry.join().unwrap_or_else(|_| Err("reading it went wrong".into())))
        });
        let states = states?;
        // Without the registry, nothing is filtered out; still worth listing.
        let (registry, warning) = match registry {
            Ok(registry) => (registry, None),
            Err(e) => (
                HashMap::new(),
                Some(format!(
                    "couldn't read Home Assistant's entity registry ({e}), so its hidden and settings \
                     entities, and its own copies of Wiz and LG devices, are listed too"
                )),
            ),
        };
        let ip = self.ip();
        let found = states
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|state| {
                let entity = state.get("entity_id")?.as_str()?;
                if registry.get(entity).is_some_and(|entry| !wanted(entry)) {
                    return None;
                }
                let (backend, data, says) = describe(entity, state)?;
                let checked = Some(checked(state, says));
                Some(Found { backend, ip, mac: None, unit: Some(entity.to_string()), data, checked })
            })
            .collect();
        Ok(Listing { found, warning })
    }

    /// The entity registry, by entity id: what the REST API doesn't say,
    /// such as which integration made an entity and whether it is hidden.
    fn registry(&self) -> Result<HashMap<String, Value>, String> {
        let address = self.host();
        let stream = address
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.find_map(|a| TcpStream::connect_timeout(&a, WAIT).ok()))
            .ok_or("can't reach Home Assistant")?;
        stream.set_read_timeout(Some(LISTING)).map_err(|e| e.to_string())?;
        let scheme = if self.secure() { "wss" } else { "ws" };
        let ws_url = format!("{scheme}://{address}/api/websocket");
        let (mut ws, _) = tungstenite::client_tls(ws_url.as_str(), stream).map_err(|e| e.to_string())?;
        read(&mut ws)?; // auth_required
        let auth = json!({"type": "auth", "access_token": self.token});
        ws.send(Message::text(auth.to_string())).map_err(|e| e.to_string())?;
        if read(&mut ws)?.get("type").and_then(Value::as_str) != Some("auth_ok") {
            return Err("Home Assistant refused the token".into());
        }
        let list = json!({"id": 1, "type": "config/entity_registry/list"});
        ws.send(Message::text(list.to_string())).map_err(|e| e.to_string())?;
        let answer = loop {
            let message = read(&mut ws)?;
            if message.get("id").and_then(Value::as_u64) == Some(1) {
                break message;
            }
        };
        let _ = ws.close(None);
        registry_of(answer)
    }
}

/// The entries of a `config/entity_registry/list` answer, by entity id, or
/// why HA didn't send them, in its own words when it gave any.
fn registry_of(answer: Value) -> Result<HashMap<String, Value>, String> {
    let Some(Value::Array(entries)) = answer.get("result").cloned() else {
        return Err(match answer.pointer("/error/message").and_then(Value::as_str) {
            Some(why) => format!("Home Assistant wouldn't list it: {why}"),
            None => "Home Assistant sent no registry".into(),
        });
    };
    Ok(entries
        .into_iter()
        .filter_map(|e| Some((e.get("entity_id")?.as_str()?.to_string(), e)))
        .collect())
}

/// Why a call never got an answer, in words that are safe to say in a
/// group. Only the socket's own error is passed on, as in "Connection
/// refused"; a TLS error can name the host its certificate was for, and
/// the rest can quote the URL, so those are said in fixed words.
///
/// Running out of time before the hub was even reached (its name didn't
/// resolve, nothing accepted the connection) means nothing was sent, so
/// that is "can't reach", not [`SLOW`]: a call that is [`SLOW`] was sent,
/// and HA may still carry it out.
fn unreached(e: ureq::Error) -> String {
    match e {
        ureq::Error::Timeout(ureq::Timeout::Resolve | ureq::Timeout::Connect) => {
            "can't reach Home Assistant: no answer from its address".into()
        }
        ureq::Error::Timeout(_) => SLOW.into(),
        ureq::Error::Io(e) => format!("can't reach Home Assistant: {}", e.kind()),
        ureq::Error::Tls(_) => "can't reach Home Assistant: its certificate isn't one this machine trusts".into(),
        _ => "can't reach Home Assistant".into(),
    }
}

/// The home's name from `/api/config`, if that is what HA answers there.
fn home_name(config: &Value) -> Option<String> {
    let name = config.get("location_name").and_then(Value::as_str);
    if name.is_none() && config.get("version").is_none() {
        return None;
    }
    Some(name.unwrap_or("Home").to_string())
}

/// What HA's answer means: its JSON, or why not, in its own words when it
/// gives them, as in "Entity … does not support action media_player.volume_set".
/// A success that isn't JSON is something else answering at that address.
fn outcome(code: u16, body: &str) -> Result<Value, String> {
    let json = serde_json::from_str::<Value>(body);
    match code {
        200..=299 => json.map_err(|_| "that address answers, but not as Home Assistant".into()),
        401 => Err("Home Assistant refused the token; make a new one under your HA profile, Security".into()),
        _ => Err(match json.ok().as_ref().and_then(|b| b.get("message")).and_then(Value::as_str) {
            Some(message) => format!("Home Assistant: {message}"),
            None => format!("Home Assistant answered HTTP {code}"),
        }),
    }
}

/// `entity`'s state among those a service call changed.
fn changed(states: &Value, entity: &str) -> Option<Value> {
    states.as_array()?.iter().find(|s| s.get("entity_id").and_then(Value::as_str) == Some(entity)).cloned()
}

/// The hub an earlier build kept in an entity's data, taken out of it.
pub fn unstamp(data: &mut Value) -> Option<Hub> {
    let fields = data.as_object_mut()?;
    let url = fields.remove("url");
    let token = fields.remove("token");
    match (url?, token?) {
        (Value::String(url), Value::String(token)) => Some(Hub { url, token }),
        _ => None,
    }
}

/// Whether a device is one of Home Assistant's, driven through the hub.
pub fn drives(d: &Device) -> bool {
    backend_of(d).is_some_and(|b| b.listed_by_hub())
}

/// The entity ids a listing has. An entity's id says its domain, and so
/// which backend drives it.
pub fn listed(listing: &[Found]) -> HashSet<String> {
    listing.iter().filter_map(|f| f.unit.clone()).collect()
}

/// The next JSON message on a WebSocket.
fn read(ws: &mut tungstenite::WebSocket<MaybeTlsStream<TcpStream>>) -> Result<Value, String> {
    loop {
        match ws.read().map_err(|e| e.to_string())? {
            Message::Text(t) => return serde_json::from_str(&t).map_err(|e| e.to_string()),
            Message::Close(_) => return Err("Home Assistant closed the connection".into()),
            _ => {}
        }
    }
}

/// Whether a registry entry is something a person would ask for: not
/// hidden or disabled, not a settings or diagnostic entity, and not from
/// an integration home drives itself.
fn wanted(entry: &Value) -> bool {
    let set = |k: &str| entry.get(k).is_some_and(|v| !v.is_null());
    let platform = entry.get("platform").and_then(Value::as_str).unwrap_or_default();
    !set("hidden_by") && !set("disabled_by") && !set("entity_category") && !NATIVE.contains(&platform)
}

fn features(state: &Value) -> u64 {
    state["attributes"].get("supported_features").and_then(Value::as_u64).unwrap_or(0)
}

/// Whether a `drive` error means the call may yet work, not that it failed.
fn undecided(e: &str) -> bool {
    e.starts_with(STILL_ON_IT) || e.starts_with(TOOK_TOO_LONG)
}

/// How a backend puts an entity's state into words.
type Says = fn(&Value) -> String;

/// Whether a label HA gives (an effect, a source) is one to pass on: one
/// line, and short enough that someone might say it.
fn sayable(label: &str) -> bool {
    !label.trim().is_empty() && label.chars().count() <= MAX_LABEL && !label.chars().any(char::is_control)
}

/// Which backend drives an entity, what it keeps about it, and how its
/// state is put into words; `None` for one home has no category for.
fn describe(entity: &str, state: &Value) -> Option<(&'static str, Value, Says)> {
    let attrs = state.get("attributes").cloned().unwrap_or_default();
    let name = attrs.get("friendly_name").and_then(Value::as_str).unwrap_or(entity);
    let (domain, _) = entity.split_once('.')?;
    match domain {
        "light" => {
            let modes: Option<Vec<&str>> = attrs
                .get("supported_color_modes")
                .and_then(Value::as_array)
                .map(|modes| modes.iter().filter_map(Value::as_str).collect());
            let (colour, white, dims) = match modes {
                Some(modes) => {
                    let colour = modes.iter().any(|m| matches!(*m, "hs" | "rgb" | "rgbw" | "rgbww" | "xy"));
                    // HA turns a white temperature into a colour for a light
                    // that has colours but no color_temp mode.
                    let white = colour || modes.contains(&"color_temp");
                    // Every mode but "onoff" has a brightness.
                    (colour, white, modes.iter().any(|m| *m != "onoff"))
                }
                // Not said, as by a light that is unavailable while listed:
                // let it try everything, and HA refuses what it can't do.
                // The next listing says what it really has.
                None => (true, true, true),
            };
            // HA lists "off" among the effects, as the way to stop one.
            let effects: Vec<&str> = attrs
                .get("effect_list")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let stops = effects.iter().any(|e| is_off(e));
            let effects: Vec<&str> = effects.into_iter().filter(|e| !is_off(e) && sayable(e)).collect();
            let flash = features(state) & LIGHT_FLASH != 0;
            Some((
                "ha_light",
                json!({
                    "name": name, "colour": colour, "white": white, "dims": dims,
                    "effects": effects, "stops": stops, "flash": flash,
                }),
                describe_light,
            ))
        }
        // A speaker or receiver is a media player too, but not a TV.
        "media_player" if attrs.get("device_class").and_then(Value::as_str) == Some("tv") => {
            Some(("ha_tv", json!({"name": name}), describe_tv))
        }
        "switch" => Some(("ha_switch", json!({"name": name}), describe_switch)),
        _ => None,
    }
}

/// Whether an effect is the absence of one.
fn is_off(effect: &str) -> bool {
    matches!(scene_key(effect).as_str(), "off" | "none")
}

fn flag(d: &Device, key: &str) -> bool {
    d.data.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn entity(d: &Device) -> Result<&str, String> {
    d.unit.as_deref().ok_or_else(|| format!("{} names no Home Assistant entity", d.name))
}

/// What an entity is doing, for the Devices panel.
fn checked(state: &Value, describe: Says) -> (State, String) {
    if state.get("state").and_then(Value::as_str) == Some("unavailable") {
        (State::Offline, "unavailable in Home Assistant".into())
    } else {
        (State::Ready, describe(state))
    }
}

/// What every HA backend says about the same things.
fn check_entity(d: &Device, describe: Says) -> (State, String) {
    match Hub::of(d).and_then(|hub| hub.state(entity(d)?)) {
        Ok(state) => checked(&state, describe),
        Err(e) => (State::Offline, e),
    }
}

/// The name HA gave an entity.
fn called(d: &Device) -> Option<String> {
    d.data.get("name").and_then(Value::as_str).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::driver::Backend;
    use crate::home::light::{Caps, LightDriver, Pilot};
    use crate::home::switch::SwitchDriver;
    use crate::home::tv::TvDriver;

    #[test]
    fn urls_as_typed() {
        assert_eq!(url("homeassistant.local").unwrap(), "http://homeassistant.local:8123");
        assert_eq!(url("http://10.0.0.4:8123/").unwrap(), "http://10.0.0.4:8123");
        assert_eq!(url("10.0.0.4:80").unwrap(), "http://10.0.0.4:80");
        assert_eq!(url("https://ha.example.com").unwrap(), "https://ha.example.com", "https is on 443, not 8123");
        assert_eq!(url("https://abc.ui.nabu.casa/").unwrap(), "https://abc.ui.nabu.casa");
        assert_eq!(url("https://ha.example.com:443").unwrap(), "https://ha.example.com:443");
        assert_eq!(url("[fd00::4]").unwrap(), "http://[fd00::4]:8123");
        assert_eq!(url("http://[fd00::4]:8123").unwrap(), "http://[fd00::4]:8123");
        assert!(url("fd00::4").unwrap_err().contains("brackets"));
        assert!(url("ftp://10.0.0.4").is_err());
        assert!(url("http://10.0.0.4:8123/lovelace").is_err());
        assert!(url("http://10.0.0.4:8123?x=1").is_err());
        assert!(url("http://user:pass@10.0.0.4:8123").is_err());
        assert!(url("10.0.0.4#frag").is_err());
    }

    #[test]
    fn an_announcement_is_believed_only_about_the_lan() {
        let seen = || Some("http://10.0.0.4:8123".to_string());
        let said = |u: &str| Some(u.to_string());
        assert_eq!(announced(seen(), said("https://evil.example.com")), seen(), "where it came from wins");
        assert_eq!(announced(seen(), said("http://10.0.0.9:8123")), seen());
        assert_eq!(announced(None, said("https://evil.example.com")), None, "off the LAN: never offered");
        assert_eq!(announced(None, said("http://8.8.8.8:8123")), None);
        assert_eq!(announced(None, said("http://homeassistant.local:8123")), said("http://homeassistant.local:8123"));
        assert_eq!(announced(None, said("http://192.168.1.20:8123")), said("http://192.168.1.20:8123"));
        assert_eq!(announced(None, said("http://169.254.3.4:8123")), said("http://169.254.3.4:8123"));
        assert_eq!(announced(None, None), None);
    }

    #[test]
    fn hosts_without_their_scheme() {
        let hub = |url: &str| Hub { url: url.into(), token: "t".into() };
        assert_eq!(hub("http://10.0.0.4:8123").host(), "10.0.0.4:8123");
        assert_eq!(hub("https://ha.lan:8123").host(), "ha.lan:8123");
        assert_eq!(hub("https://abc.ui.nabu.casa").host(), "abc.ui.nabu.casa:443", "a socket needs the port https implies");
        assert_eq!(hub("https://[fd00::4]").host(), "[fd00::4]:443");
        assert!(hub("https://ha.lan:8123").secure() && !hub("http://ha.lan:8123").secure());
    }

    #[test]
    fn a_hub_never_prints_its_token() {
        let hub = Hub { url: "http://10.0.0.4:8123".into(), token: "eyJsecret".into() };
        let printed = format!("{hub:?}");
        assert!(!printed.contains("eyJsecret") && printed.contains("10.0.0.4"), "{printed}");
    }

    #[test]
    fn registry_leaves_out_what_nobody_asks_for() {
        assert!(wanted(&json!({"platform": "hue", "hidden_by": null, "entity_category": null})));
        assert!(!wanted(&json!({"platform": "hue", "hidden_by": "user"})));
        assert!(!wanted(&json!({"platform": "hue", "disabled_by": "integration"})));
        assert!(!wanted(&json!({"platform": "hue", "entity_category": "config"})));
        assert!(!wanted(&json!({"platform": "wiz"})), "a Wiz bulb is driven by home directly");
        assert!(!wanted(&json!({"platform": "androidtv_remote"})), "an Android TV is driven by home directly");
    }

    #[test]
    fn lights_say_what_they_can_do() {
        let state = json!({"entity_id": "light.strip", "state": "on", "attributes": {
            "friendly_name": "Strip", "supported_color_modes": ["color_temp", "hs"],
            "effect_list": ["Rainbow", "Night-Light", "off"], "supported_features": 44,
        }});
        let (backend, data, _) = describe("light.strip", &state).unwrap();
        assert_eq!(backend, "ha_light");
        assert_eq!(
            data,
            json!({"name": "Strip", "colour": true, "white": true, "dims": true,
                   "effects": ["Rainbow", "Night-Light"], "stops": true, "flash": true})
        );
        let dimmer = json!({"state": "off", "attributes": {"supported_color_modes": ["brightness"]}});
        let (_, data, _) = describe("light.hall", &dimmer).unwrap();
        assert_eq!((data["name"].as_str(), data["colour"].as_bool(), data["white"].as_bool()), (Some("light.hall"), Some(false), Some(false)));
        assert_eq!((data["dims"].as_bool(), data["stops"].as_bool(), data["flash"].as_bool()), (Some(true), Some(false), Some(false)));
        assert!(describe("sensor.temperature", &state).is_none());
        let rgb = json!({"state": "on", "attributes": {"supported_color_modes": ["rgb"]}});
        let (_, data, _) = describe("light.bulb", &rgb).unwrap();
        assert_eq!((data["colour"].as_bool(), data["white"].as_bool()), (Some(true), Some(true)), "HA makes white tones from colour");
        let unknown = json!({"state": "unavailable", "attributes": {}});
        let (_, data, _) = describe("light.away", &unknown).unwrap();
        assert_eq!((data["colour"].as_bool(), data["white"].as_bool(), data["dims"].as_bool()), (Some(true), Some(true), Some(true)));
    }

    #[test]
    fn listings_carry_each_entitys_state() {
        let off = json!({"state": "off", "attributes": {}});
        assert_eq!(checked(&off, describe_light), (State::Ready, "off".into()));
        let gone = json!({"state": "unavailable", "attributes": {}});
        assert_eq!(checked(&gone, describe_switch).0, State::Offline);
        let tv = json!({"state": "playing", "attributes": {"source": "Netflix"}});
        assert_eq!(checked(&tv, describe_tv).1, "playing · Netflix");
    }

    #[test]
    fn only_tvs_are_tvs() {
        let tv = json!({"attributes": {"friendly_name": "Lounge TV", "device_class": "tv"}});
        let kept = |entity: &str, state: &Value| describe(entity, state).map(|(backend, data, _)| (backend, data));
        assert_eq!(kept("media_player.lounge", &tv), Some(("ha_tv", json!({"name": "Lounge TV"}))));
        let speaker = json!({"attributes": {"device_class": "speaker"}});
        assert!(describe("media_player.kitchen", &speaker).is_none());
        let plug = json!({"attributes": {"friendly_name": "Kettle"}});
        assert_eq!(kept("switch.kettle", &plug), Some(("ha_switch", json!({"name": "Kettle"}))));
    }

    #[test]
    fn answers_say_why_not() {
        assert_eq!(outcome(200, "[]"), Ok(json!([])));
        assert!(outcome(401, "401: Unauthorized").unwrap_err().contains("refused the token"));
        let refused = r#"{"message": "Entity media_player.tv does not support action media_player.volume_set"}"#;
        assert_eq!(outcome(400, refused).unwrap_err(), "Home Assistant: Entity media_player.tv does not support action media_player.volume_set");
        assert_eq!(outcome(500, "").unwrap_err(), "Home Assistant answered HTTP 500");
    }

    #[test]
    fn something_else_answering_isnt_home_assistant() {
        assert!(outcome(200, "<html>Sign in</html>").unwrap_err().contains("not as Home Assistant"));
        assert_eq!(home_name(&json!({"location_name": "Flat", "version": "2026.10.1"})), Some("Flat".into()));
        assert_eq!(home_name(&json!({"version": "2026.10.1"})), Some("Home".into()));
        assert_eq!(home_name(&json!({"status": "ok"})), None);
        assert_eq!(home_name(&Value::Null), None);
    }

    #[test]
    fn a_call_reports_the_state_it_changed() {
        let states = json!([{"entity_id": "light.other", "state": "off"}, {"entity_id": "switch.kettle", "state": "on"}]);
        assert_eq!(changed(&states, "switch.kettle").map(|s| describe_switch(&s)), Some("on".into()));
        assert_eq!(changed(&states, "switch.fridge"), None, "not changed yet: ask instead");
        assert_eq!(changed(&Value::Null, "switch.kettle"), None);
    }

    #[test]
    fn which_devices_are_home_assistants() {
        let d = |backend: &str| Device::new(backend, "x".into(), Ipv4Addr::UNSPECIFIED, None, Value::Null);
        assert!(drives(&d("ha_light")) && drives(&d("ha_tv")) && drives(&d("ha_switch")));
        assert!(!drives(&d("wiz")) && !drives(&d("webos")) && !drives(&d("ha_something_newer")));
    }

    fn hub_at(fake: &fake::Fake, token: &str) -> Hub {
        Hub { url: fake.url.clone(), token: token.into() }
    }

    #[test]
    fn a_hub_is_checked_and_says_why_not() {
        let fake = fake::Fake::start("t", None, |_, path, _| match path {
            "/api/config" => (200, json!({"location_name": "Flat", "version": "2026.10.1"}).to_string()),
            "/api/states/light.nope" => (404, json!({"message": "Entity not found."}).to_string()),
            "/api/states/sensor.page" => (200, "<html>Sign in</html>".into()),
            _ => (500, String::new()),
        });
        let hub = hub_at(&fake, "t");
        assert_eq!(hub.check(), Ok("Flat".into()));
        assert!(hub_at(&fake, "wrong").check().unwrap_err().contains("refused the token"));
        assert_eq!(hub.state("light.nope").unwrap_err(), "Home Assistant has no light.nope");
        assert!(hub.state("sensor.page").unwrap_err().contains("not as Home Assistant"));
        assert_eq!(hub.state("light.other").unwrap_err(), "Home Assistant answered HTTP 500");
    }

    #[test]
    fn what_goes_wrong_never_says_where_the_hub_is() {
        let down = Hub { url: "http://127.0.0.1:1".into(), token: "t".into() };
        let e = down.state("switch.kettle").unwrap_err();
        assert!(e.starts_with("can't reach Home Assistant") && !e.contains("127.0.0.1"), "{e}");
        let tls = unreached(ureq::Error::Tls("certificate is not valid for ha.home.lan"));
        assert_eq!(tls, "can't reach Home Assistant: its certificate isn't one this machine trusts");
        let uri = unreached(ureq::Error::BadUri("http://ha.home.lan:8123/x".into()));
        assert!(!uri.contains("ha.home.lan"), "{uri}");
    }

    #[test]
    fn a_call_that_never_reached_the_hub_is_not_said_to_be_on_its_way() {
        // The connection timed out: nothing was sent.
        for never_sent in [ureq::Timeout::Resolve, ureq::Timeout::Connect] {
            let e = unreached(ureq::Error::Timeout(never_sent));
            assert!(e.starts_with("can't reach Home Assistant") && e != SLOW, "{e}");
        }
        // Sent, and HA is slow to say what it did.
        for sent in [ureq::Timeout::Global, ureq::Timeout::RecvResponse, ureq::Timeout::RecvBody] {
            assert_eq!(unreached(ureq::Error::Timeout(sent)), SLOW);
        }
    }

    #[test]
    fn a_refused_registry_says_why() {
        let refused = json!({"id": 1, "type": "result", "success": false, "error": {"code": "unauthorized", "message": "Unauthorized"}});
        assert_eq!(registry_of(refused).unwrap_err(), "Home Assistant wouldn't list it: Unauthorized");
        assert_eq!(registry_of(json!({"id": 1})).unwrap_err(), "Home Assistant sent no registry");
        let listed = registry_of(json!({"result": [{"entity_id": "light.desk", "platform": "hue"}, {"platform": "x"}]})).unwrap();
        assert_eq!(listed.keys().collect::<Vec<_>>(), ["light.desk"]);
    }

    fn states() -> Value {
        json!([
            {"entity_id": "light.desk", "state": "on", "attributes": {"friendly_name": "Desk", "supported_color_modes": ["brightness"]}},
            {"entity_id": "light.hidden", "state": "off", "attributes": {"supported_color_modes": ["brightness"]}},
            {"entity_id": "light.bulb", "state": "off", "attributes": {"supported_color_modes": ["hs"]}},
            {"entity_id": "switch.kettle", "state": "unavailable", "attributes": {"friendly_name": "Kettle"}},
            {"entity_id": "sensor.temperature", "state": "21", "attributes": {}},
        ])
    }

    #[test]
    fn listings_leave_out_what_the_registry_hides() {
        let registry = json!([
            {"entity_id": "light.desk", "platform": "hue"},
            {"entity_id": "light.hidden", "platform": "hue", "hidden_by": "user"},
            {"entity_id": "light.bulb", "platform": "wiz"},
        ]);
        let fake = fake::Fake::start("t", Some(registry), |_, _, _| (200, states().to_string()));
        let listing = hub_at(&fake, "t").entities().unwrap();
        assert_eq!(listing.warning, None);
        let units: Vec<&str> = listing.found.iter().filter_map(|f| f.unit.as_deref()).collect();
        assert_eq!(units, ["light.desk", "switch.kettle"]);
        assert_eq!(listing.found[0].checked, Some((State::Ready, "on".into())));
        assert_eq!(listing.found[1].checked.as_ref().map(|c| &c.0), Some(&State::Offline));
    }

    /// Slow: answers after the five seconds a status read gets (a call
    /// that drives something gets `DRIVE`, longer).
    #[test]
    #[ignore = "takes six seconds"]
    fn a_slow_listing_is_waited_for() {
        let fake = fake::Fake::start("t", Some(json!([])), |_, path, _| {
            std::thread::sleep(Duration::from_secs(6));
            (200, if path == "/api/states" { states().to_string() } else { "{}".into() })
        });
        let hub = hub_at(&fake, "t");
        assert_eq!(hub.entities().map(|l| l.found.len()), Ok(4), "a listing gets its own budget");
        assert_eq!(hub.state("light.desk").unwrap_err(), "Home Assistant took too long to answer");
    }

    #[test]
    fn a_call_that_outlasts_its_wait_says_what_it_did_so_far() {
        let fake = fake::Fake::start("t", None, |method, _, _| match method {
            "POST" => {
                std::thread::sleep(Duration::from_secs(1));
                (200, "[]".into())
            }
            _ => (200, json!({"entity_id": "switch.kettle", "state": "on", "attributes": {}}).to_string()),
        });
        let hub = hub_at(&fake, "t");
        let wait = Duration::from_millis(300);
        let e = hub.drive_within("switch.turn_on", "switch.kettle", json!({}), describe_switch, wait).unwrap_err();
        assert_eq!(e, "Home Assistant is still on it; so far it is on", "never told it failed");
        assert!(fake.asked().iter().any(|a| a.starts_with("POST /api/services/switch/turn_on")), "the call was sent");
        let down = Hub { url: "http://127.0.0.1:1".into(), token: "t".into() };
        let e = down.drive_within("switch.turn_on", "switch.kettle", json!({}), describe_switch, wait).unwrap_err();
        assert!(e.starts_with("can't reach Home Assistant"), "never sent, so said plainly: {e}");
    }

    #[test]
    fn labels_nobody_would_say_are_left_out() {
        let long = "x".repeat(MAX_LABEL + 1);
        let state = json!({"state": "on", "attributes": {"supported_color_modes": ["hs"], "effect_list": ["Rainbow", long, "two\nlines", ""]}});
        let (_, data, _) = describe("light.strip", &state).unwrap();
        assert_eq!(data["effects"], json!(["Rainbow"]));
    }

    #[test]
    fn without_the_registry_everything_is_listed_with_a_warning() {
        let fake = fake::Fake::start("t", None, |_, _, _| (200, states().to_string()));
        let listing = hub_at(&fake, "t").entities().unwrap();
        assert_eq!(listing.found.len(), 4, "every light and switch, and no sensor");
        assert!(listing.warning.unwrap().contains("entity registry"));
    }

    /// Lists and drives a real Home Assistant. `HA_URL=… HA_TOKEN=… cargo
    /// test -- --ignored live`; HA's `demo:` integration has everything it
    /// drives.
    #[test]
    #[ignore = "needs a Home Assistant with the demo integration"]
    fn live() {
        let hub = Hub { url: url(&std::env::var("HA_URL").unwrap()).unwrap(), token: std::env::var("HA_TOKEN").unwrap() };
        println!("home: {}", hub.check().unwrap());
        let listing = hub.entities().unwrap();
        assert_eq!(listing.warning, None, "the registry was read");
        let found = listing.found;
        let device = |entity: &str| {
            let f = found.iter().find(|f| f.unit.as_deref() == Some(entity)).unwrap_or_else(|| panic!("{entity} not listed"));
            assert!(f.checked.is_some(), "{entity} is listed with its state");
            Device { unit: f.unit.clone(), hub: Some(hub.clone()), ..Device::new(f.backend, entity.into(), f.ip, None, f.data.clone()) }
        };
        let units: Vec<&str> = found.iter().filter_map(|f| f.unit.as_deref()).collect();
        println!("listed: {units:?}");
        assert!(!units.contains(&"media_player.walkman"), "only TVs are TVs");

        let mut bed = device("light.bed_light");
        assert_eq!(HaLight.caps(&bed), Caps { colour: true, white: true });
        HaLight.set(&bed, &Pilot { brightness: Some(40), kelvin: Some(2700), ..Default::default() }).unwrap();
        assert_eq!(HaLight.status(&bed).unwrap(), "on · 40% · 2700K");
        HaLight.set(&bed, &Pilot { rgb: Some((255, 0, 0)), ..Default::default() }).unwrap();
        assert!(HaLight.status(&bed).unwrap().ends_with("#ff0000"), "{}", HaLight.status(&bed).unwrap());
        HaLight.set(&bed, &Pilot { on: Some(false), ..Default::default() }).unwrap();
        assert_eq!(HaLight.check(&bed), (State::Ready, "off".into()));
        HaLight.test(&mut bed).unwrap();
        assert_eq!(HaLight.status(&bed).unwrap(), "off", "testing an off light leaves it off");

        let mut tv = device("media_player.lounge_room");
        assert_eq!(HaTv.open_app(&mut tv, "YouTube").unwrap(), "media_player.lounge_room switched to youtube");
        assert!(HaTv.open_app(&mut tv, "netflix").unwrap_err().contains("it has dvd, youtube"));
        // The demo TV has no volume control, and says so through HA.
        let volume = HaTv.volume(&mut tv, Some(30), None, None).unwrap_err();
        assert!(volume.contains("does not support action media_player.volume_set"), "{volume}");
        assert!(HaTv.status(&mut tv).unwrap().contains("Chapter 1"));
        HaTv.power(&mut tv, false).unwrap();
        assert_eq!(HaTv.status(&mut tv).unwrap(), "off");
        HaTv.power(&mut tv, true).unwrap();

        let ac = device("switch.ac");
        SwitchDriver::set(&HaSwitch, &ac, true).unwrap();
        assert_eq!(SwitchDriver::status(&HaSwitch, &ac).unwrap(), "on");
        SwitchDriver::set(&HaSwitch, &ac, false).unwrap();
        assert_eq!(SwitchDriver::status(&HaSwitch, &ac).unwrap(), "off");

        let wrong = Hub { token: "nope".into(), ..hub.clone() };
        assert!(wrong.check().unwrap_err().contains("refused the token"));
        assert_eq!(hub.state("light.nope").unwrap_err(), "Home Assistant has no light.nope");
    }
}
