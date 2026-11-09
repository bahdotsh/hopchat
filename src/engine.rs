//! Owns the `OfflineProtocol` instance on one thread and pumps it: network
//! frames in, UI commands in, protocol events and chat messages out.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use offline_protocol::{
    account_storage_namespace, Error as ProtocolError, EstablishmentState, Event, FileStorePair,
    MediaSendOptions, OfflineProtocol, ProtocolConfig, SendMessageOptions, StaticStoreKey,
};
use offline_protocol_core::{ContentType, ReplyContext, UserId};
use offline_protocol_transport::{
    Transport, TransportStatus, TransportType, WifiDirectTransport,
};

use crate::home;
use crate::lobby::{self, LOBBY};
use crate::me::{self, ME};
use crate::net::{self, LocalIdentity, NetEvent};

pub const APP_ID: &str = "dev.hopchat";
const PROFILE: &str = "default";
const TICK: Duration = Duration::from_millis(30);
/// The SDK's receive-side cap; larger files would be dropped by the peer.
pub const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
/// A quoted reply carries at most this much of the original text.
const QUOTE_CHARS: usize = 120;

/// The message a new one answers.
#[derive(Clone)]
pub struct Quote {
    pub id: String,
    pub sender: String,
    pub text: String,
}

/// What the UI asks the engine to do.
pub enum Command {
    Send {
        to: String,
        text: String,
        reply: Option<Quote>,
    },
    /// Queue `path` for `to` under `file_id`. Pass the id of an interrupted
    /// transfer to resume it; otherwise mint one with [`new_file_id`].
    SendFile {
        to: String,
        path: PathBuf,
        file_id: String,
    },
    Typing {
        to: String,
        on: bool,
    },
    Read {
        to: String,
        ids: Vec<String>,
    },
    /// Create a group and invite `members` into it.
    CreateGroup {
        name: String,
        members: Vec<String>,
    },
    Invite {
        group: String,
        member: String,
    },
    Leave {
        group: String,
    },
    SendGroup {
        group: String,
        text: String,
        reply: Option<Quote>,
        /// Written by home itself, so never handed back to it.
        from_home: bool,
    },
    /// The model this machine lends, or `None` to stop lending.
    Offer(Option<String>),
    /// Have a group member's model think about `messages`.
    Borrow {
        group: String,
        messages: Vec<serde_json::Value>,
        tools: serde_json::Value,
        /// Lenders that already failed this turn.
        skip: Vec<String>,
        reply: Sender<home::lend::Borrowed>,
    },
    /// What this machine's model thought, for whoever borrowed it.
    Lend {
        to: String,
        id: String,
        result: Result<serde_json::Value, String>,
    },
    /// Whether this machine gives its memory to pools, and how much.
    PoolGive(Option<u64>),
    /// A hidden pool message for `to`.
    PoolSend { to: String, wire: home::pool::Wire },
    /// The givers in `group` this machine is directly connected to.
    PoolGivers {
        group: String,
        reply: Sender<Vec<home::pool::Giver>>,
    },
}

/// What the engine tells the UI.
pub enum Update {
    PeerSeen { address: String, nick: String },
    PeerUp { address: String },
    PeerDown { address: String },
    /// An MLS session with `address` is usable (new or restored).
    SessionReady { address: String },
    /// A send was accepted locally under `id`.
    Queued {
        to: String,
        id: String,
        text: String,
        reply: Option<Quote>,
    },
    Incoming {
        from: String,
        id: String,
        text: String,
        hops: u8,
        reply: Option<Quote>,
    },
    /// A file is waiting for a session or a free transfer slot.
    FileQueued {
        file_id: String,
        name: String,
        size: u64,
    },
    /// Chunks of a transfer, either direction, have been acknowledged.
    FileProgress {
        file_id: String,
        done: u32,
        total: u32,
        /// Name and size, known for inbound transfers once chunk 0 lands.
        inbound: Option<(String, u64)>,
    },
    /// Every chunk of an outbound file was acknowledged.
    FileSent { file_id: String },
    FileFailed { file_id: String, reason: String },
    /// A whole file arrived and was saved to `path`.
    FileReceived {
        from: String,
        file_id: String,
        name: String,
        size: u64,
        path: PathBuf,
    },
    /// An inbound file was dropped before it completed.
    FileLost {
        from: String,
        file_id: String,
        name: String,
        reason: String,
    },
    /// A send of ours that was in flight when we last quit must be
    /// re-supplied from disk.
    ResendWanted { file_id: String, to: String },
    Typing { from: String, on: bool },
    Read { from: String, ids: Vec<String> },
    /// A group we are in, as it stands now (joined, created or changed).
    Group {
        id: String,
        name: String,
        members: Vec<String>,
    },
    GroupLeft { id: String },
    /// A group this UI asked for exists (after any `Group` for it).
    GroupCreated { id: String },
    /// A group send was accepted locally under `id`.
    GroupQueued {
        group: String,
        id: String,
        text: String,
        reply: Option<Quote>,
    },
    GroupIncoming {
        group: String,
        from: String,
        id: String,
        text: String,
        reply_to: Option<String>,
    },
    /// Something home did or wants the person at this machine to know.
    Home(String),
    /// What the Devices panel should show now.
    HomeView(Box<home::View>),
    Protocol(Box<Event>),
    Error(String),
}

pub enum Input {
    Net(NetEvent),
    Ui(Command),
}

struct PendingFile {
    to: String,
    path: PathBuf,
    file_id: String,
}

pub struct Engine {
    protocol: OfflineProtocol,
    streams: HashMap<String, TcpStream>,
    updates: Sender<Update>,
    /// Protocol events, routed through the engine so file events can be
    /// enriched (and files saved) before the UI hears of them.
    events: Receiver<Event>,
    /// Files waiting for a session or a free slot, oldest first.
    outbox: VecDeque<PendingFile>,
    /// When to try the outbox again after the SDK refused a transfer, so a
    /// waiting file is not read from disk every tick.
    files_due: Instant,
    downloads: PathBuf,
    /// Where group messages that mention `@home` or `@ai` go, when home is
    /// running.
    home: Option<Sender<home::Request>>,
    /// Beacon nicks, for telling home who is asking.
    nicks: HashMap<String, String>,
    nick: String,
    address: String,
    /// The model this machine lends, once it is ready.
    offer: Option<String>,
    /// Peers that lend their model, and which model.
    lenders: HashMap<String, String>,
    /// Borrowed turns waiting for their lender, by request id.
    borrows: HashMap<String, Borrow>,
    /// The memory this machine gives to pools, when it gives.
    giving: Option<u64>,
    /// Peers who give to pools, and how much memory.
    givers: HashMap<String, u64>,
    /// Peers told this machine gives, so only they hear when it stops.
    /// Older clients don't know pool messages and would show them as chat.
    told_give: HashSet<String>,
}

struct Borrow {
    lender: String,
    by: String,
    reply: Sender<home::lend::Borrowed>,
    since: Instant,
}

/// A fresh transfer id: our own, so the UI can track a file from the moment
/// it is queued rather than only once the SDK accepts it.
pub fn new_file_id() -> String {
    let mut bytes = [0u8; 12];
    let _ = getrandom::fill(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("hc_{hex}")
}

/// Opens (or creates) this profile's identity and stores under `root`, and
/// returns the engine with the identity it presents to peers and the key
/// the profile's other local files are sealed with.
pub fn open(
    root: &Path,
    nick: &str,
    downloads: PathBuf,
    updates: Sender<Update>,
) -> Result<(Engine, LocalIdentity, [u8; 32]), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(root)?;
    let store_key = load_or_create_key(&root.join("store.key"))?;
    let key = StaticStoreKey::new(store_key);
    let namespace = account_storage_namespace(APP_ID, PROFILE);
    let stores = FileStorePair::open(root.join("keys"), root.join("state"), &namespace, &key)?;

    let config = ProtocolConfig::builder(APP_ID, PROFILE)
        .ble_enabled(false)
        .internet_enabled(false)
        .wifi_direct_enabled(true)
        .require_encryption(true)
        .build()?;
    let mut protocol = OfflineProtocol::new(config)?;
    protocol.initialize_mls(stores.secure_storage(), stores.state_storage())?;

    let address = protocol
        .local_address()
        .ok_or("identity was not established")?
        .to_string();
    let preamble = protocol
        .mls_manager()
        .ok_or("MLS is not initialized")?
        .read()
        .map_err(|_| "MLS lock poisoned")?
        .identity_assertion(&[])?;

    let lan = WifiDirectTransport::new(address.clone());
    lan.start()?;
    lan.on_status_changed(TransportStatus::Available);
    protocol
        .transport_manager_mut()
        .add_transport(TransportType::WiFiDirect, Box::new(lan));

    // The callback runs under the SDK's state lock, so it may only queue.
    let (event_tx, events) = mpsc::channel();
    protocol.on_event(move |event| {
        let _ = event_tx.send(event);
    });
    protocol.start()?;

    let me = LocalIdentity {
        address: address.clone(),
        nick: nick.to_string(),
        preamble,
    };
    Ok((
        Engine {
            protocol,
            streams: HashMap::new(),
            updates,
            events,
            outbox: VecDeque::new(),
            files_due: Instant::now(),
            downloads,
            home: None,
            nicks: HashMap::new(),
            nick: nick.to_string(),
            address,
            offer: None,
            lenders: HashMap::new(),
            giving: None,
            givers: HashMap::new(),
            told_give: HashSet::new(),
            borrows: HashMap::new(),
        },
        me,
        store_key,
    ))
}

/// A key that exists but cannot be read is an error, never replaced: a new
/// key would make every store and all history undecryptable.
fn load_or_create_key(path: &Path) -> std::io::Result<[u8; 32]> {
    match std::fs::read(path) {
        Ok(bytes) => {
            return <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
                std::io::Error::other(format!("{} is damaged: expected 32 bytes, found {}", path.display(), bytes.len()))
            });
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        Err(_) => {}
    }
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|e| std::io::Error::other(e.to_string()))?;
    // Written aside and renamed, so a crash never leaves a short key behind.
    let fresh = path.with_extension("new");
    write_private(&fresh, &key)?;
    std::fs::rename(&fresh, path)?;
    Ok(key)
}

#[cfg(unix)]
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

impl Engine {
    fn lan(&self) -> Option<Arc<dyn Transport>> {
        self.protocol
            .transport_manager()
            .get_transport(TransportType::WiFiDirect)
    }

    fn with_lan(&self, f: impl FnOnce(&WifiDirectTransport)) {
        if let Some(lan) = self.lan()
            && let Some(lan) = lan.as_any().downcast_ref::<WifiDirectTransport>() {
                f(lan);
            }
    }

    pub fn set_home(&mut self, home: Sender<home::Request>) {
        self.home = Some(home);
    }

    fn update(&self, update: Update) {
        let _ = self.updates.send(update);
    }

    pub fn run(mut self, inputs: Receiver<Input>) {
        // Groups live in MLS storage; tell the UI which we are already in.
        for id in self.protocol.list_groups().unwrap_or_default() {
            self.announce_group(&id);
        }
        loop {
            match inputs.recv_timeout(TICK) {
                Ok(input) => self.handle(input),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while let Ok(input) = inputs.try_recv() {
                self.handle(input);
            }
            self.pump();
        }
    }

    fn handle(&mut self, input: Input) {
        match input {
            Input::Net(NetEvent::Seen { address, nick }) => {
                self.nicks.insert(address.clone(), nick.clone());
                self.update(Update::PeerSeen { address, nick });
            }
            Input::Net(NetEvent::Up { address, stream }) => {
                self.streams.insert(address.clone(), stream);
                self.with_lan(|lan| lan.on_peer_connected(address.clone()));
                // Also sends our key package, which starts the MLS handshake.
                self.protocol.on_neighbor_discovered(&address);
                // A session restored from storage fires no "established"
                // event, so report it here.
                let restored = self.session_ready(&address);
                self.update(Update::PeerUp {
                    address: address.clone(),
                });
                if restored {
                    self.offer_to(&address);
                    self.give_to(&address);
                    self.update(Update::SessionReady { address });
                }
            }
            Input::Net(NetEvent::Down { address }) => self.drop_peer(&address),
            Input::Net(NetEvent::Frame { address, body }) => {
                if let Some(lan) = self.lan()
                    && let Err(e) = lan.on_data_received_from(body, address) {
                        self.error(format!("inbound frame refused: {e}"));
                    }
            }
            Input::Ui(Command::Send { to, text, reply }) => self.send_text(to, text, reply),
            Input::Ui(Command::SendFile { to, path, file_id }) => {
                self.queue_file(to, path, file_id)
            }
            Input::Ui(Command::Typing { to, on }) => {
                // Advisory: only worth sending over a live session.
                if self.streams.contains_key(&to) && self.session_ready(&to) {
                    let _ = self.protocol.send_typing_indicator(&to, &to, on);
                }
            }
            Input::Ui(Command::Read { to, ids }) => {
                for ids in ids.chunks(256) {
                    let _ = self.protocol.send_read_receipt(&to, ids.to_vec());
                }
            }
            Input::Ui(Command::CreateGroup { name, members }) => {
                match self.protocol.create_group(&name) {
                    Ok(info) => {
                        let id = info.group_id.as_str().to_string();
                        self.announce_group(&id);
                        for member in members {
                            self.invite(&id, &member);
                        }
                        self.update(Update::GroupCreated { id });
                    }
                    Err(e) => self.error(format!("group not created: {e}")),
                }
            }
            Input::Ui(Command::Invite { group, member }) => self.invite(&group, &member),
            Input::Ui(Command::Leave { group }) => match self.protocol.leave_group(&group) {
                Ok(()) => self.update(Update::GroupLeft { id: group }),
                Err(e) => self.error(format!("leave failed: {e}")),
            },
            Input::Ui(Command::SendGroup {
                group,
                text,
                reply,
                from_home,
            }) => self.send_group(group, text, reply, from_home),
            Input::Ui(Command::Offer(model)) => {
                self.offer = model;
                let peers: Vec<String> = self.streams.keys().cloned().collect();
                for peer in peers {
                    self.offer_to(&peer);
                }
            }
            Input::Ui(Command::Borrow {
                group,
                messages,
                tools,
                skip,
                reply,
            }) => self.borrow(group, messages, tools, skip, reply),
            Input::Ui(Command::Lend { to, id, result }) => {
                let wire = match result {
                    Ok(message) => home::lend::Wire::Turn { id, message },
                    Err(error) => home::lend::Wire::Fail { id, error },
                };
                self.send_hidden(&to, &wire);
            }
            Input::Ui(Command::PoolGive(mem)) => {
                if mem != self.giving {
                    self.giving = mem;
                    let mut peers: Vec<String> = self.streams.keys().cloned().collect();
                    peers.extend(self.told_give.iter().filter(|p| !self.streams.contains_key(*p)).cloned());
                    for peer in peers {
                        self.give_to(&peer);
                    }
                }
            }
            Input::Ui(Command::PoolSend { to, wire }) => self.send_pool(&to, &wire),
            Input::Ui(Command::PoolGivers { group, reply }) => {
                let mut givers: Vec<home::pool::Giver> = self
                    .givers
                    .iter()
                    .filter(|(address, _)| {
                        **address != self.address && self.shares(&group, address) && self.session_ready(address)
                    })
                    .map(|(address, mem)| home::pool::Giver {
                        address: address.clone(),
                        nick: self.nick_or_address(address),
                        mem: *mem,
                        ip: self.ip_of(address),
                    })
                    .collect();
                givers.sort_by(|a, b| a.address.cmp(&b.address));
                let _ = reply.send(givers);
            }
        }
    }

    fn invite(&mut self, group: &str, member: &str) {
        match self.protocol.invite_to_group(group, member) {
            Ok(()) => self.announce_group(group),
            Err(ProtocolError::NoKeyPackage(_)) => {
                // Ask for a fresh one; the invite can be retried once it
                // arrives.
                if self.streams.contains_key(member) {
                    self.protocol.on_neighbor_discovered(member);
                }
                let who = self.nicks.get(member).map_or(member, |n| n.as_str());
                self.error(format!(
                    "invite failed: no key package from {who} yet, try again in a moment"
                ));
            }
            Err(e) => self.error(format!("invite failed: {e}")),
        }
    }

    fn announce_group(&self, id: &str) {
        let Ok(Some(info)) = self.protocol.get_group_info(id) else {
            return;
        };
        self.update(Update::Group {
            id: id.to_string(),
            name: info.name.unwrap_or_else(|| "group".into()),
            members: info.members,
        });
    }

    fn send_group(&mut self, group: String, text: String, reply: Option<Quote>, from_home: bool) {
        if group == LOBBY {
            return self.send_lobby(text, reply, from_home);
        }
        if group == ME {
            return self.send_me(text, reply, from_home);
        }
        let reply_to = reply.as_ref().map(|q| q.id.clone());
        match self
            .protocol
            .send_group_message(&group, &text, None, reply_to.as_deref())
        {
            Ok(ids) => {
                // One SDK message per member; the first names the whole send.
                let id = ids
                    .first()
                    .map(|id| id.as_str())
                    .unwrap_or_else(new_file_id);
                self.update(Update::GroupQueued {
                    group: group.clone(),
                    id: id.clone(),
                    text: text.clone(),
                    reply,
                });
                if !from_home {
                    self.to_home(&group, &self.address, &id, &text);
                }
            }
            Err(e) => self.error(format!("send failed: {e}")),
        }
    }

    /// Sends to everyone in the lobby right now (see [`lobby`]).
    fn send_lobby(&mut self, text: String, reply: Option<Quote>, from_home: bool) {
        let wire = lobby::Wire {
            id: lobby::new_id(),
            text: text.clone(),
            reply_to: reply.as_ref().map(|q| q.id.clone()),
        };
        let body = wire.encode();
        for peer in self.lobby_members() {
            if let Err(e) = self.protocol.send_message_with(&peer, &body, SendMessageOptions::default()) {
                self.error(format!("lobby message to {}: {e}", self.nick_or_address(&peer)));
            }
        }
        self.update(Update::GroupQueued {
            group: LOBBY.into(),
            id: wire.id.clone(),
            text: text.clone(),
            reply,
        });
        if !from_home {
            self.to_home(LOBBY, &self.address, &wire.id, &text);
        }
    }

    /// Keeps it here: nothing goes on the wire (see [`me`]).
    fn send_me(&mut self, text: String, reply: Option<Quote>, from_home: bool) {
        let id = me::new_id();
        self.update(Update::GroupQueued {
            group: ME.into(),
            id: id.clone(),
            text: text.clone(),
            reply,
        });
        if !from_home {
            self.to_home(ME, &self.address, &id, &text);
        }
    }

    /// Everyone else in the lobby: each peer connected with a secure
    /// session.
    fn lobby_members(&self) -> Vec<String> {
        let mut peers: Vec<String> = self
            .streams
            .keys()
            .filter(|p| self.session_ready(p))
            .cloned()
            .collect();
        peers.sort();
        peers
    }

    /// Whether `peer` is in `group` with us. Asked about someone's own
    /// space, a model is lent on the lobby's terms: a secure session.
    fn shares(&self, group: &str, peer: &str) -> bool {
        if group == LOBBY || group == ME {
            return self.streams.contains_key(peer) && self.session_ready(peer);
        }
        matches!(
            self.protocol.get_group_info(group),
            Ok(Some(info)) if info.members.iter().any(|m| m == peer)
        )
    }

    /// Hands a group message that mentions home, or the model, to home.
    fn to_home(&self, group: &str, sender: &str, id: &str, text: &str) {
        // Another member's home or model talking (its "mention @home" hint
        // included) is not a request.
        if text.starts_with(home::SIGNATURE) || text.starts_with(home::ai::SIGNATURE) {
            return;
        }
        let Some(home) = &self.home else { return };
        let mine = sender == self.address;
        let from = if mine {
            self.nick.clone()
        } else {
            self.nicks.get(sender).cloned().unwrap_or_else(|| sender.to_string())
        };
        if let Some(text) = home::addressed(text) {
            let _ = home.send(home::Request::Ask {
                group: group.to_string(),
                from: from.clone(),
                text,
                mine,
            });
        }
        if let Some(question) = home::ai::addressed(text) {
            let _ = home.send(home::Request::Chat(home::ai::Question {
                group: group.to_string(),
                from,
                id: id.to_string(),
                sender: sender.to_string(),
                text: question,
                mine,
            }));
        }
    }

    // ----- lending and borrowing models (see home::lend) -----------------

    fn send_hidden(&mut self, to: &str, wire: &home::lend::Wire) {
        if let Err(e) = self.protocol.send_message_with(to, wire.encode(), SendMessageOptions::default()) {
            self.error(format!("model message to {}: {e}", self.nick_or_address(to)));
        }
    }

    /// Tells `peer` whether this machine lends its model.
    fn offer_to(&mut self, peer: &str) {
        if !self.session_ready(peer) {
            return;
        }
        let wire = match &self.offer {
            Some(model) => home::lend::Wire::Have { model: model.clone() },
            None => home::lend::Wire::Gone,
        };
        self.send_hidden(peer, &wire);
    }

    // ----- pools (see home::pool) -------------------------------------------

    fn send_pool(&mut self, to: &str, wire: &home::pool::Wire) {
        if let Err(e) = self.protocol.send_message_with(to, wire.encode(), SendMessageOptions::default()) {
            self.error(format!("pool message to {}: {e}", self.nick_or_address(to)));
        }
    }

    /// Tells `peer` this machine gives to pools, or that it stopped. Nobody
    /// hears anything about pools until this machine has given once, and
    /// only those told it gives are told it stopped.
    fn give_to(&mut self, peer: &str) {
        if !self.session_ready(peer) {
            return;
        }
        if let Some(wire) = home::pool::give_wire(self.giving, &mut self.told_give, peer) {
            self.send_pool(peer, &wire);
        }
    }

    /// Where a directly connected peer's stream comes from.
    fn ip_of(&self, address: &str) -> Option<std::net::IpAddr> {
        self.streams.get(address)?.peer_addr().ok().map(|a| a.ip())
    }

    fn tell_home_givers(&self) {
        if let Some(home) = &self.home {
            let mut givers: Vec<(String, u64)> =
                self.givers.iter().map(|(address, mem)| (self.nick_or_address(address), *mem)).collect();
            givers.sort();
            let _ = home.send(home::Request::PoolGivers(givers));
        }
    }

    /// A hidden pool message from `from`.
    fn on_pool(&mut self, from: String, wire: home::pool::Wire) {
        use home::pool::Wire;
        match wire {
            Wire::Give { mem } => {
                self.givers.insert(from, mem);
                self.tell_home_givers();
            }
            Wire::Keep => {
                if self.givers.remove(&from).is_some() {
                    self.tell_home_givers();
                }
            }
            wire => match home::pool::admit(wire, |group| self.shares(group, &from)) {
                Err(refuse) => self.send_pool(&from, &refuse),
                Ok(wire) => match &self.home {
                    Some(home) => {
                        let peer = home::pool::Peer { who: self.nick_or_address(&from), ip: self.ip_of(&from), address: from };
                        let _ = home.send(home::Request::PoolWire { peer, wire });
                    }
                    None => {
                        if let Wire::Join { id, .. } = wire {
                            self.send_pool(&from, &Wire::Refuse { id, error: "isn't running home".into() });
                        }
                    }
                },
            },
        }
    }

    fn nick_or_address(&self, address: &str) -> String {
        self.nicks.get(address).cloned().unwrap_or_else(|| address.to_string())
    }

    fn tell_home_lenders(&self) {
        if let Some(home) = &self.home {
            let mut lenders: Vec<String> = self
                .lenders
                .iter()
                .map(|(address, model)| format!("{} ({model})", self.nick_or_address(address)))
                .collect();
            lenders.sort();
            let _ = home.send(home::Request::Lenders(lenders));
        }
    }

    fn borrow(
        &mut self,
        group: String,
        messages: Vec<serde_json::Value>,
        tools: serde_json::Value,
        skip: Vec<String>,
        reply: Sender<home::lend::Borrowed>,
    ) {
        let mut candidates: Vec<(&String, &String)> = self
            .lenders
            .iter()
            .filter(|(address, _)| {
                self.shares(&group, address)
                    && **address != self.address
                    && !skip.contains(address)
                    && self.streams.contains_key(*address)
            })
            .collect();
        candidates.sort();
        let Some((lender, model)) = candidates.first().map(|(a, m)| (a.to_string(), m.to_string())) else {
            let _ = reply.send(home::lend::Borrowed::Nobody);
            return;
        };
        if !self.session_ready(&lender) {
            let _ = reply.send(home::lend::Borrowed::Failed {
                lender,
                error: "no secure session yet".into(),
            });
            return;
        }
        let id = new_file_id();
        let by = format!("{}'s {model}", self.nick_or_address(&lender));
        self.send_hidden(
            &lender,
            &home::lend::Wire::Ask { id: id.clone(), group, messages, tools },
        );
        self.borrows.insert(id, Borrow { lender, by, reply, since: Instant::now() });
    }

    /// A hidden model message from `from`.
    fn on_wire(&mut self, from: String, wire: home::lend::Wire) {
        use home::lend::{Borrowed, Wire};
        match wire {
            Wire::Have { model } => {
                self.lenders.insert(from, model);
                self.tell_home_lenders();
            }
            Wire::Gone => {
                if self.lenders.remove(&from).is_some() {
                    self.tell_home_lenders();
                }
            }
            Wire::Ask { id, group, messages, tools } => {
                // Only for someone in a group, or the lobby, we are both in.
                let shared = self.shares(&group, &from);
                match &self.home {
                    Some(home) if shared => {
                        let who = self.nick_or_address(&from);
                        let _ = home.send(home::Request::Lend { to: from, who, id, messages, tools });
                    }
                    _ => self.send_hidden(
                        &from,
                        &Wire::Fail { id, error: "we aren't in that group together".into() },
                    ),
                }
            }
            Wire::Turn { id, message } => {
                if let Some(b) = self.borrows.remove(&id).filter(|b| b.lender == from) {
                    let _ = b.reply.send(Borrowed::Answer { by: b.by, message });
                }
            }
            Wire::Fail { id, error } => {
                if let Some(b) = self.borrows.remove(&id).filter(|b| b.lender == from) {
                    let error = format!("{}: {error}", b.by);
                    let _ = b.reply.send(Borrowed::Failed { lender: b.lender, error });
                }
            }
        }
    }

    fn on_lobby(&mut self, from: String, wire: lobby::Wire) {
        self.to_home(LOBBY, &from, &wire.id, &wire.text);
        self.update(Update::GroupIncoming {
            group: LOBBY.into(),
            from,
            id: wire.id,
            text: wire.text,
            reply_to: wire.reply_to,
        });
    }

    fn session_ready(&self, address: &str) -> bool {
        matches!(
            self.protocol.get_establishment_state(address),
            Ok(EstablishmentState::SessionConfirmed)
        )
    }

    fn send_text(&mut self, to: String, text: String, reply: Option<Quote>) {
        let options = SendMessageOptions {
            reply_to_msg: reply.as_ref().map(|q| q.id.clone()),
            reply_context: reply.as_ref().and_then(|q| {
                Some(ReplyContext {
                    sender: UserId::new(q.sender.clone()).ok()?,
                    text: q.text.chars().take(QUOTE_CHARS).collect(),
                    timestamp: None,
                    reply_media_label: None,
                    reply_content_type: None,
                })
            }),
            ..Default::default()
        };
        match self.protocol.send_message_with(&to, &text, options) {
            Ok(id) => self.update(Update::Queued {
                to,
                id: id.as_str(),
                text,
                reply,
            }),
            Err(e) => self.error(format!("send failed: {e}")),
        }
    }

    fn queue_file(&mut self, to: String, path: PathBuf, file_id: String) {
        let size = match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => meta.len(),
            Ok(_) => return self.file_failed(file_id, "not a regular file".into()),
            Err(e) => return self.file_failed(file_id, e.to_string()),
        };
        if size == 0 {
            return self.file_failed(file_id, "file is empty".into());
        }
        if size > MAX_FILE_BYTES {
            return self.file_failed(file_id, "larger than 100 MB".into());
        }
        let name = display_name(&path);
        self.update(Update::FileQueued {
            file_id: file_id.clone(),
            name,
            size,
        });
        self.outbox.push_back(PendingFile { to, path, file_id });
        self.files_due = Instant::now();
    }

    /// Hands queued files to the SDK as sessions and transfer slots allow.
    /// A file that cannot start yet keeps its place; the rest still try.
    fn start_files(&mut self) {
        let mut waiting = VecDeque::new();
        let mut refused = false;
        while let Some(file) = self.outbox.pop_front() {
            if !self.streams.contains_key(&file.to) || !self.session_ready(&file.to) {
                waiting.push_back(file);
                continue;
            }
            let bytes = match std::fs::read(&file.path) {
                Ok(bytes) => bytes,
                Err(e) => {
                    self.file_failed(file.file_id, e.to_string());
                    continue;
                }
            };
            let options = MediaSendOptions {
                file_id: Some(file.file_id.clone()),
                ..Default::default()
            };
            let name = display_name(&file.path);
            let kind = content_type_of(&file.path);
            match self
                .protocol
                .send_media_with(&file.to, bytes, name, kind, options)
            {
                Ok(_) => {}
                Err(ProtocolError::SessionNotReady(_) | ProtocolError::MediaTransferLimit(_)) => {
                    refused = true;
                    waiting.push_back(file)
                }
                Err(e) => self.file_failed(file.file_id, e.to_string()),
            }
        }
        self.outbox = waiting;
        if refused {
            self.files_due = Instant::now() + Duration::from_secs(1);
        }
    }

    fn file_failed(&self, file_id: String, reason: String) {
        self.update(Update::FileFailed { file_id, reason });
    }

    fn drop_peer(&mut self, address: &str) {
        if let Some(stream) = self.streams.remove(address) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        self.with_lan(|lan| {
            lan.on_peer_disconnected(address);
        });
        self.protocol.on_neighbor_lost(address);
        if self.lenders.remove(address).is_some() {
            self.tell_home_lenders();
        }
        if self.givers.remove(address).is_some() {
            self.tell_home_givers();
        }
        // It forgets this machine gives when it loses it, too.
        self.told_give.remove(address);
        if let Some(home) = &self.home {
            let who = self.nick_or_address(address);
            let _ = home.send(home::Request::PoolPeerGone { address: address.to_string(), who });
        }
        let gone: Vec<String> = self
            .borrows
            .iter()
            .filter(|(_, b)| b.lender == address)
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            if let Some(b) = self.borrows.remove(&id) {
                let error = format!("{} went away", b.by);
                let _ = b.reply.send(home::lend::Borrowed::Failed { lender: b.lender, error });
            }
        }
        self.update(Update::PeerDown {
            address: address.to_string(),
        });
    }

    /// One turn of the engine: run timers, surface chat, flush the wire.
    fn pump(&mut self) {
        if !self.outbox.is_empty() && Instant::now() >= self.files_due {
            self.start_files();
        }
        if let Err(e) = self.protocol.process() {
            self.error(format!("process: {e}"));
        }
        // Whoever borrowed stopped waiting long ago.
        self.borrows.retain(|_, b| b.since.elapsed() < Duration::from_secs(300));
        while let Some(message) = self.protocol.receive_message() {
            if let Some(wire) = home::lend::Wire::decode(&message.content) {
                if let Some(wire) = wire {
                    self.on_wire(message.sender.as_str().to_string(), wire);
                }
                continue;
            }
            if let Some(wire) = home::pool::Wire::decode(&message.content) {
                if let Some(wire) = wire {
                    self.on_pool(message.sender.as_str().to_string(), wire);
                }
                continue;
            }
            if let Some(wire) = lobby::Wire::decode(&message.content) {
                if let Some(wire) = wire {
                    self.on_lobby(message.sender.as_str().to_string(), wire);
                }
                continue;
            }
            let reply = message.reply_context.as_ref().map(|r| Quote {
                id: message
                    .reply_to_msg
                    .as_ref()
                    .map(|id| id.as_str())
                    .unwrap_or_default(),
                sender: r.sender.as_str().to_string(),
                text: r.text.clone(),
            });
            self.update(Update::Incoming {
                from: message.sender.as_str().to_string(),
                id: message.id.as_str(),
                text: message.content,
                hops: message.hop_count.value(),
                reply,
            });
        }
        while let Ok(event) = self.events.try_recv() {
            self.on_event(event);
        }

        let Some(lan) = self.lan() else { return };
        let mut broken = Vec::new();
        loop {
            match lan.get_next_message() {
                // A peer whose write already failed would stall each later
                // frame for the full write timeout, so its frames are dropped.
                Ok(Some((peer, _))) if broken.contains(&peer) => {}
                Ok(Some((peer, body))) => match self.streams.get_mut(&peer) {
                    Some(stream) => {
                        if net::write_frame(stream, &body).is_err() {
                            broken.push(peer);
                        }
                    }
                    None => broken.push(peer),
                },
                Ok(None) => break,
                Err(e) => {
                    self.error(format!("outbound frame: {e}"));
                    break;
                }
            }
        }
        broken.dedup();
        for peer in broken {
            if self.streams.contains_key(&peer) {
                self.drop_peer(&peer);
            }
        }
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::SecureSessionEstablished { ref peer_id, .. } => {
                let address = peer_id.clone();
                self.offer_to(&address);
                self.give_to(&address);
                self.update(Update::SessionReady { address });
            }
            Event::GroupMessageReceived {
                ref group_id,
                ref sender,
                ref content,
                ref message_id,
                ref reply_to_msg,
                ..
            } => {
                self.to_home(group_id, sender, message_id, content);
                self.update(Update::GroupIncoming {
                    group: group_id.clone(),
                    from: sender.clone(),
                    id: message_id.clone(),
                    text: content.clone(),
                    reply_to: reply_to_msg.clone(),
                });
            }
            Event::GroupCreated { ref group_id, .. }
            | Event::GroupMemberAdded { ref group_id, .. }
            | Event::GroupMemberRemoved { ref group_id, .. }
            | Event::GroupRenamed { ref group_id, .. } => {
                let id = group_id.clone();
                if self.protocol.list_groups().unwrap_or_default().contains(&id) {
                    self.announce_group(&id);
                } else {
                    self.update(Update::GroupLeft { id });
                }
            }
            Event::FileProgress {
                ref file_id,
                chunks_sent,
                total_chunks,
                ..
            } => {
                let inbound = self
                    .protocol
                    .file_transfer_manager()
                    .get_progress(file_id)
                    .map(|p| (p.file_name, p.file_size));
                self.update(Update::FileProgress {
                    file_id: file_id.clone(),
                    done: chunks_sent,
                    total: total_chunks,
                    inbound,
                });
                return;
            }
            Event::MediaSent { file_id, .. } => {
                return self.update(Update::FileSent { file_id });
            }
            Event::MediaSendFailed {
                file_id, reason, ..
            } => return self.file_failed(file_id, reason),
            Event::MediaResendRequired {
                file_id, recipient, ..
            } => {
                return self.update(Update::ResendWanted {
                    file_id,
                    to: recipient,
                });
            }
            Event::FileReceived {
                file_id,
                file_name,
                file_size,
                sender,
                file_data,
                ..
            } => {
                match self.save(&file_name, &file_data) {
                    Ok(path) => self.update(Update::FileReceived {
                        from: sender,
                        file_id,
                        name: file_name,
                        size: file_size,
                        path,
                    }),
                    Err(e) => self.update(Update::FileLost {
                        from: sender,
                        file_id,
                        name: file_name,
                        reason: format!("could not save: {e}"),
                    }),
                }
                return;
            }
            Event::FileReceiveFailed {
                file_id,
                file_name,
                sender,
                reason,
            } => {
                return self.update(Update::FileLost {
                    from: sender,
                    file_id,
                    name: file_name,
                    reason,
                });
            }
            Event::TypingIndicatorReceived {
                ref sender,
                is_typing,
                ..
            } => self.update(Update::Typing {
                from: sender.clone(),
                on: is_typing,
            }),
            Event::ReadReceiptReceived {
                ref sender,
                ref message_ids,
                ..
            } => self.update(Update::Read {
                from: sender.clone(),
                ids: message_ids.clone(),
            }),
            _ => {}
        }
        self.update(Update::Protocol(Box::new(event)));
    }

    /// Writes a received file into the downloads folder under a name that
    /// cannot escape it and does not overwrite anything already there.
    fn save(&self, name: &str, base64_data: &str) -> std::io::Result<PathBuf> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(base64_data)
            .map_err(std::io::Error::other)?;
        std::fs::create_dir_all(&self.downloads)?;
        let name = safe_file_name(name);
        let (stem, ext) = match name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
            _ => (name.clone(), String::new()),
        };
        for n in 0..1000 {
            let candidate = if n == 0 {
                self.downloads.join(&name)
            } else {
                self.downloads.join(format!("{stem} ({n}){ext}"))
            };
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    file.write_all(&bytes)?;
                    return Ok(candidate);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other("too many files with that name"))
    }

    fn error(&self, message: String) {
        self.update(Update::Error(message));
    }
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into())
}

/// A peer-chosen file name reduced to one harmless path component.
pub fn safe_file_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = last
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .take(120)
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        "file".into()
    } else {
        cleaned
    }
}

fn content_type_of(path: &Path) -> ContentType {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "bmp" | "svg" => ContentType::Image,
        "mp4" | "mov" | "mkv" | "webm" | "avi" => ContentType::Video,
        "mp3" | "m4a" | "wav" | "flac" | "ogg" | "opus" | "aac" => ContentType::Audio,
        _ => ContentType::File,
    }
}

#[cfg(test)]
mod tests {
    use super::safe_file_name;

    #[test]
    fn file_names_cannot_escape_downloads() {
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("..\\..\\boot.ini"), "boot.ini");
        assert_eq!(safe_file_name(".bashrc"), "bashrc");
        assert_eq!(safe_file_name(".."), "file");
        assert_eq!(safe_file_name("a\u{1b}[31mb.txt"), "a[31mb.txt");
        assert_eq!(safe_file_name("photo.jpg"), "photo.jpg");
    }
}
