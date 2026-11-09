//! `/pool`: one model split across the machines in the room.
//!
//! A model too big for any one laptop can still run on several. People who
//! are happy to lend their memory say `/pool give on`; anyone in a group
//! with them then says `/pool start MODEL`, and that machine (the host)
//! runs llama.cpp's `llama-server` with the model's layers spread over its
//! own memory and every giver's, each giver running llama.cpp's
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
//! of concept. So giving is off until switched on, a giver only joins a
//! pool started in a group it is in, and it is told whose pool it joined.

use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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
/// Loading streams every layer to its worker the first time, which is
/// gigabytes over Wi-Fi.
const LOAD: Duration = Duration::from_secs(45 * 60);
const RPC_START: Duration = Duration::from_secs(15);
/// A pooled answer crosses the network once per layer per token.
const ANSWER: Duration = Duration::from_secs(600);
const LOG: &str = "pool.log";

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Wire {
    /// I lend my memory to pools in groups we share; `mem` is in bytes.
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

/// Someone in a group with us who gives to pools, as the engine sees them.
#[derive(Clone, Debug)]
pub struct Giver {
    pub address: String,
    pub nick: String,
    pub mem: u64,
    /// Where their hopchat connects from; a pool needs a direct link.
    pub ip: Option<IpAddr>,
}

// ----- the host's side ------------------------------------------------------

#[derive(PartialEq, Debug)]
enum Phase {
    /// Waiting for givers to answer.
    Gathering,
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
    server: Option<Arc<Mutex<Child>>>,
    tunnels: Vec<tunnel::Listener>,
    url: Option<String>,
    started: Instant,
}

impl Drop for Hosting {
    fn drop(&mut self) {
        if let Some(server) = &self.server {
            stop(server);
        }
    }
}

// ----- a giver's side -------------------------------------------------------

pub(super) struct Serving {
    id: String,
    host: String,
    host_nick: String,
    rpc: Arc<Mutex<Child>>,
    _tunnel: Option<tunnel::Listener>,
}

impl Drop for Serving {
    fn drop(&mut self) {
        stop(&self.rpc);
    }
}

type Running = Arc<Mutex<Child>>;

/// Every llama.cpp process a pool started, so none outlives hopchat.
static RUNNING: Mutex<Vec<Running>> = Mutex::new(Vec::new());

fn stop(child: &Running) {
    {
        let mut child = child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
    RUNNING.lock().unwrap().retain(|c| !Arc::ptr_eq(c, child));
}

/// Stops every pool process; for when hopchat quits, which doesn't wait
/// for home to drop its pools.
pub fn stop_all() {
    let running: Vec<Running> = RUNNING.lock().unwrap().drain(..).collect();
    for child in running {
        let mut child = child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Everything about pools this machine keeps while hopchat runs.
#[derive(Default)]
pub(super) struct Pools {
    pub hosting: Option<Hosting>,
    pub serving: Option<Serving>,
    /// Peers who give, as "alice (16 GB)", from the engine.
    pub givers: Vec<String>,
}

impl Pools {
    /// Where to ask the pooled model, once it's loaded.
    pub fn url(&self) -> Option<(String, String)> {
        let h = self.hosting.as_ref().filter(|h| h.phase == Phase::Ready)?;
        Some((h.url.clone()?, h.model.clone()))
    }
}

impl Home {
    /// `/pool …`, typed in `group` when it is one.
    pub(super) fn pool_configure(&mut self, group: Option<&str>, verb: &str, value: Option<&str>) -> String {
        match (verb, value) {
            ("give", value) => {
                self.config.pool_give = !matches!(value, Some("off" | "no"));
                self.save();
                self.pool_offer();
                if self.config.pool_give {
                    if let Err(e) = find(&self.config.llama_dir, RPC_SERVERS) {
                        return format!("giving your machine to pools, but {e}");
                    }
                    format!(
                        "your machine joins pools started in groups you're in ({} of memory). \
                         The host can run computations on it through llama.cpp; give only to people you trust",
                        size(memory())
                    )
                } else {
                    self.pool_stop_serving(true);
                    "your machine doesn't join pools now".into()
                }
            }
            ("start", model) => {
                let Some(group) = group else {
                    return "open the group or lobby to pool with first: /pool start MODEL".into();
                };
                let model = model.map(str::to_string).unwrap_or_else(|| self.config.model.clone());
                self.pool_start(group, &model)
            }
            ("stop", _) => {
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
            ("bin", Some(dir)) => {
                let dir = PathBuf::from(dir);
                if !dir.is_dir() {
                    return format!("{} isn't a folder", dir.display());
                }
                self.config.llama_dir = Some(dir);
                self.save();
                let found = |names: &[&str]| find(&self.config.llama_dir, names).is_ok();
                format!(
                    "looking for llama.cpp there · llama-server {} · ggml-rpc-server {}",
                    if found(LLAMA_SERVERS) { "found" } else { "missing" },
                    if found(RPC_SERVERS) { "found" } else { "missing" },
                )
            }
            _ => self.pool_status(),
        }
    }

    fn pool_status(&self) -> String {
        let mut parts = Vec::new();
        match &self.pools.hosting {
            Some(h) => {
                let mut who = vec!["you".to_string()];
                who.extend(h.members.iter().filter(|m| m.ready.is_some()).map(|m| m.nick.clone()));
                let phase = match h.phase {
                    Phase::Gathering => "asking givers".to_string(),
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
        parts.push(format!("giving {}", if self.config.pool_give { "on" } else { "off" }));
        if !self.pools.givers.is_empty() {
            parts.push(format!("givers: {}", self.pools.givers.join(", ")));
        }
        parts.push("/pool start [MODEL] · /pool stop · /pool give on|off".into());
        parts.join(" · ")
    }

    /// Tells the engine whether this machine gives to pools.
    pub(super) fn pool_offer(&self) {
        let give = self.config.pool_give.then(memory);
        let _ = self.engine.send(Input::Ui(Command::PoolGive(give)));
    }

    fn pool_start(&mut self, group: &str, model: &str) -> String {
        if let Some(h) = &self.pools.hosting {
            return format!("already pooling {}; /pool stop first", h.model);
        }
        if let Err(e) = find(&self.config.llama_dir, LLAMA_SERVERS) {
            return e;
        }
        let path = match resolve(model) {
            Ok(path) => path,
            Err(e) => return e,
        };
        let (reply, givers) = mpsc::channel();
        let _ = self.engine.send(Input::Ui(Command::PoolGivers { group: group.to_string(), reply }));
        let givers: Vec<Giver> = givers.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
        let near: Vec<Giver> = givers.into_iter().filter(|g| g.ip.is_some()).collect();
        if near.is_empty() {
            return "nobody in this group gives their machine to pools yet; they can say /pool give on".into();
        }
        let id = crate::engine::new_file_id();
        let mut members = Vec::new();
        for g in near {
            let key = tunnel::secret();
            let wire = Wire::Join {
                id: id.clone(),
                group: group.to_string(),
                model: model.to_string(),
                key: base64::engine::general_purpose::STANDARD.encode(key),
            };
            let _ = self.engine.send(Input::Ui(Command::PoolSend { to: g.address.clone(), wire }));
            members.push(Member {
                address: g.address,
                nick: g.nick,
                mem: g.mem,
                ip: g.ip.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
                key,
                ready: None,
                refused: false,
            });
        }
        let names: Vec<String> = members.iter().map(|m| format!("{} ({})", m.nick, size(m.mem))).collect();
        self.pools.hosting = Some(Hosting {
            id: id.clone(),
            group: group.to_string(),
            model: model.to_string(),
            path,
            members,
            phase: Phase::Gathering,
            server: None,
            tunnels: Vec::new(),
            url: None,
            started: Instant::now(),
        });
        let me = self.me.clone();
        thread::spawn(move || {
            thread::sleep(GATHER);
            let _ = me.send(Request::PoolGathered { id });
        });
        format!("asking {} to pool {model} with you…", names.join(", "))
    }

    /// A hidden pool message from `from`, a peer we share a session with.
    pub(super) fn pool_wire(&mut self, from: String, who: String, ip: Option<IpAddr>, wire: Wire) {
        match wire {
            Wire::Join { id, group, model, key } => self.pool_join(from, who, ip, id, group, model, key),
            Wire::Ready { id, port, mem } => {
                let Some(h) = self.pools.hosting.as_mut().filter(|h| h.id == id) else { return };
                if h.phase != Phase::Gathering {
                    // Too late for this pool; let it go.
                    let _ = self.engine.send(Input::Ui(Command::PoolSend { to: from, wire: Wire::Leave { id } }));
                    return;
                }
                if let Some(m) = h.members.iter_mut().find(|m| m.address == from) {
                    m.ready = Some((port, mem));
                }
                self.pool_launch_if_answered();
            }
            Wire::Refuse { id, error } => {
                let Some(h) = self.pools.hosting.as_mut().filter(|h| h.id == id) else { return };
                if let Some(m) = h.members.iter_mut().find(|m| m.address == from) {
                    m.refused = true;
                }
                self.note(format!("pool: {who} {error}"));
                self.pool_launch_if_answered();
            }
            Wire::Leave { id } => {
                if self.pools.serving.as_ref().is_some_and(|s| s.id == id && s.host == from) {
                    self.pool_stop_serving(false);
                    self.note(format!("pool: {who} ended their pool"));
                }
                let left = self.pools.hosting.as_ref().is_some_and(|h| {
                    h.id == id && h.members.iter().any(|m| m.address == from && m.ready.is_some())
                });
                if left {
                    self.pool_stop_hosting(Some(format!("{who} left")));
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
        let member = self.pools.hosting.as_ref().is_some_and(|h| {
            h.members.iter().any(|m| m.address == address && m.ready.is_some())
        });
        if member {
            self.pool_stop_hosting(Some(format!("lost {who}")));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn pool_join(&mut self, host: String, who: String, ip: Option<IpAddr>, id: String, group: String, model: String, key: String) {
        let refuse = |error: String| {
            let _ = self.engine.send(Input::Ui(Command::PoolSend { to: host.clone(), wire: Wire::Refuse { id: id.clone(), error } }));
        };
        if !self.config.pool_give {
            return refuse("isn't giving their machine to pools".into());
        }
        let Some(ip) = ip else { return refuse("isn't directly connected to you".into()) };
        let Some(key) = base64::engine::general_purpose::STANDARD
            .decode(&key)
            .ok()
            .and_then(|k| <Secret>::try_from(k.as_slice()).ok())
        else {
            return refuse("got a garbled pool key".into());
        };
        if let Some(s) = &self.pools.serving
            && s.host != host
        {
            return refuse(format!("is already in {}'s pool", s.host_nick));
        }
        // A host starting again replaces its old pool here.
        self.pools.serving = None;
        let rpc = match find(&self.config.llama_dir, RPC_SERVERS) {
            Ok(rpc) => rpc,
            Err(e) => return refuse(e),
        };
        let port = match free_port() {
            Ok(port) => port,
            Err(e) => return refuse(format!("found no free port: {e}")),
        };
        let mut process = Process::new(&rpc);
        process.args(["--host", "127.0.0.1", "--port", &port.to_string(), "--cache"]);
        let child = match spawn_logged(&mut process, &self.path_of(LOG)) {
            Ok(child) => child,
            Err(e) => return refuse(format!("couldn't start {}: {e}", rpc.display())),
        };
        self.pools.serving = Some(Serving {
            id: id.clone(),
            host: host.clone(),
            host_nick: who.clone(),
            rpc: child.clone(),
            _tunnel: None,
        });
        let me = self.me.clone();
        thread::spawn(move || {
            let result = wait_for_port(port, &child).and_then(|()| {
                tunnel::serve(key, ip, SocketAddr::from((Ipv4Addr::LOCALHOST, port))).map_err(|e| e.to_string())
            });
            let _ = me.send(Request::PoolServing { id, result });
        });
        let place = if group == LOBBY { "the lobby".to_string() } else { "your group".to_string() };
        self.note(format!(
            "pool: {who} started a pool of {model} in {place}; your machine is helping · /pool give off to stop"
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

    /// Opens a tunnel to each giver that's ready and starts llama-server
    /// across them.
    fn pool_launch(&mut self) {
        let log = self.path_of(LOG);
        let server = find(&self.config.llama_dir, LLAMA_SERVERS);
        let Some(h) = self.pools.hosting.as_mut() else { return };
        h.phase = Phase::Loading;
        // Whoever didn't answer in time is let go.
        for m in h.members.iter().filter(|m| m.ready.is_none()) {
            let _ = self.engine.send(Input::Ui(Command::PoolSend { to: m.address.clone(), wire: Wire::Leave { id: h.id.clone() } }));
        }
        h.members.retain(|m| m.ready.is_some());
        if h.members.is_empty() {
            return self.pool_stop_hosting(Some("nobody could join".into()));
        }
        let mut rpc = Vec::new();
        for m in &h.members {
            let Some((port, _)) = m.ready else { continue };
            match tunnel::forward(m.key, SocketAddr::new(m.ip, port)) {
                Ok(listener) => {
                    rpc.push(format!("127.0.0.1:{}", listener.port));
                    h.tunnels.push(listener);
                }
                Err(e) => {
                    let why = format!("couldn't open a tunnel to {}: {e}", m.nick);
                    return self.pool_stop_hosting(Some(why));
                }
            }
        }
        let server = match server {
            Ok(server) => server,
            Err(e) => return self.pool_stop_hosting(Some(e)),
        };
        let port = match free_port() {
            Ok(port) => port,
            Err(e) => return self.pool_stop_hosting(Some(format!("found no free port: {e}"))),
        };
        let mut process = Process::new(&server);
        process.args(["--model"]).arg(&h.path).args([
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
        ]);
        let child = match spawn_logged(&mut process, &log) {
            Ok(child) => child,
            Err(e) => return self.pool_stop_hosting(Some(format!("couldn't start {}: {e}", server.display()))),
        };
        let url = format!("http://127.0.0.1:{port}");
        h.server = Some(child.clone());
        h.url = Some(url.clone());
        h.started = Instant::now();
        let id = h.id.clone();
        let names: Vec<String> = h.members.iter().map(|m| format!("{} ({})", m.nick, size(m.ready.map_or(0, |r| r.1)))).collect();
        let (group, model) = (h.group.clone(), h.model.clone());
        let me = self.me.clone();
        thread::spawn(move || {
            let result = wait_for_health(&url, &child, log);
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
        let Some(h) = self.pools.hosting.as_mut().filter(|h| h.id == id) else { return };
        match result {
            Ok(()) => {
                h.phase = Phase::Ready;
                let mut who = vec!["the host".to_string()];
                who.extend(h.members.iter().map(|m| m.nick.clone()));
                let total: u64 = memory() + h.members.iter().filter_map(|m| m.ready.map(|r| r.1)).sum::<u64>();
                let text = format!(
                    "{} is ready, pooled across {} ({} of memory between them). Mention {AI} to ask it.",
                    h.model,
                    who.join(", "),
                    size(total)
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

// ----- llama.cpp, models, and the machine ----------------------------------

const LLAMA_SERVERS: &[&str] = &["llama-server"];
/// The worker was called `rpc-server` before llama.cpp renamed it.
const RPC_SERVERS: &[&str] = &["ggml-rpc-server", "rpc-server"];

/// The first of `names` in `dir`, or on the PATH.
fn find(dir: &Option<PathBuf>, names: &[&str]) -> Result<PathBuf, String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs = dir.iter().cloned().chain(std::env::split_paths(&path));
    for dir in dirs {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(format!(
        "can't find llama.cpp's {}. Install a llama.cpp release built with RPC (the ones at \
         github.com/ggml-org/llama.cpp/releases are) and put it on your PATH, or say /pool bin FOLDER",
        names[0]
    ))
}

/// A `.gguf` file, or a model Ollama has pulled, as a `.gguf` on disk.
pub fn resolve(model: &str) -> Result<PathBuf, String> {
    let as_path = expand_home(model);
    if as_path.extension().is_some_and(|e| e == "gguf") {
        return if as_path.is_file() { Ok(as_path) } else { Err(format!("there's no {}", as_path.display())) };
    }
    let root = std::env::var_os("OLLAMA_MODELS")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".ollama/models")))
        .ok_or("can't find Ollama's models folder")?;
    resolve_in(&root, model)
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// The weights of Ollama model `name` under its models folder `root`.
fn resolve_in(root: &Path, name: &str) -> Result<PathBuf, String> {
    let (repo, tag) = match name.rsplit_once(':') {
        Some((repo, tag)) if !tag.contains('/') => (repo, tag),
        _ => (name, "latest"),
    };
    let parts: Vec<&str> = repo.split('/').collect();
    if parts.iter().any(|p| p.is_empty() || *p == "." || *p == "..") || tag.contains("..") {
        return Err(format!("{name} isn't a model name"));
    }
    // "qwen2.5" is library/qwen2.5 on Ollama's registry; "hf.co/x/y" names
    // its own host.
    let manifest = match parts.as_slice() {
        [model] => root.join("manifests/registry.ollama.ai/library").join(model),
        [user, model] => root.join("manifests/registry.ollama.ai").join(user).join(model),
        _ => parts.iter().fold(root.join("manifests"), |p, part| p.join(part)),
    }
    .join(tag);
    let text = fs::read_to_string(&manifest)
        .map_err(|_| format!("Ollama hasn't pulled {name}; run `ollama pull {name}` or give a .gguf path"))?;
    let manifest: Value = serde_json::from_str(&text).map_err(|_| format!("Ollama's manifest for {name} is unreadable"))?;
    let digest = manifest
        .get("layers")
        .and_then(Value::as_array)
        .and_then(|layers| {
            layers.iter().find(|l| l.get("mediaType").and_then(Value::as_str) == Some("application/vnd.ollama.image.model"))
        })
        .and_then(|l| l.get("digest"))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{name} has no weights llama.cpp can load"))?;
    let hex = digest.strip_prefix("sha256:").filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()));
    let blob = root.join("blobs").join(format!("sha256-{}", hex.ok_or("unexpected digest")?));
    if blob.is_file() { Ok(blob) } else { Err(format!("{name}'s weights are missing from Ollama's folder")) }
}

/// A port free on loopback right now.
fn free_port() -> std::io::Result<u16> {
    Ok(TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?.local_addr()?.port())
}

/// Starts `process` with its output appended to `log`, so a failed load
/// can be read about.
fn spawn_logged(process: &mut Process, log: &Path) -> std::io::Result<Running> {
    let file = fs::OpenOptions::new().create(true).append(true).open(log)?;
    let child = process.stdin(Stdio::null()).stdout(file.try_clone()?).stderr(file).spawn()?;
    let child = Arc::new(Mutex::new(child));
    RUNNING.lock().unwrap().push(child.clone());
    Ok(child)
}

fn exited(child: &Arc<Mutex<Child>>) -> Option<String> {
    let status = child.lock().unwrap().try_wait().ok()??;
    Some(status.to_string())
}

fn wait_for_port(port: u16, child: &Arc<Mutex<Child>>) -> Result<(), String> {
    let deadline = Instant::now() + RPC_START;
    while Instant::now() < deadline {
        if let Some(status) = exited(child) {
            return Err(format!("llama.cpp's RPC server stopped ({status}); see {LOG}"));
        }
        if TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), Duration::from_millis(200)).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err("llama.cpp's RPC server didn't start".into())
}

fn wait_for_health(url: &str, child: &Arc<Mutex<Child>>, log: PathBuf) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let deadline = Instant::now() + LOAD;
    while Instant::now() < deadline {
        if let Some(status) = exited(child) {
            let why = last_error(&log).unwrap_or_else(|| format!("llama-server stopped ({status})"));
            return Err(format!("{why} (more in {})", log.display()));
        }
        if let Ok(response) = agent.get(format!("{url}/health")).call()
            && response.status() == 200
        {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(1));
    }
    Err("loading took too long".into())
}

/// The last line of `log` that sounds like an error.
fn last_error(log: &Path) -> Option<String> {
    let text = fs::read_to_string(log).ok()?;
    let line = text.lines().rev().find(|l| {
        let l = l.to_ascii_lowercase();
        l.contains("error") || l.contains("failed")
    })?;
    Some(line.trim().chars().take(200).collect())
}

/// This machine's memory, in bytes, or 0 when it can't tell.
pub fn memory() -> u64 {
    #[cfg(target_os = "macos")]
    {
        Process::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }
    #[cfg(not(target_os = "macos"))]
    {
        fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| {
                let line = m.lines().find(|l| l.starts_with("MemTotal:"))?;
                let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
                Some(kb * 1024)
            })
            .unwrap_or(0)
    }
}

/// Bytes as "16 GB", or "? GB" when unknown.
pub fn size(bytes: u64) -> String {
    if bytes == 0 {
        return "? GB".into();
    }
    format!("{} GB", (bytes as f64 / (1u64 << 30) as f64).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_round_trips_and_hides_garble() {
        let join = Wire::Join { id: "1".into(), group: "g".into(), model: "m".into(), key: "k".into() };
        assert_eq!(Wire::decode(&join.encode()), Some(Some(join)));
        assert_eq!(Wire::decode(&format!("{MARK}{{nonsense")), Some(None));
        assert_eq!(Wire::decode("an ordinary message"), None);
        // The model lending marker is a different message.
        assert_eq!(Wire::decode(&super::super::lend::Wire::Gone.encode()), None);
    }

    fn ollama_folder() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("hopchat-pool-{}", crate::engine::new_file_id()));
        let hex = "ab".repeat(32);
        let blob = root.join("blobs").join(format!("sha256-{hex}"));
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        fs::write(&blob, b"GGUF").unwrap();
        let manifest = json!({"layers": [
            {"mediaType": "application/vnd.ollama.image.template", "digest": "sha256:00"},
            {"mediaType": "application/vnd.ollama.image.model", "digest": format!("sha256:{hex}")},
        ]});
        for dir in ["registry.ollama.ai/library/qwen2.5", "registry.ollama.ai/someone/tiny", "hf.co/org/repo"] {
            let dir = root.join("manifests").join(dir);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("72b"), manifest.to_string()).unwrap();
            fs::write(dir.join("latest"), manifest.to_string()).unwrap();
        }
        (root, blob)
    }

    #[test]
    fn finds_the_weights_ollama_pulled() {
        let (root, blob) = ollama_folder();
        assert_eq!(resolve_in(&root, "qwen2.5:72b").unwrap(), blob);
        assert_eq!(resolve_in(&root, "qwen2.5").unwrap(), blob);
        assert_eq!(resolve_in(&root, "someone/tiny").unwrap(), blob);
        assert_eq!(resolve_in(&root, "hf.co/org/repo:latest").unwrap(), blob);
        assert!(resolve_in(&root, "llama3:8b").unwrap_err().contains("ollama pull llama3:8b"));
        assert!(resolve_in(&root, "../../etc/passwd").is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_gguf_path_is_taken_as_it_is() {
        let (root, blob) = ollama_folder();
        let gguf = root.join("model.gguf");
        fs::copy(&blob, &gguf).unwrap();
        assert_eq!(resolve(gguf.to_str().unwrap()).unwrap(), gguf);
        assert!(resolve(root.join("missing.gguf").to_str().unwrap()).is_err());
        let _ = fs::remove_dir_all(root);
    }

    /// Stand-ins for llama.cpp: an RPC server that echoes, and a
    /// llama-server that answers once every `--rpc` worker has echoed a
    /// ping through its tunnel.
    const FAKE_RPC: &str = r#"#!/usr/bin/env python3
import socket, sys, threading
port = int(sys.argv[sys.argv.index("--port") + 1])
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", port)); s.listen()
def echo(c):
    while True:
        b = c.recv(65536)
        if not b: break
        c.sendall(b)
while True:
    c, _ = s.accept(); threading.Thread(target=echo, args=(c,), daemon=True).start()
"#;
    const FAKE_SERVER: &str = r#"#!/usr/bin/env python3
import json, socket, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
port = int(sys.argv[sys.argv.index("--port") + 1])
workers = sys.argv[sys.argv.index("--rpc") + 1].split(",")
class H(BaseHTTPRequestHandler):
    def reply(self, body):
        data = json.dumps(body).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data))); self.end_headers(); self.wfile.write(data)
    def do_GET(self): self.reply({"status": "ok"})
    def do_POST(self):
        self.rfile.read(int(self.headers["Content-Length"]))
        for w in workers:
            host, p = w.split(":")
            c = socket.create_connection((host, int(p)), timeout=5)
            c.sendall(b"ping" * 50000)
            got = b""
            while len(got) < 200000:
                got += c.recv(65536)
            assert got == b"ping" * 50000
            c.close()
        self.reply({"choices": [{"message": {"role": "assistant", "content": f"pooled over {len(workers)}"}}]})
    def log_message(self, *a): pass
HTTPServer(("127.0.0.1", port), H).serve_forever()
"#;

    fn fake_llama_cpp(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(dir).unwrap();
        for (name, script) in [("ggml-rpc-server", FAKE_RPC), ("llama-server", FAKE_SERVER)] {
            let path = dir.join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// A host and a giver arrange a pool with each other's hidden messages,
    /// and the host's answer crosses the encrypted tunnel to the giver.
    #[test]
    #[ignore = "needs python3 to stand in for llama.cpp"]
    fn a_host_and_a_giver_pool_a_model() {
        use crate::engine::Update;
        use std::sync::mpsc::Receiver;

        let root = std::env::temp_dir().join(format!("hopchat-pool-e2e-{}", crate::engine::new_file_id()));
        let bin = root.join("bin");
        fake_llama_cpp(&bin);
        let gguf = root.join("big.gguf");
        fs::write(&gguf, b"GGUF").unwrap();

        struct Side {
            home: Home,
            engine: Receiver<Input>,
            me: Receiver<Request>,
            _ui: Receiver<Update>,
        }
        let side = |name: &str| {
            let dir = root.join(name);
            fs::create_dir_all(&dir).unwrap();
            let (engine_tx, engine) = mpsc::channel();
            let (ui_tx, ui) = mpsc::channel();
            let (me_tx, me) = mpsc::channel();
            let mut home = Home::open(&dir, engine_tx, ui_tx, me_tx);
            home.config.llama_dir = Some(bin.clone());
            Side { home, engine, me, _ui: ui }
        };
        let mut host = side("hank");
        let mut giver = side("gina");
        giver.home.config.pool_give = true;
        let loopback = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));

        // Each side's engine: pool messages go to the other side.
        let route = |from: &mut Side, to: &mut Side, me: &str, who: &str| {
            while let Ok(input) = from.engine.try_recv() {
                if let Input::Ui(Command::PoolSend { wire, .. }) = input {
                    to.home.handle(Request::PoolWire { from: me.into(), who: who.into(), ip: loopback, wire });
                }
            }
            while let Ok(request) = from.me.try_recv() {
                from.home.handle(request);
            }
        };

        // Answer the givers question from another thread: start blocks on it.
        let (givers_tx, givers_rx) = mpsc::channel::<Input>();
        let engine = std::mem::replace(&mut host.engine, givers_rx);
        thread::spawn(move || {
            for input in engine {
                if let Input::Ui(Command::PoolGivers { reply, .. }) = input {
                    let _ = reply.send(vec![Giver { address: "gina".into(), nick: "gina".into(), mem: 8 << 30, ip: loopback }]);
                } else if givers_tx.send(input).is_err() {
                    break;
                }
            }
        });

        host.home.handle(Request::Configure { group: Some("g".into()), args: format!("pool start {}", gguf.display()) });
        let deadline = Instant::now() + Duration::from_secs(30);
        while host.home.pools.url().is_none() {
            assert!(Instant::now() < deadline, "the pool never became ready");
            route(&mut host, &mut giver, "hank", "hank");
            route(&mut giver, &mut host, "gina", "gina");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(giver.home.pools.serving.is_some());

        let (url, model) = host.home.pools.url().unwrap();
        assert_eq!(model, gguf.display().to_string());
        let answer = ask(&url, &[json!({"role": "user", "content": "hi"})]).unwrap();
        assert_eq!(answer["content"], "pooled over 1");

        host.home.handle(Request::Configure { group: None, args: "pool stop".into() });
        route(&mut host, &mut giver, "hank", "hank");
        assert!(host.home.pools.hosting.is_none());
        assert!(giver.home.pools.serving.is_none(), "the giver should leave when the host stops");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sizes_read_like_people_say_them() {
        assert_eq!(size(17_179_869_184), "16 GB");
        assert_eq!(size(0), "? GB");
    }
}
