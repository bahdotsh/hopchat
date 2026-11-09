//! "home": lets the people in a group talk to this machine's smart devices.
//!
//! The host finds and adds devices in the Devices panel (`/devices`). A
//! group message that mentions `@home`, in a group where home is switched
//! on, goes to a local model with one tool per thing the devices can do;
//! its answer goes back into the group. Group membership is the
//! permission: anyone MLS admitted to that group may ask.
//!
//! Home always answers the person at this machine, in any group, the
//! lobby (see [`crate::lobby`]) and Me, with nothing to switch on;
//! `/home on` lets the others there in too.
//!
//! The same model also answers `@ai`, for anything at all, in the groups
//! the host has switched that on for (see [`ai`]). The model needn't be on
//! this machine: with none ready here, a group member's is borrowed (see
//! [`lend`]), and this machine lends its own the same way.

pub mod ai;
mod androidtv;
mod assistant;
pub mod lend;
mod discover;
pub mod driver;
pub mod ha;
mod ipp;
mod light;
mod ollama;
pub mod pool;
mod printer;
mod switch;
mod tv;
pub mod webos;
mod tuya;
mod tunnel;
mod watch;
pub mod wiz;

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::engine::{Command, Input, Update};
use crate::lobby::LOBBY;
use crate::me::ME;
pub use driver::Category;
use driver::{backend_of, PairHelp, BACKENDS};

/// The word that addresses the devices in a group.
pub const MENTION: &str = "@home";
/// What every message from home starts with.
pub const SIGNATURE: &str = "🏠 ";
const MAX_REQUEST_CHARS: usize = 500;
const MAX_NAME_CHARS: usize = 32;
/// How often to look again at whether the model is ready, so one started
/// later is used and offered.
const MODEL_CHECK: Duration = Duration::from_secs(30);
/// Turns lent out at once; the rest are turned away.
const MAX_LENDING: usize = 2;

/// A device someone added.
#[derive(Clone, Serialize, Deserialize)]
pub struct Device {
    /// The [`Backend::id`] of what drives it. Earlier versions called this
    /// `kind`, and said "light" or "tv".
    #[serde(alias = "kind")]
    pub backend: String,
    pub name: String,
    pub ip: Ipv4Addr,
    /// What identifies it when DHCP hands it a new address.
    #[serde(default)]
    pub mac: Option<String>,
    /// Which of the things behind that address it is, for a hub that
    /// drives many (a Home Assistant entity); `None` for a device that is
    /// its own address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Whatever its backend keeps about it: a Wiz module name, a TV's
    /// client key and pinned certificate.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
    // What earlier versions kept beside the rest, folded into `data` when
    // read.
    #[serde(default, skip_serializing)]
    model: Option<String>,
    #[serde(default, skip_serializing)]
    key: Option<String>,
    #[serde(default, skip_serializing)]
    cert: Option<String>,
    /// Whatever a newer hopchat wrote that this one doesn't know, kept so
    /// saving here doesn't lose it.
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
    /// How to reach the hub that lists it, for a hub's entity. Home fills
    /// it in from its one [`Config::ha`] each time it hands the device to
    /// a backend, so a new token reaches every entity at once and none is
    /// ever written into the device itself.
    ///
    /// Only [`Home::reach`] sets it: a device taken straight from
    /// `config.devices` has none, and its backend answers that home isn't
    /// connected. Hand every device to a backend through `reach`. Private
    /// to home and its backends, so nothing outside can set it either.
    #[serde(skip)]
    hub: Option<ha::Hub>,
}

impl Device {
    pub fn new(backend: &str, name: String, ip: Ipv4Addr, mac: Option<String>, data: Value) -> Self {
        Self { backend: backend.into(), name, ip, mac, unit: None, data, model: None, key: None, cert: None, extra: Default::default(), hub: None }
    }

    /// A hub's entity is named by its unit alone, not by an address the
    /// hub's name happens to resolve to, so it keeps its id when the hub
    /// moves or doesn't resolve. Home talks to one hub at a time, so an
    /// entity's id is unique.
    pub fn id(&self) -> String {
        match (&self.unit, &self.mac) {
            (Some(unit), _) => unit.clone(),
            (None, Some(mac)) => mac.clone(),
            (None, None) => self.ip.to_string(),
        }
    }

    fn same(&self, f: &driver::Found) -> bool {
        let (backend, ip, mac) = (f.backend, f.ip, f.mac.as_deref());
        if self.unit.is_some() {
            // A hub's entity is itself wherever the hub moves to.
            return self.backend == backend && self.unit == f.unit;
        }
        self.backend == backend
            && f.unit.is_none()
            && match (self.mac.as_deref(), mac) {
                (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                _ => self.ip == ip,
            }
    }

    fn migrate(&mut self) {
        match self.backend.as_str() {
            "light" => self.backend = "wiz".into(),
            "tv" => self.backend = "webos".into(),
            _ => {}
        }
        let model = if self.backend == "wiz" { "module" } else { "model" };
        for (field, value) in [(model, self.model.take()), ("key", self.key.take()), ("cert", self.cert.take())] {
            if let Some(value) = value {
                if !self.data.is_object() {
                    self.data = json!({});
                }
                self.data[field] = json!(value);
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub devices: Vec<Device>,
    /// Groups whose members may use the devices.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Groups whose members may ask the model anything with `@ai`.
    #[serde(default)]
    pub ai_groups: Vec<String>,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_ollama")]
    pub ollama: String,
    /// Whether group members may borrow this machine's model.
    #[serde(default = "yes")]
    pub share: bool,
    /// The groups (and maybe the lobby) whose pools may use this machine's
    /// memory (`/pool give on` typed there).
    #[serde(default)]
    pub pool_groups: Vec<String>,
    /// Where llama.cpp's binaries are, when they aren't on the PATH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llama_dir: Option<PathBuf>,
    /// Whether a title asked for on the TV may be looked up off the network.
    #[serde(default)]
    pub lookups: bool,
    /// The two-letter country whose catalogue a title is looked up in, when
    /// it isn't the one this machine's language setting implies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// The Home Assistant whose entities a scan lists and every HA device
    /// is driven through: the one place its token is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ha: Option<ha::Hub>,
    // The single-bulb, single-TV settings of earlier versions, folded into
    // `devices` when read.
    #[serde(default, skip_serializing)]
    bulb: Option<Ipv4Addr>,
    #[serde(default, skip_serializing)]
    tv: Option<Ipv4Addr>,
    #[serde(default, skip_serializing)]
    tv_mac: Option<String>,
    #[serde(default, skip_serializing)]
    tv_key: Option<String>,
    #[serde(default, skip_serializing)]
    tv_cert: Option<String>,
    /// Whatever a newer hopchat wrote that this one doesn't know.
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

fn yes() -> bool {
    true
}

fn default_model() -> String {
    "qwen2.5:7b".into()
}

fn default_ollama() -> String {
    std::env::var("OLLAMA_HOST")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| ollama_url(&h))
        .unwrap_or_else(|| "http://127.0.0.1:11434".into())
}

/// `OLLAMA_HOST` as Ollama's own client reads it: the port defaults to
/// 11434, and a host left empty or as 0.0.0.0 means this machine.
fn ollama_url(host: &str) -> String {
    let (scheme, rest) = match host.split_once("://") {
        Some((scheme, rest)) => (scheme, rest),
        None => ("http", host),
    };
    let rest = rest.trim_end_matches('/');
    let (name, port) = match rest.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => (name, port),
        _ => (rest, "11434"),
    };
    let name = if name.is_empty() || name == "0.0.0.0" { "127.0.0.1" } else { name };
    format!("{scheme}://{name}:{port}")
}

impl Config {
    /// The country to look titles up in.
    fn region(&self) -> String {
        self.country.clone().unwrap_or_else(|| country_of(&std::env::var("LANG").unwrap_or_default()))
    }
}

/// The country in a locale such as `en_IN.UTF-8`, or the US without one.
fn country_of(locale: &str) -> String {
    let country = locale.split('.').next().and_then(|l| l.split_once('_')).map_or("", |(_, c)| c);
    if country.len() == 2 && country.bytes().all(|b| b.is_ascii_uppercase()) { country.into() } else { "US".into() }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            devices: Vec::new(),
            groups: Vec::new(),
            ai_groups: Vec::new(),
            model: default_model(),
            ollama: default_ollama(),
            share: true,
            pool_groups: Vec::new(),
            llama_dir: None,
            lookups: false,
            country: None,
            ha: None,
            bulb: None,
            tv: None,
            tv_mac: None,
            tv_key: None,
            tv_cert: None,
            extra: Default::default(),
        }
    }
}

impl Config {
    /// Brings settings from earlier versions up to date. Whether a token
    /// was taken out of a device, so it is saved without it at once rather
    /// than left on disk until something else is saved.
    fn migrate(&mut self) -> bool {
        let mut unstamped = false;
        if let Some(ip) = self.bulb.take() {
            self.devices.push(Device::new("wiz", "Light".into(), ip, None, Value::Null));
        }
        if let Some(ip) = self.tv.take() {
            let mut tv = Device::new("webos", "TV".into(), ip, self.tv_mac.take(), Value::Null);
            tv.key = self.tv_key.take();
            tv.cert = self.tv_cert.take();
            self.devices.push(tv);
        }
        for d in &mut self.devices {
            d.migrate();
            // Listed by an earlier build, an entity kept the hub's URL and
            // token itself.
            if ha::drives(d)
                && let Some(hub) = ha::unstamp(&mut d.data)
            {
                self.ha.get_or_insert(hub);
                unstamped = true;
            }
        }
        unstamped
    }
}

// ----- what the Devices panel shows ----------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// Found on the network, not added.
    New,
    Checking,
    Ready,
    /// Added, but not answering.
    Offline,
    /// Waiting for someone to allow it, as with the prompt on a TV.
    Pairing { until: Instant },
    Failed(String),
}

#[derive(Clone)]
pub struct DeviceView {
    pub id: String,
    pub category: Category,
    /// `Some` for a device that pairs, saying how.
    pub pairing: Option<&'static PairHelp>,
    /// What testing it does, after "Enter".
    pub test_help: &'static str,
    pub name: String,
    pub ip: Ipv4Addr,
    pub added: bool,
    pub state: State,
    /// What it is doing, or what it is.
    pub detail: String,
}

#[derive(Clone, Default, PartialEq)]
pub enum ModelState {
    #[default]
    Unknown,
    Ready,
    /// Ollama is up but the model isn't pulled.
    Missing,
    /// Ollama isn't answering.
    NoServer,
}

#[derive(Clone, Default)]
pub struct View {
    pub devices: Vec<DeviceView>,
    pub scanning: bool,
    pub model: String,
    pub model_state: ModelState,
    /// Groups home is switched on in.
    pub groups: Vec<String>,
    /// Groups that may ask the model with `@ai`.
    pub ai_groups: Vec<String>,
    /// Peers who lend their model, as "bob (qwen2.5:7b)".
    pub lenders: Vec<String>,
    pub share: bool,
}

/// What the rest of hopchat asks of home.
pub enum Request {
    /// A group message that mentioned [`MENTION`]; `mine` when the person
    /// at this machine wrote it.
    Ask { group: String, from: String, text: String, mine: bool },
    /// A group message that mentioned [`ai::MENTION`].
    Chat(ai::Question),
    /// `/home …` typed in a conversation (`group` when it is one).
    Configure { group: Option<String>, args: String },
    Scan,
    /// Add a found device: at once, or by pairing for one that pairs.
    Add(String),
    Forget(String),
    /// Show which device it is: blink a light, put a message on a TV.
    Test(String),
    Rename(String, String),
    SetGroup { group: String, on: bool },
    // Results from home's own worker threads.
    Found(driver::Found),
    /// Everything `hub` listed, at once, and what to warn of about it.
    /// Taken only while `hub` is still home's: a scan outlives `/home ha
    /// forget` or a move, and the old hub's word mustn't stand for the new.
    Listed { hub: ha::Hub, listing: ha::Listing },
    ScanDone,
    Checked { id: String, state: State, detail: String },
    /// What `hub` says one of its devices is doing; taken on the same
    /// terms as [`Request::Listed`].
    HubChecked { hub: ha::Hub, id: String, state: State, detail: String },
    /// `/home ha` reached a Home Assistant: the hub and what it has, or
    /// why not.
    HaConnected { attempt: u64, result: Result<(ha::Hub, String, ha::Listing), String> },
    /// A test finished: the device with what it learned and what to say,
    /// or why not; `before` is its data when the test began.
    Tested { id: String, before: Value, result: Result<(Box<Device>, String), String> },
    /// A pairing finished: the device with what it learned, and its detail.
    Paired { id: String, result: Result<(Box<Device>, String), String> },
    ModelChecked(ModelState),
    CheckModel,
    /// The peers that currently lend their model, from the engine.
    Lenders(Vec<String>),
    /// A group member wants this machine's model to think.
    Lend { to: String, who: String, id: String, messages: Vec<serde_json::Value>, tools: serde_json::Value },
    Answered { question: ai::Question, asked: serde_json::Value, answer: Result<String, String> },
    /// A hidden pool message from a peer.
    PoolWire { peer: pool::Peer, wire: pool::Wire },
    /// The engine lost a peer a pool may have needed.
    PoolPeerGone { address: String, who: String },
    /// Peers who give to pools, by nickname, and how much memory.
    PoolGivers(Vec<(String, u64)>),
    /// Who gives in pool `id`'s group, as the engine saw them.
    PoolFound { id: String, givers: Vec<pool::Giver> },
    /// The time for givers to answer pool `id` is up.
    PoolGathered { id: String },
    /// Every giver's tunnel for pool `id` answered, or why one didn't.
    PoolReached { id: String, result: Result<(), String> },
    /// This machine's RPC server and tunnel for pool `id` are up, or why not.
    PoolServing { id: String, result: Result<tunnel::Listener, String> },
    /// The host's llama-server loaded pool `id`'s model, or why not.
    PoolLoaded { id: String, result: Result<(), String> },
    /// A llama.cpp server this machine runs for pool `id` stopped, and why.
    PoolEnded { id: String, why: String },
}

/// A device name as it may be kept: one line, not too long.
fn clean_name(name: &str) -> String {
    name.trim().chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect::<String>().trim().to_string()
}

/// Whether `text` addresses home, and the request with the mention removed.
pub fn addressed(text: &str) -> Option<String> {
    mentioned(text, MENTION)
}

/// Whether `text` mentions `word`, and the text with the mention removed.
fn mentioned(text: &str, word: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let wordy = |c: char| c.is_alphanumeric() || c == '_';
    // "@homer" is someone else, and "bob@home.net" is an address.
    let (at, _) = lower.match_indices(word).find(|&(at, _)| {
        !lower[..at].chars().next_back().is_some_and(wordy)
            && !lower[at + word.len()..].chars().next().is_some_and(wordy)
    })?;
    let end = at + word.len();
    let after = text[end..].trim_start().trim_start_matches([',', ':']);
    let rest = format!("{} {after}", &text[..at]);
    Some(rest.split_whitespace().collect::<Vec<_>>().join(" "))
}

pub struct Home {
    path: PathBuf,
    config: Config,
    engine: Sender<Input>,
    ui: Sender<Update>,
    /// Home's own queue, for worker threads to report back on.
    me: Sender<Request>,
    /// Found on the network and not added.
    found: Vec<Device>,
    live: HashMap<String, (State, String)>,
    scanning: bool,
    model_state: ModelState,
    /// Each group's recent `@ai` chat, oldest first.
    memory: HashMap<String, Vec<serde_json::Value>>,
    lenders: Vec<String>,
    /// What the engine was last told this machine lends.
    offered: Option<String>,
    lending: Arc<AtomicUsize>,
    /// Who was last told about borrowing this model, and when, so one
    /// request doesn't note every turn.
    lent_to: Option<(String, Instant)>,
    /// The `/home ha` connection on its way, if one is.
    connecting_ha: Option<u64>,
    /// How many `/home ha` connections were started, to tell them apart.
    ha_attempts: u64,
    /// The pool this machine hosts or helps, if any.
    pools: pool::Pools,
}

impl Home {
    pub fn open(root: &Path, engine: Sender<Input>, ui: Sender<Update>, me: Sender<Request>) -> Self {
        let path = root.join("home.json");
        let mut config: Config = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let unstamped = config.migrate();
        let home = Self {
            path,
            config,
            engine,
            ui,
            me,
            found: Vec::new(),
            live: HashMap::new(),
            scanning: false,
            model_state: ModelState::Unknown,
            memory: HashMap::new(),
            lenders: Vec::new(),
            offered: None,
            lending: Arc::new(AtomicUsize::new(0)),
            lent_to: None,
            connecting_ha: None,
            ha_attempts: 0,
            pools: pool::Pools::default(),
        };
        if unstamped {
            home.save();
        }
        home
    }

    pub fn enabled(&self, group: &str) -> bool {
        self.config.groups.iter().any(|g| g == group)
    }

    /// Whether home answers a request in `group`: for everyone where it is
    /// switched on, and for the person at this machine in Me, the lobby,
    /// and any group where there are devices here to drive. Without any,
    /// it keeps quiet in a group, so the member hosting home there answers
    /// alone instead of beside a "no devices" from every other member.
    fn answers(&self, group: &str, mine: bool) -> bool {
        self.enabled(group) || (mine && (group == LOBBY || group == ME || !self.config.devices.is_empty()))
    }

    fn save(&self) {
        let result = serde_json::to_vec_pretty(&self.config)
            .map_err(std::io::Error::other)
            .and_then(|bytes| crate::engine::write_private(&self.path, &bytes));
        if let Err(e) = result {
            self.note(format!("home settings not saved: {e}"));
        }
    }

    fn note(&self, text: String) {
        let _ = self.ui.send(Update::Home(text));
    }

    fn publish(&self) {
        let _ = self.ui.send(Update::HomeView(Box::new(self.view())));
    }

    fn view(&self) -> View {
        let view = |d: &Device, added: bool| {
            let backend = backend_of(d)?;
            let fallback = if added { State::Checking } else { State::New };
            let (state, detail) = self.live.get(&d.id()).cloned().unwrap_or((fallback, String::new()));
            // An added device that never finished pairing is offered again.
            let state = if added && !backend.ready(d) && matches!(state, State::Ready) { State::New } else { state };
            Some(DeviceView {
                id: d.id(),
                category: backend.category(),
                pairing: backend.pairing(),
                test_help: backend.test_help(),
                name: d.name.clone(),
                ip: d.ip,
                added,
                state,
                detail,
            })
        };
        let devices: Vec<DeviceView> = (self.config.devices.iter().filter_map(|d| view(d, true)))
            .chain(self.found.iter().filter_map(|d| view(d, false)))
            .collect();
        View {
            devices,
            scanning: self.scanning,
            model: self.config.model.clone(),
            model_state: self.model_state.clone(),
            groups: self.config.groups.clone(),
            ai_groups: self.config.ai_groups.clone(),
            lenders: self.lenders.clone(),
            share: self.config.share,
        }
    }

    fn say(&self, group: &str, text: String) {
        self.say_as(group, SIGNATURE, text);
    }

    fn say_as(&self, group: &str, signature: &str, text: String) {
        let _ = self.engine.send(Input::Ui(Command::SendGroup {
            group: group.to_string(),
            text: format!("{signature}{text}"),
            reply: None,
            from_home: true,
        }));
    }

    pub fn run(mut self, requests: Receiver<Request>) {
        self.publish();
        self.pool_offer();
        self.scan();
        let me = self.me.clone();
        thread::spawn(move || {
            while me.send(Request::CheckModel).is_ok() {
                thread::sleep(MODEL_CHECK);
            }
        });
        for request in requests {
            self.handle(request);
            self.publish();
        }
    }

    fn handle(&mut self, request: Request) {
        match request {
            Request::Ask { group, from, text, mine } => {
                if self.answers(&group, mine) {
                    let text: String = text.chars().take(MAX_REQUEST_CHARS).collect();
                    let answer = self.ask(&group, &from, &text);
                    self.say(&group, answer);
                } else if mine {
                    self.note("home: no devices here, so whoever hosts home in this group answers".into());
                }
            }
            Request::Chat(question) => self.ai_ask(question),
            Request::Answered { question, asked, answer } => self.ai_answered(question, asked, answer),
            Request::Configure { group, args } => {
                let reply = self.configure(group.as_deref(), &args);
                self.note(reply);
            }
            Request::Scan => self.scan(),
            Request::Add(id) => self.add(&id),
            Request::Forget(id) => {
                if let Some(at) = self.config.devices.iter().position(|d| d.id() == id) {
                    let d = self.config.devices.remove(at);
                    self.live.remove(&id);
                    self.save();
                    self.note(format!("forgot {}", d.name));
                } else {
                    self.found.retain(|d| d.id() != id);
                }
            }
            Request::Test(id) => self.test(&id),
            Request::Tested { id, before, result } => self.on_tested(&id, before, result),
            Request::Rename(id, name) => {
                let name = clean_name(&name);
                if name.is_empty() {
                    return;
                }
                let taken = self.config.devices.iter().any(|d| d.id() != id && d.name.eq_ignore_ascii_case(&name));
                if taken {
                    return self.note(format!("there's already a device called {name}"));
                }
                if let Some(d) = self.config.devices.iter_mut().find(|d| d.id() == id) {
                    d.name = name;
                    self.save();
                } else if let Some(d) = self.found.iter_mut().find(|d| d.id() == id) {
                    d.name = name;
                }
            }
            Request::SetGroup { group, on } => {
                let verb = if on { "on" } else { "off" };
                let reply = self.configure(Some(&group), verb);
                self.note(reply);
            }
            Request::Found(found) => self.on_found(found),
            Request::Listed { hub, listing } => {
                if self.config.ha.as_ref() == Some(&hub) {
                    if let Some(warning) = listing.warning {
                        self.note(format!("home: {warning}"));
                    }
                    self.on_listed(listing.found);
                }
            }
            Request::HubChecked { hub, id, state, detail } => {
                if self.config.ha.as_ref() == Some(&hub) {
                    self.checked(id, state, detail);
                }
            }
            Request::HaConnected { attempt, result } => self.on_ha_connected(attempt, result),
            Request::ScanDone => self.scanning = false,
            Request::Checked { id, state, detail } => self.checked(id, state, detail),
            Request::Paired { id, result } => self.on_paired(&id, result),
            Request::ModelChecked(state) => {
                self.model_state = state;
                self.offer();
            }
            Request::CheckModel => self.check_model(),
            Request::Lenders(lenders) => self.lenders = lenders,
            Request::Lend { to, who, id, messages, tools } => self.lend(to, who, id, messages, tools),
            Request::PoolWire { peer, wire } => self.pool_wire(peer, wire),
            Request::PoolPeerGone { address, who } => self.pool_peer_gone(&address, who),
            Request::PoolGivers(givers) => self.pools.givers = givers,
            Request::PoolFound { id, givers } => self.pool_found(id, givers),
            Request::PoolGathered { id } => self.pool_gathered(id),
            Request::PoolReached { id, result } => self.pool_reached(id, result),
            Request::PoolServing { id, result } => self.pool_serving(id, result),
            Request::PoolLoaded { id, result } => self.pool_loaded(id, result),
            Request::PoolEnded { id, why } => self.pool_ended(id, why),
        }
    }

    // ----- finding and adding devices ----------------------------------------

    fn scan(&mut self) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        let hub = self.config.ha.clone();
        let mut listed_by_hub = Vec::new();
        for d in &self.config.devices {
            self.live.entry(d.id()).or_insert((State::Checking, String::new()));
            // The hub's listing says what its devices are doing, in one call.
            if hub.is_some() && ha::drives(d) {
                listed_by_hub.push(self.reach(d));
            } else {
                self.check(d.clone());
            }
        }
        let me = self.me.clone();
        let (url, model) = (self.config.ollama.clone(), self.config.model.clone());
        thread::spawn(move || {
            let _ = me.send(Request::ModelChecked(ollama::check(&url, &model)));
            let each = {
                let me = me.clone();
                move |found| {
                    let _ = me.send(Request::Found(found));
                }
            };
            let listing = hub.map(|hub| {
                let me = me.clone();
                thread::spawn(move || ha::connect::list_hub(&hub, listed_by_hub, &me))
            });
            discover::scan(each);
            if let Some(listing) = listing {
                let _ = listing.join();
            }
            let _ = me.send(Request::ScanDone);
        });
    }

    /// Keeps what a check found a device doing.
    fn checked(&mut self, id: String, state: State, detail: String) {
        // A check finishing must not overwrite a pairing in progress.
        if !matches!(self.live.get(&id), Some((State::Pairing { .. }, _))) {
            self.live.insert(id, (state, detail));
        }
    }

    /// Looks at an added device in the background and reports its state.
    fn check(&self, d: Device) {
        let d = self.reach(&d);
        let Some(backend) = backend_of(&d) else { return };
        let me = self.me.clone();
        thread::spawn(move || {
            let (state, detail) = backend.check(&d);
            let _ = me.send(Request::Checked { id: d.id(), state, detail });
        });
    }

    fn on_found(&mut self, f: driver::Found) {
        if self.learn(f) {
            self.save();
        }
    }

    /// Everything a hub listed, saved once rather than once per entity.
    fn on_listed(&mut self, found: Vec<driver::Found>) {
        let listed = ha::listed(&found);
        if self.learn_all(found) {
            self.save();
        }
        self.drop_unlisted_offers(&listed);
    }

    /// Drops the hub's entities that were offered, never added, and that
    /// `listed` doesn't have: hidden or gone in Home Assistant since. An
    /// added one stays, and is checked on its own.
    pub(super) fn drop_unlisted_offers(&mut self, listed: &HashSet<String>) {
        let unlisted = |d: &Device| ha::drives(d) && !d.unit.as_ref().is_some_and(|u| listed.contains(u));
        let gone: Vec<String> = self.found.iter().filter(|d| unlisted(d)).map(Device::id).collect();
        self.found.retain(|d| !unlisted(d));
        for id in gone {
            self.live.remove(&id);
        }
    }

    /// Takes in everything a hub listed; whether an added device changed,
    /// for the caller to save.
    fn learn_all(&mut self, found: Vec<driver::Found>) -> bool {
        let mut changed = false;
        for f in found {
            changed |= self.learn(f);
        }
        changed
    }

    /// Takes in a found device: offers it, or updates the one already
    /// here. Whether an added device changed, for the caller to save.
    fn learn(&mut self, f: driver::Found) -> bool {
        let Some(backend) = driver::backend(f.backend) else { return false };
        // Something we already have, perhaps at a new address.
        if let Some(d) = self.config.devices.iter_mut().find(|d| d.same(&f)) {
            let mut changed = false;
            let mut moved = None;
            // An unresolved hub says 0.0.0.0; that isn't a move.
            if d.ip != f.ip && !f.ip.is_unspecified() {
                // Every entity of a hub moves with it; one note per entity
                // would only be noise.
                if d.unit.is_none() {
                    moved = Some(format!("{} moved to {}", d.name, f.ip));
                }
                d.ip = f.ip;
                changed = true;
            }
            if d.mac.is_none() && f.mac.is_some() {
                // The id changes with the MAC; carry its state over.
                if let Some(live) = self.live.remove(&d.id()) {
                    d.mac = f.mac.clone();
                    self.live.insert(d.id(), live);
                } else {
                    d.mac = f.mac.clone();
                }
                changed = true;
            }
            // Anything the backend learned that it didn't have yet. What a
            // hub lists is what the hub says now, so that replaces what an
            // entity had: its effects and colour modes can change.
            let current = f.unit.is_some();
            if let Value::Object(learned) = &f.data {
                if !d.data.is_object() {
                    d.data = json!({});
                }
                for (key, value) in learned {
                    let stale = d.data.get(key).is_none_or(Value::is_null) || (current && d.data[key] != *value);
                    if stale {
                        d.data[key] = value.clone();
                        changed = true;
                    }
                }
            }
            let d = d.clone();
            if let Some(moved) = moved {
                self.note(moved);
            }
            match f.checked {
                Some((state, detail)) => self.checked(d.id(), state, detail),
                None => self.check(d),
            }
            return changed;
        }
        if let Some(d) = self.found.iter_mut().find(|d| d.same(&f)) {
            // One waiting to be added keeps up with what its hub says too.
            if f.unit.is_some() {
                d.data = f.data;
                if !f.ip.is_unspecified() {
                    d.ip = f.ip;
                }
                if let Some((State::New, detail)) = self.live.get_mut(&d.id()) {
                    *detail = backend.found_detail(d);
                }
            }
            return false;
        }
        let mut d = Device { unit: f.unit, ..Device::new(f.backend, String::new(), f.ip, f.mac, f.data) };
        let called = backend.name(&d).map(|n| clean_name(&n)).filter(|n| !n.is_empty());
        d.name = self.free_name(called.as_deref().unwrap_or(backend.category().noun()));
        self.live.insert(d.id(), (State::New, backend.found_detail(&d)));
        self.found.push(d);
        false
    }

    /// `base`, or `base 2`, `base 3`… whichever no device, added or
    /// found, has yet.
    fn free_name(&self, base: &str) -> String {
        let taken = |n: &str| {
            self.config.devices.iter().chain(&self.found).any(|d| d.name.eq_ignore_ascii_case(n))
        };
        if !taken(base) {
            return base.to_string();
        }
        (2..).map(|i| format!("{base} {i}")).find(|n| !taken(n)).unwrap()
    }

    fn add(&mut self, id: &str) {
        let candidate = match self.found.iter().position(|d| d.id() == id) {
            Some(at) => Some(self.found[at].clone()),
            // An added device whose pairing never finished can pair again.
            None => self
                .config
                .devices
                .iter()
                .find(|d| d.id() == id && backend_of(d).is_some_and(|b| b.pairing().is_some() && !b.ready(d)))
                .cloned(),
        };
        let Some(d) = candidate else { return };
        let Some(backend) = backend_of(&d) else { return };
        let Some(help) = backend.pairing() else {
            self.found.retain(|f| f.id() != id);
            let name = d.name.clone();
            self.config.devices.push(d.clone());
            self.save();
            self.live.insert(d.id(), (State::Checking, String::new()));
            self.check(d);
            return self.note(format!("added {name}"));
        };
        if matches!(self.live.get(id), Some((State::Pairing { .. }, _))) {
            return;
        }
        self.live.insert(d.id(), (State::Pairing { until: Instant::now() + help.takes }, String::new()));
        let me = self.me.clone();
        thread::spawn(move || {
            let id = d.id();
            let mut d = d;
            let result = backend.pair(&mut d).map(|detail| (Box::new(d), detail));
            let _ = me.send(Request::Paired { id, result });
        });
    }

    fn on_paired(&mut self, id: &str, result: Result<(Box<Device>, String), String>) {
        match result {
            Ok((mut paired, detail)) => {
                let d = match self.found.iter().position(|d| d.id() == id) {
                    Some(at) => self.found.remove(at),
                    None => match self.config.devices.iter().position(|d| d.id() == id) {
                        Some(at) => self.config.devices.remove(at),
                        None => return,
                    },
                };
                // It may have been renamed while it paired.
                paired.name = d.name;
                let name = paired.name.clone();
                self.live.remove(id);
                self.live.insert(paired.id(), (State::Ready, detail));
                self.config.devices.push(*paired);
                self.save();
                self.note(format!("paired with {name}"));
            }
            Err(e) => {
                self.live.insert(id.to_string(), (State::Failed(e.clone()), String::new()));
                self.note(format!("pairing failed: {e}"));
            }
        }
    }

    /// Shows which device it is, on a worker thread: a blink takes a
    /// second or more, and a hub that is down takes its whole timeout, and
    /// home answers nothing else while it waits.
    fn test(&mut self, id: &str) {
        let Some(d) = self.config.devices.iter().find(|d| d.id() == id).map(|d| self.reach(d)) else {
            return self.note("add it first, then test it".into());
        };
        let Some(backend) = backend_of(&d) else { return };
        self.checked(id.to_string(), State::Checking, String::new());
        let (me, id) = (self.me.clone(), id.to_string());
        thread::spawn(move || {
            let before = d.data.clone();
            let mut d = d;
            let result = backend.test(&mut d).map(|text| (Box::new(d), text));
            let _ = me.send(Request::Tested { id, before, result });
        });
    }

    /// A test finished: keeps what the backend learned doing it, unless
    /// something else changed the device meanwhile.
    fn on_tested(&mut self, id: &str, before: Value, result: Result<(Box<Device>, String), String>) {
        let Some(at) = self.config.devices.iter().position(|d| d.id() == id) else { return };
        match result {
            Ok((tested, text)) => {
                let d = &mut self.config.devices[at];
                if tested.data != d.data && d.data == before {
                    d.data = tested.data;
                    self.save();
                }
                self.note(text);
                self.check(self.config.devices[at].clone());
            }
            Err(e) => {
                let name = self.config.devices[at].name.clone();
                self.checked(id.to_string(), State::Failed(e.clone()), String::new());
                self.note(format!("{name}: {e}"));
            }
        }
    }

    /// Runs `f` on an added device and keeps whatever its backend learned
    /// doing it, such as a TV's new client key. `None` if there's no such
    /// device.
    fn drive<T>(&mut self, id: &str, f: impl FnOnce(&mut Device) -> T) -> Option<T> {
        let at = self.config.devices.iter().position(|d| d.id() == id)?;
        let mut d = self.reach(&self.config.devices[at]);
        let out = f(&mut d);
        if d.data != self.config.devices[at].data {
            self.config.devices[at].data = d.data;
            self.save();
        }
        Some(out)
    }

    /// `d` as its backend needs it: a hub's entity with the hub to reach.
    pub(super) fn reach(&self, d: &Device) -> Device {
        let mut d = d.clone();
        if ha::drives(&d) {
            d.hub = self.config.ha.clone();
        }
        d
    }

    // ----- /home ---------------------------------------------------------

    fn configure(&mut self, group: Option<&str>, args: &str) -> String {
        let mut words = args.split_whitespace();
        let verb = words.next().unwrap_or("status");
        let value = words.next();
        match (verb, value) {
            ("ai", verb) => self.ai_configure(group, verb.unwrap_or("status"), words.next()),
            // A model or a folder may have spaces in it, so take the rest whole.
            ("pool", verb) => self.pool_configure(group, verb.unwrap_or("status"), pool::after_words(args, 2)),
            ("on", _) => {
                let Some(group) = group else {
                    return "open a group first: /home on lets that group use your devices".into();
                };
                if !self.enabled(group) {
                    self.config.groups.push(group.to_string());
                    self.save();
                }
                let names: Vec<String> = self.config.devices.iter().map(|d| d.name.clone()).collect();
                let what = if names.is_empty() { "the devices".to_string() } else { names.join(", ") };
                self.say(group, format!("is listening. Mention {MENTION} to use {what}."));
                if group == LOBBY {
                    "everyone in the lobby can use your devices now".into()
                } else {
                    "home is on in this group".into()
                }
            }
            ("off", _) => {
                let Some(group) = group else { return "open the group to switch off".into() };
                self.config.groups.retain(|g| g != group);
                self.save();
                self.say(group, "is only listening to its owner now.".into());
                if group == LOBBY {
                    return "only you can use your devices in the lobby now".into();
                }
                "only you can use your devices in this group now".into()
            }
            ("add" | "bulb" | "light" | "tv", Some(ip)) => match ip.parse::<Ipv4Addr>() {
                Ok(ip) => self.add_by_address(ip),
                Err(_) => format!("{ip} isn't an IPv4 address"),
            },
            ("ha", value) => self.ha_configure(value, words.next()),
            ("tuya", value) => self.tuya_configure(value, words.next(), words.next()),
            ("code", value) => androidtv::give_code(value),
            ("model", Some(model)) => {
                self.config.model = model.to_string();
                self.save();
                self.model_state = ModelState::Unknown;
                self.offer();
                self.check_model();
                format!("home will use {}", self.config.model)
            }
            ("lookups", Some(value @ ("on" | "off"))) => {
                let country = words.next().map(str::to_ascii_uppercase);
                if let Some(country) = country.filter(|_| value == "on") {
                    if country.len() != 2 || !country.bytes().all(|b| b.is_ascii_uppercase()) {
                        return format!("{country} isn't a two-letter country code");
                    }
                    self.config.country = Some(country);
                }
                self.config.lookups = value == "on";
                self.save();
                if self.config.lookups {
                    format!("titles asked for on the TV now go to youtube.com and justwatch.com, looked up for {}", self.config.region())
                } else {
                    "titles stay on this network; YouTube gets a search on the TV instead".into()
                }
            }
            ("lookups", _) => {
                format!("lookups are {}; /home lookups on [COUNTRY] or off", if self.config.lookups { "on" } else { "off" })
            }
            ("scan", _) => {
                self.scan();
                "looking for devices…".into()
            }
            _ => format!(
                "home: {} device{} · model {} · on in {} group{} · /devices to manage",
                self.config.devices.len(),
                if self.config.devices.len() == 1 { "" } else { "s" },
                self.config.model,
                self.config.groups.len(),
                if self.config.groups.len() == 1 { "" } else { "s" },
            ),
        }
    }

    // ----- the model: whether it is ready, and lending it -------------------

    fn check_model(&self) {
        let me = self.me.clone();
        let (url, model) = (self.config.ollama.clone(), self.config.model.clone());
        thread::spawn(move || {
            let _ = me.send(Request::ModelChecked(ollama::check(&url, &model)));
        });
    }

    /// Where this machine's model turns get computed, for `group`.
    fn mind(&self, group: &str, local: ollama::Ollama) -> lend::Mind {
        lend::Mind {
            local,
            local_ready: self.model_state == ModelState::Ready,
            pool: self.pools.url().map(|(url, _)| url),
            engine: self.engine.clone(),
            group: group.to_string(),
        }
    }

    /// Tells the engine what this machine lends, when that changed.
    fn offer(&mut self) {
        // A pool writes every plain answer a borrower gets, so it is what
        // this machine says it lends while it runs.
        let offer = if !self.config.share {
            None
        } else if let Some((_, model)) = self.pools.url() {
            Some(format!("{model}, pooled"))
        } else {
            (self.model_state == ModelState::Ready).then(|| self.config.model.clone())
        };
        if offer != self.offered {
            self.offered = offer.clone();
            let _ = self.engine.send(Input::Ui(Command::Offer(offer)));
        }
    }

    fn lend(&mut self, to: String, who: String, id: String, messages: Vec<serde_json::Value>, tools: serde_json::Value) {
        let refuse = |error: &str| {
            let _ = self.engine.send(Input::Ui(Command::Lend { to: to.clone(), id: id.clone(), result: Err(error.into()) }));
        };
        if !self.config.share {
            return refuse("isn't sharing its model");
        }
        let tools = tools.as_array().filter(|t| !t.is_empty()).map(|_| tools.clone());
        let pool = self.pools.answerer(tools.as_ref());
        if pool.is_none() && self.model_state != ModelState::Ready {
            // The pooled model is offered for plain answers; a turn that
            // calls tools needs Ollama, so the borrower should try elsewhere.
            if self.pools.url().is_some() {
                return refuse("its pooled model only writes plain answers, and its own model isn't ready");
            }
            return refuse("its model isn't ready");
        }
        if self.lending.load(Ordering::SeqCst) >= MAX_LENDING {
            return refuse("is busy; try again in a moment");
        }
        let fresh = !matches!(&self.lent_to, Some((w, at)) if *w == who && at.elapsed() < Duration::from_secs(60));
        if fresh {
            let model = pool.as_ref().map_or(self.config.model.clone(), |(_, m)| format!("pooled {m}"));
            self.note(format!("{who} is using your {model}"));
        }
        self.lent_to = Some((who, Instant::now()));
        // A plain answer is written out in full, so it gets longer.
        let (url, model) = (self.config.ollama.clone(), self.config.model.clone());
        let local = if tools.is_some() { ollama::Ollama::new(url, model) } else { ollama::Ollama::patient(url, model) };
        let engine = self.engine.clone();
        let lending = self.lending.clone();
        lending.fetch_add(1, Ordering::SeqCst);
        thread::spawn(move || {
            let result = match pool {
                Some((url, _)) => pool::ask(&url, &messages),
                None => local.raw(&messages, tools.as_ref()),
            };
            lending.fetch_sub(1, Ordering::SeqCst);
            let _ = engine.send(Input::Ui(Command::Lend { to, id, result }));
        });
    }

    /// For a device the scan can't see (another subnet, broadcasts
    /// blocked): asks the address what it is, then offers it like a find.
    fn add_by_address(&mut self, ip: Ipv4Addr) -> String {
        match BACKENDS.iter().find_map(|b| b.probe(ip)) {
            Some(found) => self.on_found(found),
            None => {
                let labels: Vec<&str> = BACKENDS.iter().map(|b| b.label()).collect();
                return format!("nothing at {ip} answers as any of: {}", labels.join(", "));
            }
        }
        match self.found.iter().find(|d| d.ip == ip).map(Device::id) {
            Some(id) => {
                self.add(&id);
                format!("found a device at {ip}; adding it")
            }
            None => format!("{ip} is already added"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::assistant::{colour, flag, number, text};
    use super::ollama::{self, Ollama, Turn};
    use super::*;
    use super::ha::connect::{answered, found_only, ha_args, list_hub, looks_like_token};

    /// A settings directory of this test's own, so what one test saves (a
    /// hub, with its token) is never what another test opens. It is
    /// removed when dropped, so no settings outlive the test.
    struct Root(PathBuf);

    impl Root {
        fn new() -> Root {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!("hopchat-home-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            Root(root)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A home opened in a [`Root`] of its own, which goes when it does.
    struct TestHome {
        home: Home,
        _root: Root,
    }

    impl std::ops::Deref for TestHome {
        type Target = Home;
        fn deref(&self) -> &Home {
            &self.home
        }
    }

    impl std::ops::DerefMut for TestHome {
        fn deref_mut(&mut self) -> &mut Home {
            &mut self.home
        }
    }

    fn open_home(engine: Sender<Input>, ui: Sender<Update>, me: Sender<Request>) -> TestHome {
        let root = Root::new();
        TestHome { home: Home::open(&root.0, engine, ui, me), _root: root }
    }

    #[test]
    fn a_tuya_light_joins_home_once_its_key_answers() {
        const KEY: &str = "0123456789abcdef";
        let mut home = home_with(Vec::new());
        assert!(home.configure(None, "tuya").contains("/home tuya IP DEVICE_ID LOCAL_KEY"));
        assert!(home.configure(None, "tuya 127.0.0.1 bf01 short").contains("16 characters"));
        assert!(home.found.is_empty());

        let reply = home.configure(None, &format!("tuya 127.0.0.1 bf01 {KEY}"));
        assert_eq!(reply, "trying that key on the light at 127.0.0.1…");
        let waiting = home.found.iter().find(|d| d.backend == "tuya").unwrap().clone();
        assert_eq!(waiting.data, serde_json::json!({"id": "bf01", "key": KEY}));
        assert!(home.config.devices.is_empty(), "added before the key answered");

        // The pairing worker tried it, on a port nothing listens on here;
        // this is what it reports for a bulb that took the key.
        let mut paired = waiting.clone();
        let extra = serde_json::json!({"version": "3.4", "layout": "b", "colour": true, "white": true});
        for (k, v) in extra.as_object().unwrap() {
            paired.data[k] = v.clone();
        }
        home.on_paired(&waiting.id(), Ok((Box::new(paired), "on · 50%".into())));

        assert!(home.found.iter().all(|d| d.backend != "tuya"));
        assert_eq!(home.lights().len(), 1);
        let view = home.view();
        let shown = view.devices.iter().find(|d| d.category == Category::Light).unwrap();
        assert_eq!(shown.detail, "on · 50%");
        assert!(view.devices.iter().all(|d| !d.detail.contains(KEY) && !d.name.contains(KEY)));
        let again = home.configure(None, &format!("tuya 127.0.0.1 bf01 {KEY}"));
        assert!(again.contains("already added"), "{again}");
    }

    const TUYA_KEY: &str = "0123456789abcdef";

    fn paired_tuya(ip: [u8; 4], mac: &str) -> Device {
        let data = serde_json::json!({"id": "bf01", "key": TUYA_KEY, "version": "3.4", "layout": "b", "colour": true, "white": true});
        Device::new("tuya", "Lamp".into(), ip.into(), Some(mac.into()), data)
    }

    #[test]
    fn a_light_still_waiting_for_its_key_is_not_offered_to_the_model() {
        let waiting = Device::new("tuya", "Lamp".into(), [127, 0, 0, 1].into(), None, serde_json::json!({"id": "bf01", "key": TUYA_KEY}));
        let home = home_with(vec![waiting]);
        assert!(home.lights().is_empty());
        assert!(home.light_tools().is_empty());
    }

    #[test]
    fn a_tuya_light_that_moved_keeps_its_pairing() {
        let mut home = home_with(vec![paired_tuya([127, 0, 0, 9], "aa:bb:cc:dd:ee:ff")]);
        let moved = driver::Found {
            backend: "tuya",
            ip: [127, 0, 0, 8].into(),
            mac: Some("aa:bb:cc:dd:ee:ff".into()),
            unit: None,
            data: Value::Null,
            checked: None,
        };
        let reply = home.tuya_add(moved, "bf01".into(), "fedcba9876543210".into());
        assert!(reply.contains("already added"), "{reply}");
        let d = &home.config.devices[0];
        assert_eq!(d.ip, Ipv4Addr::new(127, 0, 0, 8));
        assert_eq!(d.data["key"], TUYA_KEY);
        assert_eq!(d.data["layout"], "b");
    }

    #[test]
    fn a_second_tuya_key_waits_for_the_first_try() {
        let mut home = home_with(Vec::new());
        home.configure(None, &format!("tuya 127.0.0.1 bf01 {TUYA_KEY}"));
        let reply = home.configure(None, "tuya 127.0.0.1 bf01 fedcba9876543210");
        assert!(reply.contains("still trying"), "{reply}");
        let d = home.found.iter().find(|d| d.backend == "tuya").unwrap();
        assert_eq!(d.data["key"], TUYA_KEY);
    }

    fn home_with(devices: Vec<Device>) -> TestHome {
        let (engine, _) = std::sync::mpsc::channel();
        let (ui, _) = std::sync::mpsc::channel();
        let (me, _) = std::sync::mpsc::channel();
        let mut home = open_home(engine, ui, me);
        home.config = Config { devices, ..Default::default() };
        home
    }

    fn light(name: &str, ip: [u8; 4], module: &str) -> Device {
        Device::new("wiz", name.into(), ip.into(), Some(format!("mac-{name}")), json!({"module": module}))
    }

    #[test]
    fn home_reads_a_printer_for_whoever_asks() {
        let (port, _) = ipp::fake::printer(ipp::fake::inkjet);
        let printer = Device::new("ipp", "Office printer".into(), Ipv4Addr::LOCALHOST, Some("abcd-1234".into()), json!({"port": port, "path": "ipp/print"}));
        let mut home = home_with(vec![printer, light("Desk", [10, 0, 0, 2], "ESP01_SHRGB_03")]);
        let tools = home.tools().to_string();
        assert!(tools.contains("printer_status") && !tools.contains("switch_set"));
        assert!(home.system_prompt().contains("- printer \"Office printer\""));
        assert_eq!(
            home.run_tool("printer_status", &json!({"printer": "office"})).unwrap(),
            "Office printer is printing · black 60%, cyan 8% (low) · out of paper, ink low"
        );
        assert_eq!(home.run_tool("printer_status", &json!({"printer": "garage"})).unwrap_err(), "there's no printer called garage");
    }

    #[test]
    fn every_category_icon_takes_two_columns() {
        use unicode_width::UnicodeWidthStr;
        for c in [Category::Light, Category::Tv, Category::Switch, Category::Printer] {
            assert_eq!(c.icon().width(), 2, "{c:?}");
        }
    }

    #[test]
    fn mentions() {
        assert_eq!(addressed("@home lights off").as_deref(), Some("lights off"));
        assert_eq!(addressed("hey @Home, put on netflix").as_deref(), Some("hey put on netflix"));
        assert_eq!(addressed("dim it @home").as_deref(), Some("dim it"));
        assert_eq!(addressed("ask @homer"), None);
        assert_eq!(addressed("no mention"), None);
        assert_eq!(addressed("tell @homer then @home lights off").as_deref(), Some("tell @homer then lights off"));
        assert_eq!(addressed("mail bob@home.net"), None);
        assert_eq!(ollama_url("0.0.0.0"), "http://127.0.0.1:11434");
        assert_eq!(ollama_url("box:8080"), "http://box:8080");
        assert_eq!(ollama_url("https://box/"), "https://box:11434");
    }

    /// A home whose messages to the engine can be read.
    fn wired_home() -> (TestHome, std::sync::mpsc::Receiver<Input>) {
        let (engine, from_home) = std::sync::mpsc::channel();
        let (ui, _) = std::sync::mpsc::channel();
        let (me, _) = std::sync::mpsc::channel();
        let mut home = open_home(engine, ui, me);
        home.config = Config::default();
        (home, from_home)
    }

    #[test]
    fn ai_mentions() {
        assert_eq!(ai::addressed("@ai what's a monad?").as_deref(), Some("what's a monad?"));
        assert_eq!(ai::addressed("hey @AI: translate this").as_deref(), Some("hey translate this"));
        assert_eq!(ai::addressed("said@aim"), None);
        assert_eq!(ai::addressed("@home lights off"), None);
    }

    #[test]
    fn older_settings_share_the_model_and_have_no_ai_groups() {
        let config: Config = serde_json::from_value(json!({"groups": ["g"]})).unwrap();
        assert!(config.share);
        assert!(config.ai_groups.is_empty());
    }

    #[test]
    fn lends_only_a_ready_shared_model() {
        let (mut home, engine) = wired_home();
        let refused = |engine: &std::sync::mpsc::Receiver<Input>| match engine.try_recv() {
            Ok(Input::Ui(Command::Lend { to, id, result: Err(e) })) => {
                assert_eq!((to.as_str(), id.as_str()), ("bob", "7"));
                e
            }
            _ => panic!("expected a refusal"),
        };
        let ask = || Request::Lend {
            to: "bob".into(),
            who: "bob".into(),
            id: "7".into(),
            messages: vec![json!({"role": "user", "content": "hi"})],
            tools: serde_json::Value::Null,
        };
        home.handle(ask());
        assert!(refused(&engine).contains("isn't ready"));
        home.model_state = ModelState::Ready;
        home.config.share = false;
        home.handle(ask());
        assert!(refused(&engine).contains("isn't sharing"));
    }

    #[test]
    fn offers_the_model_only_when_it_changes() {
        let (mut home, engine) = wired_home();
        home.handle(Request::ModelChecked(ModelState::Ready));
        assert!(matches!(engine.try_recv(), Ok(Input::Ui(Command::Offer(Some(m)))) if m == "qwen2.5:7b"));
        home.handle(Request::ModelChecked(ModelState::Ready));
        assert!(engine.try_recv().is_err(), "the same offer isn't sent again");
        home.handle(Request::Configure { group: None, args: "ai share off".into() });
        assert!(matches!(engine.try_recv(), Ok(Input::Ui(Command::Offer(None)))));
    }

    #[test]
    fn without_a_model_here_a_members_is_borrowed() {
        let (engine, requests) = std::sync::mpsc::channel();
        // A stand-in engine with one lender in the group.
        std::thread::spawn(move || {
            for input in requests {
                if let Input::Ui(Command::Borrow { group, skip, reply, .. }) = input {
                    assert_eq!(group, "g");
                    let _ = reply.send(if skip.is_empty() {
                        lend::Borrowed::Answer {
                            by: "bob's qwen2.5:7b".into(),
                            message: json!({"role": "assistant", "content": "four"}),
                        }
                    } else {
                        lend::Borrowed::Nobody
                    });
                }
            }
        });
        // Nothing listens on the discard port.
        let mind = lend::Mind {
            local: Ollama::new("http://127.0.0.1:9".into(), "m".into()),
            local_ready: false,
            pool: None,
            engine,
            group: "g".into(),
        };
        let thought = mind.think(&[json!({"role": "user", "content": "2+2?"})], None).unwrap();
        assert_eq!(thought.by.as_deref(), Some("bob's qwen2.5:7b"));
        assert_eq!(ollama::text_of(&thought.message), "four");
    }

    #[test]
    fn nobody_to_borrow_from_says_so() {
        let (engine, requests) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for input in requests {
                if let Input::Ui(Command::Borrow { reply, .. }) = input {
                    let _ = reply.send(lend::Borrowed::Nobody);
                }
            }
        });
        let mind = lend::Mind {
            local: Ollama::new("http://127.0.0.1:9".into(), "m".into()),
            local_ready: false,
            pool: None,
            engine,
            group: "g".into(),
        };
        let e = mind.think(&[], None).err().unwrap();
        assert!(e.contains("can't reach Ollama") && e.contains("nobody else in the group shares"), "{e}");
    }

    #[test]
    fn ai_answers_reply_to_the_question_and_remember() {
        let (mut home, engine) = wired_home();
        home.config.ai_groups.push("g".into());
        for n in 0..20 {
            let q = ai::Question {
                group: "g".into(),
                from: "bob".into(),
                id: format!("m{n}"),
                sender: "off1bob".into(),
                text: format!("question {n}"),
                mine: false,
            };
            let asked = json!({"role": "user", "content": format!("bob: question {n}")});
            home.handle(Request::Answered { question: q, asked, answer: Ok(format!("answer {n}")) });
        }
        let Ok(Input::Ui(Command::SendGroup { group, text, reply: Some(quote), from_home: true })) = engine.try_recv() else {
            panic!("expected an answer in the group");
        };
        assert_eq!((group.as_str(), text.as_str()), ("g", "🤖 answer 0"));
        assert_eq!((quote.id.as_str(), quote.sender.as_str()), ("m0", "off1bob"));
        let memory = &home.memory["g"];
        assert_eq!(memory.len(), 24, "twelve questions and their answers");
        assert_eq!(memory.last().unwrap()["content"], "answer 19");
        // Switched off: no answer, and the memory goes.
        home.handle(Request::Configure { group: Some("g".into()), args: "ai off".into() });
        assert!(!home.memory.contains_key("g"));
    }

    #[test]
    fn the_owner_is_answered_anywhere_with_nothing_switched_on() {
        let (mut home, engine) = wired_home();
        let answer = |engine: &std::sync::mpsc::Receiver<Input>| match engine.try_recv() {
            Ok(Input::Ui(Command::SendGroup { group, text, .. })) => Some((group, text)),
            _ => None,
        };
        // With no devices, home answers at once, without a model.
        let ask = |group: &str, mine| Request::Ask {
            group: group.into(),
            from: "someone".into(),
            text: "lights off".into(),
            mine,
        };
        home.handle(ask(LOBBY, false));
        assert!(answer(&engine).is_none(), "anyone else needs /home on");
        home.handle(ask("g", false));
        assert!(answer(&engine).is_none(), "the rest of a group needs /home on");
        home.handle(ask("g", true));
        assert!(answer(&engine).is_none(), "with no devices here, the group's host answers");
        home.config.devices.push(Device::new("wiz", "Lamp".into(), Ipv4Addr::new(192, 168, 1, 10), None, Value::Null));
        assert!(home.answers("g", true), "its owner is answered in any group once it has devices");
        assert!(!home.answers("g", false));
        home.config.devices.clear();
        home.handle(ask(LOBBY, true));
        let (group, text) = answer(&engine).expect("an answer in the lobby");
        assert_eq!(group, LOBBY);
        assert!(text.starts_with(SIGNATURE) && text.contains("no devices"), "{text}");

        let question = |group: &str, mine| ai::Question {
            group: group.into(),
            from: "someone".into(),
            id: "lb_1".into(),
            sender: "off1someone".into(),
            text: "hi".into(),
            mine,
        };
        let answered = |group: &str, mine| Request::Answered {
            question: question(group, mine),
            asked: json!({"role": "user", "content": "someone: hi"}),
            answer: Ok("hello".into()),
        };
        home.handle(answered(LOBBY, false));
        assert!(answer(&engine).is_none(), "anyone else needs /ai on");
        home.handle(answered(LOBBY, true));
        assert_eq!(answer(&engine), Some((LOBBY.to_string(), "🤖 hello".to_string())));
        // In a group, the owner's own model answers only while it runs here;
        // without one, whoever switched @ai on there answers alone.
        home.handle(answered("g", true));
        assert!(answer(&engine).is_none(), "no model here, no second answer in a group");
        home.model_state = ModelState::Ready;
        home.handle(answered("g", true));
        assert_eq!(answer(&engine), Some(("g".to_string(), "🤖 hello".to_string())));
        home.model_state = ModelState::default();

        // Switched on in the lobby, everyone nearby may ask.
        home.handle(Request::Configure { group: Some(LOBBY.into()), args: "on".into() });
        assert!(answer(&engine).is_some_and(|(_, t)| t.contains("is listening")));
        home.handle(ask(LOBBY, false));
        assert!(answer(&engine).is_some_and(|(_, t)| t.contains("no devices")));
        // And off again, the owner still is.
        home.handle(Request::Configure { group: Some(LOBBY.into()), args: "off".into() });
        assert!(answer(&engine).is_some_and(|(_, t)| t.contains("only listening to its owner")));
        home.handle(ask(LOBBY, false));
        assert!(answer(&engine).is_none());
        home.handle(ask(LOBBY, true));
        assert!(answer(&engine).is_some());
    }

    #[test]
    fn me_answers_only_its_owner() {
        let (mut home, engine) = wired_home();
        let answer = |engine: &std::sync::mpsc::Receiver<Input>| match engine.try_recv() {
            Ok(Input::Ui(Command::SendGroup { group, text, .. })) => Some((group, text)),
            _ => None,
        };
        let ask = |mine| Request::Ask { group: ME.into(), from: "me".into(), text: "lights off".into(), mine };
        home.handle(ask(false));
        assert!(answer(&engine).is_none(), "nobody else is ever in Me");
        home.handle(ask(true));
        let (group, text) = answer(&engine).expect("an answer in Me");
        assert_eq!(group, ME);
        assert!(text.starts_with(SIGNATURE), "{text}");

        let answered = Request::Answered {
            question: ai::Question {
                group: ME.into(),
                from: "me".into(),
                id: "me_1".into(),
                sender: "off1me".into(),
                text: "hi".into(),
                mine: true,
            },
            asked: json!({"role": "user", "content": "me: hi"}),
            answer: Ok("hello".into()),
        };
        home.handle(answered);
        assert_eq!(answer(&engine), Some((ME.to_string(), "🤖 hello".to_string())));
    }

    #[test]
    fn old_settings_become_devices() {
        let old = json!({"bulb": "192.168.1.10", "tv": "192.168.1.30", "tv_key": "k", "groups": ["g"]});
        let mut config: Config = serde_json::from_value(old).unwrap();
        config.migrate();
        assert_eq!(config.devices.len(), 2);
        assert_eq!((config.devices[0].backend.as_str(), config.devices[1].backend.as_str()), ("wiz", "webos"));
        assert_eq!(config.devices[1].data, json!({"key": "k"}));
        let saved = serde_json::to_value(&config).unwrap();
        assert!(saved.get("bulb").is_none(), "legacy fields are not written back");
    }

    #[test]
    fn devices_of_the_kind_era_find_their_backends() {
        let old = json!({"devices": [
            {"kind": "light", "name": "Desk", "ip": "10.0.0.5", "mac": "m1", "model": "ESP01_SHTW1C_31"},
            {"kind": "tv", "name": "TV", "ip": "10.0.0.6", "model": "OLED55C3", "key": "k", "cert": "c"},
        ]});
        let mut config: Config = serde_json::from_value(old).unwrap();
        config.migrate();
        let [desk, tv] = &config.devices[..] else { panic!() };
        assert_eq!((desk.backend.as_str(), &desk.data), ("wiz", &json!({"module": "ESP01_SHTW1C_31"})));
        assert_eq!((tv.backend.as_str(), &tv.data), ("webos", &json!({"model": "OLED55C3", "key": "k", "cert": "c"})));
        let saved = serde_json::to_value(&config).unwrap();
        assert_eq!(saved["devices"][1], json!({
            "backend": "webos", "name": "TV", "ip": "10.0.0.6", "mac": null,
            "data": {"model": "OLED55C3", "key": "k", "cert": "c"},
        }));
        // Read back as it was written, and a backend this hopchat doesn't know is kept, not driven.
        let mut again: Config = serde_json::from_value(saved).unwrap();
        again.migrate();
        assert_eq!(again.devices[1].data, tv.data);
        let mut home = home_with(vec![Device::new("hue", "Strip".into(), [10, 0, 0, 7].into(), None, Value::Null)]);
        assert!(home.view().devices.is_empty() && home.lights().is_empty());
        home.handle(Request::Configure { group: None, args: "status".into() });
        assert_eq!(home.config.devices.len(), 1);
    }

    #[test]
    fn found_devices_get_free_names_and_follow_their_mac() {
        let mut home = home_with(vec![light("Light", [10, 0, 0, 5], "ESP01_SHTW1C_31")]);
        home.on_found(driver::Found {
            backend: "wiz",
            ip: [10, 0, 0, 6].into(),
            mac: Some("other".into()),
            unit: None,
            data: json!({"module": "ESP01_SHRGB1C_31"}),
            checked: None,
        });
        assert_eq!(home.found[0].name, "Light 2");
        assert_eq!(home.live[&home.found[0].id()].1, "Wiz · colour");
        // The added light, answering from a new address.
        home.on_found(driver::Found {
            backend: "wiz",
            ip: [10, 0, 0, 9].into(),
            mac: Some("mac-Light".into()),
            unit: None,
            data: Value::Null,
            checked: None,
        });
        assert_eq!(home.config.devices[0].ip, Ipv4Addr::new(10, 0, 0, 9));
        assert_eq!(home.found.len(), 1, "a known device is not offered again");
    }

    #[test]
    fn things_behind_one_address_are_told_apart() {
        let mut home = home_with(Vec::new());
        let entity = |unit: &str| driver::Found {
            backend: "wiz",
            ip: [10, 0, 0, 4].into(),
            mac: Some("hub".into()),
            unit: Some(unit.into()),
            data: Value::Null,
            checked: None,
        };
        home.on_found(entity("light.desk"));
        home.on_found(entity("light.strip"));
        home.on_found(entity("light.desk"));
        let ids: Vec<String> = home.found.iter().map(Device::id).collect();
        assert_eq!(ids, ["light.desk", "light.strip"], "named by the entity alone, not the hub's address");
        let saved = serde_json::to_value(&home.found[0]).unwrap();
        assert_eq!(saved["unit"], "light.desk");
        assert!(serde_json::to_value(light("Desk", [10, 0, 0, 5], "m")).unwrap().get("unit").is_none());
    }

    fn listing(found: driver::Found) -> ha::Listing {
        ha::Listing { found: vec![found], warning: None }
    }

    fn hub(url: &str, token: &str) -> ha::Hub {
        ha::Hub { url: url.into(), token: token.into() }
    }

    fn entity(backend: &'static str, unit: &str, data: Value) -> driver::Found {
        driver::Found { backend, ip: [10, 0, 0, 4].into(), mac: None, unit: Some(unit.into()), data, checked: None }
    }

    /// The hub an added device would be driven through.
    fn reached(home: &Home, at: usize) -> Option<ha::Hub> {
        home.reach(&home.config.devices[at]).hub
    }

    #[test]
    fn home_assistant_entities_are_named_and_go_through_the_hub() {
        let mut home = home_with(Vec::new());
        let strip = || entity("ha_light", "light.strip", json!({"name": "Kitchen strip", "colour": true, "white": false, "effects": []}));
        home.ha_connected(Ok((hub("http://10.0.0.4:8123", "old"), "Home".into(), listing(strip()))));
        assert_eq!(home.found[0].name, "Kitchen strip");
        assert_eq!(home.found[0].id(), "light.strip", "an entity is named by itself, not the hub's address");
        let id = home.found[0].id();
        home.add(&id);
        assert!(home.tools().to_string().contains("\"color\""));
        // A new token reaches the devices already added, and they aren't offered again.
        home.ha_connected(Ok((hub("http://10.0.0.4:8123", "new"), "Home".into(), listing(strip()))));
        assert!(home.found.is_empty());
        assert_eq!(reached(&home, 0), Some(hub("http://10.0.0.4:8123", "new")));
    }

    #[test]
    fn a_new_token_reaches_devices_the_hub_no_longer_lists() {
        let mut home = home_with(Vec::new());
        let plug = entity("ha_switch", "switch.kettle", json!({"name": "Kettle"}));
        home.ha_connected(Ok((hub("http://10.0.0.4:8123", "old"), "Home".into(), listing(plug))));
        home.add("switch.kettle");
        // Hidden in HA since: the listing leaves it out, and it still gets the new token.
        let empty = ha::Listing { found: Vec::new(), warning: None };
        home.ha_connected(Ok((hub("http://10.0.0.4:8123", "new"), "Home".into(), empty)));
        assert_eq!(reached(&home, 0).map(|h| h.token), Some("new".into()));
    }

    #[test]
    fn the_token_is_kept_once_and_shown_nowhere() {
        let mut home = home_with(Vec::new());
        let secret = "eyJsecret.eyJpc3Mi.sig";
        let plug = entity("ha_switch", "switch.kettle", json!({"name": "Kettle"}));
        home.ha_connected(Ok((hub("http://10.0.0.4:8123", secret), "Home".into(), listing(plug))));
        home.add("switch.kettle");
        let saved = serde_json::to_string(&home.config).unwrap();
        assert_eq!(saved.matches(secret).count(), 1, "in the hub, not in its devices: {saved}");
        assert!(!home.system_prompt().contains(secret) && !home.tools().to_string().contains(secret));
        assert!(home.view().devices.iter().all(|d| !d.name.contains(secret) && !d.detail.contains(secret)));
    }

    #[test]
    fn entities_listed_by_an_earlier_build_give_up_their_token() {
        let old = json!({"devices": [{"backend": "ha_switch", "name": "Kettle", "ip": "10.0.0.4", "unit": "switch.kettle",
                                      "data": {"name": "Kettle", "url": "http://10.0.0.4:8123", "token": "t"}}]});
        let mut config: Config = serde_json::from_value(old).unwrap();
        config.migrate();
        assert_eq!(config.ha, Some(hub("http://10.0.0.4:8123", "t")));
        assert_eq!(config.devices[0].data, json!({"name": "Kettle"}));
    }

    #[test]
    fn a_token_taken_out_of_an_old_device_leaves_the_file_at_once() {
        let root = std::env::temp_dir().join(format!("hopchat-home-unstamp-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let old = json!({"devices": [{"backend": "ha_switch", "name": "Kettle", "ip": "10.0.0.4", "unit": "switch.kettle",
                                      "data": {"name": "Kettle", "url": "http://10.0.0.4:8123", "token": "eyJold.eyJ.sig"}}]});
        std::fs::write(root.join("home.json"), old.to_string()).unwrap();
        let (engine, _) = std::sync::mpsc::channel();
        let (ui, _) = std::sync::mpsc::channel();
        let (me, _) = std::sync::mpsc::channel();
        let home = Home::open(&root, engine, ui, me);
        let saved = std::fs::read_to_string(root.join("home.json")).unwrap();
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(home.config.ha, Some(hub("http://10.0.0.4:8123", "eyJold.eyJ.sig")));
        assert_eq!(saved.matches("eyJold.eyJ.sig").count(), 1, "kept in the hub only: {saved}");
        let saved: Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(saved["devices"][0]["data"], json!({"name": "Kettle"}));
    }

    #[test]
    fn a_new_token_at_the_same_address_replaces_nothing() {
        let (engine, _) = std::sync::mpsc::channel();
        let (ui, notes) = std::sync::mpsc::channel();
        let (me, _) = std::sync::mpsc::channel();
        let mut home = open_home(engine, ui, me);
        home.config = Config::default();
        let empty = || ha::Listing { found: Vec::new(), warning: None };
        home.ha_connected(Ok((hub("http://10.0.0.4:8123", "a"), "Home".into(), empty())));
        home.ha_connected(Ok((hub("http://10.0.0.4:8123", "b"), "Home".into(), empty())));
        let said: Vec<String> = notes.try_iter().filter_map(|u| match u { Update::Home(t) => Some(t), _ => None }).collect();
        assert!(!said.iter().any(|t| t.contains("replaced")), "{said:?}");
        home.ha_connected(Ok((hub("http://10.0.0.5:8123", "b"), "Home".into(), empty())));
        let said: Vec<String> = notes.try_iter().filter_map(|u| match u { Update::Home(t) => Some(t), _ => None }).collect();
        assert!(said.iter().any(|t| t.contains("http://10.0.0.4:8123 is replaced by the one at http://10.0.0.5:8123")), "{said:?}");
    }

    #[test]
    fn a_strip_with_a_hundred_effects_doesnt_fill_the_prompt() {
        let effects: Vec<String> = (0..100).map(|n| format!("Effect {n}")).collect();
        let strip = Device {
            unit: Some("light.strip".into()),
            ..Device::new("ha_light", "Strip".into(), [10, 0, 0, 4].into(), None,
                          json!({"name": "Strip", "colour": true, "white": true, "effects": effects}))
        };
        let desk = light("Desk", [10, 0, 0, 5], "ESP01_SHRGB1C_31");
        let wiz_first = driver::Backend::as_light(&wiz::Wiz).unwrap().scenes(&desk)[0].clone();
        let mut home = home_with(vec![strip, desk]);
        let prompt = home.system_prompt();
        assert!(prompt.contains("Effect 0") && prompt.contains(&wiz_first), "every light gets a share");
        assert!(!prompt.contains("Effect 99") && prompt.contains("more; use any scene name"), "{prompt}");
        // Left out of the prompt, but still a scene the strip has.
        let e = home.run_tool("light_set", &json!({"light": "strip", "scene": "effect 99"}));
        assert!(!matches!(&e, Err(e) if e.contains("no scene called")), "{e:?}");
    }

    #[test]
    fn forgetting_home_assistant_forgets_what_it_only_offered() {
        let mut home = home_with(Vec::new());
        let kettle = entity("ha_switch", "switch.kettle", json!({"name": "Kettle"}));
        home.ha_connected(Ok((hub("http://10.0.0.4:8123", "t"), "Home".into(), listing(kettle))));
        assert!(home.live.contains_key("switch.kettle"));
        home.forget_ha();
        assert!(home.found.is_empty() && !home.live.contains_key("switch.kettle"));
    }

    /// `/home ha URL TOKEN` against a real Home Assistant, through to the
    /// tools the model calls. `HA_URL=… HA_TOKEN=… cargo test -- --ignored
    /// live`, with HA's `demo:` integration.
    #[test]
    #[ignore = "needs a Home Assistant with the demo integration"]
    fn live_home_assistant() {
        let (engine, _) = std::sync::mpsc::channel();
        let (ui, _) = std::sync::mpsc::channel();
        let (me, requests) = std::sync::mpsc::channel();
        let mut home = open_home(engine, ui, me);
        home.config = Config::default();
        let (url, token) = (std::env::var("HA_URL").unwrap(), std::env::var("HA_TOKEN").unwrap());
        assert_eq!(home.configure(None, &format!("ha {url} {token}")), "connecting to Home Assistant…");
        let connected = requests.recv_timeout(Duration::from_secs(20)).unwrap();
        assert!(matches!(connected, Request::HaConnected { result: Ok(_), .. }));
        home.handle(connected);
        let names: Vec<String> = home.found.iter().map(|d| d.name.clone()).collect();
        println!("found: {names:?}");
        for name in ["Bed Light", "Lounge room", "AC"] {
            let id = home.found.iter().find(|d| d.name == name).unwrap().id();
            home.add(&id);
        }
        let tools = home.tools().to_string();
        assert!(tools.contains("light_set") && tools.contains("tv_open_app") && tools.contains("tv_play") && tools.contains("switch_set"));
        println!("{}", home.system_prompt());
        let run = |home: &mut Home, tool: &str, args: Value| home.run_tool(tool, &args).unwrap();
        assert_eq!(run(&mut home, "light_set", json!({"light": "bed light", "brightness": "25%", "color": "blue"})), "Bed Light is now on · 25% · #0000ff");
        assert_eq!(run(&mut home, "light_set", json!({"light": "bed light", "scene": "Rainbow"})), "Bed Light is now on · 25% · rainbow");
        assert_eq!(run(&mut home, "tv_open_app", json!({"app": "youtube"})), "Lounge room switched to youtube");
        assert_eq!(run(&mut home, "switch_set", json!({"switch": "ac", "on": true})), "AC is now on");
        assert_eq!(run(&mut home, "switch_set", json!({"switch": "ac", "on": false})), "AC is now off");
    }

    /// A home whose notes to the person at this machine can be read, and
    /// whose own queue can be.
    fn noted_home() -> (TestHome, std::sync::mpsc::Receiver<Update>, std::sync::mpsc::Receiver<Request>) {
        let (engine, _) = std::sync::mpsc::channel();
        let (ui, notes) = std::sync::mpsc::channel();
        let (me, requests) = std::sync::mpsc::channel();
        let mut home = open_home(engine, ui, me);
        home.config = Config::default();
        (home, notes, requests)
    }

    fn said(notes: &std::sync::mpsc::Receiver<Update>) -> Vec<String> {
        notes.try_iter().filter_map(|u| match u { Update::Home(t) => Some(t), _ => None }).collect()
    }

    #[test]
    fn a_scan_that_couldnt_filter_says_so() {
        // No registry: everything is listed, with a warning.
        let fake = ha::fake::Fake::start("t", None, |_, _, _| {
            (200, json!([{"entity_id": "switch.kettle", "state": "on", "attributes": {}}]).to_string())
        });
        let (me, requests) = std::sync::mpsc::channel();
        list_hub(&hub(&fake.url, "t"), Vec::new(), &me);
        drop(me);
        let listed = requests.into_iter().find(|r| matches!(r, Request::Listed { .. })).expect("a listing");
        let (mut home, notes, _) = noted_home();
        home.config.ha = Some(hub(&fake.url, "t"));
        home.handle(listed);
        let said = said(&notes);
        assert!(said.iter().any(|t| t.contains("entity registry")), "{said:?}");
        assert_eq!(home.found.len(), 1, "still listed");
    }

    #[test]
    fn another_home_assistant_at_the_same_address_is_not_trusted_blindly() {
        let mut home = home_with(Vec::new());
        let at = "http://10.0.0.4:8123";
        let light = |name: &str| entity("ha_light", "light.kitchen", json!({"name": name, "colour": true, "white": true}));
        let desk = entity("ha_switch", "switch.desk", json!({"name": "Desk"}));
        home.ha_connected(Ok((hub(at, "a"), "Home".into(), ha::Listing { found: vec![light("Kitchen"), desk], warning: None })));
        home.add("light.kitchen");
        home.add("switch.desk");
        // A reinstalled HA at the same address: its light.kitchen is
        // something else, and it has no desk (hidden, perhaps).
        home.ha_connected(Ok((hub(at, "b"), "Home".into(), listing(light("Garage strip")))));
        let names: Vec<&str> = home.config.devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Desk"], "the kitchen light isn't vouched for; the desk may only be hidden");
        assert_eq!(home.found.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["Garage strip"]);
    }

    #[test]
    fn a_device_without_its_ha_name_is_kept_where_its_entity_is_listed() {
        let unnamed = |unit: &str| Device { unit: Some(unit.into()), ..Device::new("ha_light", unit.into(), [10, 0, 0, 4].into(), None, json!({})) };
        let mut home = home_with(vec![unnamed("light.kitchen"), unnamed("light.porch")]);
        home.config.ha = Some(hub("http://10.0.0.4:8123", "a"));
        let kitchen = entity("ha_light", "light.kitchen", json!({"name": "Kitchen"}));
        home.ha_connected(Ok((hub("http://10.0.0.9:8123", "a"), "Home".into(), listing(kitchen))));
        let units: Vec<&str> = home.config.devices.iter().filter_map(|d| d.unit.as_deref()).collect();
        assert_eq!(units, ["light.kitchen"], "listed: kept; not listed by a hub that moved: forgotten");
    }

    #[test]
    fn one_home_assistant_connection_at_a_time() {
        let (mut home, notes, requests) = noted_home();
        let token = "eyJhbGciOi.eyJpc3Mi.sig";
        // Nothing listens on port 1, so this fails at once.
        assert_eq!(home.configure(None, &format!("ha http://127.0.0.1:1 {token}")), "connecting to Home Assistant…");
        assert!(home.configure(None, &format!("ha http://127.0.0.1:2 {token}")).starts_with("already connecting"));
        let answered = requests.recv_timeout(Duration::from_secs(10)).unwrap();
        home.handle(answered);
        assert!(said(&notes).iter().any(|t| t.starts_with("home: can't reach Home Assistant")));
        assert_eq!(home.connecting_ha, None, "a failure ends the attempt");
        // A connection that answers after /home ha forget is dropped.
        let kettle = entity("ha_switch", "switch.kettle", json!({"name": "Kettle"}));
        home.configure(None, &format!("ha http://127.0.0.1:1 {token}"));
        home.forget_ha();
        let late = Request::HaConnected { attempt: home.ha_attempts, result: Ok((hub("http://10.0.0.4:8123", "t"), "Home".into(), listing(kettle))) };
        home.handle(late);
        assert!(home.config.ha.is_none() && home.found.is_empty());
    }

    #[test]
    fn a_hub_entity_stays_put_and_keeps_up_with_the_hub() {
        let strip = |ip: [u8; 4], effects: Value| driver::Found {
            backend: "ha_light",
            ip: ip.into(),
            mac: None,
            unit: Some("light.strip".into()),
            data: json!({"name": "Strip", "colour": true, "white": false, "effects": effects}),
            checked: None,
        };
        let mut home = home_with(Vec::new());
        home.on_found(strip([10, 0, 0, 4], json!(["Rainbow"])));
        home.add("light.strip");
        // The hub's name didn't resolve this time: not a move.
        home.on_found(strip([0, 0, 0, 0], json!(["Rainbow"])));
        assert_eq!(home.config.devices[0].ip, Ipv4Addr::new(10, 0, 0, 4));
        // A real move keeps the id, so its state isn't lost.
        home.on_found(strip([10, 0, 0, 7], json!(["Rainbow"])));
        assert_eq!(home.config.devices[0].ip, Ipv4Addr::new(10, 0, 0, 7));
        assert_eq!(home.config.devices[0].id(), "light.strip");
        assert!(home.live.contains_key("light.strip"));
        // What the hub says now replaces what it said before.
        home.on_found(strip([10, 0, 0, 7], json!(["Fire", "Rainbow"])));
        assert_eq!(home.config.devices[0].data["effects"], json!(["Fire", "Rainbow"]));
        assert!(home.system_prompt().contains("Fire"));
    }

    #[test]
    fn home_assistant_address_then_token() {
        let (url, token) = ("http://10.0.0.4:8123", "eyJhbGciOi.eyJpc3Mi.sig");
        assert_eq!(ha_args(url, Some(token)), Ok((Some(url.into()), token.into())));
        assert_eq!(ha_args("homeassistant.local", Some(token)), Ok((Some("homeassistant.local".into()), token.into())));
        assert_eq!(ha_args(token, None), Ok((None, token.into())));
        // Mistakes are said back, and never echo the token.
        // Two tokens: the first is never tried, or quoted, as an address.
        for (first, second) in [
            (token, Some(url)),
            (token, Some("ha.home.lan")),
            (token, Some(token)),
            (url, None),
            ("homeassistant.local", None),
            ("10.0.0.4", None),
            ("status", None),
            ("help", None),
        ] {
            let e = ha_args(first, second).unwrap_err();
            assert!(e.contains("/home ha URL TOKEN") && !e.contains(token), "{first} {second:?}: {e}");
        }
        assert!(looks_like_token(token) && !looks_like_token("ha.home.lan") && !looks_like_token("10.0.0.4"));
    }

    #[test]
    fn a_lone_address_asks_for_the_token_and_connects_nowhere() {
        let mut home = home_with(Vec::new());
        assert!(home.configure(None, "ha http://10.0.0.4:8123").starts_with("give the token too"));
    }

    #[test]
    fn an_attempt_that_panics_still_answers() {
        let result: Result<(), String> = answered(|| panic!("the TLS stack fell over"));
        assert!(result.unwrap_err().contains("went wrong"));
        assert_eq!(answered(|| Ok::<_, String>(4)), Ok(4));
    }

    #[test]
    fn a_word_that_isnt_a_token_looks_for_nothing() {
        let mut home = home_with(Vec::new());
        for word in ["status", "help", "eyJnot-quite"] {
            let reply = home.configure(None, &format!("ha {word}"));
            assert!(reply.starts_with("that isn't a Home Assistant token") && !reply.contains(word), "{word}: {reply}");
        }
        assert_eq!(home.connecting_ha, None, "no find was started");
    }

    #[test]
    fn one_waiting_to_be_added_keeps_up_with_its_hub() {
        let mut home = home_with(Vec::new());
        let strip = |effects: Value| entity("ha_light", "light.strip", json!({"name": "Strip", "colour": true, "white": true, "effects": effects}));
        home.on_listed(vec![strip(json!(["Rainbow"]))]);
        home.on_listed(vec![strip(json!(["Fire", "Rainbow"]))]);
        assert_eq!(home.found.len(), 1);
        assert_eq!(home.found[0].data["effects"], json!(["Fire", "Rainbow"]));
        home.add("light.strip");
        assert!(home.system_prompt().contains("Fire"), "added with what the hub says now");
    }

    #[test]
    fn a_listing_says_what_each_device_is_doing() {
        let mut home = home_with(Vec::new());
        let kettle = |checked| driver::Found {
            backend: "ha_switch",
            ip: [10, 0, 0, 4].into(),
            mac: None,
            unit: Some("switch.kettle".into()),
            data: json!({"name": "Kettle"}),
            checked,
        };
        home.on_found(kettle(None));
        home.add("switch.kettle");
        home.on_found(kettle(Some((State::Ready, "on".into()))));
        assert_eq!(home.live["switch.kettle"], (State::Ready, "on".into()));
    }

    #[test]
    fn home_has_one_hub_and_another_address_replaces_it() {
        let mut home = home_with(Vec::new());
        let (first, second) = (hub("http://10.0.0.4:8123", "a"), hub("http://10.0.0.5:8123", "b"));
        let plug = |unit: &str| entity("ha_switch", unit, json!({"name": unit}));
        home.ha_connected(Ok((first, "Home".into(), listing(plug("switch.kettle")))));
        home.add("switch.kettle");
        home.ha_connected(Ok((second.clone(), "Home".into(), listing(plug("switch.fan")))));
        assert_eq!(home.config.ha, Some(second.clone()));
        assert!(home.config.devices.is_empty(), "the new hub has no kettle, so it can't drive one");
        assert_eq!(home.found.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["switch.fan"]);
        assert!(!home.live.contains_key("switch.kettle"));
    }

    #[test]
    fn another_homes_entity_of_the_same_id_is_not_driven() {
        let mut home = home_with(Vec::new());
        let light = |name: &str| entity("ha_light", "light.kitchen", json!({"name": name, "colour": true, "white": true}));
        let desk = entity("ha_switch", "switch.desk", json!({"name": "Desk"}));
        let first = ha::Listing { found: vec![light("Kitchen"), desk], warning: None };
        home.ha_connected(Ok((hub("http://10.0.0.4:8123", "a"), "Home".into(), first)));
        home.add("light.kitchen");
        home.add("switch.desk");
        home.config.devices[0].name = "Kitchen ceiling".into();
        // Same home's HA at a new address: the desk is still the Desk.
        // Another HA's light.kitchen is someone's "Garage strip".
        let second = ha::Listing { found: vec![light("Garage strip"), entity("ha_switch", "switch.desk", json!({"name": "Desk"}))], warning: None };
        home.ha_connected(Ok((hub("http://10.0.0.9:8123", "b"), "Home".into(), second)));
        let names: Vec<&str> = home.config.devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Desk"], "kept where the new hub vouches for it, and only there");
        assert_eq!(reached(&home, 0), Some(hub("http://10.0.0.9:8123", "b")));
        assert_eq!(home.found.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["Garage strip"], "offered, to add if wanted");
    }

    #[test]
    fn a_test_runs_off_home_and_reports_back() {
        let (engine, _) = std::sync::mpsc::channel();
        let (ui, notes) = std::sync::mpsc::channel();
        let (me, requests) = std::sync::mpsc::channel();
        let mut home = open_home(engine, ui, me);
        let plug = Device { unit: Some("switch.fridge".into()), ..Device::new("ha_switch", "Fridge".into(), [10, 0, 0, 4].into(), None, json!({})) };
        home.config = Config { devices: vec![plug], ..Default::default() };
        home.handle(Request::Test("switch.fridge".into()));
        assert_eq!(home.live["switch.fridge"].0, State::Checking, "home answers at once, the test still running");
        let tested = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(tested, Request::Tested { .. }));
        home.handle(tested);
        assert!(matches!(home.live["switch.fridge"].0, State::Failed(_)), "no hub to test it through");
        let said: Vec<String> = notes.try_iter().filter_map(|u| match u { Update::Home(t) => Some(t), _ => None }).collect();
        assert!(said.iter().any(|t| t.starts_with("Fridge: home isn't connected")), "{said:?}");
    }

    #[test]
    fn all_the_lights_take_as_long_as_the_slowest() {
        let fake = ha::fake::Fake::start("t", None, |method, _, _| {
            std::thread::sleep(Duration::from_millis(400));
            match method {
                "POST" => (200, "[]".into()),
                _ => (200, json!({"state": "off", "attributes": {}}).to_string()),
            }
        });
        let at = hub(&fake.url, "t");
        let lamps: Vec<Device> = (0..8)
            .map(|n| Device { unit: Some(format!("light.lamp_{n}")), ..Device::new("ha_light", format!("Lamp {n}"), [127, 0, 0, 1].into(), None, json!({})) })
            .collect();
        let mut home = home_with(lamps);
        home.config.ha = Some(at);
        let started = Instant::now();
        let said = home.run_tool("light_set", &json!({"light": "all", "on": false})).unwrap();
        assert!(started.elapsed() < Duration::from_secs(3), "eight lights side by side, not one after another: {:?}", started.elapsed());
        assert!(said.starts_with("Lamp 0 is now off; Lamp 1 is now off"), "{said}");
    }

    /// What list_hub reported for each device, by id.
    fn listed_states(hub: &ha::Hub, added: Vec<Device>) -> HashMap<String, State> {
        let (me, requests) = std::sync::mpsc::channel();
        list_hub(hub, added, &me);
        drop(me);
        requests
            .into_iter()
            .filter_map(|r| match r {
                Request::HubChecked { hub: by, id, state, .. } if by == *hub => Some((id, state)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_scan_that_outlived_its_hub_is_not_believed() {
        let mut home = home_with(Vec::new());
        let old = hub("http://10.0.0.4:8123", "a");
        let kettle = || entity("ha_switch", "switch.kettle", json!({"name": "Kettle"}));
        home.ha_connected(Ok((old.clone(), "Home".into(), listing(kettle()))));
        home.add("switch.kettle");
        // Forgotten while a scan was still listing: the listing arrives late.
        home.forget_ha();
        home.handle(Request::Listed { hub: old.clone(), listing: listing(kettle()) });
        home.handle(Request::HubChecked { hub: old.clone(), id: "switch.kettle".into(), state: State::Ready, detail: "on".into() });
        assert!(home.found.is_empty() && home.config.devices.is_empty() && home.live.is_empty(), "nothing comes back from the dead");
        // Replaced by another hub while scanning: the old one's entities aren't offered against the new.
        let new = hub("http://10.0.0.9:8123", "b");
        home.ha_connected(Ok((new.clone(), "Home".into(), listing(entity("ha_switch", "switch.fan", json!({"name": "Fan"}))))));
        home.add("switch.fan");
        home.handle(Request::Listed { hub: old.clone(), listing: listing(kettle()) });
        home.handle(Request::HubChecked { hub: old, id: "switch.fan".into(), state: State::Offline, detail: "down".into() });
        assert!(home.found.is_empty(), "the old hub's kettle isn't offered");
        assert_ne!(home.live["switch.fan"].0, State::Offline, "the old hub doesn't speak for the new one's fan");
        home.handle(Request::Listed { hub: new.clone(), listing: listing(entity("ha_switch", "switch.fan", json!({"name": "Fan"}))) });
        home.handle(Request::HubChecked { hub: new, id: "switch.fan".into(), state: State::Offline, detail: "down".into() });
        assert_eq!(home.live["switch.fan"].0, State::Offline, "the current hub does");
    }

    #[test]
    fn the_same_hub_renaming_an_entity_is_a_rename() {
        let mut home = home_with(Vec::new());
        let at = hub("http://10.0.0.4:8123", "a");
        let light = |name: &str| entity("ha_light", "light.kitchen", json!({"name": name, "colour": true, "white": true}));
        home.ha_connected(Ok((at.clone(), "Home".into(), listing(light("Kitchen")))));
        home.add("light.kitchen");
        // Connected again to the same hub, same token: renamed in HA since.
        home.ha_connected(Ok((at, "Home".into(), listing(light("Kitchen ceiling")))));
        assert_eq!(home.config.devices.len(), 1, "kept: the hub that answers to this token is the one that listed it");
        assert_eq!(home.config.devices[0].data["name"], "Kitchen ceiling", "and kept up with, as a scan would");
        assert!(home.found.is_empty());
    }

    #[test]
    fn an_offer_the_hub_stops_listing_goes() {
        let mut home = home_with(Vec::new());
        home.config.ha = Some(hub("http://10.0.0.4:8123", "a"));
        let strip = entity("ha_light", "light.strip", json!({"name": "Strip", "colour": true, "white": true}));
        let plug = entity("ha_switch", "switch.kettle", json!({"name": "Kettle"}));
        home.on_listed(vec![strip, plug]);
        home.add("switch.kettle");
        // Hidden in HA since: the strip is withdrawn, the added kettle stays.
        home.on_listed(vec![entity("ha_light", "light.desk", json!({"name": "Desk", "colour": false, "white": false}))]);
        let offered: Vec<&str> = home.found.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(offered, ["Desk"]);
        assert!(!home.live.contains_key("light.strip"));
        assert_eq!(home.config.devices.len(), 1);
    }

    #[test]
    fn finding_home_assistant_never_sends_the_token() {
        let (mut home, notes, requests) = noted_home();
        let token = "eyJhbGciOi.eyJpc3Mi.sig";
        // A token alone only looks; the only way to send it is with an address typed.
        let reply = home.configure(None, &format!("ha {token}"));
        assert!(reply.starts_with("looking for"), "{reply}");
        let answered = requests.recv_timeout(Duration::from_secs(10)).unwrap();
        let Request::HaConnected { result: Err(e), .. } = &answered else { panic!("a find answers with where HA is, or isn't") };
        assert!(!e.contains(token) && e.contains("TOKEN"), "{e}");
        home.handle(answered);
        assert!(home.config.ha.is_none() && home.connecting_ha.is_none());
        assert!(said(&notes).iter().any(|t| t.starts_with("home: ") && !t.contains(token)));
        assert!(home.configure(None, "ha find").starts_with("looking for"));
        let url = "http://10.0.0.4:8123";
        let e = found_only(Some(url.into()));
        assert!(e.contains(&format!("/home ha {url} TOKEN")), "{e}");
        assert!(found_only(None).contains("/home ha http://IP:8123 TOKEN"));
    }

    fn ha_device(backend: &str, unit: &str, hub: &ha::Hub) -> Device {
        Device { unit: Some(unit.into()), hub: Some(hub.clone()), ..Device::new(backend, unit.into(), [127, 0, 0, 1].into(), None, json!({})) }
    }

    #[test]
    fn tv_play_needs_a_title_and_defaults_to_youtube() {
        let fake = ha::fake::Fake::start("t", None, |_, _, _| {
            (200, json!({"state": "on", "attributes": {"source_list": ["YouTube"]}}).to_string())
        });
        let mut home = home_with(vec![ha_device("ha_tv", "media_player.tv", &hub(&fake.url, "t"))]);
        home.config.ha = Some(hub(&fake.url, "t"));
        assert_eq!(home.run_tool("tv_play", &json!({})).unwrap_err(), "play what?");
        let said = home.run_tool("tv_play", &json!({"what": "x"})).unwrap();
        assert!(said.starts_with("opened YouTube"), "{said}");
    }

    #[test]
    fn lookups_stay_off_until_asked_for() {
        let mut home = home_with(Vec::new());
        assert!(!home.config.lookups);
        assert!(home.configure(None, "lookups").contains("off"));
        assert!(home.configure(None, "lookups on in").contains("for IN"));
        assert!(home.config.lookups);
        assert!(home.configure(None, "lookups on india").contains("isn't a two-letter"));
        assert_eq!(home.config.country.as_deref(), Some("IN"));
        assert_eq!(country_of("en_GB.UTF-8"), "GB");
        assert_eq!(country_of("C"), "US");
        home.configure(None, "lookups off");
        assert!(!home.config.lookups);
    }

    #[test]
    fn a_hub_that_is_down_takes_its_devices_with_it_at_once() {
        // Nothing listens on port 1: refused at once, as a hub that is off.
        let down = hub("http://127.0.0.1:1", "t");
        let added: Vec<Device> = (0..20).map(|n| ha_device("ha_switch", &format!("switch.plug_{n}"), &down)).collect();
        let started = Instant::now();
        let states = listed_states(&down, added);
        assert!(started.elapsed() < Duration::from_secs(2), "one answer for all, not a wait each");
        assert_eq!(states.len(), 20);
        assert!(states.values().all(|s| *s == State::Offline));
    }

    #[test]
    fn a_device_the_hub_no_longer_lists_is_checked_on_its_own() {
        let fake = ha::fake::Fake::start("t", Some(json!([])), |_, path, _| match path {
            "/api/states" => (200, json!([{"entity_id": "switch.kettle", "state": "on", "attributes": {}}]).to_string()),
            _ => (200, json!({"entity_id": "switch.fan", "state": "off", "attributes": {}}).to_string()),
        });
        let at = hub(&fake.url, "t");
        let states = listed_states(&at, vec![ha_device("ha_switch", "switch.kettle", &at), ha_device("ha_switch", "switch.fan", &at)]);
        assert_eq!(states.into_iter().collect::<Vec<_>>(), [("switch.fan".to_string(), State::Ready)], "the kettle came with the listing");
        assert!(fake.asked().contains(&"GET /api/states/switch.fan".to_string()));
    }

    #[test]
    fn forgetting_home_assistant_drops_its_devices_and_token() {
        let mut home = home_with(vec![light("Desk", [10, 0, 0, 5], "ESP01_SHTW1C_31")]);
        assert!(home.configure(None, "ha forget").contains("isn't connected"));
        let kettle = entity("ha_switch", "switch.kettle", json!({"name": "Kettle"}));
        home.ha_connected(Ok((hub("http://10.0.0.4:8123", "t"), "Home".into(), listing(kettle))));
        home.add("switch.kettle");
        assert_eq!(home.configure(None, "ha forget"), "disconnected Home Assistant at http://10.0.0.4:8123 and forgot its 1 device");
        assert!(home.config.ha.is_none());
        let names: Vec<&str> = home.config.devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Desk"], "devices home drives itself stay");
    }

    #[test]
    fn what_a_newer_hopchat_saved_survives_a_save_here() {
        let newer = json!({"devices": [{"backend": "wiz", "name": "Desk", "ip": "10.0.0.5", "mac": "m", "room": "study"}],
                           "hubs": [{"url": "x"}]});
        let config: Config = serde_json::from_value(newer).unwrap();
        let saved = serde_json::to_value(&config).unwrap();
        assert_eq!(saved["devices"][0]["room"], "study");
        assert_eq!(saved["hubs"], json!([{"url": "x"}]));
    }

    #[test]
    fn a_switch_never_claims_a_state_it_couldnt_see() {
        // No hub to go through: setting fails before anything is claimed.
        let plug = Device { unit: Some("switch.fridge".into()), ..Device::new("ha_switch", "Fridge".into(), [10, 0, 0, 4].into(), None, json!({})) };
        let mut home = home_with(vec![plug]);
        let e = home.run_tool("switch_set", &json!({"switch": "fridge", "on": false})).unwrap_err();
        assert!(e.contains("isn't connected to Home Assistant"), "{e}");
    }

    #[test]
    fn switches_are_named_never_all() {
        let plug = |name: &str, entity: &str| Device {
            unit: Some(entity.into()),
            ..Device::new("ha_switch", name.into(), [10, 0, 0, 4].into(), None, json!({}))
        };
        let mut home = home_with(vec![plug("Kettle", "switch.kettle"), plug("Fridge plug", "switch.fridge")]);
        let tools = home.tools().to_string();
        assert!(tools.contains("switch_set") && tools.contains("\"Kettle\"") && !tools.contains("\"all\""));
        assert!(home.system_prompt().contains("- switch \"Kettle\" (on or off)"));
        let pick = |home: &Home, which: Option<&str>| {
            home.pick_switch(&json!({"switch": which})).map(|(_, d)| d.name)
        };
        assert_eq!(pick(&home, Some("kettle")).as_deref(), Ok("Kettle"));
        assert_eq!(pick(&home, Some("fridge")).as_deref(), Ok("Fridge plug"));
        assert_eq!(pick(&home, None), Err("say which switch".into()));
        assert!(home.run_tool("switch_set", &json!({"switch": "kettle"})).unwrap_err().contains("on or off"));
    }

    #[test]
    fn tools_follow_what_the_lights_can_do() {
        let home = home_with(vec![light("Desk", [10, 0, 0, 5], "ESP01_SHTW1C_31")]);
        let tools = home.tools().to_string();
        assert!(tools.contains("kelvin"));
        assert!(!tools.contains("\"color\""), "a white-tones bulb is offered no colours");
        let home = home_with(vec![
            light("Desk", [10, 0, 0, 5], "ESP01_SHTW1C_31"),
            light("Strip", [10, 0, 0, 6], "ESP03_SHRGB1C_01"),
        ]);
        let tools = home.tools().to_string();
        assert!(tools.contains("\"color\"") && tools.contains("\"Strip\"") && tools.contains("\"all\""));
        assert!(home.system_prompt().contains("\"Desk\" (white tones)"));
    }

    #[test]
    fn lenient_arguments() {
        let args = json!({"brightness": "40%", "on": "yes", "kelvin": 2700, "app": " Netflix "});
        assert_eq!(number(&args, "brightness"), Some(40.0));
        assert_eq!(number(&args, "kelvin"), Some(2700.0));
        assert_eq!(flag(&args, "on"), Some(true));
        assert_eq!(text(&args, "app").as_deref(), Some("Netflix"));
        assert_eq!(colour("#ff8000"), Some((255, 128, 0)));
        assert_eq!(colour("Purple"), Some((140, 0, 255)));
        assert_eq!(colour("plaid"), None);
    }

    /// Asks the real local model what it would do, without doing it.
    /// `cargo test -- --ignored` with `ollama serve` running.
    #[test]
    #[ignore = "needs a local Ollama with the model pulled"]
    fn model_picks_the_right_tools() {
        let tv = Device::new("webos", "Living room".into(), [10, 0, 0, 2].into(), None, json!({"key": "k"}));
        let kettle = Device { unit: Some("switch.kettle".into()), ..Device::new("ha_switch", "Kettle".into(), [10, 0, 0, 4].into(), None, Value::Null) };
        let home = home_with(vec![
            light("Desk lamp", [10, 0, 0, 1], "ESP01_SHTW1C_31"),
            light("Strip", [10, 0, 0, 3], "ESP03_SHRGB1C_01"),
            tv,
            kettle,
        ]);
        let model = Ollama::new(home.config.ollama.clone(), home.config.model.clone());
        let cases: &[(&str, &[&str])] = &[
            ("turn off the lights", &["light_set"]),
            ("make the strip purple", &["light_set"]),
            ("dim the desk lamp to 20% and put on netflix", &["light_set", "tv_open_app"]),
            ("turn the volume down a bit", &["tv_volume"]),
            ("is the tv on?", &["tv_status"]),
            ("put the kettle on", &["switch_set"]),
        ];
        for (ask, want) in cases {
            let messages = vec![
                json!({"role": "system", "content": home.system_prompt()}),
                json!({"role": "user", "content": format!("bob: {ask}")}),
            ];
            let Turn::Act(calls, _) = Turn::from_message(model.raw(&messages, Some(&home.tools())).unwrap()) else {
                panic!("{ask}: no tool call");
            };
            let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
            println!("{ask} -> {:?}", calls.iter().map(|c| format!("{} {}", c.name, c.args)).collect::<Vec<_>>());
            for w in *want {
                assert!(names.contains(w), "{ask}: wanted {w}, got {names:?}");
            }
        }
    }
}

