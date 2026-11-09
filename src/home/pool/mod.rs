//! `/pool`: one model split across the machines in the room.
//!
//! A model too big for any one laptop can still run on several. People who
//! are happy to lend their memory to a group say `/pool give on` there;
//! anyone in that group then says `/pool start MODEL`, and that machine (the
//! host) runs llama.cpp's `llama-server` with the model's layers spread over
//! its own memory and every giver's, each giver running llama.cpp's
//! `ggml-rpc-server`. `@ai` on the host then answers with the pooled model,
//! and anyone the host lends to borrows it like any other model.
//!
//! The pool is arranged with hidden one-to-one messages, end-to-end
//! encrypted like any other (see [`Wire`]). The RPC traffic itself never
//! touches the network in the clear: it travels through `tunnel`, keyed
//! with a secret that went inside the encrypted session, and each worker's
//! RPC server listens on loopback only.
//!
//! Lending memory to a pool lets its host run computations on this
//! machine through llama.cpp's RPC server, which its authors call a proof
//! of concept. So giving is off until switched on, and it is switched on
//! for one conversation at a time: a giver only joins pools started in a
//! group (or the lobby) where it said `/pool give on`, and is told whose
//! pool it joined.

mod llama;

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use llama::{memory, ram, size};
use llama::{LLAMA_SERVERS, RPC_SERVERS};

use super::ai::{MENTION as AI, SIGNATURE as AI_SIGNATURE};
use super::tunnel::{self, Secret};
use super::{Home, Request};
use crate::engine::{Command, Input};
use crate::lobby::LOBBY;

/// What a hidden pool message starts with; nobody types this.
pub const MARK: &str = "\u{1}hopchat-pool\u{1}";
/// How long the host waits for givers to answer before starting with
/// whoever did.
const GATHER: Duration = Duration::from_secs(20);
/// How long the engine may take to say who gives.
const FIND: Duration = Duration::from_secs(2);
/// A pooled answer crosses the network once per layer per token.
const ANSWER: Duration = Duration::from_secs(600);
const LOG: &str = "pool.log";
/// Where a giver's RPC server keeps the weights hosts sent it, so a model
/// loads faster the next time.
const CACHE: &str = "pool-cache";

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Wire {
    /// I lend my memory to pools in some groups; `mem` is in bytes.
    Give { mem: u64 },
    /// Not any more.
    Keep,
    /// Host to giver: join pool `id`, started in `group`, through a tunnel
    /// keyed with `key` (base64).
    Join { id: String, group: String, model: String, key: String },
    /// Giver to host: my tunnel listens on `port`.
    Ready { id: String, port: u16, mem: u64 },
    Refuse { id: String, error: String },
    /// Either way: this pool is over for me.
    Leave { id: String },
}

impl Wire {
    pub fn encode(&self) -> String {
        format!("{MARK}{}", serde_json::to_string(self).unwrap_or_default())
    }

    /// `Some` for any hidden pool message, even one too garbled to read,
    /// so it is never shown as chat.
    pub fn decode(text: &str) -> Option<Option<Wire>> {
        let body = text.strip_prefix(MARK)?;
        Some(serde_json::from_str(body).ok())
    }
}

/// What the engine does with a pool message before home sees it: an
/// invitation to a pool in a group the sender and this machine don't share
/// is answered with the refusal in `Err`, and never reaches home.
pub fn admit(wire: Wire, shares: impl Fn(&str) -> bool) -> Result<Wire, Wire> {
    match wire {
        Wire::Join { ref id, ref group, .. } if !shares(group) => {
            Err(Wire::Refuse { id: id.clone(), error: "isn't in that group with you".into() })
        }
        wire => Ok(wire),
    }
}

/// What to tell `peer` about this machine giving `giving`, remembering in
/// `told` who heard it gives. Nothing is said to someone never told it
/// gives: clients older than pools would show the message as chat.
pub fn give_wire(giving: Option<u64>, told: &mut HashSet<String>, peer: &str) -> Option<Wire> {
    match giving {
        Some(mem) => {
            told.insert(peer.to_string());
            Some(Wire::Give { mem })
        }
        None => told.remove(peer).then_some(Wire::Keep),
    }
}

/// Whether a pool may write this turn. It writes plain answers; a turn
/// that offers tools stays with a model that has been asked to call them.
pub fn takes(tools: Option<&Value>) -> bool {
    tools.and_then(Value::as_array).is_none_or(|t| t.is_empty())
}

/// Someone in a group with us who gives to pools, as the engine sees them.
#[derive(Clone, Debug)]
pub struct Giver {
    pub address: String,
    pub nick: String,
    pub mem: u64,
    /// Where their hopchat connects from; a pool needs a direct link.
    pub ip: Option<IpAddr>,
}

/// Who a pool message came from.
#[derive(Clone, Debug)]
pub struct Peer {
    pub address: String,
    pub who: String,
    /// Where their hopchat connects from, when directly.
    pub ip: Option<IpAddr>,
}

// ----- the host's side ------------------------------------------------------

#[derive(PartialEq, Debug)]
enum Phase {
    /// Asking the engine who gives here.
    Finding,
    /// Waiting for givers to answer.
    Gathering,
    /// Checking each giver's tunnel can be reached from here.
    Connecting,
    /// llama-server is streaming layers to the workers.
    Loading,
    Ready,
}

struct Member {
    address: String,
    nick: String,
    /// The memory it said it gives.
    mem: u64,
    ip: IpAddr,
    key: Secret,
    /// The worker's tunnel port once it said it's ready, and its memory.
    ready: Option<(u16, u64)>,
    refused: bool,
}

pub(super) struct Hosting {
    id: String,
    group: String,
    model: String,
    path: PathBuf,
    members: Vec<Member>,
    phase: Phase,
    server: Option<Arc<llama::Process>>,
    tunnels: Vec<tunnel::Listener>,
    url: Option<String>,
    started: Instant,
}

impl Drop for Hosting {
    fn drop(&mut self) {
        // Threads waiting on llama-server hold it too; stop it now anyway.
        if let Some(server) = &self.server {
            server.stop();
        }
    }
}

// ----- a giver's side -------------------------------------------------------

pub(super) struct Serving {
    id: String,
    host: String,
    host_nick: String,
    group: String,
    rpc: Arc<llama::Process>,
    _tunnel: Option<tunnel::Listener>,
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.rpc.stop();
    }
}

/// Everything about pools this machine keeps while hopchat runs.
#[derive(Default)]
pub(super) struct Pools {
    pub hosting: Option<Hosting>,
    pub serving: Option<Serving>,
    /// Peers who give, by nickname, and how much memory.
    pub givers: Vec<(String, u64)>,
}

impl Pools {
    /// Where to ask the pooled model, and its name, once it's loaded.
    pub fn url(&self) -> Option<(String, String)> {
        let h = self.hosting.as_ref().filter(|h| h.phase == Phase::Ready)?;
        Some((h.url.clone()?, h.model.clone()))
    }

    /// The pooled model that writes a turn offering `tools`, if one does.
    pub fn answerer(&self, tools: Option<&Value>) -> Option<(String, String)> {
        self.url().filter(|_| takes(tools))
    }
}

/// `text` after its first `n` words, so a path may have spaces in it.
pub(super) fn after_words(text: &str, n: usize) -> &str {
    let mut rest = text.trim_start();
    for _ in 0..n {
        rest = rest.find(char::is_whitespace).map_or("", |at| rest[at..].trim_start());
    }
    rest.trim_end()
}

impl Home {
    /// `/pool VERB REST`, typed in `group` when it is one.
    pub(super) fn pool_configure(&mut self, group: Option<&str>, verb: &str, rest: &str) -> String {
        match verb {
            "give" => self.pool_give(group, rest),
            "start" => {
                let Some(group) = group else {
                    return "open the group or lobby to pool with first: /pool start MODEL".into();
                };
                let model = if rest.is_empty() { self.config.model.clone() } else { rest.to_string() };
                self.pool_start(group, &model)
            }
            "stop" => {
                let mut said = Vec::new();
                if self.pools.hosting.is_some() {
                    self.pool_stop_hosting(None);
                    said.push("stopped your pool");
                }
                if self.pools.serving.is_some() {
                    self.pool_stop_serving(true);
                    said.push("left the pool you were in");
                }
                if said.is_empty() { "no pool to stop".into() } else { said.join("; ") }
            }
            "bin" if !rest.is_empty() => {
                let dir = llama::expand_home(rest);
                if !dir.is_dir() {
                    return format!("{} isn't a folder", dir.display());
                }
                self.config.llama_dir = Some(dir);
                self.save();
                let found = |names: &[&str]| llama::find(&self.config.llama_dir, names).is_ok();
                format!(
                    "looking for llama.cpp there · llama-server {} · ggml-rpc-server {}",
                    if found(LLAMA_SERVERS) { "found" } else { "missing" },
                    if found(RPC_SERVERS) { "found" } else { "missing" },
                )
            }
            "clear" => {
                if self.pools.serving.is_some() {
                    return "your machine is helping a pool; /pool stop first".into();
                }
                let cache = self.path_of(CACHE);
                let freed = llama::size_of(&cache);
                match std::fs::remove_dir_all(&cache) {
                    Ok(()) => format!("cleared the weights pools left here ({})", size(freed)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => "no pool weights are kept here".into(),
                    Err(e) => format!("couldn't clear {}: {e}", cache.display()),
                }
            }
            _ => self.pool_status(group),
        }
    }

    /// `/pool give on|off`: whether pools started in `group` may use this
    /// machine. Off with no group stops giving everywhere.
    fn pool_give(&mut self, group: Option<&str>, value: &str) -> String {
        let on = match value {
            "on" | "yes" => true,
            "off" | "no" => false,
            _ => return "/pool give on lets pools started here use your machine · /pool give off stops".into(),
        };
        if on {
            let Some(group) = group else {
                return "open the group (or the lobby) whose pools may use your machine, then /pool give on".into();
            };
            if !self.config.pool_groups.iter().any(|g| g == group) {
                self.config.pool_groups.push(group.to_string());
                self.save();
            }
            self.pool_offer();
            if let Err(e) = llama::find(&self.config.llama_dir, RPC_SERVERS) {
                return format!("giving your machine to pools here, but {e}");
            }
            let place = if group == LOBBY {
                "the lobby, by anyone on this network,"
            } else {
                "this group"
            };
            return format!(
                "pools started in {place} may use your machine ({}). \
                 Their host can run computations on it through llama.cpp; give only where you trust everyone",
                ram(memory())
            );
        }
        let stop = match group {
            Some(group) => {
                self.config.pool_groups.retain(|g| g != group);
                self.pools.serving.as_ref().is_some_and(|s| s.group == group)
            }
            None => {
                self.config.pool_groups.clear();
                true
            }
        };
        self.save();
        self.pool_offer();
        if stop {
            self.pool_stop_serving(true);
        }
        match self.config.pool_groups.len() {
            0 => "your machine doesn't join pools now".into(),
            n => format!("pools started here can't use your machine now; you still give in {n} other conversation(s)"),
        }
    }

    fn pool_status(&self, group: Option<&str>) -> String {
        let mut parts = Vec::new();
        match &self.pools.hosting {
            Some(h) => {
                let mut who = vec!["you".to_string()];
                who.extend(h.members.iter().filter(|m| m.ready.is_some()).map(|m| m.nick.clone()));
                let phase = match h.phase {
                    Phase::Finding | Phase::Gathering => "asking givers".to_string(),
                    Phase::Connecting => "reaching givers".to_string(),
                    Phase::Loading => format!("loading for {}s", h.started.elapsed().as_secs()),
                    Phase::Ready => format!("ready, mention {AI}"),
                };
                parts.push(format!("pool: {} on {} · {phase}", h.model, who.join(", ")));
            }
            None => parts.push("pool: none here".into()),
        }
        if let Some(s) = &self.pools.serving {
            parts.push(format!("helping {}'s pool", s.host_nick));
        }
        let here = group.is_some_and(|g| self.config.pool_groups.iter().any(|p| p == g));
        parts.push(match (here, self.config.pool_groups.len()) {
            (_, 0) => "giving off".into(),
            (true, n) => format!("giving here (and {} other)", n - 1),
            (false, n) => format!("giving in {n} other conversation(s), not here"),
        });
        if !self.pools.givers.is_empty() {
            let givers: Vec<String> = self.pools.givers.iter().map(|(nick, mem)| format!("{nick} ({})", ram(*mem))).collect();
            parts.push(format!("givers: {}", givers.join(", ")));
        }
        let cached = llama::size_of(&self.path_of(CACHE));
        if cached > 0 {
            parts.push(format!("{} of pool weights kept · /pool clear", size(cached)));
        }
        parts.push("/pool start [MODEL] · /pool stop · /pool give on|off".into());
        parts.join(" · ")
    }

    /// Tells the engine whether this machine gives to pools anywhere.
    pub(super) fn pool_offer(&self) {
        let give = (!self.config.pool_groups.is_empty()).then(memory);
        let _ = self.engine.send(Input::Ui(Command::PoolGive(give)));
    }

    fn pool_start(&mut self, group: &str, model: &str) -> String {
        if let Some(h) = &self.pools.hosting {
            return format!("already pooling {}; /pool stop first", h.model);
        }
        if let Some(s) = &self.pools.serving {
            return format!("your machine is helping {}'s pool, so its memory is taken; /pool stop first", s.host_nick);
        }
        if let Err(e) = llama::find(&self.config.llama_dir, LLAMA_SERVERS) {
            return e;
        }
        let path = match llama::resolve(model) {
            Ok(path) => path,
            Err(e) => return e,
        };
        let id = crate::engine::new_file_id();
        self.pools.hosting = Some(Hosting {
            id: id.clone(),
            group: group.to_string(),
            model: model.to_string(),
            path,
            members: Vec::new(),
            phase: Phase::Finding,
            server: None,
            tunnels: Vec::new(),
            url: None,
            started: Instant::now(),
        });
        // The engine answers on its own thread; home waits for nobody.
        let (reply, givers) = mpsc::channel();
        let _ = self.engine.send(Input::Ui(Command::PoolGivers { group: group.to_string(), reply }));
        let me = self.me.clone();
        thread::spawn(move || {
            let givers = givers.recv_timeout(FIND).unwrap_or_default();
            let _ = me.send(Request::PoolFound { id, givers });
        });
        format!("looking for givers to pool {model} with…")
    }

    /// The engine said who gives in the pool's group.
    pub(super) fn pool_found(&mut self, id: String, givers: Vec<Giver>) {
        let Some(h) = self.pools.hosting.as_mut().filter(|h| h.id == id && h.phase == Phase::Finding) else { return };
        let mut unlinked = Vec::new();
        for g in givers {
            // A pool's RPC needs a direct connection; one through other
            // machines can't carry it.
            let Some(ip) = g.ip else {
                unlinked.push(g.nick);
                continue;
            };
            let key = tunnel::secret();
            let wire = Wire::Join {
                id: id.clone(),
                group: h.group.clone(),
                model: h.model.clone(),
                key: base64::engine::general_purpose::STANDARD.encode(key),
            };
            let _ = self.engine.send(Input::Ui(Command::PoolSend { to: g.address.clone(), wire }));
            h.members.push(Member { address: g.address, nick: g.nick, mem: g.mem, ip, key, ready: None, refused: false });
        }
        let unlinked = match unlinked.as_slice() {
            [] => None,
            [one] => Some(format!("{one} gives but isn't directly connected to you, so can't join")),
            many => Some(format!("{} give but aren't directly connected to you, so can't join", many.join(", "))),
        };
        if h.members.is_empty() {
            self.pools.hosting = None;
            let why = unlinked.unwrap_or_else(|| "nobody here gives their machine to pools yet; they can say /pool give on here".into());
            return self.note(format!("pool: {why}"));
        }
        h.phase = Phase::Gathering;
        let names: Vec<String> = h.members.iter().map(|m| format!("{} ({})", m.nick, ram(m.mem))).collect();
        let mut text = format!("pool: asking {} to pool {} with you…", names.join(", "), h.model);
        if let Some(unlinked) = unlinked {
            text.push_str(&format!(" ({unlinked})"));
        }
        let me = self.me.clone();
        thread::spawn(move || {
            thread::sleep(GATHER);
            let _ = me.send(Request::PoolGathered { id });
        });
        self.note(text);
    }

    /// A hidden pool message from `peer`, which the engine let through.
    pub(super) fn pool_wire(&mut self, peer: Peer, wire: Wire) {
        match wire {
            Wire::Join { id, group, model, key } => self.pool_join(peer, id, group, model, key),
            Wire::Ready { id, port, mem } => {
                let Some(h) = self.pools.hosting.as_mut().filter(|h| h.id == id) else { return };
                // Only a first answer while gathering counts. A repeat from a
                // member changes nothing, and anyone let go at launch was
                // already told to leave.
                let Some(m) = h.members.iter_mut().find(|m| m.address == peer.address) else { return };
                if h.phase == Phase::Gathering && m.ready.is_none() && !m.refused {
                    m.ready = Some((port, mem));
                    self.pool_launch_if_answered();
                }
            }
            Wire::Refuse { id, error } => {
                let Some(h) = self.pools.hosting.as_mut().filter(|h| h.id == id) else { return };
                let Some(m) = h.members.iter_mut().find(|m| m.address == peer.address) else { return };
                m.refused = true;
                self.note(format!("pool: {} {error}", peer.who));
                self.pool_launch_if_answered();
            }
            Wire::Leave { id } => {
                if self.pools.serving.as_ref().is_some_and(|s| s.id == id && s.host == peer.address) {
                    self.pool_stop_serving(false);
                    self.note(format!("pool: {} ended their pool", peer.who));
                }
                let Some(h) = self.pools.hosting.as_mut().filter(|h| h.id == id) else { return };
                let Some(m) = h.members.iter_mut().find(|m| m.address == peer.address) else { return };
                if m.ready.is_some() {
                    self.pool_stop_hosting(Some(format!("{} left", peer.who)));
                } else {
                    // Said no before answering: don't wait for it.
                    m.refused = true;
                    self.pool_launch_if_answered();
                }
            }
            Wire::Give { .. } | Wire::Keep => {}
        }
    }

    /// The engine lost `address`: a pool can't run without any of its parts.
    pub(super) fn pool_peer_gone(&mut self, address: &str, who: String) {
        if self.pools.serving.as_ref().is_some_and(|s| s.host == address) {
            self.pool_stop_serving(false);
            self.note(format!("pool: lost {who}, so left their pool"));
        }
        let Some(h) = self.pools.hosting.as_mut() else { return };
        let Some(m) = h.members.iter_mut().find(|m| m.address == address) else { return };
        if m.ready.is_some() {
            self.pool_stop_hosting(Some(format!("lost {who}")));
        } else {
            // It will never answer; don't wait for it.
            m.refused = true;
            self.pool_launch_if_answered();
        }
    }

    fn pool_join(&mut self, host: Peer, id: String, group: String, model: String, key: String) {
        let refuse = |error: &str| {
            let wire = Wire::Refuse { id: id.clone(), error: error.to_string() };
            let _ = self.engine.send(Input::Ui(Command::PoolSend { to: host.address.clone(), wire }));
        };
        if !self.config.pool_groups.contains(&group) {
            return refuse("isn't giving their machine to pools there");
        }
        if self.pools.hosting.is_some() {
            return refuse("is hosting a pool of their own");
        }
        let Some(ip) = host.ip else { return refuse("isn't directly connected to you") };
        let Some(key) = base64::engine::general_purpose::STANDARD
            .decode(&key)
            .ok()
            .and_then(|k| <Secret>::try_from(k.as_slice()).ok())
        else {
            return refuse("got a garbled pool key");
        };
        if let Some(s) = &self.pools.serving
            && s.host != host.address
        {
            return refuse(&format!("is already in {}'s pool", s.host_nick));
        }
        // A host starting again replaces its old pool here.
        self.pools.serving = None;
        let rpc = match llama::find(&self.config.llama_dir, RPC_SERVERS) {
            Ok(rpc) => rpc,
            Err(e) => return refuse(&e),
        };
        let port = match llama::free_port() {
            Ok(port) => port,
            Err(e) => return refuse(&format!("found no free port: {e}")),
        };
        let (log, cache) = (self.path_of(LOG), self.path_of(CACHE));
        let args = ["--host", "127.0.0.1", "--port", &port.to_string(), "--cache"].map(str::to_string);
        // llama.cpp keeps its cache where LLAMA_CACHE says, so `/pool clear` can find it.
        let process = match llama::Process::start(&rpc, args, &[("LLAMA_CACHE", &cache)], &log) {
            Ok(process) => Arc::new(process),
            Err(e) => return refuse(&format!("couldn't start {}: {e}", rpc.display())),
        };
        self.pools.serving = Some(Serving {
            id: id.clone(),
            host: host.address.clone(),
            host_nick: host.who.clone(),
            group: group.clone(),
            rpc: process.clone(),
            _tunnel: None,
        });
        let me = self.me.clone();
        thread::spawn(move || {
            let result = llama::wait_for_port(port, &process, &log).and_then(|()| {
                tunnel::serve(key, ip, SocketAddr::from((Ipv4Addr::LOCALHOST, port))).map_err(|e| e.to_string())
            });
            let _ = me.send(Request::PoolServing { id, result });
        });
        let place = if group == LOBBY { "the lobby" } else { "your group" };
        self.note(format!(
            "pool: {} started a pool of {model} in {place}; your machine is helping · /pool give off to stop",
            host.who
        ));
    }

    /// This machine's RPC server came up, or didn't.
    pub(super) fn pool_serving(&mut self, id: String, result: Result<tunnel::Listener, String>) {
        let Some(s) = self.pools.serving.as_mut().filter(|s| s.id == id) else { return };
        let host = s.host.clone();
        let wire = match result {
            Ok(listener) => {
                let port = listener.port;
                s._tunnel = Some(listener);
                Wire::Ready { id, port, mem: memory() }
            }
            Err(error) => {
                self.pools.serving = None;
                self.note(format!("pool: couldn't help: {error}"));
                Wire::Refuse { id, error }
            }
        };
        let _ = self.engine.send(Input::Ui(Command::PoolSend { to: host, wire }));
    }

    fn pool_stop_serving(&mut self, tell: bool) {
        if let Some(s) = self.pools.serving.take()
            && tell
        {
            let _ = self.engine.send(Input::Ui(Command::PoolSend { to: s.host.clone(), wire: Wire::Leave { id: s.id.clone() } }));
        }
    }

    /// Ends this machine's pool, telling its members and, with `why`, the
    /// group.
    pub(super) fn pool_stop_hosting(&mut self, why: Option<String>) {
        let Some(h) = self.pools.hosting.take() else { return };
        for m in &h.members {
            let _ = self.engine.send(Input::Ui(Command::PoolSend { to: m.address.clone(), wire: Wire::Leave { id: h.id.clone() } }));
        }
        if let Some(why) = why {
            let was = if h.phase == Phase::Ready { "is over" } else { "didn't start" };
            self.say_as(&h.group, AI_SIGNATURE, format!("the pool of {} {was}: {why}.", h.model));
        }
        self.offer();
    }

    /// The gathering time is up.
    pub(super) fn pool_gathered(&mut self, id: String) {
        if self.pools.hosting.as_ref().is_some_and(|h| h.id == id && h.phase == Phase::Gathering) {
            self.pool_launch();
        }
    }

    fn pool_launch_if_answered(&mut self) {
        let answered = self.pools.hosting.as_ref().is_some_and(|h| {
            h.phase == Phase::Gathering && h.members.iter().all(|m| m.ready.is_some() || m.refused)
        });
        if answered {
            self.pool_launch();
        }
    }

    /// Opens a tunnel to each giver that's ready, then checks each can be
    /// reached before llama-server is started across them.
    fn pool_launch(&mut self) {
        let Some(h) = self.pools.hosting.as_mut() else { return };
        h.phase = Phase::Connecting;
        // Whoever didn't answer in time is let go.
        for m in h.members.iter().filter(|m| m.ready.is_none() && !m.refused) {
            let _ = self.engine.send(Input::Ui(Command::PoolSend { to: m.address.clone(), wire: Wire::Leave { id: h.id.clone() } }));
        }
        h.members.retain(|m| m.ready.is_some());
        if h.members.is_empty() {
            return self.pool_stop_hosting(Some("nobody could join".into()));
        }
        let mut reach = Vec::new();
        for m in &h.members {
            let Some((port, _)) = m.ready else { continue };
            let remote = SocketAddr::new(m.ip, port);
            match tunnel::forward(m.key, remote) {
                Ok(listener) => h.tunnels.push(listener),
                Err(e) => {
                    let why = format!("couldn't open a tunnel to {}: {e}", m.nick);
                    return self.pool_stop_hosting(Some(why));
                }
            }
            reach.push((m.nick.clone(), m.key, remote));
        }
        let (id, me) = (h.id.clone(), self.me.clone());
        thread::spawn(move || {
            let result = reach.into_iter().try_for_each(|(nick, key, remote)| {
                tunnel::probe(key, remote).map_err(|e| format!("couldn't reach {nick} at {remote}: {e}"))
            });
            let _ = me.send(Request::PoolReached { id, result });
        });
    }

    /// Every giver's tunnel answered, or one didn't: start llama-server.
    pub(super) fn pool_reached(&mut self, id: String, result: Result<(), String>) {
        if !self.pools.hosting.as_ref().is_some_and(|h| h.id == id && h.phase == Phase::Connecting) {
            return;
        }
        if let Err(why) = result {
            return self.pool_stop_hosting(Some(why));
        }
        let log = self.path_of(LOG);
        let server = match llama::find(&self.config.llama_dir, LLAMA_SERVERS) {
            Ok(server) => server,
            Err(e) => return self.pool_stop_hosting(Some(e)),
        };
        let port = match llama::free_port() {
            Ok(port) => port,
            Err(e) => return self.pool_stop_hosting(Some(format!("found no free port: {e}"))),
        };
        let Some(h) = self.pools.hosting.as_mut() else { return };
        let rpc: Vec<String> = h.tunnels.iter().map(|t| format!("127.0.0.1:{}", t.port)).collect();
        let mut args: Vec<std::ffi::OsString> = vec!["--model".into(), h.path.clone().into()];
        args.extend(
            [
                "--rpc",
                &rpc.join(","),
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--alias",
                &h.model,
                // Every layer off the CPU: onto this machine's GPU and the workers'.
                "--n-gpu-layers",
                "999",
                "--jinja",
            ]
            .map(std::ffi::OsString::from),
        );
        let process = match llama::Process::start(&server, args, &[], &log) {
            Ok(process) => Arc::new(process),
            Err(e) => return self.pool_stop_hosting(Some(format!("couldn't start {}: {e}", server.display()))),
        };
        let url = format!("http://127.0.0.1:{port}");
        h.phase = Phase::Loading;
        h.server = Some(process.clone());
        h.url = Some(url.clone());
        h.started = Instant::now();
        let names: Vec<String> = h.members.iter().map(|m| format!("{} ({})", m.nick, ram(m.ready.map_or(0, |r| r.1)))).collect();
        let (group, model) = (h.group.clone(), h.model.clone());
        let me = self.me.clone();
        thread::spawn(move || {
            let result = llama::wait_for_health(&url, &process, &log);
            let _ = me.send(Request::PoolLoaded { id, result });
        });
        self.say_as(
            &group,
            AI_SIGNATURE,
            format!("is loading {model} across this machine and {}. This can take a few minutes.", names.join(", ")),
        );
    }

    /// llama-server finished loading, or gave up.
    pub(super) fn pool_loaded(&mut self, id: String, result: Result<(), String>) {
        let Some(h) = self.pools.hosting.as_mut().filter(|h| h.id == id && h.phase == Phase::Loading) else { return };
        match result {
            Ok(()) => {
                h.phase = Phase::Ready;
                // Only the host's GPU holds layers, not its main memory, so
                // its share isn't counted as a number it may not have given.
                let names: Vec<String> =
                    h.members.iter().map(|m| format!("{} ({})", m.nick, ram(m.ready.map_or(0, |r| r.1)))).collect();
                let text = format!(
                    "{} is ready, pooled across this machine and {}. Mention {AI} to ask it.",
                    h.model,
                    names.join(", ")
                );
                let group = h.group.clone();
                self.say_as(&group, AI_SIGNATURE, text);
                self.offer();
            }
            Err(e) => self.pool_stop_hosting(Some(e)),
        }
    }

    fn path_of(&self, name: &str) -> PathBuf {
        self.path.with_file_name(name)
    }
}

// ----- asking the pooled model ---------------------------------------------

/// The assistant message the pooled model writes for `messages`, through
/// llama-server's OpenAI-style API.
pub fn ask(url: &str, messages: &[Value]) -> Result<Value, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(ANSWER)).build().new_agent();
    let mut response = agent
        .post(format!("{url}/v1/chat/completions"))
        .send_json(json!({ "messages": messages, "stream": false }))
        .map_err(|e| match e {
            ureq::Error::Timeout(_) => "the pool took too long to answer".to_string(),
            ureq::Error::StatusCode(code) => format!("the pool answered HTTP {code}"),
            _ => "can't reach the pool's llama-server".to_string(),
        })?;
    let body: Value = response.body_mut().read_json().map_err(|e| e.to_string())?;
    let message = body.pointer("/choices/0/message").cloned().ok_or("the pool sent no message")?;
    // Ollama's shape, which the rest of hopchat reads.
    Ok(json!({ "role": "assistant", "content": message.get("content").cloned().unwrap_or(Value::Null) }))
}

#[cfg(test)]
mod tests;
