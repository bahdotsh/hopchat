//! TVs Home Assistant drives, as media players.

use serde_json::{json, Value};

use super::{features, sayable, MAX_SOURCES_SAID, VOLUME_SET, VOLUME_STEP, called, check_entity, entity, Hub};
use crate::home::driver::{Backend, Category};
use crate::home::{Device, State};
use crate::home::tv::TvDriver;
use crate::home::watch;

/// The bit in `supported_features` that says a player takes `play_media`.
const PLAY_MEDIA: u64 = 512;

/// TVs Home Assistant drives, as media players.
pub struct HaTv;

impl Backend for HaTv {
    fn id(&self) -> &'static str {
        "ha_tv"
    }

    fn category(&self) -> Category {
        Category::Tv
    }

    fn label(&self) -> &'static str {
        "Home Assistant TVs"
    }

    fn listed_by_hub(&self) -> bool {
        true
    }

    fn name(&self, d: &Device) -> Option<String> {
        called(d)
    }

    fn about(&self, _d: &Device) -> String {
        "through Home Assistant".into()
    }

    fn found_detail(&self, _d: &Device) -> String {
        "Home Assistant · TV".into()
    }

    fn check(&self, d: &Device) -> (State, String) {
        check_entity(d, describe_tv)
    }

    /// Nothing an HA media player does is a harmless hello, so this only
    /// says what it is doing.
    fn test(&self, d: &mut Device) -> Result<String, String> {
        self.status(d).map(|state| format!("{} is {state}", d.name))
    }

    fn test_help(&self) -> &'static str {
        " shows what it is doing"
    }

    fn as_tv(&self) -> Option<&dyn TvDriver> {
        Some(self)
    }
}

/// How to move a TV's volume by `change` percentage points.
#[derive(Debug, PartialEq)]
enum Volume {
    /// To this level, 0 to 1, in one call.
    Set(f64),
    /// By this many of the integration's own steps, for one that can't be
    /// set to a level.
    Step(u8),
}

/// The same `change` means the same on every TV: points of volume, not
/// whatever one `volume_up` happens to move by.
fn volume_change(state: &Value, change: i8) -> Volume {
    match state["attributes"].get("volume_level").and_then(Value::as_f64) {
        Some(now) if features(state) & VOLUME_SET != 0 => {
            Volume::Set((((now * 100.0).round() + f64::from(change)) / 100.0).clamp(0.0, 1.0))
        }
        _ => Volume::Step(change.unsigned_abs().div_ceil(VOLUME_STEP).max(1)),
    }
}

impl TvDriver for HaTv {
    fn power(&self, d: &mut Device, on: bool) -> Result<String, String> {
        let service = if on { "media_player.turn_on" } else { "media_player.turn_off" };
        Hub::of(d)?.drive(service, entity(d)?, json!({}), describe_tv)?;
        Ok(format!("{} is turning {}", d.name, if on { "on" } else { "off" }))
    }

    /// Every call goes through [`Hub::drive`], as power and sources do, so
    /// a slow answer says what the TV is doing so far rather than failing.
    fn volume(&self, d: &mut Device, level: Option<u8>, change: Option<i8>, mute: Option<bool>) -> Result<String, String> {
        let hub = Hub::of(d)?;
        let entity = entity(d)?;
        let call = |service: &str, body: Value| hub.drive(service, entity, body, describe_tv).map(drop);
        if let Some(mute) = mute {
            call("media_player.volume_mute", json!({"is_volume_muted": mute}))?;
        }
        let set = |level: f64| call("media_player.volume_set", json!({"volume_level": level}));
        if let Some(level) = level {
            set(f64::from(level.min(100)) / 100.0)?;
        } else if let Some(change) = change.filter(|c| *c != 0) {
            match volume_change(&hub.state(entity)?, change) {
                Volume::Set(level) => {
                    set(level)?;
                }
                Volume::Step(steps) => {
                    let service = if change > 0 { "media_player.volume_up" } else { "media_player.volume_down" };
                    for _ in 0..steps {
                        call(service, json!({}))?;
                    }
                }
            }
        }
        self.status(d).map(|state| format!("{} is {state}", d.name))
    }

    fn status(&self, d: &mut Device) -> Result<String, String> {
        Ok(describe_tv(&Hub::of(d)?.state(entity(d)?)?))
    }

    /// An app and an input are both a source to HA.
    fn open_app(&self, d: &mut Device, app: &str) -> Result<String, String> {
        self.source(d, app)
    }

    /// Whether a link gets through is up to the integration behind the
    /// TV, and HA says when it can't take one.
    fn play(&self, d: &mut Device, app: &str, what: &str) -> Result<String, String> {
        let hub = Hub::of(d)?;
        let entity = entity(d)?;
        if let Some(pick) = watch::pick(app, what).filter(|_| hub.state(entity).is_ok_and(|s| features(&s) & PLAY_MEDIA != 0)) {
            let body = json!({"media_content_type": "url", "media_content_id": pick.link()});
            hub.drive("media_player.play_media", entity, body, describe_tv)?;
            return Ok(pick.said(what));
        }
        self.source(d, app)?;
        Ok(watch::only_opened(app, what))
    }

    fn input(&self, d: &mut Device, input: &str) -> Result<String, String> {
        self.source(d, input)
    }

    fn media(&self, d: &mut Device, action: &str) -> Result<String, String> {
        let service = match action {
            "play" => "media_player.media_play",
            "pause" => "media_player.media_pause",
            "stop" => "media_player.media_stop",
            _ => return Err(format!("{} can't {action} through Home Assistant", d.name)),
        };
        Hub::of(d)?.drive(service, entity(d)?, json!({}), describe_tv)?;
        Ok(format!("{} did {action}", d.name))
    }
}

impl HaTv {
    /// Switches to the source whose name `wanted` is, or is part of.
    fn source(&self, d: &mut Device, wanted: &str) -> Result<String, String> {
        let hub = Hub::of(d)?;
        let entity = entity(d)?;
        let state = hub.state(entity)?;
        let sources: Vec<&str> = state["attributes"]
            .get("source_list")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|s| sayable(s))
            .collect();
        let Some(source) = pick_source(&sources, wanted) else {
            return Err(if sources.is_empty() {
                format!("{} has no sources to switch to", d.name)
            } else {
                format!("{} has no {wanted}; it has {}", d.name, said_sources(&sources))
            });
        };
        hub.drive("media_player.select_source", entity, json!({"source": source}), describe_tv)?;
        Ok(format!("{} switched to {source}", d.name))
    }
}

/// The sources as named back to someone: the first few, and how many more.
fn said_sources(sources: &[&str]) -> String {
    match sources.len() {
        n if n <= MAX_SOURCES_SAID => sources.join(", "),
        n => format!("{} and {} more", sources[..MAX_SOURCES_SAID].join(", "), n - MAX_SOURCES_SAID),
    }
}

/// The source `wanted` means: the same name, else the one it is part of,
/// ignoring case, spaces and punctuation ("hdmi 1" is "HDMI_1").
fn pick_source<'a>(sources: &[&'a str], wanted: &str) -> Option<&'a str> {
    let key = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
    let wanted = key(wanted);
    if wanted.is_empty() {
        return None;
    }
    sources
        .iter()
        .find(|s| key(s) == wanted)
        .or_else(|| sources.iter().find(|s| key(s).contains(&wanted)))
        .copied()
}

/// A TV's state as "on · Netflix · volume 20".
pub(super) fn describe_tv(state: &Value) -> String {
    let power = state.get("state").and_then(Value::as_str).unwrap_or("unknown");
    if matches!(power, "off" | "standby" | "unavailable") {
        return power.into();
    }
    let attrs = &state["attributes"];
    let mut parts = vec![power.to_string()];
    let showing = ["media_title", "app_name", "source"]
        .iter()
        .find_map(|k| attrs.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()));
    if let Some(showing) = showing {
        parts.push(showing.to_string());
    }
    if attrs.get("is_volume_muted").and_then(Value::as_bool) == Some(true) {
        parts.push("muted".into());
    } else if let Some(v) = attrs.get("volume_level").and_then(Value::as_f64) {
        parts.push(format!("volume {}", (v * 100.0).round() as u8));
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::super::fake::Fake;
    use super::*;

    #[test]
    fn volume_mutes_then_steps_where_it_cant_be_set() {
        let fake = Fake::start("t", None, |method, _, _| match method {
            "GET" => (200, json!({"entity_id": "media_player.tv", "state": "on",
                                  "attributes": {"volume_level": 0.2, "supported_features": 0}}).to_string()),
            _ => (200, "[]".into()),
        });
        let mut tv = Device {
            unit: Some("media_player.tv".into()),
            hub: Some(Hub { url: fake.url.clone(), token: "t".into() }),
            ..Device::new("ha_tv", "TV".into(), Ipv4Addr::LOCALHOST, None, json!({"name": "TV"}))
        };
        assert_eq!(HaTv.volume(&mut tv, None, Some(-15), Some(false)).unwrap(), "TV is on · volume 20");
        let asked = fake.asked();
        let calls: Vec<&str> = asked.iter().map(|a| a.splitn(3, ' ').take(2).last().unwrap()).collect();
        assert_eq!(calls, [
            "/api/services/media_player/volume_mute",
            "/api/states/media_player.tv",
            "/api/services/media_player/volume_down",
            "/api/services/media_player/volume_down",
            "/api/states/media_player.tv",
        ], "fifteen points is two of HA's tenth steps");
        assert!(asked[0].contains(r#""is_volume_muted":false"#), "{}", asked[0]);
    }

    #[test]
    fn volume_moves_by_points_on_every_tv() {
        let tv = |level: f64, features: u64| json!({"attributes": {"volume_level": level, "supported_features": features}});
        assert_eq!(volume_change(&tv(0.2, VOLUME_SET), -3), Volume::Set(0.17));
        assert_eq!(volume_change(&tv(0.98, VOLUME_SET | 8), 5), Volume::Set(1.0));
        assert_eq!(volume_change(&tv(0.01, VOLUME_SET), -5), Volume::Set(0.0));
        // Without a level to set, HA's own steps, a tenth each.
        assert_eq!(volume_change(&tv(0.2, 0), -3), Volume::Step(1));
        assert_eq!(volume_change(&tv(0.2, 0), 15), Volume::Step(2));
        assert_eq!(volume_change(&json!({"attributes": {"supported_features": VOLUME_SET}}), 1), Volume::Step(1));
    }

    #[test]
    fn sources_by_what_people_say() {
        let sources = ["HDMI_1", "HDMI 2", "Netflix", "YouTube TV"];
        assert_eq!(pick_source(&sources, "hdmi 1"), Some("HDMI_1"));
        assert_eq!(pick_source(&sources, "netflix"), Some("Netflix"));
        assert_eq!(pick_source(&sources, "youtube"), Some("YouTube TV"));
        assert_eq!(pick_source(&sources, "disney+"), None);
        assert_eq!(pick_source(&sources, "+"), None);
    }

    #[test]
    fn a_long_source_list_is_cut_short() {
        let names: Vec<String> = (0..25).map(|n| format!("HDMI {n}")).collect();
        let sources: Vec<&str> = names.iter().map(String::as_str).collect();
        let said = said_sources(&sources);
        assert!(said.ends_with("HDMI 19 and 5 more") && !said.contains("HDMI 20"), "{said}");
        assert_eq!(said_sources(&["Netflix", "HDMI 1"]), "Netflix, HDMI 1");
    }

    #[test]
    fn tv_states() {
        let on = json!({"state": "playing", "attributes": {"source": "Netflix", "volume_level": 0.2, "is_volume_muted": false}});
        assert_eq!(describe_tv(&on), "playing · Netflix · volume 20");
        let muted = json!({"state": "on", "attributes": {"app_name": "YouTube", "is_volume_muted": true}});
        assert_eq!(describe_tv(&muted), "on · YouTube · muted");
        assert_eq!(describe_tv(&json!({"state": "off", "attributes": {}})), "off");
    }
}
