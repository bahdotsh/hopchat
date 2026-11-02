//! The terminal UI: conversations on the left, the open one in the middle,
//! the protocol's own event stream on the right (toggle with Ctrl-E).

mod devices;
mod editor;
mod groups;
mod picker;
mod recall;
mod render;
mod selection;
mod tasks;
pub mod theme;

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use base64::Engine as _;
use offline_protocol::Event;
use ratatui::crossterm::event::{
    self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;
use ratatui::DefaultTerminal;

use crate::engine::{self, Command, Input, Quote, Update};
use crate::history::{self, Body, Conversation, Entry, Quoted, Status, Transfer};
use crate::home;
use crate::lobby::{self, LOBBY};
use crate::me::{self, ME};
use devices::Devices;
use editor::Editor;
use groups::{Candidate, GroupPanel};
use picker::{Outcome, Picker};
use recall::Recall;
use selection::Selection;
use tasks::TaskPanel;

const LOG_LINES: usize = 300;
/// The longest nick or group name kept from the network; a new group's
/// own name is capped the same.
const NAME_MAX: usize = 40;
/// How long a peer's "typing…" lasts without a refresh.
const TYPING_SHOWN: Duration = Duration::from_secs(6);
/// How often we re-announce that we are typing, and how long after the last
/// keystroke we say we stopped.
const TYPING_REFRESH: Duration = Duration::from_secs(3);
const TYPING_IDLE: Duration = Duration::from_secs(5);
const SAVE_EVERY: Duration = Duration::from_secs(1);
const FLASH_FOR: Duration = Duration::from_secs(4);
/// What switching home or the model on or off in Me says.
const ALWAYS_YOURS: &str = "Me is only you: your devices and model always answer you here";

/// Whether `/home ARGS` or `/ai ARGS` switches it on or off somewhere,
/// rather than asking about it, lending it or adding a device.
fn switches(args: &str) -> bool {
    matches!(args.split_whitespace().next(), Some("on" | "off"))
}

pub struct Convo {
    pub data: Conversation,
    pub online: bool,
    pub secure: bool,
    pub unread: usize,
    pub typing_until: Option<Instant>,
    /// For ordering the list: the newest message, or when we first saw them.
    pub active_ms: i64,
}

impl Convo {
    fn new(address: &str) -> Self {
        Self {
            data: Conversation {
                address: address.to_string(),
                nick: render::short(address),
                ..Default::default()
            },
            online: false,
            secure: false,
            unread: 0,
            typing_until: None,
            active_ms: now_ms(),
        }
    }

    pub fn typing(&self) -> bool {
        self.typing_until.is_some_and(|t| t > Instant::now())
    }

    fn push(&mut self, entry: Entry) {
        self.active_ms = self.active_ms.max(entry.at_ms);
        self.data.entries.push(entry);
        self.data.dirty = true;
    }

    fn find(&mut self, id: &str) -> Option<&mut Entry> {
        self.data.entries.iter_mut().rev().find(|e| e.id == id)
    }
}

/// An inbound transfer whose sender we do not know until it completes.
pub struct Incoming {
    pub name: String,
    pub size: u64,
    pub done: u32,
    pub total: u32,
}

pub enum Mode {
    Normal,
    /// Picking a message in the open conversation, by entry index.
    Select(usize),
    Picker(Picker),
    Devices(Devices),
    Groups(GroupPanel),
    /// Todos and reminders.
    Tasks(TaskPanel),
    /// Keys and commands, scrolled down this many rows.
    Help(usize),
}

pub struct App {
    pub me_nick: String,
    pub me_address: String,
    pub convos: Vec<Convo>,
    pub selected: Option<String>,
    pub editor: Editor,
    recall: Recall,
    pub attachments: Vec<PathBuf>,
    pub reply: Option<String>,
    pub mode: Mode,
    /// Rows scrolled up from the newest message.
    pub scroll: usize,
    pub log: VecDeque<Line<'static>>,
    pub show_log: bool,
    pub incoming: HashMap<String, Incoming>,
    /// When each transfer started moving, for its rate.
    pub started: HashMap<String, Instant>,
    pub flash: Option<(String, bool, Instant)>,
    pub downloads: PathBuf,
    /// Areas from the last draw, for the mouse.
    pub peer_rows: Vec<(Rect, String)>,
    pub new_group_row: Rect,
    pub chat_area: Rect,
    pub composer_area: Rect,
    /// The protocol log's text, when it is showing.
    pub log_area: Rect,
    /// Text being dragged over, or just copied.
    selection: Option<Selection>,
    /// The footer's key buttons, and the key each one presses.
    pub footer_hits: Vec<(Rect, KeyEvent)>,
    /// Screen row → the message drawn on it.
    pub message_rows: Vec<(u16, usize)>,
    /// When the boot screen went up; it goes after a moment or a key.
    pub boot: Option<Instant>,
    /// Message id or file id → the conversation it belongs to.
    owners: HashMap<String, String>,
    store: Option<history::Store>,
    last_save: Instant,
    focused: bool,
    typing_sent: Option<(String, Instant)>,
    last_edit: Instant,
    title: String,
    quit: bool,
    home: Sender<home::Request>,
    /// What home last reported, for the Devices panel.
    pub home_view: home::View,
    /// Todos and reminders, kept beside the history.
    pub tasks: crate::tasks::Tasks,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl App {
    pub fn new(
        me_nick: String,
        me_address: String,
        store: Option<history::Store>,
        downloads: PathBuf,
        home: Sender<home::Request>,
    ) -> Self {
        let mut convos = Vec::new();
        let mut owners = HashMap::new();
        let mut tasks = store.as_ref().map(|s| s.load_tasks()).unwrap_or_default();
        tasks.prune(now_ms());
        if let Some(store) = &store {
            for data in store.load() {
                for e in data.entries.iter().filter(|e| e.mine && !e.status.settled()) {
                    owners.insert(e.id.clone(), data.address.clone());
                }
                let active_ms = data.entries.last().map_or(0, |e| e.at_ms);
                // A group we are still in is announced by the engine at start.
                convos.push(Convo {
                    data,
                    online: false,
                    secure: false,
                    unread: 0,
                    typing_until: None,
                    active_ms,
                });
            }
        }
        let mut app = Self {
            me_nick,
            me_address,
            convos,
            selected: None,
            editor: Editor::default(),
            recall: Recall::default(),
            attachments: Vec::new(),
            reply: None,
            mode: Mode::Normal,
            scroll: 0,
            log: VecDeque::new(),
            show_log: false,
            incoming: HashMap::new(),
            started: HashMap::new(),
            flash: None,
            downloads,
            peer_rows: Vec::new(),
            new_group_row: Rect::default(),
            chat_area: Rect::default(),
            composer_area: Rect::default(),
            log_area: Rect::default(),
            selection: None,
            footer_hits: Vec::new(),
            message_rows: Vec::new(),
            boot: None,
            owners,
            store,
            last_save: Instant::now(),
            focused: true,
            typing_sent: None,
            last_edit: Instant::now(),
            title: String::new(),
            quit: false,
            home,
            home_view: home::View::default(),
            tasks,
        };
        // The lobby is always there, and it is where hopchat opens.
        let lobby = app.convo_mut(LOBBY);
        lobby.data.group = true;
        lobby.data.nick = lobby::NAME.into();
        lobby.online = true;
        lobby.secure = true;
        // So is Me, just under it: only you, and your devices.
        let me_address = app.me_address.clone();
        let mine = app.convo_mut(ME);
        mine.data.group = true;
        mine.data.nick = me::NAME.into();
        mine.data.members = vec![me_address];
        mine.online = true;
        mine.secure = true;
        app.refresh_lobby();
        app.selected = app.ordered().first().map(|c| c.data.address.clone());
        app
    }

    pub fn run(
        mut self,
        terminal: &mut DefaultTerminal,
        updates: Receiver<Update>,
        commands: Sender<Input>,
    ) -> std::io::Result<()> {
        let send = |c: Command| {
            let _ = commands.send(Input::Ui(c));
        };
        self.boot = Some(Instant::now());
        while !self.quit {
            while let Ok(update) = updates.try_recv() {
                self.apply(update, &send);
            }
            self.housekeeping(&send);
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(50))? {
                // Drain everything already waiting, so a paste or a burst of
                // keys costs one redraw, not one each.
                loop {
                    match event::read()? {
                        TermEvent::Key(key) if key.kind != KeyEventKind::Release => {
                            self.on_key(key, &send)
                        }
                        TermEvent::Paste(text) => self.on_paste(&text, &send),
                        TermEvent::Mouse(mouse) => self.on_mouse(mouse, &send),
                        TermEvent::FocusGained => self.focused = true,
                        TermEvent::FocusLost => self.focused = false,
                        TermEvent::Resize(..) => self.selection = None,
                        _ => {}
                    }
                    if self.quit || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
        }
        self.stop_typing(&send);
        self.save_all();
        Ok(())
    }

    // ----- state -------------------------------------------------------

    /// Conversations in display order: the lobby, Me, then reachable
    /// ones, then most recent.
    pub fn ordered(&self) -> Vec<&Convo> {
        let mut list: Vec<&Convo> = self.convos.iter().collect();
        list.sort_by(|a, b| {
            (b.data.address == LOBBY)
                .cmp(&(a.data.address == LOBBY))
                .then((b.data.address == ME).cmp(&(a.data.address == ME)))
                .then(b.online.cmp(&a.online))
                .then(b.active_ms.cmp(&a.active_ms))
                .then_with(|| a.data.nick.cmp(&b.data.nick))
        });
        list
    }

    /// The lobby's members: us, and everyone with a secure session now.
    fn refresh_lobby(&mut self) {
        let mut members = vec![self.me_address.clone()];
        members.extend(
            self.convos
                .iter()
                .filter(|c| !c.data.group && c.online && c.secure)
                .map(|c| c.data.address.clone()),
        );
        if let Some(lobby) = self.convos.iter_mut().find(|c| c.data.address == LOBBY) {
            lobby.data.members = members;
        }
    }

    /// Whether the boot screen is still up.
    pub fn booting(&self) -> bool {
        self.boot.is_some_and(|at| at.elapsed() < render::BOOT_FOR)
    }

    pub fn in_lobby(&self) -> bool {
        self.selected.as_deref() == Some(LOBBY)
    }

    pub fn in_me(&self) -> bool {
        self.selected.as_deref() == Some(ME)
    }

    fn convo_mut(&mut self, address: &str) -> &mut Convo {
        if let Some(i) = self.convos.iter().position(|c| c.data.address == address) {
            return &mut self.convos[i];
        }
        self.convos.push(Convo::new(address));
        if self.selected.is_none() {
            self.selected = Some(address.to_string());
        }
        self.convos.last_mut().unwrap()
    }

    pub fn current(&self) -> Option<&Convo> {
        let address = self.selected.as_deref()?;
        self.convos.iter().find(|c| c.data.address == address)
    }

    fn current_mut(&mut self) -> Option<&mut Convo> {
        let address = self.selected.clone()?;
        self.convos.iter_mut().find(|c| c.data.address == address)
    }

    pub fn nick_of(&self, address: &str) -> String {
        if address == self.me_address {
            return "you".into();
        }
        self.convos
            .iter()
            .find(|c| c.data.address == address)
            .map(|c| c.data.nick.clone())
            .unwrap_or_else(|| render::short(address))
    }

    fn flash(&mut self, text: impl Into<String>) {
        self.flash = Some((text.into(), false, Instant::now()));
    }

    fn flash_error(&mut self, text: impl Into<String>) {
        self.flash = Some((text.into(), true, Instant::now()));
    }

    pub fn flash_text(&self) -> Option<(&str, bool)> {
        self.flash
            .as_ref()
            .filter(|(_, _, at)| at.elapsed() < FLASH_FOR)
            .map(|(t, e, _)| (t.as_str(), *e))
    }

    /// The entry a reply in progress points at.
    pub fn reply_target(&self) -> Option<&Entry> {
        let id = self.reply.as_deref()?;
        self.current()?.data.entries.iter().rev().find(|e| e.id == id)
    }

    fn select(&mut self, address: Option<String>, send: &impl Fn(Command)) {
        if address != self.selected {
            self.stop_typing(send);
            self.selected = address;
            self.recall.stop();
            self.scroll = 0;
            self.reply = None;
            if matches!(self.mode, Mode::Select(_)) {
                self.mode = Mode::Normal;
            }
        }
    }

    fn cycle(&mut self, step: isize, send: &impl Fn(Command)) {
        let order: Vec<String> = self.ordered().iter().map(|c| c.data.address.clone()).collect();
        if order.is_empty() {
            return;
        }
        let at = self
            .selected
            .as_ref()
            .and_then(|s| order.iter().position(|a| a == s))
            .unwrap_or(0) as isize;
        let next = (at + step).rem_euclid(order.len() as isize) as usize;
        self.select(Some(order[next].clone()), send);
    }

    // ----- updates from the engine ------------------------------------

    fn apply(&mut self, update: Update, send: &impl Fn(Command)) {
        match update {
            Update::PeerSeen { address, nick } => {
                let nick = render::display_name(&nick, NAME_MAX).unwrap_or_else(|| render::short(&address));
                let convo = self.convo_mut(&address);
                if convo.data.nick != nick {
                    convo.data.nick = nick;
                    convo.data.dirty = true;
                }
            }
            Update::PeerUp { address } => {
                self.convo_mut(&address).online = true;
                let who = self.nick_of(&address);
                self.note(render::ok(), format!("link up    {who}"));
            }
            Update::PeerDown { address } => {
                let convo = self.convo_mut(&address);
                convo.online = false;
                convo.secure = false;
                convo.typing_until = None;
                self.refresh_lobby();
                let who = self.nick_of(&address);
                self.note(render::err(), format!("link down  {who}"));
                self.flash(format!("{who} dropped off the mesh"));
            }
            Update::SessionReady { address } => {
                let convo = self.convo_mut(&address);
                let fresh = !convo.secure;
                convo.secure = true;
                self.refresh_lobby();
                let who = self.nick_of(&address);
                self.note(render::ok(), format!("mls ready  {who}"));
                if fresh {
                    self.flash(format!("{who} jacked in · click them to talk privately"));
                }
            }
            Update::Queued { to, id, text, reply } => {
                self.owners.insert(id.clone(), to.clone());
                let me = self.me_address.clone();
                self.convo_mut(&to).push(Entry {
                    id,
                    mine: true,
                    at_ms: now_ms(),
                    body: Body::Text(text),
                    status: Status::Queued,
                    hops: 0,
                    reply: reply.map(|q| quoted(q, &me)),
                    read: false,
                    sender: None,
                });
            }
            Update::Incoming {
                from,
                id,
                text,
                hops,
                reply,
                ..
            } => {
                let me = self.me_address.clone();
                let entry = Entry {
                    id,
                    mine: false,
                    at_ms: now_ms(),
                    body: Body::Text(text.clone()),
                    status: Status::Sent,
                    hops,
                    reply: reply.map(|q| quoted(q, &me)),
                    read: false,
                    sender: None,
                };
                self.receive(&from, entry, &text);
            }
            Update::FileQueued { file_id, size, .. } => {
                if let Some(entry) = self.file_entry(&file_id)
                    && let Body::File { size: s, .. } = &mut entry.body
                {
                    *s = size;
                }
            }
            Update::FileProgress {
                file_id,
                done,
                total,
                inbound,
            } => {
                self.started.entry(file_id.clone()).or_insert_with(Instant::now);
                if let Some(entry) = self.file_entry(&file_id) {
                    if let Body::File { transfer, .. } = &mut entry.body {
                        *transfer = Transfer::Moving { done, total };
                    }
                } else if let Some((name, size)) = inbound {
                    self.incoming.insert(
                        file_id,
                        Incoming {
                            name,
                            size,
                            done,
                            total,
                        },
                    );
                } else if let Some(incoming) = self.incoming.get_mut(&file_id) {
                    incoming.done = done;
                    incoming.total = total;
                }
            }
            Update::FileSent { file_id } => {
                self.started.remove(&file_id);
                if let Some(entry) = self.file_entry(&file_id) {
                    if let Body::File { transfer, .. } = &mut entry.body {
                        *transfer = Transfer::Done;
                    }
                    entry.status.advance(Status::Delivered { hops: 1, ms: 0 });
                }
            }
            Update::FileFailed { file_id, reason } => {
                self.started.remove(&file_id);
                if let Some(entry) = self.file_entry(&file_id) {
                    if let Body::File { transfer, .. } = &mut entry.body {
                        *transfer = Transfer::Failed(reason.clone());
                    }
                    entry.status = Status::Failed(reason.clone());
                }
                self.flash_error(format!("file not sent: {reason}"));
            }
            Update::FileReceived {
                from,
                file_id,
                name,
                size,
                path,
                ..
            } => {
                self.incoming.remove(&file_id);
                self.started.remove(&file_id);
                let preview = format!("📎 {name}");
                let entry = Entry {
                    id: file_id.clone(),
                    mine: false,
                    at_ms: now_ms(),
                    body: Body::File {
                        file_id,
                        name,
                        size,
                        path,
                        transfer: Transfer::Done,
                    },
                    status: Status::Sent,
                    hops: 1,
                    reply: None,
                    read: false,
                    sender: None,
                };
                self.receive(&from, entry, &preview);
            }
            Update::FileLost {
                from,
                file_id,
                name,
                reason,
            } => {
                self.incoming.remove(&file_id);
                self.started.remove(&file_id);
                self.convo_mut(&from).push(notice(format!("{name} didn't arrive: {reason}")));
            }
            Update::ResendWanted { file_id, to } => {
                // We hold the path of everything we sent: re-supply it.
                let path = self.file_entry(&file_id).and_then(|e| match &e.body {
                    Body::File { path, .. } => Some(path.clone()),
                    _ => None,
                });
                if let Some(path) = path.filter(|p| p.is_file()) {
                    if let Some(entry) = self.file_entry(&file_id)
                        && let Body::File { transfer, .. } = &mut entry.body
                    {
                        *transfer = Transfer::Queued;
                    }
                    send(Command::SendFile { to, path, file_id });
                }
            }
            Update::Typing { from, on } => {
                let convo = self.convo_mut(&from);
                convo.typing_until = on.then(|| Instant::now() + TYPING_SHOWN);
            }
            Update::Read { from, ids } => {
                let convo = self.convo_mut(&from);
                for id in ids {
                    if let Some(entry) = convo.find(&id)
                        && entry.mine
                    {
                        entry.status.advance(Status::Read);
                        convo.data.dirty = true;
                    }
                }
            }
            Update::Group { id, name, members } => {
                let name = render::display_name(&name, NAME_MAX).unwrap_or_else(|| "group".into());
                let me = self.me_address.clone();
                let convo = self.convo_mut(&id);
                let fresh = !convo.data.group || !convo.online;
                let joined: Vec<String> =
                    members.iter().filter(|m| !convo.data.members.contains(m)).cloned().collect();
                let had_members = !convo.data.members.is_empty();
                convo.data.group = true;
                convo.data.nick = name.clone();
                convo.data.members = members;
                convo.online = true;
                convo.secure = true;
                convo.data.dirty = true;
                if had_members {
                    for member in joined.into_iter().filter(|m| *m != me) {
                        let who = self.nick_of(&member);
                        self.convo_mut(&id).push(notice(format!("{who} joined")));
                    }
                }
                if fresh {
                    self.note(render::ok(), format!("group      {name}"));
                }
            }
            Update::GroupLeft { id } => {
                let convo = self.convo_mut(&id);
                convo.online = false;
                convo.secure = false;
                convo.push(notice("you are no longer in this group".into()));
            }
            Update::GroupCreated { id } => self.select(Some(id), send),
            Update::GroupQueued {
                group,
                id,
                text,
                reply,
            } => {
                self.owners.insert(id.clone(), group.clone());
                let me = self.me_address.clone();
                self.convo_mut(&group).push(Entry {
                    id,
                    mine: true,
                    at_ms: now_ms(),
                    body: Body::Text(text),
                    status: Status::Queued,
                    hops: 0,
                    reply: reply.map(|q| quoted(q, &me)),
                    read: false,
                    sender: None,
                });
            }
            Update::GroupIncoming {
                group,
                from,
                id,
                text,
                reply_to,
            } => {
                let reply = reply_to.and_then(|rid| {
                    let convo = self.convos.iter().find(|c| c.data.address == group)?;
                    let e = convo.data.entries.iter().rev().find(|e| e.id == rid)?;
                    Some(Quoted {
                        id: rid,
                        mine: e.mine,
                        text: render::entry_summary(e),
                    })
                });
                let preview = format!("{}: {text}", self.nick_of(&from));
                let entry = Entry {
                    id,
                    mine: false,
                    at_ms: now_ms(),
                    body: Body::Text(text),
                    status: Status::Sent,
                    hops: 1,
                    reply,
                    read: false,
                    sender: Some(from),
                };
                self.receive(&group, entry, &preview);
            }
            Update::Home(text) => {
                self.note(render::accent(), text.clone());
                self.flash(text);
            }
            Update::HomeView(view) => self.home_view = *view,
            Update::Protocol(event) => self.on_protocol(*event),
            Update::Error(e) => {
                self.note(render::err(), format!("error      {e}"));
                if e.starts_with("send failed") {
                    self.flash_error(e);
                }
            }
        }
    }

    /// Files one of theirs into its conversation, and gets attention for
    /// it if it is not already in view.
    fn receive(&mut self, from: &str, entry: Entry, preview: &str) {
        let visible = self.focused && self.selected.as_deref() == Some(from);
        let convo = self.convo_mut(from);
        convo.typing_until = None;
        convo.push(entry);
        if !visible {
            convo.unread += 1;
            let who = convo.data.nick.clone();
            self.flash(format!("{who}: {}", first_line(preview)));
            // The bell: most terminals turn it into a dock bounce or badge.
            let _ = std::io::stdout().write_all(b"\x07");
        }
    }

    fn file_entry(&mut self, file_id: &str) -> Option<&mut Entry> {
        let owner = self.owners.get(file_id)?.clone();
        self.convos
            .iter_mut()
            .find(|c| c.data.address == owner)?
            .find(file_id)
    }

    fn set_status(&mut self, id: &str, next: Status) {
        let Some(owner) = self.owners.get(id).cloned() else {
            return;
        };
        let convo = self.convo_mut(&owner);
        if let Some(entry) = convo.find(id) {
            entry.status.advance(next);
            convo.data.dirty = true;
        }
    }

    fn on_protocol(&mut self, event: Event) {
        match &event {
            Event::MessageSent { message_id, .. } => self.set_status(message_id, Status::Sent),
            Event::MessageDelivered {
                message_id,
                hop_count,
                latency_ms,
                ..
            } => self.set_status(
                message_id,
                Status::Delivered {
                    hops: *hop_count,
                    ms: *latency_ms,
                },
            ),
            Event::MessageDeferred {
                message_id, reason, ..
            } => self.set_status(message_id, Status::Waiting(reason.clone())),
            Event::MessageRetrying {
                message_id,
                retry_count,
                ..
            } => self.set_status(message_id, Status::Retrying(*retry_count)),
            Event::MessageFailed {
                message_id, reason, ..
            }
            | Event::MessageUndeliverable {
                message_id, reason, ..
            } => self.set_status(message_id, Status::Failed(reason.clone())),
            _ => {}
        }
        // Chunks, receipts and typing pings are messages too; only log the
        // ones a person sent.
        let ours = |id: &str| self.owners.contains_key(id);
        let relevant = match &event {
            Event::MessageSent { message_id, .. }
            | Event::MessageDelivered { message_id, .. }
            | Event::MessageDeferred { message_id, .. }
            | Event::MessageRetrying { message_id, .. } => ours(message_id),
            _ => true,
        };
        if relevant && let Some(line) = render::describe(&event, |a| self.nick_of(a)) {
            self.push_log(line);
        }
    }

    pub fn note(&mut self, color: ratatui::style::Color, text: String) {
        let at = chrono::Local::now().format("%H:%M:%S").to_string();
        self.push_log(render::log_line(&at, color, text));
    }

    fn push_log(&mut self, line: Line<'static>) {
        self.log.push_back(line);
        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }

    /// Per-frame chores: reminders, read receipts, typing timeouts, saving,
    /// the title.
    fn housekeeping(&mut self, send: &impl Fn(Command)) {
        self.ring(chrono::Local::now());

        let picking = matches!(self.mode, Mode::Picker(_));
        if self.focused && !picking
            && let Some(convo) = self.current_mut()
        {
            convo.unread = 0;
            if convo.online && convo.secure && !convo.data.group {
                let mut ids = Vec::new();
                for entry in convo.data.entries.iter_mut().filter(|e| !e.mine && !e.read) {
                    entry.read = true;
                    if matches!(entry.body, Body::Text(_)) {
                        ids.push(entry.id.clone());
                    }
                }
                if !ids.is_empty() {
                    convo.data.dirty = true;
                    let to = convo.data.address.clone();
                    send(Command::Read { to, ids });
                }
            }
        }

        if self.typing_sent.is_some() && self.last_edit.elapsed() > TYPING_IDLE {
            self.stop_typing(send);
        }

        if self.last_save.elapsed() > SAVE_EVERY {
            self.save_all();
        }

        let unread: usize = self.convos.iter().map(|c| c.unread).sum();
        let title = if unread > 0 {
            format!("hopchat ({unread})")
        } else {
            format!("hopchat · {}", self.me_nick)
        };
        if title != self.title {
            let _ = ratatui::crossterm::execute!(
                std::io::stdout(),
                ratatui::crossterm::terminal::SetTitle(&title)
            );
            self.title = title;
        }
    }

    fn save_all(&mut self) {
        self.last_save = Instant::now();
        let Some(store) = &self.store else { return };
        let mut failed = None;
        if self.tasks.dirty {
            match store.save_tasks(&self.tasks) {
                Ok(()) => self.tasks.dirty = false,
                Err(e) => failed = Some(e),
            }
        }
        for convo in self.convos.iter_mut().filter(|c| c.data.dirty) {
            match store.save(&convo.data) {
                Ok(()) => convo.data.dirty = false,
                Err(e) => failed = Some(e),
            }
        }
        if let Some(e) = failed {
            self.flash_error(format!("history not saved: {e}"));
        }
    }

    // ----- typing --------------------------------------------------------

    fn edited(&mut self, send: &impl Fn(Command)) {
        self.last_edit = Instant::now();
        let Some(to) = self.selected.clone() else { return };
        if self.in_group() {
            return;
        }
        if self.editor.is_empty() || self.editor.text().starts_with('/') {
            return self.stop_typing(send);
        }
        let fresh = match &self.typing_sent {
            Some((peer, at)) => *peer != to || at.elapsed() > TYPING_REFRESH,
            None => true,
        };
        if fresh {
            send(Command::Typing {
                to: to.clone(),
                on: true,
            });
            self.typing_sent = Some((to, Instant::now()));
        }
    }

    fn stop_typing(&mut self, send: &impl Fn(Command)) {
        if let Some((to, _)) = self.typing_sent.take() {
            send(Command::Typing { to, on: false });
        }
    }

    // ----- input -----------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent, send: &impl Fn(Command)) {
        self.selection = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Any key gets past the boot screen, and does nothing else.
        if self.booting() {
            self.boot = None;
            if !(ctrl && matches!(key.code, KeyCode::Char('c' | 'q'))) {
                return;
            }
        }
        // Global keys, whatever is open.
        match key.code {
            KeyCode::Char('c' | 'q') if ctrl => {
                self.quit = true;
                return;
            }
            KeyCode::Char('e') if ctrl => {
                self.show_log = !self.show_log;
                return;
            }
            KeyCode::F(1) => {
                self.mode = match self.mode {
                    Mode::Help(_) => Mode::Normal,
                    _ => Mode::Help(0),
                };
                return;
            }
            _ => {}
        }

        match std::mem::replace(&mut self.mode, Mode::Normal) {
            // The arrows and pages scroll it; any other key closes it.
            Mode::Help(at) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.mode = Mode::Help(at.saturating_sub(1)),
                KeyCode::Down | KeyCode::Char('j') => self.mode = Mode::Help(at + 1),
                KeyCode::PageUp => self.mode = Mode::Help(at.saturating_sub(10)),
                KeyCode::PageDown | KeyCode::Char(' ') => self.mode = Mode::Help(at + 10),
                KeyCode::Home => self.mode = Mode::Help(0),
                _ => {}
            },
            Mode::Picker(mut picker) => {
                if let Some(chosen) = picker_key(&mut picker, key) {
                    self.attach(chosen);
                } else if !matches!(key.code, KeyCode::Esc) {
                    self.mode = Mode::Picker(picker);
                }
            }
            Mode::Devices(mut panel) => match panel.key(key, &self.home_view) {
                devices::Action::Close => {}
                devices::Action::Stay => self.mode = Mode::Devices(panel),
                devices::Action::Ask(request) => {
                    let _ = self.home.send(request);
                    self.mode = Mode::Devices(panel);
                }
                devices::Action::ToggleGroup => {
                    self.toggle_home_here();
                    self.mode = Mode::Devices(panel);
                }
                devices::Action::ToggleAi => {
                    self.toggle_ai_here();
                    self.mode = Mode::Devices(panel);
                }
            },
            Mode::Groups(mut panel) => {
                let people = self.candidates(&panel);
                match panel.key(key, &people) {
                    groups::Action::Close => {}
                    groups::Action::Stay => self.mode = Mode::Groups(panel),
                    groups::Action::Create { name, members } => {
                        self.flash(format!("starting #{name}{}", self.with_whom(&members)));
                        send(Command::CreateGroup { name, members });
                    }
                    groups::Action::Invite { group, members } => {
                        self.flash(format!("inviting{}…", self.with_whom(&members).trim_start_matches(" with")));
                        for member in members {
                            send(Command::Invite {
                                group: group.clone(),
                                member,
                            });
                        }
                    }
                }
            }
            Mode::Tasks(mut panel) => {
                let action = panel.key(key, &self.tasks);
                if let Some(panel) = self.task_action(panel, action, send) {
                    self.mode = Mode::Tasks(panel);
                }
            }
            Mode::Select(at) => self.select_key(at, key, send),
            Mode::Normal if ctrl && key.code == KeyCode::Char('t') => self.open_tasks(),
            Mode::Normal if ctrl && key.code == KeyCode::Char('d') => self.open_devices(),
            Mode::Normal if ctrl && key.code == KeyCode::Char('g') => self.open_groups(""),
            Mode::Normal => self.normal_key(key, ctrl, alt, send),
        }
    }

    fn normal_key(&mut self, key: KeyEvent, ctrl: bool, alt: bool, send: &impl Fn(Command)) {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page = (self.chat_area.height as usize).saturating_sub(4).max(1);
        match key.code {
            KeyCode::Enter if shift || alt => self.editor.insert('\n'),
            KeyCode::Char('j') if ctrl => self.editor.insert('\n'),
            KeyCode::Enter => self.submit(send),
            KeyCode::Tab if self.editor.text().starts_with('/') => {
                if let Some(cmd) = render::command_matches(self.editor.text()).first() {
                    self.editor.set(&format!("{} ", cmd.0));
                }
            }
            KeyCode::Tab if self.mentioning() => {
                if let Some((name, _)) = render::mention_matches(self.editor.text()).first() {
                    let text = self.editor.text();
                    let start = text.rfind([' ', '\n']).map_or(0, |i| i + 1);
                    let done = format!("{}{name} ", &text[..start]);
                    self.editor.set(&done);
                    self.edited(send);
                }
            }
            KeyCode::Tab => self.cycle(1, send),
            KeyCode::BackTab => self.cycle(-1, send),
            KeyCode::Char('n') if ctrl => self.cycle(1, send),
            KeyCode::Char('p') if ctrl => self.cycle(-1, send),
            KeyCode::Char(c @ '1'..='9') if alt => {
                let n = c as usize - '1' as usize;
                let address = self.ordered().get(n).map(|c| c.data.address.clone());
                if address.is_some() {
                    self.select(address, send);
                }
            }
            KeyCode::Char('o') if ctrl => {
                let start = std::env::current_dir()
                    .ok()
                    .or_else(picker::home_dir)
                    .unwrap_or_else(|| PathBuf::from("/"));
                self.mode = Mode::Picker(Picker::new(start));
            }
            KeyCode::Esc => {
                if self.reply.take().is_none() {
                    self.scroll = 0;
                }
            }
            KeyCode::PageUp => self.scroll += page,
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(page),
            KeyCode::Up if ctrl || alt => self.enter_select(),
            KeyCode::Up => {
                if !self.editor.up() {
                    self.recall_older(send);
                }
            }
            KeyCode::Down => {
                if !self.editor.down() {
                    self.recall_newer(send);
                }
            }
            KeyCode::Left if ctrl || alt => self.editor.word_left(),
            KeyCode::Right if ctrl || alt => self.editor.word_right(),
            KeyCode::Char('b') if alt => self.editor.word_left(),
            KeyCode::Char('f') if alt => self.editor.word_right(),
            KeyCode::Left => self.editor.left(),
            KeyCode::Right => self.editor.right(),
            KeyCode::Home => self.editor.home(),
            KeyCode::End => self.editor.end(),
            KeyCode::Char('a') if ctrl => self.editor.home(),
            KeyCode::Char('e') if ctrl => self.editor.end(),
            KeyCode::Char('w') if ctrl => {
                self.editor.delete_word();
                self.edited(send);
            }
            KeyCode::Backspace if alt => {
                self.editor.delete_word();
                self.edited(send);
            }
            KeyCode::Char('u') if ctrl => {
                self.editor.kill_line();
                self.edited(send);
            }
            KeyCode::Backspace if self.editor.is_empty() => {
                if self.attachments.pop().is_none() {
                    self.reply = None;
                }
            }
            KeyCode::Backspace => {
                self.editor.backspace();
                self.edited(send);
            }
            KeyCode::Delete => {
                self.editor.delete();
                self.edited(send);
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.editor.insert(c);
                self.edited(send);
            }
            _ => {}
        }
    }

    /// Makes sure the open conversation's sent lines are loaded, and names it.
    /// Commands run before anyone is open are kept under no address.
    fn recall_address(&mut self) -> String {
        let address = self.selected.clone().unwrap_or_default();
        if !self.recall.has(&address) {
            let sent = self
                .convos
                .iter()
                .find(|c| c.data.address == address)
                .map(|c| sent_lines(&c.data.entries))
                .unwrap_or_default();
            self.recall.load(&address, sent);
        }
        address
    }

    fn remember(&mut self, text: &str) {
        let address = self.recall_address();
        self.recall.push(&address, text.to_string());
    }

    fn recall_older(&mut self, send: &impl Fn(Command)) {
        let address = self.recall_address();
        match self.recall.older(&address, self.editor.text()) {
            Some(line) => {
                self.editor.set(&line);
                self.edited(send);
            }
            // Nothing of yours to go back to, so the messages are next.
            None if self.editor.is_empty() => self.enter_select(),
            None => {}
        }
    }

    fn recall_newer(&mut self, send: &impl Fn(Command)) {
        let address = self.recall_address();
        if let Some(line) = self.recall.newer(&address, self.editor.text()) {
            self.editor.set(&line);
            self.edited(send);
        }
    }

    fn enter_select(&mut self) {
        let Some(convo) = self.current() else { return };
        if let Some(last) = selectable(convo, convo.data.entries.len(), -1) {
            self.mode = Mode::Select(last);
        }
    }

    fn select_key(&mut self, at: usize, key: KeyEvent, send: &impl Fn(Command)) {
        let Some(convo) = self.current() else { return };
        let Some(entry) = convo.data.entries.get(at).cloned() else {
            return;
        };
        let len = convo.data.entries.len();
        let mut next = Mode::Select(at);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(i) = selectable(convo, at, -1) {
                    next = Mode::Select(i);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                next = match selectable(convo, at, 1) {
                    Some(i) => Mode::Select(i),
                    None => Mode::Normal,
                }
            }
            KeyCode::Char('g') | KeyCode::Home => {
                if let Some(i) = selectable(convo, 0, 0) {
                    next = Mode::Select(i);
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                if let Some(i) = selectable(convo, len, -1) {
                    next = Mode::Select(i);
                }
            }
            // A reminder that went off: check it off, put it off, or go
            // back to where it was set.
            KeyCode::Char('x' | ' ') if matches!(entry.body, Body::Reminder { .. }) => {
                if let Body::Reminder { task, .. } = &entry.body {
                    // Checking off a repeat would end the whole series.
                    if self.tasks.get(task).is_some_and(|t| t.repeat.is_some() && !t.done()) {
                        self.flash("that one repeats · delete it in Ctrl-T to stop it");
                    } else {
                        self.toggle_task(task);
                    }
                }
            }
            KeyCode::Char('s') if matches!(entry.body, Body::Reminder { .. }) => {
                if let Body::Reminder { task, .. } = &entry.body {
                    self.snooze_task(task);
                }
            }
            KeyCode::Char('o') | KeyCode::Enter if matches!(entry.body, Body::Reminder { .. }) => {
                let origin = match &entry.body {
                    Body::Reminder { task, .. } => self.tasks.get(task).and_then(|t| t.origin.clone()),
                    _ => None,
                };
                match origin.filter(|o| self.convos.iter().any(|c| c.data.address == o.address)) {
                    Some(origin) => {
                        self.select(Some(origin.address), send);
                        next = Mode::Normal;
                    }
                    None => self.flash_error("that reminder wasn't set in a chat"),
                }
            }
            KeyCode::Char('r') => {
                self.reply = Some(entry.id.clone());
                next = Mode::Normal;
            }
            KeyCode::Char('o') | KeyCode::Enter => match &entry.body {
                Body::File { path, .. } if path.exists() => {
                    open_path(path);
                    self.flash(format!("opening {}", picker::tilde(path)));
                }
                Body::File { .. } => self.flash_error("that file is no longer there"),
                _ => {
                    self.reply = Some(entry.id.clone());
                    next = Mode::Normal;
                }
            },
            // Anything said becomes something to do.
            KeyCode::Char('t') => {
                match &entry.body {
                    Body::Reminder { task, .. } => {
                        let mut panel = TaskPanel::new(self.origin_here(), "");
                        if self.tasks.get(task).is_some() {
                            panel.follow(&self.tasks, task);
                            panel.focus = tasks::Focus::List;
                        }
                        next = Mode::Tasks(panel);
                    }
                    body => {
                        let text = match body {
                            Body::File { name, .. } => format!("look at {name}"),
                            _ => render::entry_summary(&entry),
                        };
                        next = Mode::Tasks(TaskPanel::new(self.origin_here(), &text));
                    }
                }
            }
            KeyCode::Char('f') => {
                if let Body::File { path, .. } = &entry.body {
                    reveal_path(path);
                }
            }
            KeyCode::Char('y') => {
                let text = match &entry.body {
                    Body::Text(t) | Body::Notice(t) => t.clone(),
                    Body::Reminder { text, .. } => text.clone(),
                    Body::File { path, .. } => path.display().to_string(),
                };
                copy_to_clipboard(&text);
                self.flash("copied");
            }
            KeyCode::Char('R') if entry.mine => {
                self.retry(&entry, send);
                next = Mode::Normal;
            }
            KeyCode::Esc | KeyCode::Char('i') | KeyCode::Char('q') => next = Mode::Normal,
            _ => {}
        }
        self.mode = next;
    }

    fn retry(&mut self, entry: &Entry, send: &impl Fn(Command)) {
        let Some(to) = self.selected.clone() else { return };
        if self.in_group() {
            if let Body::Text(text) = &entry.body
                && matches!(entry.status, Status::Failed(_))
            {
                send(Command::SendGroup {
                    group: to,
                    text: text.clone(),
                    reply: None,
                    from_home: false,
                });
            } else {
                self.flash("nothing to retry");
            }
            return;
        }
        match &entry.body {
            Body::Text(text) if matches!(entry.status, Status::Failed(_)) => send(Command::Send {
                to,
                text: text.clone(),
                reply: None,
            }),
            Body::File {
                path,
                transfer: Transfer::Failed(_) | Transfer::Interrupted,
                ..
            } => {
                self.send_file(to, path.clone(), send);
            }
            _ => self.flash("nothing to retry"),
        }
    }

    fn on_paste(&mut self, text: &str, send: &impl Fn(Command)) {
        match &mut self.mode {
            Mode::Picker(picker) => {
                picker.filter.push_str(text.trim());
                picker.selected = 0;
                return;
            }
            Mode::Devices(panel) => {
                if let Some(name) = &mut panel.renaming {
                    let room = 32usize.saturating_sub(name.chars().count());
                    name.extend(text.chars().filter(|c| !c.is_control()).take(room));
                }
                return;
            }
            Mode::Groups(panel) => return panel.type_str(text),
            Mode::Tasks(panel) => return panel.type_str(text),
            Mode::Normal => {}
            _ => self.mode = Mode::Normal,
        }
        // Dropping files on a terminal pastes their paths.
        if let Some(paths) = dropped_paths(text) {
            self.attach(paths);
        } else {
            self.editor.insert_str(text);
            self.edited(send);
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent, send: &impl Fn(Command)) {
        let inside = |r: Rect| {
            mouse.column >= r.x
                && mouse.column < r.x + r.width
                && mouse.row >= r.y
                && mouse.row < r.y + r.height
        };
        if self.booting() {
            if matches!(mouse.kind, MouseEventKind::Down(_)) {
                self.boot = None;
            }
            return;
        }
        let at = Position::new(mouse.column, mouse.row);
        if matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) {
            // The text moves; what was selected would not move with it.
            self.selection = None;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => match &mut self.mode {
                Mode::Picker(p) => p.up(),
                Mode::Tasks(p) => p.cursor = p.cursor.saturating_sub(1),
                Mode::Help(at) => *at = at.saturating_sub(3),
                _ if inside(self.chat_area) => self.scroll += 3,
                _ => {}
            },
            MouseEventKind::ScrollDown => match &mut self.mode {
                Mode::Picker(p) => p.down(),
                Mode::Tasks(p) => p.cursor = (p.cursor + 1).min(self.tasks.items.len().saturating_sub(1)),
                Mode::Help(at) => *at += 3,
                _ if inside(self.chat_area) => self.scroll = self.scroll.saturating_sub(3),
                _ => {}
            },
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection = None;
                // The keys along the bottom are buttons, whatever is open.
                let key = self.footer_hits.iter().find(|(r, _)| inside(*r)).map(|(_, k)| *k);
                if let Some(key) = key {
                    return self.on_key(key, send);
                }
                if !matches!(self.mode, Mode::Normal | Mode::Select(_)) {
                    return;
                }
                let hit = self
                    .peer_rows
                    .iter()
                    .find(|(r, _)| inside(*r))
                    .map(|(_, a)| a.clone());
                if hit.is_some() {
                    self.select(hit, send);
                    self.mode = Mode::Normal;
                } else if inside(self.new_group_row) {
                    self.open_groups("");
                } else if inside(self.composer_area) {
                    self.mode = Mode::Normal;
                } else if inside(self.chat_area) {
                    // A click or the start of a drag: the release tells.
                    self.selection = Some(Selection::new(at, self.chat_area));
                } else if inside(self.log_area) {
                    self.selection = Some(Selection::new(at, self.log_area));
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(selection) = &mut self.selection {
                    selection.drag_to(at);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(selection) = &mut self.selection else { return };
                let anchor = selection.anchor();
                if selection.dragged {
                    selection.copy = true;
                    return;
                }
                self.selection = None;
                if self.chat_area.contains(anchor) {
                    self.click_message(anchor.y);
                }
            }
            _ => {}
        }
    }

    /// A click on a message picks it, for reply, open, copy…
    fn click_message(&mut self, row: u16) {
        let at = self.message_rows.iter().find(|(y, _)| *y == row).map(|(_, i)| *i);
        let picked = at.and_then(|i| {
            let convo = self.current()?;
            selectable(convo, i, 0).filter(|&j| j == i)
        });
        self.mode = match (picked, &self.mode) {
            (Some(i), Mode::Select(j)) if i == *j => Mode::Normal,
            (Some(i), _) => Mode::Select(i),
            (None, _) => Mode::Normal,
        };
    }

    /// Shows the selection over a finished frame, and copies it once it has
    /// been released: the frame is where its text is.
    fn finish_selection(&mut self, buffer: &mut Buffer) {
        let Some(selection) = &mut self.selection else { return };
        selection.paint(buffer);
        if !std::mem::take(&mut selection.copy) {
            return;
        }
        let text = selection.text(buffer);
        if text.is_empty() {
            self.selection = None;
        } else {
            copy_to_clipboard(&text);
            self.flash("copied to clipboard");
        }
    }

    fn attach(&mut self, paths: Vec<PathBuf>) {
        let mut added = 0;
        for path in paths {
            match std::fs::metadata(&path) {
                Ok(m) if m.len() > engine::MAX_FILE_BYTES => {
                    self.flash_error(format!("{} is over 100 MB", file_name(&path)));
                }
                Ok(m) if m.is_file() => {
                    if !self.attachments.contains(&path) {
                        self.attachments.push(path);
                        added += 1;
                    }
                }
                _ => self.flash_error(format!("can't read {}", picker::tilde(&path))),
            }
        }
        if added > 0 {
            self.flash(format!(
                "attached {added} file{} · Enter sends",
                if added == 1 { "" } else { "s" }
            ));
        }
    }

    fn send_file(&mut self, to: String, path: PathBuf, send: &impl Fn(Command)) {
        let file_id = engine::new_file_id();
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        self.owners.insert(file_id.clone(), to.clone());
        self.convo_mut(&to).push(Entry {
            id: file_id.clone(),
            mine: true,
            at_ms: now_ms(),
            body: Body::File {
                file_id: file_id.clone(),
                name: file_name(&path),
                size,
                path: path.clone(),
                transfer: Transfer::Queued,
            },
            status: Status::Queued,
            hops: 0,
            reply: None,
            read: false,
            sender: None,
        });
        send(Command::SendFile { to, path, file_id });
    }

    fn submit(&mut self, send: &impl Fn(Command)) {
        let text = self.editor.text().trim().to_string();
        if let Some(command) = text.strip_prefix('/') {
            self.remember(&text);
            self.editor.take();
            return self.run_command(command, send);
        }
        let Some(to) = self.selected.clone() else {
            if !text.is_empty() || !self.attachments.is_empty() {
                self.flash_error("no one to send to yet");
            }
            return;
        };
        if !text.is_empty() {
            self.remember(&text);
        }
        if self.in_group() {
            if !self.attachments.is_empty() {
                self.attachments.clear();
                if self.in_me() {
                    self.flash_error("files can't be kept in Me yet; send them to one person");
                } else {
                    self.flash_error("files can't go to a group yet; send them to one person");
                }
            }
            if !text.is_empty() {
                let reply = self.reply_target().map(|e| Quote {
                    id: e.id.clone(),
                    sender: e.sender.clone().unwrap_or_else(|| self.me_address.clone()),
                    text: render::entry_summary(e),
                });
                send(Command::SendGroup {
                    group: to,
                    text,
                    reply,
                    from_home: false,
                });
            }
            self.editor.take();
            self.reply = None;
            self.scroll = 0;
            return;
        }
        for path in std::mem::take(&mut self.attachments) {
            self.send_file(to.clone(), path, send);
        }
        if !text.is_empty() {
            let reply = self.reply_target().map(|e| Quote {
                id: e.id.clone(),
                sender: if e.mine { self.me_address.clone() } else { to.clone() },
                text: render::entry_summary(e),
            });
            send(Command::Send {
                to,
                text,
                reply,
            });
        }
        self.editor.take();
        self.reply = None;
        self.scroll = 0;
        self.stop_typing(send);
    }

    fn run_command(&mut self, line: &str, send: &impl Fn(Command)) {
        let (name, arg) = line.split_once(' ').unwrap_or((line, ""));
        let arg = arg.trim();
        match name {
            "file" | "send" | "attach" if !arg.is_empty() => {
                let paths = dropped_paths(arg)
                    .unwrap_or_else(|| vec![expand_tilde(arg)]);
                self.attach(paths);
                // Groups have no stream to carry a file, so it would wait forever.
                if self.in_group() {
                    self.attachments.clear();
                    if self.in_me() {
                        self.flash_error("files can't be kept in Me yet; send them to one person");
                    } else {
                        self.flash_error("files can't go to a group yet; send them to one person");
                    }
                } else if let Some(to) = self.selected.clone() {
                    for path in std::mem::take(&mut self.attachments) {
                        self.send_file(to.clone(), path, send);
                    }
                }
            }
            "file" | "send" | "attach" => {
                let start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
                self.mode = Mode::Picker(Picker::new(start));
            }
            "downloads" | "dl" => {
                let _ = std::fs::create_dir_all(&self.downloads);
                open_path(&self.downloads);
            }
            "clear" => {
                if let Some(convo) = self.current_mut() {
                    convo.data.entries.clear();
                    let address = convo.data.address.clone();
                    convo.data.dirty = false;
                    if let Some(store) = &self.store {
                        store.forget(&address);
                    }
                    self.recall.forget(&address);
                    self.reply = None;
                    self.scroll = 0;
                    self.flash("conversation cleared on this device");
                }
            }
            "info" | "whois" => {
                if let Some(convo) = self.current() {
                    let text = format!(
                        "{} is {} · {} · {}",
                        convo.data.nick,
                        convo.data.address,
                        if convo.online { "online" } else { "offline" },
                        if convo.secure { "MLS session up" } else { "no session yet" }
                    );
                    let address = convo.data.address.clone();
                    self.convo_mut(&address).push(notice(text));
                }
            }
            "me" | "whoami" => {
                let text = format!("you are {} · {}", self.me_nick, self.me_address);
                copy_to_clipboard(&self.me_address);
                self.flash(format!("{text} (address copied)"));
            }
            "group" | "newgroup" if !arg.is_empty() => {
                send(Command::CreateGroup {
                    name: arg.to_string(),
                    members: Vec::new(),
                });
            }
            "group" | "newgroup" => self.open_groups(""),
            "invite" | "add" | "leave" if self.in_me() => {
                self.flash_error("Me is just you and your devices; start a group with Ctrl-G to talk to people")
            }
            "home" | "ai" if self.in_me() && switches(arg) => self.flash_error(ALWAYS_YOURS),
            "invite" | "add" | "leave" if self.in_lobby() => {
                self.flash_error("everyone nearby is in the lobby; there's no one to add and no leaving it")
            }
            "invite" | "add" if arg.is_empty() => match self.current().filter(|c| c.data.group && c.online) {
                Some(c) => {
                    let panel = GroupPanel::add_to(c.data.address.clone(), c.data.nick.clone());
                    self.mode = Mode::Groups(panel);
                }
                None => self.open_groups(""),
            },
            "invite" | "add" => {
                let Some(group) = self.selected.clone().filter(|_| self.in_group()) else {
                    return self.flash_error("open a group first, then /invite NICK");
                };
                match self.find_peer(arg) {
                    Some(member) => {
                        let who = self.nick_of(&member);
                        send(Command::Invite { group, member });
                        self.flash(format!("inviting {who}…"));
                    }
                    None => self.flash_error(format!("no one nearby called {arg}")),
                }
            }
            "leave" => match self.selected.clone().filter(|_| self.in_group()) {
                Some(group) => send(Command::Leave { group }),
                None => self.flash_error("/leave works in a group"),
            },
            "members" | "who" => {
                if let Some(convo) = self.current().filter(|c| c.data.group) {
                    let names: Vec<String> =
                        convo.data.members.iter().map(|m| self.nick_of(m)).collect();
                    let text = format!("members: {}", names.join(", "));
                    let address = convo.data.address.clone();
                    self.convo_mut(&address).push(notice(text));
                }
            }
            "devices" | "home" if arg.is_empty() => self.open_devices(),
            "home" => {
                let group = self.selected.clone().filter(|_| self.in_group());
                let _ = self.home.send(home::Request::Configure {
                    group,
                    args: arg.to_string(),
                });
            }
            "ai" => {
                let group = self.selected.clone().filter(|_| self.in_group());
                let _ = self.home.send(home::Request::Configure {
                    group,
                    args: format!("ai {arg}"),
                });
            }
            "todo" | "todos" | "tasks" | "task" | "remind" | "reminders" | "reminder" if arg.is_empty() => {
                self.open_tasks()
            }
            "todo" | "task" | "remind" | "reminder" => {
                let draft = crate::tasks::Draft::read(arg, chrono::Local::now());
                if name.starts_with("remind") && draft.due.is_none() {
                    return self.flash_error("when? e.g. /remind 20m stretch · /remind call mom at 5pm");
                }
                self.add_task(draft, self.origin_here());
            }
            "log" => self.show_log = !self.show_log,
            "theme" => {
                let light = match arg {
                    "" => !theme::is_light(),
                    _ => match theme::parse(arg) {
                        Some(light) => light,
                        None => return self.flash_error("/theme dark or /theme light"),
                    },
                };
                match theme::set(light) {
                    Ok(()) => self.flash(format!("{} theme", if light { "light" } else { "dark" })),
                    Err(e) => self.flash_error(format!("theme not saved: {e}")),
                }
            }
            "help" | "?" => self.mode = Mode::Help(0),
            "quit" | "exit" | "q" => self.quit = true,
            _ => self.flash_error(format!("unknown command /{name} · /help lists them")),
        }
    }
}

impl App {
    /// The open conversation, as somewhere a task can come from; Me is
    /// where they all land anyway.
    fn origin_here(&self) -> Option<crate::tasks::Origin> {
        let convo = self.current().filter(|c| c.data.address != ME)?;
        let name = match () {
            _ if convo.data.address == LOBBY => lobby::NAME.to_string(),
            _ if convo.data.group => format!("#{}", convo.data.nick),
            _ => convo.data.nick.clone(),
        };
        Some(crate::tasks::Origin { address: convo.data.address.clone(), name })
    }

    /// The list on its own: what's typed there belongs to no chat.
    fn open_tasks(&mut self) {
        self.mode = Mode::Tasks(TaskPanel::new(None, ""));
    }

    /// Adds what was typed, and says what it became.
    fn add_task(&mut self, draft: crate::tasks::Draft, origin: Option<crate::tasks::Origin>) -> Option<String> {
        if draft.text.is_empty() {
            self.flash_error("nothing to do there");
            return None;
        }
        let now = chrono::Local::now();
        if draft.due.is_some_and(|d| d <= now) {
            self.flash_error("that time has already passed");
            return None;
        }
        let task = self.tasks.add(draft, origin, now_ms()).clone();
        let what = render::clip(&task.text, 40);
        match task.due_ms {
            Some(due) => self.flash(format!(
                "⏰ {what} · {} ({})",
                crate::tasks::when_label(due, now),
                crate::tasks::until(due, now_ms())
            )),
            None => self.flash(format!("added {what} to your todos · Ctrl-T")),
        }
        Some(task.id)
    }

    fn toggle_task(&mut self, id: &str) {
        let text = self.tasks.get(id).map(|t| render::clip(&t.text, 40)).unwrap_or_default();
        match self.tasks.toggle(id, now_ms()) {
            Some(true) => self.flash(format!("checked off {text}")),
            Some(false) => self.flash(format!("{text} is back on your list")),
            None => self.flash_error("that one has been deleted"),
        }
    }

    fn snooze_task(&mut self, id: &str) {
        match self.tasks.snooze(id, now_ms()) {
            Some(at) => self.flash(format!("snoozed until {}", render::clock(at))),
            None => self.flash_error("that one has been deleted"),
        }
    }

    /// Does what a key in the tasks panel asked; the panel comes back
    /// unless it was closed.
    fn task_action(
        &mut self,
        mut panel: TaskPanel,
        action: tasks::Action,
        send: &impl Fn(Command),
    ) -> Option<TaskPanel> {
        use tasks::Action;
        match action {
            Action::Stay => {}
            Action::Close => return None,
            Action::Add(draft) => {
                if let Some(id) = self.add_task(draft, panel.origin.clone()) {
                    panel.follow(&self.tasks, &id);
                }
            }
            Action::Save(id, draft) => {
                self.tasks.edit(&id, draft);
                panel.follow(&self.tasks, &id);
                self.flash("saved");
            }
            Action::Toggle(id) => {
                self.toggle_task(&id);
                panel.follow(&self.tasks, &id);
            }
            Action::Snooze(id) => {
                self.snooze_task(&id);
                panel.follow(&self.tasks, &id);
            }
            Action::Delete(id) => {
                if let Some(task) = self.tasks.remove(&id) {
                    self.flash(format!("deleted {} · u brings it back", render::clip(&task.text, 30)));
                    panel.undo = Some(task);
                }
            }
            Action::Undo(task) => {
                let id = task.id.clone();
                self.tasks.restore(task);
                panel.follow(&self.tasks, &id);
                self.flash("it's back");
            }
            Action::Go(address) => {
                if self.convos.iter().any(|c| c.data.address == address) {
                    self.select(Some(address), send);
                    return None;
                }
                self.flash_error("that chat is gone from this device");
            }
            Action::ClearDone => {
                let n = self.tasks.clear_done();
                self.flash(format!("cleared {n} done"));
                panel.cursor = 0;
            }
        }
        Some(panel)
    }

    /// Says each reminder whose time has come in Me, and gets attention
    /// for it.
    fn ring(&mut self, now: chrono::DateTime<chrono::Local>) {
        for fired in self.tasks.fire(now) {
            let visible = self.focused && self.in_me();
            let entry = Entry {
                id: format!("rm_{}_{}", fired.id, fired.due_ms),
                mine: false,
                at_ms: now_ms(),
                body: Body::Reminder {
                    task: fired.id.clone(),
                    text: fired.text.clone(),
                    from: fired.origin.map(|o| o.name),
                },
                status: Status::Sent,
                hops: 0,
                reply: None,
                read: true,
                sender: None,
            };
            self.receive(ME, entry, &fired.text);
            self.flash(format!("⏰ {} · in Me", first_line(&fired.text)));
            self.note(render::accent(), format!("reminder   {}", first_line(&fired.text)));
            if visible {
                let _ = std::io::stdout().write_all(b"\x07");
            }
            notify("hopchat reminder", &fired.text);
        }
    }

    fn open_groups(&mut self, name: &str) {
        self.mode = Mode::Groups(GroupPanel::new_group(name));
    }

    /// Everyone the group panel lists: people who are around first, then
    /// the rest by name.
    pub fn candidates(&self, panel: &GroupPanel) -> Vec<Candidate> {
        let members = panel
            .target
            .as_ref()
            .and_then(|(id, _)| self.convos.iter().find(|c| c.data.address == *id))
            .map(|c| c.data.members.clone())
            .unwrap_or_default();
        let mut people: Vec<Candidate> = self
            .convos
            .iter()
            .filter(|c| !c.data.group)
            .map(|c| Candidate {
                address: c.data.address.clone(),
                nick: c.data.nick.clone(),
                online: c.online,
                secure: c.secure,
                member: members.contains(&c.data.address),
            })
            .collect();
        people.sort_by(|a, b| {
            b.online
                .cmp(&a.online)
                .then_with(|| a.nick.to_lowercase().cmp(&b.nick.to_lowercase()))
                .then_with(|| a.address.cmp(&b.address))
        });
        people
    }

    /// " with bob and carol", or nothing for nobody.
    fn with_whom(&self, members: &[String]) -> String {
        let names: Vec<String> = members.iter().map(|m| self.nick_of(m)).collect();
        match names.as_slice() {
            [] => String::new(),
            [one] => format!(" with {one}"),
            [rest @ .., last] => format!(" with {} and {last}", rest.join(", ")),
        }
    }

    fn open_devices(&mut self) {
        self.mode = Mode::Devices(Devices::default());
        let _ = self.home.send(home::Request::Scan);
    }

    /// The open group, and whether home is on in it.
    pub fn home_here(&self) -> Option<(String, String, bool)> {
        let convo = self.current().filter(|c| c.data.group && c.online && c.data.address != ME)?;
        let on = self.home_view.groups.contains(&convo.data.address);
        Some((convo.data.address.clone(), convo.data.nick.clone(), on))
    }

    /// The open group, and whether it may ask the model with `@ai`.
    pub fn ai_here(&self) -> Option<(String, String, bool)> {
        let (group, name, _) = self.home_here()?;
        let on = self.home_view.ai_groups.contains(&group);
        Some((group, name, on))
    }

    fn toggle_ai_here(&mut self) {
        match self.ai_here() {
            Some((group, _, on)) => {
                let verb = if on { "off" } else { "on" };
                let _ = self.home.send(home::Request::Configure {
                    group: Some(group),
                    args: format!("ai {verb}"),
                });
            }
            None if self.in_me() => self.flash_error(ALWAYS_YOURS),
            None => self.flash_error("open a group first; i then lets that group ask your model"),
        }
    }

    fn toggle_home_here(&mut self) {
        match self.home_here() {
            Some((group, _, on)) => {
                let _ = self.home.send(home::Request::SetGroup { group, on: !on });
            }
            None if self.in_me() => self.flash_error(ALWAYS_YOURS),
            None => self.flash_error("open a group first; g then lets that group use your devices"),
        }
    }

    fn in_group(&self) -> bool {
        self.current().is_some_and(|c| c.data.group)
    }

    /// Whether an `@` name is being typed where `@home` and `@ai` answer.
    fn mentioning(&self) -> bool {
        self.in_group() && self.editor.at_end() && !render::mention_matches(self.editor.text()).is_empty()
    }

    /// A person by nick (case-insensitive) or by address.
    fn find_peer(&self, who: &str) -> Option<String> {
        let who = who.trim().trim_start_matches('@');
        if who.is_empty() {
            return None;
        }
        self.convos
            .iter()
            .filter(|c| !c.data.group)
            .find(|c| c.data.address == who || c.data.nick.eq_ignore_ascii_case(who))
            .map(|c| c.data.address.clone())
    }
}

fn picker_key(picker: &mut Picker, key: KeyEvent) -> Option<Vec<PathBuf>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Up => picker.up(),
        KeyCode::Down => picker.down(),
        KeyCode::Char('p') if ctrl => picker.up(),
        KeyCode::Char('n') if ctrl => picker.down(),
        KeyCode::PageUp => picker.page(-10),
        KeyCode::PageDown => picker.page(10),
        KeyCode::Left => picker.parent(),
        KeyCode::Char('~') if picker.filter.is_empty() => picker.home(),
        KeyCode::Char(' ') => picker.toggle_mark(),
        KeyCode::Right | KeyCode::Enter | KeyCode::Tab => {
            if let Outcome::Chosen(paths) = picker.choose() {
                return Some(paths);
            }
        }
        KeyCode::Backspace => picker.backspace(),
        KeyCode::Char(c) if !ctrl => picker.type_char(c),
        _ => {}
    }
    None
}

/// The next entry from `from` in direction `step` that a person can act
/// on (not a notice). `step` 0 searches forward from `from` inclusive.
fn selectable(convo: &Convo, from: usize, step: isize) -> Option<usize> {
    let entries = &convo.data.entries;
    let ok = |i: usize| !matches!(entries[i].body, Body::Notice(_));
    match step {
        0 => (from..entries.len()).find(|&i| ok(i)),
        s if s < 0 => (0..from.min(entries.len())).rev().find(|&i| ok(i)),
        _ => (from + 1..entries.len()).find(|&i| ok(i)),
    }
}

fn quoted(q: Quote, me: &str) -> Quoted {
    Quoted {
        id: q.id,
        mine: q.sender == me,
        text: q.text,
    }
}

fn notice(text: String) -> Entry {
    Entry {
        id: String::new(),
        mine: false,
        at_ms: now_ms(),
        body: Body::Notice(text),
        status: Status::Sent,
        hops: 0,
        reply: None,
        read: true,
        sender: None,
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    let mut out: String = line.chars().take(60).collect();
    if line.chars().count() > 60 || text.lines().count() > 1 {
        out.push('…');
    }
    out
}

/// What you wrote in a conversation, oldest first: not files, and not
/// home's and the AI's replies, which go out in your name.
fn sent_lines(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .filter(|e| e.mine)
        .filter_map(|e| match &e.body {
            Body::Text(text) => Some(text),
            _ => None,
        })
        .filter(|text| !text.starts_with(home::SIGNATURE) && !text.starts_with(home::ai::SIGNATURE))
        .cloned()
        .collect()
}

pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn expand_tilde(text: &str) -> PathBuf {
    match (text.strip_prefix("~/"), picker::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ if text == "~" => picker::home_dir().unwrap_or_default(),
        _ => PathBuf::from(text),
    }
}

/// Reads pasted text as the paths a terminal pastes for dropped files:
/// shell-escaped or quoted, space separated, or `file://` URLs. `None`
/// unless every word names an existing file.
fn dropped_paths(text: &str) -> Option<Vec<PathBuf>> {
    let text = text.trim();
    if text.is_empty() || text.len() > 64 * 1024 {
        return None;
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    let mut started = false;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '\\') => {
                word.extend(chars.next());
                started = true;
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (Some(q), c) if c == q => quote = None,
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (_, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    let paths: Vec<PathBuf> = words
        .iter()
        .map(|w| match w.strip_prefix("file://") {
            Some(rest) => PathBuf::from(percent_decode(rest)),
            None => expand_tilde(w),
        })
        .collect();
    let all_files = !paths.is_empty() && paths.iter().all(|p| p.is_absolute() || w_relative(p));
    (all_files && paths.iter().all(|p| p.is_file())).then_some(paths)
}

/// Relative paths count only when they plainly look like one, so pasting a
/// single word that happens to match a file here stays text.
fn w_relative(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.starts_with("./") || s.starts_with("../")
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = text.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn open_path(path: &Path) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Shows the file in its folder.
fn reveal_path(path: &Path) {
    if cfg!(target_os = "macos") {
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    } else if let Some(dir) = path.parent() {
        open_path(dir);
    }
}

/// OSC 9: a desktop notification, from the terminals known to show one.
/// Others could print it, so they get only the bell.
fn notify(title: &str, body: &str) {
    let term = std::env::var("TERM_PROGRAM").unwrap_or_default();
    if !matches!(term.as_str(), "iTerm.app" | "WezTerm" | "ghostty") || std::env::var_os("TMUX").is_some() {
        return;
    }
    let clean = |s: &str| -> String { s.chars().filter(|c| !c.is_control() && *c != ';').take(200).collect() };
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]9;{}: {}\x07", clean(title), clean(body));
    let _ = out.flush();
}

/// OSC 52: the terminal puts it on the system clipboard, even over SSH.
/// Terminal.app ignores that, so on a Mac's own screen pbcopy does it too.
fn copy_to_clipboard(text: &str) {
    if cfg!(test) {
        return;
    }
    if cfg!(target_os = "macos") && std::env::var_os("SSH_TTY").is_none() {
        pbcopy(text);
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{encoded}\x07");
    let _ = out.flush();
}

fn pbcopy(text: &str) {
    use std::process::{Command, Stdio};
    let child = Command::new("pbcopy").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    if let Ok(mut child) = child {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_dropped_paths() {
        let dir = std::env::temp_dir().join(format!("hopchat-drop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("my file.txt");
        let b = dir.join("b.png");
        std::fs::write(&a, "x").unwrap();
        std::fs::write(&b, "x").unwrap();
        let escaped = format!("{} {}", a.display().to_string().replace(' ', "\\ "), b.display());
        assert_eq!(dropped_paths(&escaped), Some(vec![a.clone(), b.clone()]));
        let quoted = format!("'{}'", a.display());
        assert_eq!(dropped_paths(&quoted), Some(vec![a.clone()]));
        let url = format!("file://{}", a.display().to_string().replace(' ', "%20"));
        assert_eq!(dropped_paths(&url), Some(vec![a]));
        assert_eq!(dropped_paths("hello there"), None);
        assert_eq!(dropped_paths(&format!("see {}", b.display())), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use chrono::TimeZone;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn text(id: &str, mine: bool, body: &str, status: Status) -> Entry {
        Entry {
            id: id.into(),
            mine,
            at_ms: now_ms(),
            body: Body::Text(body.into()),
            status,
            hops: 1,
            reply: None,
            read: true,
            sender: None,
        }
    }

    fn test_app() -> App {
        let (home, _) = std::sync::mpsc::channel();
        App::new("alice".into(), "off1alice000000000000".into(), None, PathBuf::from("/tmp/dl"), home)
    }

    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    #[test]
    fn renders_the_devices_panel() {
        use crate::home::{driver, DeviceView, ModelState, State, View};
        let mut app = test_app();
        let send = |_: Command| {};
        app.apply(
            Update::Group { id: "g1".into(), name: "house".into(), members: vec!["off1alice000000000000".into()] },
            &send,
        );
        app.selected = Some("g1".into());
        let device = |name: &str, backend, ip: [u8; 4], added, state, detail: &str| DeviceView {
            id: name.into(),
            category: driver::backend(backend).unwrap().category(),
            pairing: driver::backend(backend).unwrap().pairing(),
            test_help: driver::backend(backend).unwrap().test_help(),
            name: name.into(),
            ip: ip.into(),
            added,
            state,
            detail: detail.into(),
        };
        app.home_view = View {
            devices: vec![
                device("Desk lamp", "wiz", [192, 168, 1, 10], true, State::Ready, "on · 100% · focus"),
                device("TV", "webos", [192, 168, 1, 30], false, State::Pairing { until: Instant::now() + Duration::from_secs(48) }, ""),
                device("Light 2", "wiz", [192, 168, 1, 11], false, State::New, "Wiz · colour"),
            ],
            scanning: true,
            model: "qwen2.5:7b".into(),
            model_state: ModelState::NoServer,
            groups: Vec::new(),
            ..Default::default()
        };
        app.mode = Mode::Devices(Devices { selected: 1, ..Default::default() });
        let screen = screen(&mut app);
        println!("{screen}");
        for needle in [
            "Devices", "scanning", "Your devices", "Desk lamp", "on · 100% · focus", "Found on this network",
            "waiting for the TV · 4", "it's asking whether to allow hopchat", "Wiz · colour",
            "ollama serve", "house", "g lets this group in", "i lets it ask with @ai",
        ] {
            assert!(screen.contains(needle), "missing {needle:?}");
        }
        // No model here, but a group member lends theirs.
        app.home_view.lenders = vec!["bob (llama3.2)".into()];
        let lent = self::screen(&mut app);
        assert!(lent.contains("borrowing bob (llama3.2)"), "{lent}");
        assert!(!lent.contains("ollama serve"));
    }

    #[test]
    fn creates_a_group_from_the_panel() {
        let mut app = test_app();
        let commands = std::cell::RefCell::new(Vec::new());
        let send = |c: Command| commands.borrow_mut().push(c);
        app.apply(Update::PeerSeen { address: "off1bob00000000000000".into(), nick: "bob".into() }, &send);
        app.apply(Update::PeerUp { address: "off1bob00000000000000".into() }, &send);
        app.apply(Update::SessionReady { address: "off1bob00000000000000".into() }, &send);
        app.apply(Update::PeerSeen { address: "off1dave0000000000000".into(), nick: "dave".into() }, &send);
        app.selected = Some("off1bob00000000000000".into());

        let ctrl_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        app.on_key(ctrl_g, &send);
        for c in "house".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)), &send);
        }
        app.on_key(KeyEvent::from(KeyCode::Down), &send);
        app.on_key(KeyEvent::from(KeyCode::Char(' ')), &send);
        let shown = screen(&mut app);
        println!("{shown}");
        for needle in ["+ New group", "New group", "#house", "1 picked", "bob", "online", "dave", "offline · can't be added now", "In it: you, bob"] {
            assert!(shown.contains(needle), "missing {needle:?}");
        }

        app.on_key(KeyEvent::from(KeyCode::Enter), &send);
        assert!(matches!(app.mode, Mode::Normal));
        match commands.borrow().last() {
            Some(Command::CreateGroup { name, members }) => {
                assert_eq!(name, "house");
                assert_eq!(members, &["off1bob00000000000000".to_string()]);
            }
            _ => panic!("Enter should create the group"),
        }
        // The new group opens once the engine has made it.
        app.apply(Update::Group { id: "g1".into(), name: "house".into(), members: vec![] }, &send);
        app.apply(Update::GroupCreated { id: "g1".into() }, &send);
        assert_eq!(app.selected.as_deref(), Some("g1"));
    }

    #[test]
    fn renders_a_group_with_senders() {
        let mut app = test_app();
        let send = |_: Command| {};
        app.apply(Update::PeerSeen { address: "off1bob00000000000000".into(), nick: "bob".into() }, &send);
        app.apply(
            Update::Group {
                id: "g1".into(),
                name: "house".into(),
                members: vec!["off1alice000000000000".into(), "off1bob00000000000000".into()],
            },
            &send,
        );
        app.apply(
            Update::GroupIncoming {
                group: "g1".into(),
                from: "off1bob00000000000000".into(),
                id: "m1".into(),
                text: "@home dim the lights".into(),
                reply_to: None,
            },
            &send,
        );
        app.apply(
            Update::GroupQueued { group: "g1".into(), id: "m2".into(), text: "🏠 light is now on at 30%".into(), reply: None },
            &send,
        );
        app.selected = Some("g1".into());
        let screen = screen(&mut app);
        println!("{screen}");
        for needle in ["# house", "2 members", "bob", "@home dim the lights", "you", "light is now on at 30%"] {
            assert!(screen.contains(needle), "missing {needle:?}");
        }
        assert!(app.find_peer("BOB").is_some());
        assert!(app.find_peer("house").is_none(), "a group is not a person to invite");
    }

    #[test]
    fn me_is_yours_alone() {
        let mut app = test_app();
        let commands = std::cell::RefCell::new(Vec::new());
        let send = |c: Command| commands.borrow_mut().push(c);
        let order: Vec<&str> = app.ordered().iter().map(|c| c.data.address.as_str()).collect();
        assert_eq!(order[..2], [LOBBY, ME], "Me sits under the lobby");

        app.selected = Some(ME.into());
        let empty = screen(&mut app);
        println!("{empty}");
        for needle in ["◇ Me", "you and your devices", "only yours", "@home", "@ai", "to yourself", "Ctrl-D"] {
            assert!(empty.contains(needle), "missing {needle:?}");
        }

        app.editor.set("@home lights off");
        app.submit(&send);
        match commands.borrow_mut().pop() {
            Some(Command::SendGroup { group, text, from_home: false, .. }) => {
                assert_eq!((group.as_str(), text.as_str()), (ME, "@home lights off"));
            }
            _ => panic!("expected a send to Me"),
        }
        app.apply(Update::GroupQueued { group: ME.into(), id: "me_1".into(), text: "buy milk".into(), reply: None }, &send);
        let shown = screen(&mut app);
        assert!(shown.contains("buy milk") && !shown.contains("◷"), "no receipt in Me: {shown}");

        // Nobody to invite, and nothing to switch on.
        for command in ["/invite bob", "/leave", "/home on", "/ai off"] {
            app.editor.set(command);
            app.submit(&send);
            assert!(app.flash_text().is_some_and(|(_, error)| error), "{command} should be refused");
        }
        assert!(commands.borrow().is_empty(), "a refused command still went out");
        assert!(super::switches("on") && !super::switches("share on") && !super::switches(""));
    }

    #[test]
    fn opens_in_the_lobby_and_talks_there() {
        let mut app = test_app();
        let commands = std::cell::RefCell::new(Vec::new());
        let send = |c: Command| commands.borrow_mut().push(c);
        assert_eq!(app.selected.as_deref(), Some(LOBBY));
        let alone = screen(&mut app);
        println!("{alone}");
        for needle in ["◎ Lobby", "everyone nearby", "just you so far", "nobody nearby yet", "Looking for people", "@home", "@ai", "Only you can use them"] {
            assert!(alone.contains(needle), "missing {needle:?}");
        }

        let bob = "off1bob00000000000000";
        app.apply(Update::PeerSeen { address: bob.into(), nick: "bob".into() }, &send);
        app.apply(Update::PeerUp { address: bob.into() }, &send);
        app.apply(Update::SessionReady { address: bob.into() }, &send);
        assert_eq!(app.ordered()[0].data.address, LOBBY, "the lobby stays on top");
        app.apply(
            Update::GroupIncoming { group: LOBBY.into(), from: bob.into(), id: "lb_1".into(), text: "anyone up for lunch?".into(), reply_to: None },
            &send,
        );
        app.editor.set("@ai where should we eat?");
        app.submit(&send);
        match commands.borrow().last() {
            Some(Command::SendGroup { group, text, from_home: false, .. }) => {
                assert_eq!((group.as_str(), text.as_str()), (LOBBY, "@ai where should we eat?"));
            }
            _ => panic!("Enter should say it in the lobby"),
        }
        app.apply(
            Update::GroupQueued { group: LOBBY.into(), id: "lb_2".into(), text: "@ai where should we eat?".into(), reply: None },
            &send,
        );
        let shown = screen(&mut app);
        println!("{shown}");
        for needle in ["you and 1 nearby", "bob", "anyone up for lunch?", "@ai where should we eat?"] {
            assert!(shown.contains(needle), "missing {needle:?}");
        }
        assert!(!shown.contains('◷'), "no delivery tick in the lobby");

        app.apply(Update::PeerDown { address: bob.into() }, &send);
        assert!(screen(&mut app).contains("just you so far"));
        app.run_command("leave", &send);
        assert!(app.flash_text().is_some_and(|(t, error)| error && t.contains("no leaving")));
        assert!(!commands.borrow().iter().any(|c| matches!(c, Command::Leave { .. })));

        app.mode = Mode::Devices(Devices::default());
        let panel = screen(&mut app);
        for needle in ["only you can use them", "g lets everyone nearby in", "i lets everyone nearby ask"] {
            assert!(panel.contains(needle), "missing {needle:?}");
        }
        app.home_view.groups = vec![LOBBY.into()];
        assert!(screen(&mut app).contains("everyone nearby can use them"));
    }

    #[test]
    fn boots_and_any_key_gets_past() {
        let mut app = test_app();
        let send = |_: Command| {};
        app.boot = Some(Instant::now() - Duration::from_millis(1_500));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let boot: String = (0..buffer.area.height)
            .map(|y| (0..buffer.area.width).map(|x| buffer[(x, y)].symbol().to_string()).collect::<String>() + "\n")
            .collect();
        println!("{boot}");
        for needle in ["h o p c h a t", "no internet needed", "alice"] {
            assert!(boot.contains(needle), "missing {needle:?}");
        }
        // The key that dismisses it is not typed.
        app.on_key(KeyEvent::from(KeyCode::Char('x')), &send);
        assert!(!app.booting());
        assert!(app.editor.is_empty());
        assert!(screen(&mut app).contains("◎ Lobby"));
    }

    #[test]
    fn completes_mentions_and_clicks_keys() {
        let mut app = test_app();
        let send = |_: Command| {};
        app.editor.set("lights please @ho");
        let shown = screen(&mut app);
        assert!(shown.contains("@home") && shown.contains("ask the devices"), "{shown}");
        app.on_key(KeyEvent::from(KeyCode::Tab), &send);
        assert_eq!(app.editor.text(), "lights please @home ");
        assert_eq!(app.selected.as_deref(), Some(LOBBY), "Tab completed instead of switching chats");

        // The footer's keys are buttons.
        let (rect, _) = app
            .footer_hits
            .iter()
            .find(|(_, k)| *k == KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .copied()
            .expect("a devices button");
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 1,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(click, &send);
        assert!(matches!(app.mode, Mode::Devices(_)));
    }

    #[test]
    fn clicking_a_message_picks_it() {
        let mut app = test_app();
        let send = |_: Command| {};
        let bob = "off1bob00000000000000";
        let convo = app.convo_mut(bob);
        convo.data.nick = "bob".into();
        convo.push(text("m1", false, "first", Status::Sent));
        convo.push(text("m2", false, "second", Status::Sent));
        app.selected = Some(bob.into());
        screen(&mut app);
        let (row, _) = *app.message_rows.iter().find(|(_, i)| *i == 0).unwrap();
        let click = |row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.chat_area.x + 4,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let first = click(row);
        let release = MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), ..first };
        app.on_mouse(first, &send);
        app.on_mouse(release, &send);
        assert!(matches!(app.mode, Mode::Select(0)));
        assert!(screen(&mut app).contains(" reply "), "the footer offers what to do with it");
        app.on_mouse(first, &send);
        app.on_mouse(release, &send);
        assert!(matches!(app.mode, Mode::Normal), "a second click lets go");
    }

    #[test]
    fn dragging_over_a_message_copies_it_instead_of_picking_it() {
        let mut app = test_app();
        let send = |_: Command| {};
        let bob = "off1bob00000000000000";
        let convo = app.convo_mut(bob);
        convo.data.nick = "bob".into();
        convo.push(text("m1", false, "first", Status::Sent));
        app.selected = Some(bob.into());
        screen(&mut app);
        let (row, _) = *app.message_rows.last().unwrap();
        let event = |kind, column| MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE };
        let left = app.chat_area.x;
        let right = left + app.chat_area.width - 1;
        app.on_mouse(event(MouseEventKind::Down(MouseButton::Left), left), &send);
        app.on_mouse(event(MouseEventKind::Drag(MouseButton::Left), right), &send);
        app.on_mouse(event(MouseEventKind::Up(MouseButton::Left), right), &send);
        assert!(matches!(app.mode, Mode::Normal), "a drag picks nothing");
        assert!(app.selection.as_ref().is_some_and(|s| s.copy));

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(app.selection.as_ref().map(|s| s.text(buffer).contains("first")), Some(true));
        assert_eq!(app.flash_text(), Some(("copied to clipboard", false)));
        assert!(app.selection.as_ref().is_some_and(|s| !s.copy), "copied once");

        app.on_key(KeyEvent::from(KeyCode::Char('x')), &send);
        assert!(app.selection.is_none(), "typing lets it go");
    }

    #[test]
    fn renders_a_conversation() {
        let mut app = test_app();
        let convo = app.convo_mut("off1bob00000000000000");
        convo.data.nick = "bob".into();
        convo.online = true;
        convo.secure = true;
        convo.push(text("m1", false, "hey, can you send me the deck?", Status::Sent));
        let mut reply = text("m2", true, "sure, one sec", Status::Read);
        reply.reply = Some(Quoted { id: "m1".into(), mine: false, text: "hey, can you send me the deck?".into() });
        convo.push(reply);
        convo.push(Entry {
            id: "f1".into(),
            mine: true,
            at_ms: now_ms(),
            body: Body::File {
                file_id: "f1".into(),
                name: "q3-deck.pdf".into(),
                size: 4_200_000,
                path: "/tmp/q3-deck.pdf".into(),
                transfer: Transfer::Moving { done: 40, total: 100 },
            },
            status: Status::Queued,
            hops: 0,
            reply: None,
            read: true,
            sender: None,
        });
        convo.typing_until = Some(Instant::now() + Duration::from_secs(5));
        app.convo_mut("off1carol0000000000000").data.nick = "carol".into();
        app.selected = Some("off1bob00000000000000".into());
        app.editor.set("looks good");

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        println!("{screen}");
        for needle in ["bob", "carol", "Today", "sure, one sec", "✓✓ read", "q3-deck.pdf", "40%", "bob is typing", "looks good"] {
            assert!(screen.contains(needle), "missing {needle:?}");
        }
    }

    #[test]
    fn long_names_stay_in_their_lane() {
        let mut app = test_app();
        let send = |_: Command| {};
        let long = "Bartholomew the Extremely Long-Named Wanderer of the Northern Hills";
        let bob = "off1bob00000000000000";
        app.apply(Update::PeerSeen { address: bob.into(), nick: format!("{long}\n\x1b[2J") }, &send);
        let convo = app.convo_mut(bob);
        convo.online = true;
        convo.secure = true;
        convo.push(text("m1", false, &format!("{} https://example.com/a/really/long/link", "word ".repeat(30)), Status::Sent));
        let mut reply = text("m2", true, "sure", Status::Read);
        reply.reply = Some(Quoted { id: "m1".into(), mine: false, text: "word ".repeat(30) });
        convo.push(reply);
        convo.push(Entry {
            id: "f1".into(),
            mine: false,
            at_ms: now_ms(),
            body: Body::File {
                file_id: "f1".into(),
                name: "the-quarterly-planning-report-final-final-really-final-v12.pdf".into(),
                size: 4_200_000,
                path: "/tmp/some/deeply/nested/folder/for/downloads/the-quarterly-planning-report-final-final-really-final-v12.pdf".into(),
                transfer: Transfer::Done,
            },
            status: Status::Sent,
            hops: 1,
            reply: None,
            read: true,
            sender: None,
        });
        convo.typing_until = Some(Instant::now() + Duration::from_secs(5));
        app.selected = Some(bob.into());
        app.reply = Some("m1".into());

        assert_eq!(app.nick_of(bob), long[..40].trim_end(), "one tidy line, capped");
        for (w, h) in [(100, 30), (72, 30), (150, 36)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
            let buffer = terminal.backend().buffer();
            let shown: String = (0..buffer.area.height)
                .map(|y| (0..buffer.area.width).map(|x| buffer[(x, y)].symbol().to_string()).collect::<String>() + "\n")
                .collect();
            println!("{w}x{h}\n{shown}");
            assert!(!shown.contains('\x1b'));
            assert!(shown.contains(".pdf"), "the extension survives at {w}");
            assert!(shown.contains(&clock_now()), "the time beside the name survives at {w}");
            assert!(shown.contains("is typing"), "at {w}");
        }
    }

    #[test]
    fn todos_and_reminders_from_anywhere() {
        let mut app = test_app();
        let commands = std::cell::RefCell::new(Vec::new());
        let send = |c: Command| commands.borrow_mut().push(c);
        let bob = "off1bob00000000000000";
        app.apply(Update::PeerSeen { address: bob.into(), nick: "bob".into() }, &send);
        let convo = app.convo_mut(bob);
        convo.online = true;
        convo.secure = true;
        convo.push(text("m1", false, "can you send the lease by friday?", Status::Sent));
        app.selected = Some(bob.into());

        // A message becomes a todo with t, prefilled and ready for a time.
        screen(&mut app);
        app.mode = Mode::Select(0);
        app.on_key(KeyEvent::from(KeyCode::Char('t')), &send);
        let Mode::Tasks(panel) = &app.mode else { panic!("t should open the list") };
        assert_eq!(panel.input.text(), "can you send the lease by friday?");
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), &send);
        for c in "send bob the lease in 20m".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)), &send);
        }
        let shown = screen(&mut app);
        println!("{shown}");
        for needle in ["Todos & reminders", "Add  ›", "send bob the lease in 20m", "◷ reminder", "in 20m"] {
            assert!(shown.contains(needle), "missing {needle:?}");
        }
        app.on_key(KeyEvent::from(KeyCode::Enter), &send);
        for c in "buy milk".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)), &send);
        }
        app.on_key(KeyEvent::from(KeyCode::Enter), &send);
        app.run_command("remind", &send);
        app.run_command("todo water the plants", &send);
        app.run_command("remind water the plants", &send);
        assert!(app.flash_text().is_some_and(|(t, error)| error && t.contains("when?")), "a reminder needs a time");
        assert_eq!(app.tasks.items.len(), 3);
        let lease = app.tasks.items.iter().find(|t| t.text == "send bob the lease").unwrap().clone();
        assert_eq!(lease.origin.as_ref().map(|o| o.name.as_str()), Some("bob"));

        app.mode = Mode::Tasks(TaskPanel::new(None, ""));
        let shown = screen(&mut app);
        println!("{shown}");
        for needle in ["Reminders", "send bob the lease", "Todo", "[ ] buy milk", "water the plants", "3 open"] {
            assert!(shown.contains(needle), "missing {needle:?}");
        }
        assert!(commands.borrow().is_empty(), "nothing about tasks goes on the network");

        // Its time comes while you're in bob's chat: it rings in Me.
        app.mode = Mode::Normal;
        let due = chrono::Local.timestamp_millis_opt(lease.due_ms.unwrap()).unwrap();
        app.ring(due);
        app.ring(due);
        let me = app.convos.iter().find(|c| c.data.address == ME).unwrap();
        assert_eq!(me.unread, 1, "once, and Me says so");
        assert!(matches!(&me.data.entries.last().unwrap().body, Body::Reminder { text, .. } if text == "send bob the lease"));
        assert!(app.flash_text().is_some_and(|(t, _)| t.contains("send bob the lease")));
        let top = screen(&mut app);
        assert!(top.contains("1 due Ctrl-T"), "the strip at the top says so: {top}");

        app.selected = Some(ME.into());
        let shown = screen(&mut app);
        println!("{shown}");
        for needle in ["reminder", "send bob the lease", "due", "set in bob", "x done · s snooze"] {
            assert!(shown.contains(needle), "missing {needle:?}");
        }
        // Checked off from the message itself.
        let at = app.current().unwrap().data.entries.len() - 1;
        app.mode = Mode::Select(at);
        assert!(screen(&mut app).contains("snooze 10m"), "the footer offers what a reminder can do");
        app.on_key(KeyEvent::from(KeyCode::Char('x')), &send);
        assert!(app.tasks.get(&lease.id).unwrap().done());
        assert!(screen(&mut app).contains("✓ done"));
        app.on_key(KeyEvent::from(KeyCode::Char('o')), &send);
        assert_eq!(app.selected.as_deref(), Some(bob), "o goes back to where it was set");
    }

    #[test]
    fn the_list_checks_off_snoozes_and_undoes() {
        let mut app = test_app();
        let send = |_: Command| {};
        app.run_command("todo buy milk", &send);
        app.run_command("todo call mom tomorrow at 5pm", &send);
        app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL), &send);
        assert!(matches!(app.mode, Mode::Tasks(_)));
        let key = |app: &mut App, code| app.on_key(KeyEvent::from(code), &send);
        key(&mut app, KeyCode::Down);
        // The reminder sorts first.
        key(&mut app, KeyCode::Char('j'));
        key(&mut app, KeyCode::Char(' '));
        let milk = app.tasks.items.iter().find(|t| t.text == "buy milk").unwrap().clone();
        assert!(app.tasks.get(&milk.id).unwrap().done());
        let shown = screen(&mut app);
        println!("{shown}");
        assert!(shown.contains("Done · 1") && shown.contains("[✓] buy milk"), "{shown}");
        key(&mut app, KeyCode::Char('d'));
        assert!(app.tasks.get(&milk.id).is_none());
        key(&mut app, KeyCode::Char('u'));
        assert!(app.tasks.get(&milk.id).is_some(), "u brings it back");
        key(&mut app, KeyCode::Home);
        key(&mut app, KeyCode::Char('s'));
        let mom = app.tasks.items.iter().find(|t| t.text == "call mom").unwrap();
        assert!(mom.due_ms.unwrap() - now_ms() <= crate::tasks::SNOOZE_MS, "snoozing moves it to soon");
        key(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
        for (w, h) in [(72, 20), (150, 40)] {
            app.mode = Mode::Tasks(TaskPanel::new(None, "a really long thing to do that goes on and on and on past the edge of the panel at 5pm"));
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
        }
    }

    #[test]
    fn up_recalls_what_was_sent_and_enter_sends_it_again() {
        let mut app = test_app();
        let commands = std::cell::RefCell::new(Vec::new());
        let send = |c: Command| commands.borrow_mut().push(c);
        let key = |app: &mut App, code| app.on_key(KeyEvent::from(code), &send);
        app.select(Some(LOBBY.into()), &send);
        app.apply(
            Update::GroupQueued { group: LOBBY.into(), id: "m1".into(), text: "from last time".into(), reply: None },
            &send,
        );
        app.apply(
            Update::GroupQueued { group: LOBBY.into(), id: "m2".into(), text: "🏠 lights are off".into(), reply: None },
            &send,
        );
        app.editor.set("hello");
        key(&mut app, KeyCode::Enter);
        app.editor.set("/log");
        key(&mut app, KeyCode::Enter);
        assert!(app.show_log);

        app.editor.set("half written");
        key(&mut app, KeyCode::Up);
        assert_eq!(app.editor.text(), "/log", "commands come back too");
        key(&mut app, KeyCode::Up);
        assert_eq!(app.editor.text(), "hello");
        key(&mut app, KeyCode::Up);
        assert_eq!(app.editor.text(), "from last time", "history survives, home's replies don't");
        key(&mut app, KeyCode::Up);
        assert_eq!(app.editor.text(), "from last time", "stops at the oldest");
        assert!(matches!(app.mode, Mode::Normal));
        for _ in 0..3 {
            key(&mut app, KeyCode::Down);
        }
        assert_eq!(app.editor.text(), "half written", "the draft comes back");

        app.editor.take();
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Up);
        commands.borrow_mut().clear();
        key(&mut app, KeyCode::Enter);
        assert!(app.editor.is_empty());
        assert!(
            commands.borrow().iter().any(|c| matches!(c, Command::SendGroup { text, .. } if text == "hello")),
            "Enter sends it again"
        );
        key(&mut app, KeyCode::Up);
        assert_eq!(app.editor.text(), "hello", "the resend is the newest line");
    }

    #[test]
    fn recalled_lines_stay_in_their_conversation() {
        let mut app = test_app();
        let send = |_: Command| {};
        let key = |app: &mut App, code| app.on_key(KeyEvent::from(code), &send);
        app.select(Some(LOBBY.into()), &send);
        app.editor.set("for the lobby");
        key(&mut app, KeyCode::Enter);
        app.select(Some(ME.into()), &send);
        key(&mut app, KeyCode::Up);
        assert_eq!(app.editor.text(), "", "nothing was sent here");

        app.select(Some(LOBBY.into()), &send);
        app.mode = Mode::Normal;
        app.editor.set("/clear");
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Up);
        assert_eq!(app.editor.text(), "", "clearing a conversation clears its recall");
    }

    #[test]
    fn multi_line_text_moves_before_it_recalls() {
        let mut app = test_app();
        let send = |_: Command| {};
        let key = |app: &mut App, code| app.on_key(KeyEvent::from(code), &send);
        app.select(Some(LOBBY.into()), &send);
        app.editor.set("sent");
        key(&mut app, KeyCode::Enter);
        app.editor.set("a\nb");
        key(&mut app, KeyCode::Up);
        assert_eq!(app.editor.text(), "a\nb");
        key(&mut app, KeyCode::Up);
        assert_eq!(app.editor.text(), "sent");
    }

    #[test]
    fn alt_up_selects_a_message_and_up_does_with_nothing_sent() {
        let mut app = test_app();
        let send = |_: Command| {};
        app.select(Some(LOBBY.into()), &send);
        app.apply(
            Update::GroupIncoming {
                group: LOBBY.into(),
                from: "off1bob00000000000000".into(),
                id: "m1".into(),
                text: "hi".into(),
                reply_to: None,
            },
            &send,
        );
        app.on_key(KeyEvent::from(KeyCode::Up), &send);
        assert!(matches!(app.mode, Mode::Select(_)));

        app.mode = Mode::Normal;
        app.editor.set("mine");
        app.on_key(KeyEvent::from(KeyCode::Enter), &send);
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT), &send);
        assert!(matches!(app.mode, Mode::Select(_)));
    }

    fn clock_now() -> String {
        chrono::Local::now().format("%H:%M").to_string()
    }
}
