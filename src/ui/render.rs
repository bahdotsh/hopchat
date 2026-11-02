//! Drawing: everything here reads the app and paints; nothing changes
//! state except the remembered hit areas, the scroll clamp, and copying a
//! released selection, which is read off the finished frame.

use std::time::Duration;

use chrono::{DateTime, Datelike, Local, TimeZone};
use offline_protocol::Event;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::devices::{self, Devices, Primary};
use super::groups::{Candidate, Focus, GroupPanel};
use super::tasks::{self as task_panel, TaskPanel};
use crate::tasks::{self, Section, Task};
use super::theme;
use crate::lobby::LOBBY;
use crate::me::ME;
use super::picker::{self, Picker};
use crate::home::{self, DeviceView, ModelState, State};
use super::{App, Convo, Mode};
use crate::history::{Body, Entry, Status, Transfer};

pub fn accent() -> Color { theme::iris() }
pub fn theirs() -> Color { theme::foam() }
pub fn ok() -> Color { theme::sage() }
pub fn warn() -> Color { theme::peach() }
pub fn err() -> Color { theme::red() }
pub fn read() -> Color { theme::foam() }
/// Keys to press, wherever they are mentioned.
fn key_color() -> Color { theme::gold() }
fn selected_bg() -> Color { theme::raised() }
const SIDEBAR: u16 = 30;
/// The most a name takes in a line of other things, so whatever follows
/// it (a time, a hint, a key) still shows.
const NAME_ROOM: usize = 24;
/// A new header is drawn when the same sender pauses this long.
const GROUP_GAP_MS: i64 = 5 * 60 * 1000;
const INDENT: &str = "  ";
/// Messages wrap at this many columns however wide the window, so a
/// tick never sits a screen away from what it ticks.
const READING_WIDTH: usize = 96;
/// How long the boot screen shows, unless a key skips it.
pub const BOOT_FOR: Duration = Duration::from_millis(1_800);

pub const COMMANDS: &[(&str, &str)] = &[
    ("/file", "attach and send files (path, or no path for the picker)"),
    ("/downloads", "open the folder received files are saved in"),
    ("/info", "show who this peer is and how they are connected"),
    ("/group", "start a group and pick who's in it (also Ctrl-G) · /group NAME"),
    ("/invite", "add people to this group · /invite NICK"),
    ("/members", "list who is in this group"),
    ("/leave", "leave this group"),
    ("/todo", "your todos and reminders (also Ctrl-T) · /todo buy milk · /todo call mom at 5pm"),
    ("/remind", "set a reminder · /remind 20m stretch · /remind standup every weekday at 9:30"),
    ("/devices", "find, pair and test your smart devices (also Ctrl-D)"),
    ("/home", "/home on|off lets everyone here use your devices (you always can) · /home add IP"),
    ("/ai", "/ai on|off lets everyone here ask your model (you always can) · /ai share on|off lends it"),
    ("/me", "show and copy your own address"),
    ("/clear", "delete this conversation from this device"),
    ("/log", "show or hide the protocol event log"),
    ("/theme", "switch between dark and light · /theme dark · /theme light"),
    ("/help", "keys and commands"),
    ("/quit", "leave"),
];

/// What can be called on by name in a group or the lobby.
pub const MENTIONS: &[(&str, &str)] = &[
    ("@home", "ask the devices: lights, the TV"),
    ("@ai", "ask the model anything"),
];

/// Commands whose name starts with what is typed, while the name is still
/// being typed.
pub fn command_matches(input: &str) -> Vec<(&'static str, &'static str)> {
    if input.contains(' ') || input.contains('\n') {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .copied()
        .filter(|(name, _)| name.starts_with(input))
        .collect()
}

/// Mentions that start with the word being typed at the end of `input`.
pub fn mention_matches(input: &str) -> Vec<(&'static str, &'static str)> {
    let word = input.rsplit([' ', '\n']).next().unwrap_or_default();
    if !word.starts_with('@') {
        return Vec::new();
    }
    MENTIONS
        .iter()
        .copied()
        .filter(|(name, _)| name.starts_with(word))
        .collect()
}

impl App {
    pub fn draw(&mut self, frame: &mut Frame) {
        frame.render_widget(Block::new().style(Style::new().bg(theme::bg()).fg(theme::text())), frame.area());
        if self.booting() {
            self.draw_boot(frame);
            return;
        }
        let [hud, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        let narrow = body.width < 70;
        let side_width = if narrow { 0 } else { SIDEBAR };
        let log_width = if self.show_log && body.width > 110 { 46 } else { 0 };
        let [side, main, log] = Layout::horizontal([
            Constraint::Length(side_width),
            Constraint::Min(30),
            Constraint::Length(log_width),
        ])
        .areas(body);

        self.draw_hud(frame, hud);
        if side_width > 0 {
            self.draw_sidebar(frame, side);
        } else {
            self.peer_rows.clear();
            self.new_group_row = Rect::default();
        }
        // The log's text starts below its border, title and rule.
        self.log_area = if log_width > 0 {
            Rect::new(log.x + 1, log.y + 2, log.width - 1, log.height.saturating_sub(2))
        } else {
            Rect::default()
        };
        self.draw_main(frame, main);
        if log_width > 0 {
            self.draw_log(frame, log);
        }
        self.draw_footer(frame, footer);

        match &self.mode {
            Mode::Picker(picker) => draw_picker(frame, picker),
            Mode::Devices(panel) => self.draw_devices(frame, panel),
            Mode::Groups(panel) => self.draw_groups(frame, panel),
            Mode::Tasks(panel) => self.draw_tasks(frame, panel),
            &Mode::Help(at) => {
                let max = draw_help(frame, at);
                self.mode = Mode::Help(at.min(max));
            }
            _ => {}
        }
        self.finish_selection(frame.buffer_mut());
    }

    /// The opening moment: the name, one line about what this is, and who
    /// you are, each fading in.
    fn draw_boot(&self, frame: &mut Frame) {
        let area = frame.area();
        let t = self.boot.map_or(i64::MAX, |at| at.elapsed().as_millis() as i64);
        let lines = vec![
            Line::from("◌".fg(theme::fade_in(theme::iris(), t, 0, 500))).centered(),
            Line::from(""),
            Line::from("h o p c h a t".fg(theme::fade_in(theme::text(), t, 100, 600)).bold()).centered(),
            Line::from(""),
            Line::from(
                "talk to the people around you, no internet needed".fg(theme::fade_in(theme::muted(), t, 450, 600)),
            )
            .centered(),
            Line::from(""),
            Line::from(""),
            Line::from(vec![
                clip(&self.me_nick, (area.width as usize).saturating_sub(24)).fg(theme::fade_in(theme::iris(), t, 800, 500)),
                format!("  ·  {}", short(&self.me_address)).fg(theme::fade_in(theme::faint(), t, 800, 500)),
            ])
            .centered(),
        ];
        let top = (area.height as usize).saturating_sub(lines.len()) / 2;
        let at = Rect::new(area.x, area.y + top as u16, area.width, area.height - top as u16);
        frame.render_widget(Paragraph::new(lines), at);
    }

    /// The strip along the top: who you are, who's around, and the time.
    fn draw_hud(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Block::new().style(Style::new().bg(theme::panel())), area);
        let online = self.convos.iter().filter(|c| !c.data.group && c.online && c.secure).count();
        let clock = Local::now().format("%H:%M  ").to_string();
        let room = (area.width as usize).saturating_sub(clock.width() + 1);

        let nearby = match online {
            0 => "nobody nearby yet".fg(theme::faint()),
            1 => "1 person nearby".fg(theme::muted()),
            n => format!("{n} people nearby").fg(theme::muted()),
        };
        // What's waiting for you: due first, else the next reminder, else
        // how many todos.
        let due = self.tasks.due_count();
        let waiting = if due > 0 {
            vec!["   ".into(), format!("⏰ {due} due").fg(theme::gold()).bold(), " Ctrl-T".fg(theme::faint())]
        } else if let Some(next) = self.tasks.next() {
            let at = next.due_ms.unwrap_or_default();
            let when = if at - theme::now_ms() < 60 * 60 * 1000 { tasks::until(at, theme::now_ms()) } else { self::clock(at) };
            vec!["   ".into(), "◷ ".fg(theme::foam()), when.fg(theme::muted()), " ".into(), clip(&next.text, 24).fg(theme::faint())]
        } else {
            match self.tasks.open_count() {
                0 => Vec::new(),
                n => vec!["   ".into(), format!("☐ {n} todo{}", if n == 1 { "" } else { "s" }).fg(theme::muted())],
            }
        };
        let mut segments: Vec<Vec<Span>> = vec![
            vec!["  hopchat".fg(theme::iris()).bold()],
            vec!["   ".into(), clip(&self.me_nick, NAME_ROOM).fg(theme::text())],
            vec!["   ".into(), "●".fg(if online > 0 { theme::sage() } else { theme::faint() }), " ".into(), nearby],
        ];
        if !waiting.is_empty() {
            segments.push(waiting);
        }
        segments.push(vec!["   end-to-end · offline".fg(theme::faint())]);
        frame.render_widget(Line::from(fitting(segments, room)), area);
        frame.render_widget(Line::from(clock.fg(theme::faint())).alignment(Alignment::Right), area);
    }

    fn draw_sidebar(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::new()
            .borders(Borders::RIGHT)
            .border_style(Style::new().fg(theme::line()))
            .style(Style::new().bg(theme::panel()));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // The button and its rule sit level with the chat's header, so one
        // line runs under both.
        let [new_group, rule, list, me] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .areas(inner);
        frame.render_widget(theme::rule(rule.width as usize), rule);
        junction(frame, inner.right(), rule.y, "┼");

        // A button: the same panel as Ctrl-G.
        let open = matches!(self.mode, Mode::Groups(_));
        // Its + sits in the column of the chats' dots, its words in theirs.
        let row = Rect::new(new_group.x, new_group.y, new_group.width, 1);
        if open {
            frame.render_widget(Block::new().style(Style::new().bg(theme::raised())), row);
        }
        frame.render_widget(
            Line::from(vec![" + ".fg(theme::iris()), "New group".fg(if open { theme::text() } else { theme::muted() })]),
            row,
        );
        frame.render_widget(Line::from("Ctrl-G  ".fg(theme::faint())).alignment(Alignment::Right), row);
        self.new_group_row = row;

        let width = me.width as usize;
        junction(frame, inner.right(), me.y, "┤");
        frame.render_widget(
            Paragraph::new(vec![
                theme::rule(width),
                Line::from(vec![
                    "   ".into(),
                    clip(&self.me_nick, width.saturating_sub(9)).fg(theme::iris()).bold(),
                    "  you".fg(theme::faint()),
                ]),
                Line::from(format!("   {}", short(&self.me_address)).fg(theme::faint())),
            ]),
            me,
        );

        self.peer_rows.clear();
        let ordered: Vec<&Convo> = self.ordered();
        // The lobby and Me are always listed; with no one else, say how to
        // get people.
        let alone = ordered.iter().all(|c| c.data.address == LOBBY || c.data.address == ME);

        let list = Rect::new(list.x, list.y + 1, list.width, list.height.saturating_sub(1));
        let width = list.width as usize;
        let mut y = list.y;
        let mut rows = Vec::new();
        for convo in &ordered {
            if y + 2 > list.y + list.height {
                break;
            }
            let selected = self.selected.as_deref() == Some(&convo.data.address);
            let style = if selected {
                Style::new().bg(selected_bg())
            } else {
                Style::new()
            };
            let dot = match (convo.data.address == LOBBY, convo.data.group, convo.online, convo.secure) {
                (true, ..) => "◎".fg(theme::foam()),
                _ if convo.data.address == ME => "◇".fg(theme::iris()),
                (_, true, true, _) => "#".fg(theme::rose()),
                (_, true, false, _) => "#".fg(theme::faint()),
                (_, false, true, true) => "●".fg(theme::sage()),
                (_, false, true, false) => "◐".fg(theme::peach()),
                (_, false, false, _) => "○".fg(theme::faint()),
            };
            let tint = match () {
                _ if convo.data.group => theme::text(),
                _ if convo.online => theme::person(&convo.data.address),
                _ => theme::muted(),
            };
            let marker = if selected { "▎".fg(theme::iris()) } else { " ".into() };
            let badge = if convo.unread > 0 {
                format!(" {} ", convo.unread.min(99))
            } else {
                String::new()
            };
            // Marker, dot and a space before; the badge and a margin after.
            let room = width.saturating_sub(3 + badge.width() + 2);
            let mut name = clip(&convo.data.nick, room).fg(tint);
            name = if selected || convo.unread > 0 {
                name.bold()
            } else {
                name
            };
            let used = 3 + UnicodeWidthStr::width(name.content.as_ref());
            let pad = width.saturating_sub(used + badge.width() + 2);
            let badge = if convo.unread > 0 {
                Span::styled(badge, Style::new().fg(theme::bg()).bg(theme::iris()))
            } else {
                badge.fg(theme::faint())
            };
            let top = Line::from(vec![marker, dot, " ".into(), name, " ".repeat(pad).into(), badge]);

            let preview = if convo.typing() {
                format!("typing{}", dots()).fg(tint).italic()
            } else {
                let text = convo
                    .data
                    .entries
                    .iter()
                    .rev()
                    .find(|e| !matches!(e.body, Body::Notice(_)))
                    .map(|e| {
                        let who = match &e.sender {
                            _ if e.mine => "you: ".to_string(),
                            Some(sender) => format!("{}: ", self.nick_of(sender)),
                            None => String::new(),
                        };
                        format!("{who}{}", entry_summary(e))
                    })
                    .unwrap_or_else(|| match convo.data.address.as_str() {
                        LOBBY => "everyone nearby".into(),
                        ME => "notes, @home, @ai".into(),
                        _ if convo.data.group => match convo.data.members.len() {
                            1 => "just you so far".into(),
                            n => format!("{n} members"),
                        },
                        address => short(address),
                    });
                clip(&text.replace('\n', " "), width.saturating_sub(4)).fg(if selected { theme::muted() } else { theme::faint() })
            };
            let bottom = Line::from(vec!["   ".into(), preview]);

            let row = Rect::new(list.x, y, list.width, 2);
            frame.render_widget(Paragraph::new(vec![top, bottom]).style(style), row);
            rows.push((row, convo.data.address.clone()));
            y += 3;
        }
        self.peer_rows = rows;
        if alone && y < list.y + list.height {
            let lines = vec![
                Line::from(vec!["  Looking for people".fg(theme::muted()), dots().fg(theme::muted())]),
                Line::from(""),
                Line::from("  Run hopchat on another".fg(theme::faint())),
                Line::from("  device on this network.".fg(theme::faint())),
            ];
            frame.render_widget(Paragraph::new(lines), Rect::new(list.x, y, list.width, list.y + list.height - y));
        }
    }

    fn draw_main(&mut self, frame: &mut Frame, area: Rect) {
        // The composer sits a column in from each side, level with the
        // messages, and keeps a column inside its frame: borders and
        // padding take 6, the prompt 2.
        let composer_height = self.composer_height(area.width.saturating_sub(8) as usize);
        // A blank row between the newest message and the composer, so the
        // last line never sits on the frame.
        let [header, history, _, composer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(composer_height),
        ])
        .areas(area);
        let composer = Rect::new(composer.x + 1, composer.y, composer.width.saturating_sub(2), composer.height);
        self.chat_area = history;
        self.composer_area = composer;

        self.draw_header(frame, header);
        self.draw_history(frame, history);
        self.draw_composer(frame, composer);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let [top, line] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
        frame.render_widget(theme::rule(line.width as usize), line);
        let Some(convo) = self.current() else {
            frame.render_widget(Line::from("  no conversation yet".fg(theme::muted())), top);
            return;
        };
        let (icon, tint) = match (convo.online, convo.secure) {
            _ if convo.data.address == LOBBY => ("◎ ".fg(theme::foam()), theme::text()),
            _ if convo.data.address == ME => ("◇ ".fg(theme::iris()), theme::text()),
            _ if convo.data.group => ("# ".fg(if convo.online { theme::rose() } else { theme::faint() }), theme::text()),
            (true, true) => ("● ".fg(theme::sage()), theme::person(&convo.data.address)),
            (true, false) => ("◐ ".fg(theme::peach()), theme::person(&convo.data.address)),
            (false, _) => ("○ ".fg(theme::faint()), theme::muted()),
        };
        let state = match (convo.online, convo.secure) {
            _ if convo.data.address == LOBBY => {
                let others = convo.data.members.len().saturating_sub(1);
                let here = match others {
                    0 => "just you so far".to_string(),
                    n => format!("you and {n} nearby"),
                };
                vec![here.fg(theme::muted()), "  ·  everyone on this network".fg(theme::faint())]
            }
            _ if convo.data.address == ME => {
                vec!["you and your devices".fg(theme::muted()), "  ·  never leaves this machine".fg(theme::faint())]
            }
            _ if convo.data.group => {
                let n = convo.data.members.len();
                let members = format!("{n} member{}", if n == 1 { "" } else { "s" });
                if convo.online {
                    vec![members.fg(theme::muted()), "  ·  end-to-end encrypted group".fg(theme::faint())]
                } else {
                    vec!["you left this group".fg(theme::faint())]
                }
            }
            (true, true) => vec!["online".fg(theme::sage()), "  ·  end-to-end encrypted".fg(theme::faint())],
            (true, false) => vec![format!("securing the link{}", dots()).fg(theme::peach())],
            (false, _) => vec!["away  ·  messages wait for them".fg(theme::faint())],
        };
        let width = top.width as usize;
        let name = clip(&convo.data.nick, width.saturating_sub(6)).fg(tint).bold();
        let mut spans = vec!["  ".into(), icon, name];
        let lead: usize = spans.iter().map(Span::width).sum();
        // The state gives way a piece at a time; the address only shows
        // when everything fits beside it.
        let segments: Vec<Vec<Span>> = state
            .into_iter()
            .enumerate()
            .map(|(i, s)| if i == 0 { vec!["   ".into(), s] } else { vec![s] })
            .collect();
        let full: usize = segments.iter().flatten().map(Span::width).sum();
        let address = (convo.data.address != LOBBY && convo.data.address != ME)
            .then(|| format!("{}  ", short(&convo.data.address)))
            .filter(|a| lead + full + a.width() + 2 <= width);
        let room = width.saturating_sub(lead + address.as_ref().map_or(0, |a| a.width()));
        spans.extend(fitting(segments, room));
        frame.render_widget(Line::from(spans), top);
        if let Some(address) = address {
            frame.render_widget(Line::from(address.fg(theme::faint())).alignment(Alignment::Right), top);
        }
    }

    fn draw_history(&mut self, frame: &mut Frame, area: Rect) {
        self.message_rows.clear();
        let Some(convo) = self.current() else {
            let lines = vec![
                Line::from(""),
                Line::from("Nobody here yet.".fg(theme::muted())).centered(),
                Line::from(""),
                Line::from("hopchat finds people on your local network by itself —".fg(theme::faint())).centered(),
                Line::from("start it on another laptop and they'll appear on the left.".fg(theme::faint())).centered(),
            ];
            frame.render_widget(Paragraph::new(lines), area);
            return;
        };

        // Dividers, notices and the selection bar span the chat, centred
        // in it; only the messages themselves keep to a reading width.
        let full = area.width.saturating_sub(2).max(10) as usize;
        let width = full.min(READING_WIDTH);
        let selected = match self.mode {
            Mode::Select(i) => Some(i),
            _ => None,
        };
        let reply_to = self.reply.as_deref();
        let mut rows: Vec<(Line<'static>, Option<usize>)> = Vec::new();
        let mut prev: Option<&Entry> = None;
        for (i, entry) in convo.data.entries.iter().enumerate() {
            if prev.is_none_or(|p| !same_day(p.at_ms, entry.at_ms)) {
                rows.push((day_separator(entry.at_ms, full), None));
            }
            let lines = self.entry_lines(convo, entry, prev, width);
            prev = Some(entry);
            let highlight = selected == Some(i) || (reply_to == Some(entry.id.as_str()) && !entry.id.is_empty());
            for line in lines {
                let line = if highlight { highlight_line(line, full) } else { line };
                rows.push((line, Some(i)));
            }
        }
        if convo.typing() {
            let tint = theme::person(&convo.data.address);
            rows.push((Line::from(""), None));
            rows.push((
                Line::from(vec![
                    "▎ ".fg(theme::dim(tint)),
                    format!("{} is typing", clip(&convo.data.nick, NAME_ROOM)).fg(tint).italic(),
                    dots().fg(tint),
                ]),
                None,
            ));
        }
        if convo.data.entries.is_empty() {
            let mut hints: Vec<Line<'static>> = Vec::new();
            if convo.data.address == LOBBY {
                let others = convo.data.members.len() > 1;
                if !others && area.height >= 16 {
                    rows.push((Line::from(""), None));
                    for line in theme::ripples(29, 7) {
                        rows.push((line.centered(), None));
                    }
                }
                let view = &self.home_view;
                let open = view.groups.iter().chain(&view.ai_groups).any(|g| g == LOBBY);
                hints.extend(lobby_hint(others, open));
            }
            if convo.data.address == ME {
                hints.extend(me_hint(&self.home_view));
            }
            let hint = if convo.data.address == LOBBY || convo.data.address == ME {
                ""
            } else if convo.data.group {
                "A new group. /invite picks people to add; Ctrl-D, then g, lets them use your devices."
            } else if convo.secure {
                "Say hello — or drop a file here."
            } else {
                "Messages you write now are delivered when they're reachable."
            };
            if !hint.is_empty() {
                hints.push(Line::from(hint.fg(theme::muted())).centered());
            }
            rows.push((Line::from(""), None));
            for line in hints {
                if line.width() == 0 {
                    rows.push((line, None));
                    continue;
                }
                rows.extend(balanced(line, full.saturating_sub(4)).into_iter().map(|l| (l, None)));
            }
        }

        let height = area.height as usize;
        let total = rows.len();
        let max_scroll = total.saturating_sub(height);
        // Keep the selected message in view.
        if let Some(sel) = selected {
            let first = rows.iter().position(|(_, i)| *i == Some(sel)).unwrap_or(0);
            let last = rows.iter().rposition(|(_, i)| *i == Some(sel)).unwrap_or(0);
            let bottom = total - self.scroll.min(max_scroll);
            let top = bottom.saturating_sub(height);
            if last >= bottom {
                self.scroll = total - (last + 1);
            } else if first < top {
                self.scroll = total - (first + height).min(total);
            }
        }
        self.scroll = self.scroll.min(max_scroll);
        let end = total - self.scroll;
        let start = end.saturating_sub(height);
        // Anchor to the bottom, like every chat.
        let pad = height.saturating_sub(end - start) as u16;
        let inner = Rect::new(area.x + 1, area.y + pad, area.width.saturating_sub(2), area.height - pad);
        let mut visible = Vec::with_capacity(end - start);
        for (k, (line, entry)) in rows[start..end].iter().enumerate() {
            if let Some(i) = entry {
                self.message_rows.push((inner.y + k as u16, *i));
            }
            visible.push(line.clone());
        }
        frame.render_widget(Paragraph::new(visible), inner);

        if self.scroll > 0 && area.height > 0 {
            let label = format!(" ↓ {} more · Esc to jump down ", self.scroll);
            let w = label.width() as u16;
            let at = Rect::new(area.x + area.width.saturating_sub(w + 1), area.y + area.height - 1, w, 1);
            frame.render_widget(Line::from(label.fg(theme::text()).bg(theme::raised())), at);
        }
    }

    fn entry_lines(
        &self,
        convo: &Convo,
        entry: &Entry,
        prev: Option<&Entry>,
        width: usize,
    ) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if let Body::Notice(text) = &entry.body {
            out.push(Line::from(""));
            for row in wrap(text, width.saturating_sub(8)) {
                out.push(Line::from(row.fg(theme::faint()).italic()).centered());
            }
            return out;
        }
        let reminder = matches!(entry.body, Body::Reminder { .. });
        let tint = match &entry.sender {
            _ if reminder => theme::gold(),
            _ if entry.mine => theme::iris(),
            Some(sender) => theme::person(sender),
            None => theme::person(&convo.data.address),
        };
        let gutter = || -> Span<'static> { "▎ ".fg(theme::dim(tint)) };
        let continues = !reminder && prev.is_some_and(|p| {
            p.mine == entry.mine
                && p.sender == entry.sender
                && !matches!(p.body, Body::Notice(_) | Body::Reminder { .. })
                && entry.at_ms - p.at_ms < GROUP_GAP_MS
                && same_day(p.at_ms, entry.at_ms)
        });
        if !continues {
            if prev.is_some() {
                out.push(Line::from(""));
            }
            let name = match &entry.sender {
                _ if reminder => "⏰ reminder".to_string(),
                _ if entry.mine => "you".to_string(),
                Some(sender) => self.nick_of(sender),
                None => convo.data.nick.clone(),
            };
            let name = clip(&name, NAME_ROOM.min(width.saturating_sub(10)));
            let mut header = vec!["  ".into(), name.fg(tint).bold(), "  ".into(), clock(entry.at_ms).fg(theme::faint())];
            if !entry.mine && entry.hops > 1 {
                header.push(format!("  via {} hops", entry.hops).fg(theme::faint()));
            }
            out.push(Line::from(header));
        }

        if let Some(q) = &entry.reply {
            let who = match () {
                _ if q.mine => "you".to_string(),
                _ if convo.data.group => "reply".to_string(),
                _ => clip(&convo.data.nick, NAME_ROOM),
            };
            let text = clip(&q.text.replace('\n', " "), width.saturating_sub(who.width() + 8));
            out.push(Line::from(vec![
                gutter(),
                "↳ ".fg(theme::faint()),
                who.fg(theme::muted()).bold(),
                format!(" {text}").fg(theme::muted()),
            ]));
        }

        // Lobby messages go to whoever is around and Me's go nowhere; there
        // is no one receipt to show.
        let tick = (entry.mine && convo.data.address != LOBBY && convo.data.address != ME)
            .then(|| tick(&entry.status));
        match &entry.body {
            Body::Text(text) => {
                let rows = wrap(text, width.saturating_sub(INDENT.len()));
                let n = rows.len();
                for (i, row) in rows.into_iter().enumerate() {
                    let mut spans = vec![gutter()];
                    spans.extend(rich(&row));
                    if i + 1 == n
                        && let Some(tick) = &tick
                    {
                        let used = INDENT.len() + row.width();
                        let need = tick.content.width() + 2;
                        if used + need <= width {
                            spans.push(" ".repeat(width - used - need + 2).into());
                            spans.push(tick.clone());
                        } else {
                            out.push(Line::from(spans));
                            let pad = width.saturating_sub(tick.content.width());
                            out.push(Line::from(vec![" ".repeat(pad).into(), tick.clone()]));
                            continue;
                        }
                    }
                    out.push(Line::from(spans));
                }
            }
            Body::File {
                name,
                size,
                path,
                transfer,
                ..
            } => {
                let icon = file_icon(name);
                let size_text = human_size(*size);
                let name_room = width.saturating_sub(size_text.width() + 10);
                out.push(Line::from(vec![
                    gutter(),
                    format!(" {icon} ").bg(theme::raised()),
                    clip_middle(name, name_room).fg(theme::text()).bg(theme::raised()),
                    format!("  {size_text} ").fg(theme::muted()).bg(theme::raised()),
                ]));
                // Lined up under the icon. The first piece always shows; the
                // hints after it only when there's room.
                let mut detail = vec![gutter(), " ".into()];
                let mut hint: Option<Span<'static>> = None;
                match transfer {
                    Transfer::Queued => {
                        let why = if convo.online && convo.secure {
                            "starting…"
                        } else {
                            "waiting for them to come online"
                        };
                        detail.push(format!("◷ {why}").fg(theme::muted()));
                    }
                    Transfer::Moving { done, total } => {
                        let fraction = if *total == 0 { 0.0 } else { *done as f64 / *total as f64 };
                        let bar_width = (width.saturating_sub(30)).clamp(10, 30);
                        detail.extend(progress_bar(fraction, bar_width));
                        detail.push(format!(" {:>3}%", (fraction * 100.0) as u32).fg(theme::text()).bold());
                        if let Some(rate) = self.rate(&entry.id, *size, fraction) {
                            detail.push(format!("  {rate}").fg(theme::muted()));
                        }
                    }
                    Transfer::Done if entry.mine => {
                        detail.push("sent".fg(theme::muted()));
                        if let Some(tick) = &tick {
                            detail.push("  ".into());
                            detail.push(tick.clone());
                        }
                    }
                    Transfer::Done => {
                        let saved = clip_start(&picker::tilde(path), width.saturating_sub(14));
                        detail.push("saved to ".fg(theme::muted()));
                        detail.push(saved.fg(theme::muted()).underlined());
                        hint = Some("  click, then o to open".fg(theme::faint()));
                    }
                    Transfer::Failed(reason) => {
                        detail.push(format!("✗ {}", clip(reason, width.saturating_sub(6))).fg(err()));
                        if entry.mine {
                            hint = Some("  click, then R to retry".fg(theme::faint()));
                        }
                    }
                    Transfer::Interrupted => {
                        detail.push("interrupted when hopchat closed".fg(warn()));
                        hint = Some("  click, then R to retry".fg(theme::faint()));
                    }
                }
                let used: usize = detail.iter().map(Span::width).sum();
                if let Some(hint) = hint.filter(|h| used + h.width() <= width) {
                    detail.push(hint);
                }
                out.push(Line::from(detail));
            }
            Body::Reminder { task, text, from } => {
                for row in wrap(text, width.saturating_sub(6)) {
                    out.push(Line::from(vec![gutter(), format!(" {row} ").fg(theme::text()).bg(theme::raised()).bold()]));
                }
                // What has happened to it since, from the list itself.
                let now = theme::now_ms();
                let state = match self.tasks.get(task) {
                    None => "deleted".fg(theme::faint()),
                    Some(t) if t.done() => "✓ done".fg(ok()),
                    Some(t) if t.due() => "due".fg(theme::gold()).bold(),
                    Some(t) => match (t.repeat, t.due_ms) {
                        (Some(r), Some(at)) => format!("↻ {} · next {}", r.describe(), tasks::when_label(at, Local::now())).fg(theme::muted()),
                        (None, Some(at)) => format!("snoozed until {}", clock(at)).fg(theme::muted()),
                        _ => "on your todo list".fg(theme::muted()),
                    },
                };
                let mut detail = vec![gutter(), " ".into(), state];
                if let Some(from) = from {
                    detail.push(format!("  ·  set in {}", clip(from, NAME_ROOM)).fg(theme::faint()));
                }
                let hint = match self.tasks.get(task) {
                    Some(t) if t.due() => "  ·  click, then x done · s snooze",
                    Some(t) if !t.done() && t.due_ms.is_some_and(|at| at > now) => "  ·  Ctrl-T to change it",
                    _ => "",
                };
                let used: usize = detail.iter().map(Span::width).sum();
                if used + hint.width() <= width {
                    detail.push(hint.fg(theme::faint()));
                }
                out.push(Line::from(detail));
            }
            Body::Notice(_) => {}
        }
        out
    }

    fn rate(&self, id: &str, size: u64, fraction: f64) -> Option<String> {
        let started = self.started.get(id)?;
        let secs = started.elapsed().as_secs_f64();
        if secs < 0.5 || fraction <= 0.0 {
            return None;
        }
        let rate = size as f64 * fraction / secs;
        let left = (size as f64 * (1.0 - fraction) / rate).ceil() as u64;
        Some(format!("{}/s · {}s left", human_size(rate as u64), left))
    }

    fn composer_height(&self, width: usize) -> u16 {
        let (rows, _) = self.editor.layout(width.max(10));
        let extra = !self.attachments.is_empty() as usize + self.reply.is_some() as usize;
        (rows.len().clamp(1, 8) + extra + 2) as u16
    }

    fn draw_composer(&self, frame: &mut Frame, area: Rect) {
        let focused = matches!(self.mode, Mode::Normal);
        let edge = if focused { theme::dim(theme::iris()) } else { theme::mix(theme::faint(), theme::bg(), 0.3) };
        // The name gets what the corners, " to " and the send hint leave.
        let send_hint = Line::from(vec![" enter ".fg(theme::muted()), "to send ".fg(theme::faint())]);
        let name_room = (area.width as usize).saturating_sub(2 + 4 + 1 + send_hint.width() + 2);
        let to = match self.current() {
            Some(c) if c.data.address == LOBBY => vec!["everyone nearby".fg(theme::foam())],
            Some(c) if c.data.address == ME => vec!["yourself".fg(theme::iris())],
            Some(c) if c.data.group => vec![clip(&format!("#{}", c.data.nick), name_room).fg(theme::rose())],
            Some(c) => vec![clip(&c.data.nick, name_room).fg(theme::person(&c.data.address))],
            None => vec!["nobody yet".fg(theme::muted())],
        };
        let mut title = vec![" to ".fg(theme::faint())];
        title.extend(to);
        title.push(" ".into());
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(edge))
            .style(Style::new().bg(theme::panel()))
            .padding(Padding::horizontal(1))
            .title(Line::from(title));
        if name_room > 0 {
            block = block.title(send_hint.alignment(Alignment::Right));
        }
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();
        if let Some(target) = self.reply_target() {
            let who = if target.mine {
                "yourself".to_string()
            } else {
                self.current().map(|c| clip(&c.data.nick, NAME_ROOM)).unwrap_or_default()
            };
            // The quote gives way first, then the hint to cancel.
            let room = inner.width as usize;
            let cancel = "  Esc cancels";
            let fixed = "↳ replying to ".width() + who.width() + 2;
            let cancel = if fixed + 8 + cancel.width() <= room { cancel } else { "" };
            let text = clip(&entry_summary(target).replace('\n', " "), room.saturating_sub(fixed + cancel.width()));
            lines.push(Line::from(vec![
                "↳ replying to ".fg(theme::faint()),
                who.fg(theme::iris()),
                format!("  {text}").fg(theme::muted()),
                cancel.fg(theme::faint()),
            ]));
        }
        if !self.attachments.is_empty() {
            // As many chips as fit on the one row; the rest are counted.
            let room = inner.width as usize;
            let removes = "⌫ removes";
            let mut spans = Vec::new();
            let mut used = 0;
            for (i, path) in self.attachments.iter().enumerate() {
                let name = super::file_name(path);
                let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                let chip = format!(" {} {} · {} ", file_icon(&name), clip_middle(&name, 24), human_size(size));
                let left = self.attachments.len() - i - 1;
                let more = if left > 0 { format!("+{left} more  ").width() } else { 0 };
                if i > 0 && used + chip.width() + 1 + more.max(removes.width()) > room {
                    spans.push(format!("+{} more  ", left + 1).fg(theme::muted()));
                    break;
                }
                used += chip.width() + 1;
                spans.push(chip.fg(theme::text()).bg(theme::raised()));
                spans.push(" ".into());
            }
            let used: usize = spans.iter().map(Span::width).sum();
            if used + removes.width() <= room {
                spans.push(removes.fg(theme::faint()));
            }
            lines.push(Line::from(spans));
        }

        let text_width = inner.width.saturating_sub(2) as usize;
        let (rows, (cx, cy)) = self.editor.layout(text_width);
        let first_row = cy.saturating_sub(7);
        let top = lines.len() as u16;
        let prompt = || "› ".fg(if focused { theme::iris() } else { theme::faint() });
        if self.editor.is_empty() {
            let hint = match self.current() {
                _ if !self.attachments.is_empty() => "add a message, or just press Enter to send".to_string(),
                Some(c) if c.data.address == LOBBY => {
                    "say something to everyone nearby  ·  @ai asks the model  ·  / commands".to_string()
                }
                Some(c) if c.data.address == ME => {
                    "a note to yourself  ·  @home your devices  ·  @ai your model".to_string()
                }
                Some(c) if c.data.group => {
                    format!("message #{}  ·  @home @ai  ·  / commands", clip(&c.data.nick, NAME_ROOM))
                }
                Some(c) => format!("message {}  ·  Ctrl-O attach a file  ·  / commands", clip(&c.data.nick, NAME_ROOM)),
                None => "waiting for someone to talk to  ·  F1 help".to_string(),
            };
            // Whole hints only: the tail ones drop off when it's narrow.
            let mut parts = hint.split("  ·  ");
            let first = clip(parts.next().unwrap_or_default(), text_width).fg(theme::faint());
            let rest: Vec<Vec<Span>> = parts.map(|p| vec![format!("  ·  {p}").fg(theme::faint())]).collect();
            let mut spans = vec![prompt(), first];
            let used: usize = spans.iter().map(Span::width).sum();
            spans.extend(fitting(rest, (inner.width as usize).saturating_sub(used)));
            lines.push(Line::from(spans));
        } else {
            for (i, row) in rows.iter().enumerate().skip(first_row).take(8) {
                let lead = if i == 0 { prompt() } else { "  ".into() };
                let mut spans = vec![lead];
                if self.editor.text().starts_with('/') && i == 0 {
                    spans.push(Span::raw(row.clone()).fg(theme::iris()));
                } else {
                    spans.extend(rich(row));
                }
                lines.push(Line::from(spans));
            }
        }
        frame.render_widget(Paragraph::new(lines), inner);
        if focused {
            frame.set_cursor_position(Position::new(
                inner.x + 2 + cx as u16,
                inner.y + top + (cy - first_row) as u16,
            ));
        }

        // Suggestions float above the composer: commands, or who to call on.
        if !focused {
            return;
        }
        let (matches, title) = if self.editor.text().starts_with('/') {
            (command_matches(self.editor.text()), " Tab completes ".to_string())
        } else if self.in_group() && self.editor.at_end() {
            let who = if self.in_me() { "yours answer" } else { "the host's model and devices answer" };
            (mention_matches(self.editor.text()), format!(" Tab completes · {who} "))
        } else {
            (Vec::new(), String::new())
        };
        if !matches.is_empty() {
            let h = matches.len() as u16 + 2;
            let w = area.width.min(72);
            let popup = Rect::new(area.x, area.y.saturating_sub(h), w, h);
            let help_width = (w as usize).saturating_sub(2 + 13 + 1);
            let lines: Vec<Line> = matches
                .iter()
                .enumerate()
                .map(|(i, (name, help))| {
                    let line = Line::from(vec![
                        format!(" {name:<12}").fg(theme::iris()),
                        clip(help, help_width).fg(theme::muted()),
                    ]);
                    if i == 0 { line.style(Style::new().bg(theme::raised())) } else { line }
                })
                .collect();
            let inner = float(frame, popup, panel_block("").title(title.fg(theme::faint())));
            frame.render_widget(Paragraph::new(lines), inner);
        }
    }

    fn draw_log(&self, frame: &mut Frame, area: Rect) {
        let block = Block::new()
            .borders(Borders::LEFT)
            .border_style(Style::new().fg(theme::line()))
            .style(Style::new().bg(theme::panel()));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        // A header like the chat's, on the same line.
        let [title, rule, inner] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)]).areas(inner);
        frame.render_widget(Line::from(vec![" protocol".fg(theme::muted()), "  events as they happen".fg(theme::faint())]), title);
        frame.render_widget(theme::rule(rule.width as usize), rule);
        junction(frame, area.x, rule.y, "┼");
        let height = inner.height as usize;
        let skip = self.log.len().saturating_sub(height);
        let lines: Vec<Line> = self.log.iter().skip(skip).cloned().collect();
        let inner = Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), inner.height);
        frame.render_widget(Paragraph::new(lines), inner);
    }

    /// The keys that work right now, as buttons: each can be clicked too.
    fn draw_footer(&mut self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Block::new().style(Style::new().bg(theme::panel())), area);
        let ctrl = |c: char| Some(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
        let press = |code: KeyCode| Some(KeyEvent::from(code));
        let on_reminder = match self.mode {
            Mode::Select(i) => self
                .current()
                .and_then(|c| c.data.entries.get(i))
                .is_some_and(|e| matches!(e.body, Body::Reminder { .. })),
            _ => false,
        };
        let keys: Vec<(&str, &str, Option<KeyEvent>)> = match self.mode {
            Mode::Select(_) if on_reminder => vec![
                ("↑↓", "move", None),
                ("x", "done", press(KeyCode::Char('x'))),
                ("s", "snooze 10m", press(KeyCode::Char('s'))),
                ("o", "go to chat", press(KeyCode::Char('o'))),
                ("t", "in the list", press(KeyCode::Char('t'))),
                ("y", "copy", press(KeyCode::Char('y'))),
                ("Esc", "back", press(KeyCode::Esc)),
            ],
            Mode::Select(_) => vec![
                ("↑↓", "move", None),
                ("r", "reply", press(KeyCode::Char('r'))),
                ("o", "open", press(KeyCode::Char('o'))),
                ("f", "show in folder", press(KeyCode::Char('f'))),
                ("y", "copy", press(KeyCode::Char('y'))),
                ("t", "todo", press(KeyCode::Char('t'))),
                ("R", "retry", press(KeyCode::Char('R'))),
                ("Esc", "back", press(KeyCode::Esc)),
            ],
            Mode::Picker(_) => vec![
                ("↑↓", "move", None),
                ("Enter", "open/choose", press(KeyCode::Enter)),
                ("Space", "mark", press(KeyCode::Char(' '))),
                ("←", "up a folder", press(KeyCode::Left)),
                ("~", "home", press(KeyCode::Char('~'))),
                ("type", "filter", None),
                ("Esc", "cancel", press(KeyCode::Esc)),
            ],
            Mode::Help(_) => vec![("↑↓", "scroll", None), ("any key", "close", press(KeyCode::Esc))],
            Mode::Devices(ref panel) if panel.renaming.is_some() => vec![
                ("type", "a name", None),
                ("Enter", "save", press(KeyCode::Enter)),
                ("Esc", "cancel", press(KeyCode::Esc)),
            ],
            Mode::Groups(ref panel) if panel.focus == Focus::Name => vec![
                ("type", "a name", None),
                ("↓", "pick people", press(KeyCode::Down)),
                ("Enter", "create", press(KeyCode::Enter)),
                ("Esc", "cancel", press(KeyCode::Esc)),
            ],
            Mode::Groups(ref panel) => vec![
                ("↑↓", "move", None),
                ("Space", "pick", press(KeyCode::Char(' '))),
                ("Enter", if panel.target.is_some() { "add them" } else { "create" }, press(KeyCode::Enter)),
                ("Esc", "cancel", press(KeyCode::Esc)),
            ],
            Mode::Tasks(ref panel) if panel.focus == task_panel::Focus::Input => vec![
                ("type", "a todo, or when", None),
                ("Enter", if panel.editing.is_some() { "save" } else { "add" }, press(KeyCode::Enter)),
                ("↓", "the list", press(KeyCode::Down)),
                ("Esc", if panel.editing.is_some() { "stop editing" } else { "close" }, press(KeyCode::Esc)),
            ],
            Mode::Tasks(_) => vec![
                ("↑↓", "move", None),
                ("Space", "done", press(KeyCode::Char(' '))),
                ("s", "snooze", press(KeyCode::Char('s'))),
                ("Enter", "edit", press(KeyCode::Enter)),
                ("d", "delete", press(KeyCode::Char('d'))),
                ("o", "go to chat", press(KeyCode::Char('o'))),
                ("Esc", "close", press(KeyCode::Esc)),
            ],
            Mode::Devices(_) => vec![
                ("↑↓", "move", None),
                ("Esc", "close", press(KeyCode::Esc)),
                ("Enter", "add/pair/test", press(KeyCode::Enter)),
                ("n", "rename", press(KeyCode::Char('n'))),
                ("x", "forget", press(KeyCode::Char('x'))),
                ("r", "rescan", press(KeyCode::Char('r'))),
                ("g", "access", press(KeyCode::Char('g'))),
                ("i", "@ai", press(KeyCode::Char('i'))),
            ],
            Mode::Normal => vec![
                ("Enter", "send", press(KeyCode::Enter)),
                ("Ctrl-O", "attach", ctrl('o')),
                ("Ctrl-G", "new group", ctrl('g')),
                ("Ctrl-T", "todos", ctrl('t')),
                ("Ctrl-D", "devices", ctrl('d')),
                ("Tab", "next chat", press(KeyCode::Tab)),
                ("Alt-↑", "select", Some(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT))),
                ("F1", "help", press(KeyCode::F(1))),
            ],
        };
        // Right side: what's happening. It wins the room over the keys.
        let mut right: Vec<Span> = Vec::new();
        if let Some((text, error)) = self.flash_text() {
            let (mark, tone) = if error { ("✗ ", err()) } else { ("✓ ", theme::sage()) };
            right.push(mark.fg(tone));
            // Never more than half the strip: the keys keep the rest.
            let room = 60.min((area.width as usize / 2).saturating_sub(4));
            right.push(clip(text, room).fg(if error { err() } else { theme::text() }));
            right.push("  ".into());
        }
        for incoming in self.incoming.values().take(2) {
            let pct = (incoming.done * 100).checked_div(incoming.total).unwrap_or(0);
            right.push(
                format!("⇣ {} {}% of {}  ", clip_middle(&incoming.name, 18), pct, human_size(incoming.size))
                    .fg(theme::foam()),
            );
        }
        if !self.show_log && right.is_empty() && matches!(self.mode, Mode::Normal) {
            right.push("Ctrl-E log ".fg(theme::faint()));
        }
        let right = Line::from(right).alignment(Alignment::Right);
        let right_width = (right.width() as u16).min(area.width);

        self.footer_hits.clear();
        let mut spans: Vec<Span> = vec![" ".into()];
        let mut x = area.x + 1;
        let room = area.right().saturating_sub(right_width + 1);
        for (key, what, event) in keys {
            let cap = format!(" {key} ");
            let label = format!(" {what}  ");
            let w = (cap.width() + label.width()) as u16;
            if x + w > room {
                break;
            }
            if let Some(event) = event {
                self.footer_hits.push((Rect::new(x, area.y, w - 2, 1), event));
            }
            spans.push(cap.fg(key_color()).bg(theme::raised()));
            spans.push(label.fg(theme::faint()));
            x += w;
        }
        frame.render_widget(Line::from(spans), area);
        let at = Rect::new(area.right() - right_width, area.y, right_width, 1);
        frame.render_widget(right, at);
    }
}

/// A row of message text, with @mentions and links lit up.
fn rich(row: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for word in row.split_inclusive(' ') {
        let bare = word.trim_end();
        let gap = &word[bare.len()..];
        let styled = if bare.len() > 1 && bare.starts_with('@') {
            bare.to_string().fg(theme::gold())
        } else if bare.starts_with("http://") || bare.starts_with("https://") {
            bare.to_string().fg(theme::foam()).underlined()
        } else {
            Span::raw(bare.to_string())
        };
        spans.push(styled);
        if !gap.is_empty() {
            spans.push(Span::raw(gap.to_string()));
        }
    }
    spans
}

/// Three dots that fill up, for anything still going.
fn dots() -> &'static str {
    ["   ", ".  ", ".. ", "..."][(theme::now_ms() / 350).rem_euclid(4) as usize]
}

/// The look of every floating panel: a soft rounded frame.
fn panel_block(title: &str) -> Block<'static> {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::dim(theme::iris())))
        .style(Style::new().bg(theme::panel()).fg(theme::text()));
    if title.is_empty() {
        return block;
    }
    block.title(Line::from(format!(" {title} ").fg(theme::text()).bold()))
}

/// Draws `block` floating over whatever is under `area`, with a shadow,
/// and gives back the space inside it.
fn float(frame: &mut Frame, area: Rect, block: Block) -> Rect {
    let screen = frame.area();
    let buf = frame.buffer_mut();
    let right = area.right();
    let bottom = area.bottom();
    let mut shade = |x: u16, y: u16| {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_bg(theme::shadow());
            cell.set_fg(theme::mix(theme::faint(), theme::shadow(), 0.6));
        }
    };
    if right < screen.right() {
        for y in area.y + 1..=bottom.min(screen.bottom().saturating_sub(1)) {
            shade(right, y);
        }
    }
    if bottom < screen.bottom() {
        for x in area.x + 1..=right.min(screen.right().saturating_sub(1)) {
            shade(x, bottom);
        }
    }
    frame.render_widget(ratatui::widgets::Clear, area);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// Where two dividers meet, at the cell `x`, `y`: `symbol` joins them.
fn junction(frame: &mut Frame, x: u16, y: u16, symbol: &str) {
    if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
        cell.set_symbol(symbol).set_fg(theme::line());
    }
}

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner() -> &'static str {
    let ms = chrono::Utc::now().timestamp_millis();
    SPINNER[(ms / 90) as usize % SPINNER.len()]
}

impl App {
    fn draw_devices(&self, frame: &mut Frame, panel: &Devices) {
        let view = &self.home_view;
        let screen = frame.area();
        let width = (fit_width(screen, 96) as usize).saturating_sub(2);
        let selected = panel.selected.min(view.devices.len().saturating_sub(1));
        let mut lines: Vec<Line> = Vec::new();
        let mut selected_row = 0;
        let added = view.devices.iter().filter(|d| d.added).count();
        for (i, d) in view.devices.iter().enumerate() {
            if i == 0 && d.added {
                lines.push(Line::from(" Your devices".fg(theme::muted()).bold()));
            }
            if i == added {
                if added > 0 {
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(" Found on this network".fg(theme::muted()).bold()));
            }
            if i == selected {
                selected_row = lines.len();
            }
            lines.push(device_line(d, i == selected, panel, width));
        }
        if view.devices.is_empty() {
            lines.push(Line::from(""));
            if view.scanning {
                let wanted: Vec<&str> = home::driver::BACKENDS.iter().map(|b| b.label()).collect();
                lines.push(Line::from(format!("  {} Looking for {} on this network…", spinner(), and_list(&wanted)).fg(theme::muted())));
            } else {
                lines.push(Line::from("  Nothing found.".fg(theme::muted())));
                lines.push(Line::from(""));
                lines.push(Line::from("  Devices have to be on the same network as this machine, and switched on.".fg(theme::faint())));
                lines.push(Line::from("  r scans again · /home add IP adds one by its address".fg(theme::faint())));
            }
        } else if view.scanning && !view.devices.iter().any(|d| !d.added) {
            lines.push(Line::from(""));
            lines.push(Line::from(format!("  {} looking for more…", spinner()).fg(theme::faint())));
        }
        if !view.scanning && !view.devices.iter().any(|d| d.category == home::Category::Tv) {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                "  📺 No TV? ".fg(theme::muted()),
                "A TV only shows up while it's switched on. Turn it on, then ".fg(theme::faint()),
                "r".fg(key_color()).bold(),
                ".".fg(theme::faint()),
            ]));
        }
        // Tall enough for what's listed, up to most of the screen.
        let area = fit(screen, 96, lines.len() as u16 + 3 + 4 + 2);
        let status = if view.scanning {
            format!(" {} scanning ", spinner()).fg(theme::foam())
        } else {
            " r rescans ".fg(theme::muted())
        };
        let block = panel_block("Devices").title(Line::from(status).alignment(Alignment::Right));
        let inner = float(frame, area, block);
        let [list, help, footer] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(4),
        ])
        .areas(inner);
        // Keep the selection in view.
        let height = list.height as usize;
        let skip = (selected_row + 2).saturating_sub(height);
        let lines: Vec<Line> = lines.into_iter().skip(skip).collect();
        frame.render_widget(Paragraph::new(lines), list);

        let current = panel.current(view);
        frame.render_widget(Paragraph::new(device_help(current, panel, width)), help);

        frame.render_widget(Paragraph::new(self.devices_footer(width)), footer);
    }

    fn devices_footer(&self, width: usize) -> Vec<Line<'static>> {
        let view = &self.home_view;
        let rule = Line::from("─".repeat(width).fg(theme::faint()));
        let model = match view.model_state {
            ModelState::Ready => vec!["● ".fg(ok()), clip(&view.model, 32).bold(), " ready".fg(theme::muted())],
            // The command is what matters, and is only any use whole.
            ModelState::Missing => vec![
                "◐ ".fg(warn()),
                "not downloaded · ".fg(warn()),
                format!("ollama pull {}", view.model).fg(accent()),
            ],
            ModelState::NoServer => vec![
                "○ ".fg(err()),
                "Ollama isn't running · start it with ".fg(warn()),
                "ollama serve".fg(accent()),
            ],
            ModelState::Unknown => vec![format!("{} ", spinner()).fg(theme::muted()), "checking…".fg(theme::muted())],
        };
        let mut model_line = vec!["  Model   ".fg(theme::muted())];
        model_line.extend(model);
        // Without a model here, home thinks with a group member's.
        if view.model_state != ModelState::Ready && !view.lenders.is_empty() {
            model_line = vec![
                "  Model   ".fg(theme::muted()),
                "● ".fg(ok()),
                "borrowing ".fg(theme::muted()),
                clip(&view.lenders.join(", "), width.saturating_sub(24)).bold(),
            ];
        } else if view.model_state == ModelState::Ready && view.share {
            model_line.push("  · lent to the group".fg(theme::faint()));
        }
        let access = match self.home_here() {
            None if self.in_me() => vec!["◇ ".fg(theme::iris()), "Me".bold(), ": always yours".fg(theme::muted())],
            Some((id, name, true)) if id == LOBBY => vec![
                "# ".fg(accent()),
                clip(&name, NAME_ROOM).bold(),
                ": everyone nearby can use them".fg(ok()),
                "  · g lets only you".fg(theme::faint()),
            ],
            Some((id, name, false)) if id == LOBBY => vec![
                "# ".fg(accent()),
                clip(&name, NAME_ROOM).bold(),
                ": only you can use them".fg(theme::muted()),
                "  · g lets everyone nearby in".fg(accent()),
            ],
            Some((_, name, true)) => vec![
                "# ".fg(accent()),
                clip(&name, NAME_ROOM).bold(),
                " can use them".fg(ok()),
                "  · g switches off".fg(theme::faint()),
            ],
            Some((_, name, false)) => vec![
                "# ".fg(theme::faint()),
                clip(&name, NAME_ROOM).bold(),
                " can't use them".fg(theme::muted()),
                "  · g lets this group in".fg(accent()),
            ],
            None => {
                let n = view.groups.len();
                vec![
                    format!("on in {n} group{}", if n == 1 { "" } else { "s" }).fg(theme::muted()),
                    "  · open a group, then g to let it in".fg(theme::faint()),
                ]
            }
        };
        let mut access_line = vec!["  Access  ".fg(theme::muted())];
        access_line.extend(access);
        let ai = match self.ai_here() {
            None if self.in_me() => vec!["◇ ".fg(theme::iris()), "Me".bold(), ": always answers you".fg(theme::muted())],
            Some((id, name, true)) if id == LOBBY => vec![
                "# ".fg(accent()),
                clip(&name, NAME_ROOM).bold(),
                ": everyone nearby can ask the model".fg(ok()),
                "  · i lets only you".fg(theme::faint()),
            ],
            Some((id, name, false)) if id == LOBBY => vec![
                "# ".fg(accent()),
                clip(&name, NAME_ROOM).bold(),
                ": only you can ask the model".fg(theme::muted()),
                "  · i lets everyone nearby ask".fg(accent()),
            ],
            Some((_, name, true)) => vec![
                "# ".fg(accent()),
                clip(&name, NAME_ROOM).bold(),
                " can ask the model with @ai".fg(ok()),
                "  · i switches off".fg(theme::faint()),
            ],
            Some((_, name, false)) => vec![
                "# ".fg(theme::faint()),
                clip(&name, NAME_ROOM).bold(),
                " can't ask the model".fg(theme::muted()),
                "  · i lets it ask with @ai".fg(accent()),
            ],
            None => {
                let n = view.ai_groups.len();
                vec![
                    format!("on in {n} group{}", if n == 1 { "" } else { "s" }).fg(theme::muted()),
                    "  · open a group, then i to let it ask the model".fg(theme::faint()),
                ]
            }
        };
        let mut ai_line = vec!["  @ai     ".fg(theme::muted())];
        ai_line.extend(ai);
        vec![
            rule,
            Line::from(squeeze(model_line, width.saturating_sub(1))),
            Line::from(squeeze(access_line, width.saturating_sub(1))),
            Line::from(squeeze(ai_line, width.saturating_sub(1))),
        ]
    }
}

impl App {
    fn draw_groups(&self, frame: &mut Frame, panel: &GroupPanel) {
        let people = self.candidates(panel);
        let name_height = if panel.target.is_some() { 0 } else { 2 };
        // Name, the people (room for a few even when there are none), help.
        let rows = name_height + 1 + people.len().max(3) as u16 + 1 + 3 + 2;
        let area = fit(frame.area(), 72, rows);
        let title = match &panel.target {
            Some((_, name)) => format!("Add people to #{}", clip(name, (area.width as usize).saturating_sub(40))),
            None => "New group".to_string(),
        };
        let picked = people.iter().filter(|p| panel.is_picked(&p.address)).count();
        let count = if picked == 0 {
            " Space picks people ".fg(theme::muted())
        } else {
            format!(" {picked} picked ").fg(theme::bg()).bg(theme::sage()).bold()
        };
        let block = panel_block(&title).title(Line::from(count).alignment(Alignment::Right));
        let inner = float(frame, area, block);
        let [name, list, help] = Layout::vertical([
            Constraint::Length(name_height),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .areas(inner);

        if panel.target.is_none() {
            let naming = panel.focus == Focus::Name;
            // While typing, the end of a long name stays in view.
            let room = (name.width as usize).saturating_sub(12);
            let shown = if naming { clip_start(&panel.name, room) } else { clip(&panel.name, room) };
            let field = if panel.name.is_empty() && !naming {
                "untitled".fg(theme::faint())
            } else if panel.name.is_empty() {
                "e.g. house, weekend trip".fg(theme::faint())
            } else {
                Span::raw(shown.clone()).bold()
            };
            let label = if naming { " Name  › ".fg(accent()).bold() } else { " Name  › ".fg(theme::muted()) };
            frame.render_widget(Line::from(vec![label, "#".fg(accent()), field]), name);
            if naming {
                let x = name.x + 10 + shown.width() as u16;
                frame.set_cursor_position(Position::new(x.min(name.right().saturating_sub(1)), name.y));
            }
        }

        let width = list.width as usize;
        let mut lines = vec![Line::from(" People".fg(theme::muted()).bold())];
        let in_list = panel.focus == Focus::People;
        for (i, person) in people.iter().enumerate() {
            lines.push(candidate_line(person, panel, in_list && i == panel.cursor, width));
        }
        if people.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from("  Nobody nearby yet.".fg(theme::muted())));
            lines.push(Line::from("  You can still create it now and add people later with /invite.".fg(theme::faint())));
        }
        let height = list.height as usize;
        let skip = (panel.cursor + 3).saturating_sub(height);
        let lines: Vec<Line> = lines.into_iter().skip(skip).collect();
        frame.render_widget(Paragraph::new(lines), list);

        let key = |k: &str| k.to_string().fg(key_color()).bold();
        let say = |t: &str| t.to_string().fg(theme::muted());
        let names: Vec<String> = people
            .iter()
            .filter(|p| panel.is_picked(&p.address))
            .map(|p| p.nick.clone())
            .collect();
        let who = if names.is_empty() { "just you".to_string() } else { format!("you, {}", names.join(", ")) };
        let first = match (panel.warning, &panel.target) {
            (Some(warning), _) => Line::from(vec!["  ".into(), warning.fg(warn())]),
            (None, Some(_)) => Line::from(vec!["  ".into(), key("Space"), say(" picks someone · "), key("Enter"), say(" invites them")]),
            (None, None) if panel.focus == Focus::Name => Line::from(vec!["  ".into(), say("Name it, then "), key("↓"), say(" to pick people · "), key("Enter"), say(" creates it")]),
            (None, None) => Line::from(vec!["  ".into(), key("Space"), say(" picks · "), key("↑"), say(" back to the name · "), key("Enter"), say(" creates it")]),
        };
        let second = match &panel.target {
            Some(_) => Line::from(vec!["  ".into(), clip(&format!("Adding: {}", if names.is_empty() { "nobody yet".into() } else { names.join(", ") }), width.saturating_sub(4)).fg(theme::faint())]),
            None => Line::from(vec!["  ".into(), clip(&format!("In it: {who}"), width.saturating_sub(4)).fg(theme::faint())]),
        };
        frame.render_widget(Paragraph::new(vec![Line::from(""), first, second]), help);
    }
}

impl App {
    fn draw_tasks(&self, frame: &mut Frame, panel: &TaskPanel) {
        let list = self.tasks.ordered();
        let sections = {
            let mut seen: Vec<Section> = list.iter().map(|t| Section::of(t)).collect();
            seen.dedup();
            seen.len()
        };
        // The line, what it reads as, a gap, the list with its headings,
        // and the help.
        let rows = 2 + 1 + (list.len() + sections).max(4) as u16 + 3 + 2;
        let area = fit(frame.area(), 80, rows);
        let due = self.tasks.due_count();
        let open = self.tasks.open_count();
        let count = match (due, open) {
            (0, 0) => " nothing to do ".fg(theme::muted()),
            (0, n) => format!(" {n} open ").fg(theme::muted()),
            (d, _) => format!(" {d} due ").fg(theme::bg()).bg(theme::gold()).bold(),
        };
        let block = panel_block("Todos & reminders").title(Line::from(count).alignment(Alignment::Right));
        let inner = float(frame, area, block);
        let [line, gap, body, help] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .areas(inner);
        let _ = gap;
        let [field, reads] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(line);

        // The line to type into.
        let typing = panel.focus == task_panel::Focus::Input;
        let label = if panel.editing.is_some() { " Edit › " } else { " Add  › " };
        let label = if typing { label.fg(accent()).bold() } else { label.fg(theme::muted()) };
        let lead = label.width();
        let room = (field.width as usize).saturating_sub(lead + 1).max(1);
        let text = panel.input.text();
        let (_, (col, _)) = panel.input.layout(usize::MAX / 2);
        // Scrolled so the cursor stays in view.
        let skip = col.saturating_sub(room.saturating_sub(1));
        let shown: String = {
            let mut used = 0;
            let mut out = String::new();
            let mut at = 0;
            for c in text.chars() {
                let w = UnicodeWidthChar::width(c).unwrap_or(0);
                if at < skip {
                    at += w;
                    continue;
                }
                if used + w > room {
                    break;
                }
                out.push(c);
                used += w;
            }
            out
        };
        let value = if text.is_empty() {
            if typing { "buy milk · call mom at 5pm · stretch in 20m".fg(theme::faint()) } else { "".into() }
        } else {
            Span::raw(shown)
        };
        frame.render_widget(Line::from(vec![label, value]), field);
        if typing {
            let x = field.x + (lead + col - skip) as u16;
            frame.set_cursor_position(Position::new(x.min(field.right().saturating_sub(1)), field.y));
        }

        // What it will be, before Enter.
        let pad = " ".repeat(lead);
        let width = reads.width as usize;
        let reading = match panel.warning {
            Some(warning) => Line::from(vec![pad.into(), warning.fg(warn())]),
            None if text.trim().is_empty() => Line::from(vec![
                pad.into(),
                clip("a time makes it a reminder: tomorrow 9am, every weekday at 9:30", width.saturating_sub(lead)).fg(theme::faint()),
            ]),
            None => {
                let draft = panel.draft();
                let now = Local::now();
                let mut spans: Vec<Span> = vec![pad.into()];
                match draft.due {
                    Some(due) if due <= now => spans.push("that time has already passed".fg(warn())),
                    Some(due) => {
                        let ms = due.timestamp_millis();
                        spans.push("◷ reminder ".fg(theme::foam()).bold());
                        spans.push(tasks::when_label(ms, now).fg(theme::text()));
                        spans.push(format!(" · {}", tasks::until(ms, now.timestamp_millis())).fg(theme::muted()));
                        if let Some(r) = draft.repeat {
                            spans.push(format!(" · ↻ {}", r.describe()).fg(theme::muted()));
                        }
                    }
                    None => {
                        spans.push("☐ todo".fg(theme::sage()).bold());
                        spans.push("  add a time to be reminded".fg(theme::faint()));
                    }
                }
                Line::from(fitting(spans.into_iter().map(|s| vec![s]).collect(), width))
            }
        };
        frame.render_widget(reading, reads);

        // The list, under its headings.
        let width = body.width as usize;
        let mut lines: Vec<Line> = Vec::new();
        let mut cursor_row = 0;
        let mut last: Option<Section> = None;
        let in_list = panel.focus == task_panel::Focus::List;
        let cursor = panel.cursor.min(list.len().saturating_sub(1));
        for (i, task) in list.iter().enumerate() {
            let section = Section::of(task);
            if last != Some(section) {
                let n = list.iter().filter(|t| Section::of(t) == section).count();
                let tone = if section == Section::Due { theme::gold() } else { theme::muted() };
                let mut heading = vec![format!(" {}", section.title()).fg(tone).bold()];
                if section == Section::Done {
                    heading.push(format!(" · {n}").fg(theme::faint()));
                }
                lines.push(Line::from(heading));
                last = Some(section);
            }
            if i == cursor {
                cursor_row = lines.len();
            }
            lines.push(task_line(task, in_list && i == cursor, width));
        }
        if list.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from("  Nothing yet. Type above, then Enter.".fg(theme::muted())));
            lines.push(Line::from("  Or press t on any message to keep it here.".fg(theme::faint())));
        }
        let height = body.height as usize;
        let skip = (cursor_row + 2).saturating_sub(height);
        let lines: Vec<Line> = lines.into_iter().skip(skip).collect();
        frame.render_widget(Paragraph::new(lines), body);

        let key = |k: &str| k.to_string().fg(key_color()).bold();
        let say = |t: &str| t.to_string().fg(theme::muted());
        let current = panel.current(&self.tasks);
        let first = match (typing, current) {
            (true, _) if panel.editing.is_some() => Line::from(vec!["  ".into(), key("Enter"), say(" saves · "), key("Esc"), say(" stops editing")]),
            (true, _) => Line::from(vec!["  ".into(), key("Enter"), say(" adds it · "), key("↓"), say(" goes to the list")]),
            (false, Some(t)) if t.due() => Line::from(vec!["  ".into(), key("Space"), say(" done · "), key("s"), say(" snooze 10m · "), key("Enter"), say(" edit")]),
            (false, _) => Line::from(vec!["  ".into(), key("Space"), say(" done · "), key("Enter"), say(" edit · "), key("d"), say(" delete · "), key("u"), say(" undo")]),
        };
        let second = match current.and_then(|t| t.origin.as_ref()) {
            Some(origin) if !typing => Line::from(vec![
                "  ".into(),
                clip(&format!("set in {}", origin.name), width.saturating_sub(20)).fg(theme::faint()),
                "  ".into(),
                key("o"),
                say(" goes there"),
            ]),
            _ if !typing && self.tasks.items.iter().any(Task::done) => Line::from(vec!["  ".into(), key("c"), say(" clears what's done")]),
            _ => Line::from(vec!["  ".into(), "Reminders ring in Me, even from another chat.".fg(theme::faint())]),
        };
        frame.render_widget(Paragraph::new(vec![Line::from(""), first, second]), help);
    }
}

fn task_line(task: &Task, selected: bool, width: usize) -> Line<'static> {
    let marker = if selected { "▎".fg(accent()) } else { " ".into() };
    let now = Local::now();
    let now_ms = now.timestamp_millis();
    let check = match Section::of(task) {
        Section::Done => "[✓]".fg(ok()),
        Section::Due => "[!]".fg(theme::gold()).bold(),
        _ => "[ ]".fg(theme::muted()),
    };
    // On the right: when, and how it repeats; where it came from if room.
    let mut meta: Vec<Span<'static>> = Vec::new();
    match (Section::of(task), task.due_ms) {
        (Section::Due, Some(at)) => meta.push(format!("due {}", tasks::until(at, now_ms)).fg(theme::gold())),
        (Section::Upcoming, Some(at)) => {
            if let Some(r) = task.repeat {
                meta.push(format!("↻ {} · ", r.describe()).fg(theme::faint()));
            }
            meta.push(tasks::when_label(at, now).fg(theme::foam()));
            meta.push(format!(" · {}", tasks::until(at, now_ms)).fg(theme::muted()));
        }
        (Section::Done, _) => {}
        _ => {}
    }
    let meta_width: usize = meta.iter().map(Span::width).sum();
    let origin = task
        .origin
        .as_ref()
        .map(|o| format!("{}  ", clip(&o.name, 14)))
        .unwrap_or_default();
    // Marker, space, check, space before; a margin after.
    let fixed = 6 + meta_width + 2;
    let show_origin = !origin.is_empty() && width >= fixed + 16 + origin.width();
    let room = width.saturating_sub(fixed + if show_origin { origin.width() } else { 0 });
    let text = clip(&task.text, room);
    let pad = room.saturating_sub(text.width());
    let text = match () {
        _ if task.done() => text.fg(theme::faint()).crossed_out(),
        _ if selected => text.fg(theme::text()).bold(),
        _ => text.fg(theme::text()),
    };
    let mut spans = vec![marker, " ".into(), check, " ".into(), text, " ".repeat(pad + 1).into()];
    if show_origin {
        spans.push(origin.fg(theme::faint()));
    }
    spans.extend(meta);
    let line = Line::from(spans);
    if selected { line.style(Style::new().bg(selected_bg())) } else { line }
}

fn candidate_line(person: &Candidate, panel: &GroupPanel, selected: bool, width: usize) -> Line<'static> {
    let marker = if selected { "▎".fg(accent()) } else { " ".into() };
    let pickable = person.pickable();
    let check = match () {
        _ if person.member => "[✓]".fg(theme::faint()),
        _ if panel.is_picked(&person.address) => "[✓]".fg(accent()).bold(),
        _ if pickable => "[ ]".fg(theme::muted()),
        _ => "[ ]".fg(theme::faint()),
    };
    let (dot, state) = match (person.member, person.online, person.secure) {
        (true, _, _) => ("●".fg(theme::faint()), "already in the group".fg(theme::faint())),
        (false, true, true) => ("●".fg(ok()), "online".fg(theme::muted())),
        (false, true, false) => ("◐".fg(warn()), "connecting…".fg(warn())),
        (false, false, _) => ("○".fg(theme::faint()), "offline · can't be added now".fg(theme::faint())),
    };
    let name = clip(&person.nick, 20);
    let pad = 22usize.saturating_sub(name.width());
    // The address tells two people with one nick apart, when there's room.
    let address = short(&person.address);
    let address = if 30 + state.width() + 2 + address.width() < width { address } else { String::new() };
    let state = Span::styled(clip(&state.content, width.saturating_sub(31)), state.style);
    let name = if !pickable {
        name.fg(theme::faint())
    } else if selected {
        name.bold()
    } else {
        name.into()
    };
    let line = Line::from(vec![
        marker,
        " ".into(),
        check,
        " ".into(),
        dot,
        " ".into(),
        name,
        " ".repeat(pad).into(),
        state,
        "  ".into(),
        address.fg(theme::faint()),
    ]);
    if selected { line.style(Style::new().bg(selected_bg())) } else { line }
}

fn device_line(d: &DeviceView, selected: bool, panel: &Devices, width: usize) -> Line<'static> {
    let icon = d.category.icon();
    let (dot, state) = match &d.state {
        State::Ready => ("●".fg(ok()), d.detail.clone().fg(theme::text())),
        State::Offline => ("○".fg(theme::faint()), d.detail.clone().fg(theme::muted())),
        State::Checking => (spinner().fg(theme::muted()), "checking…".fg(theme::muted())),
        State::New => ("✦".fg(accent()), d.detail.clone().fg(theme::muted())),
        State::Pairing { until } => {
            let left = until.saturating_duration_since(std::time::Instant::now()).as_secs();
            (spinner().fg(warn()), format!("waiting for the {} · {left}s", d.category.word()).fg(warn()))
        }
        State::Failed(_) => ("✗".fg(err()), "didn't work".fg(err())),
    };
    let marker = if selected { "▎".fg(accent()) } else { " ".into() };
    let name = match &panel.renaming {
        Some(typed) if selected => Span::from(format!("{}▏", clip_start(typed, 21))).fg(accent()).bold(),
        _ if selected => Span::from(clip(&d.name, 22)).bold(),
        _ => Span::from(clip(&d.name, 22)),
    };
    let ip = d.ip.to_string();
    let used = 6 + name.content.width().max(22) + 2 + 15 + 2;
    let state = Span::styled(clip(&state.content, width.saturating_sub(used + 2)), state.style);
    let name_pad = 22usize.saturating_sub(name.content.width());
    let line = Line::from(vec![
        marker,
        " ".into(),
        format!("{icon} ").into(),
        name,
        " ".repeat(name_pad + 2).into(),
        format!("{ip:<15}").fg(theme::faint()),
        "  ".into(),
        dot,
        " ".into(),
        state,
    ]);
    if selected { line.style(Style::new().bg(selected_bg())) } else { line }
}

/// "a", "a and b", "a, b and c".
fn and_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Two lines under the list saying what the selected device needs.
fn device_help(d: Option<&DeviceView>, panel: &Devices, width: usize) -> Vec<Line<'static>> {
    let key = |k: &str| k.to_string().fg(key_color()).bold();
    let say = |t: &str| t.to_string().fg(theme::muted());
    let Some(d) = d else { return Vec::new() };
    if panel.renaming.is_some() {
        return vec![Line::from(""), Line::from(vec!["  ".into(), say("Type a name people will use, like "), "Desk lamp".bold(), say(". "), key("Enter"), say(" saves.")])];
    }
    if panel.confirm_forget {
        return vec![Line::from(""), Line::from(vec!["  ".into(), format!("Forget {}? ", clip(&d.name, NAME_ROOM)).fg(warn()), key("x"), say(" again to forget, anything else keeps it.")])];
    }
    let lines = match (&d.state, devices::primary(d), d.pairing) {
        (State::Pairing { .. }, _, Some(help)) => vec![
            Line::from(vec!["  ".into(), help.during[0].fg(warn()).bold()]),
            Line::from(vec!["  ".into(), say(help.during[1])]),
        ],
        (State::Failed(why), _, _) => vec![
            Line::from(vec!["  ".into(), clip(why, width.saturating_sub(4)).fg(err())]),
            Line::from(vec!["  ".into(), key("Enter"), say(" tries again")]),
        ],
        (_, Primary::Pair, Some(help)) => vec![
            Line::from(vec!["  ".into(), key("Enter"), say(help.before[0])]),
            Line::from(vec!["  ".into(), say(help.before[1])]),
        ],
        (_, Primary::Add, _) => vec![
            Line::from(vec!["  ".into(), key("Enter"), say(" adds it. "), key("n"), say(" names it first, e.g. Kitchen.")]),
            Line::from(vec!["  ".into(), say(&format!("Not sure which {} this is? Add it, then ", d.category.word())), key("t"), say(&format!("{}.", d.test_help))]),
        ],
        (State::Offline, _, _) => vec![
            Line::from(vec!["  ".into(), say("Not answering. Is it switched on, and on this network?")]),
            Line::from(vec!["  ".into(), key("r"), say(" checks again · "), key("x"), say(" forgets it")]),
        ],
        _ => {
            vec![
                Line::from(vec!["  ".into(), key("Enter"), say(d.test_help), say(" · "), key("n"), say(" renames · "), key("x"), say(" forgets")]),
                Line::from(vec![
                    "  ".into(),
                    say("In a group, people ask for it by name: "),
                    format!("@home {}", d.category.example(&clip(&d.name.to_lowercase(), NAME_ROOM))).fg(theirs()),
                ]),
            ]
        }
    };
    let mut out = vec![Line::from("")];
    out.extend(lines.into_iter().map(|l| Line::from(squeeze(l.spans, width.saturating_sub(1)))));
    out.truncate(3);
    out
}

fn draw_picker(frame: &mut Frame, picker: &Picker) {
    let screen = frame.area();
    let area = fit(screen, 100, screen.height * 4 / 5);
    let block = panel_block("Attach files").title_bottom(
        Line::from(if picker.marked.is_empty() {
            " Space marks several ".to_string()
        } else {
            format!(" {} marked · Enter sends them ", picker.marked.len())
        })
        .fg(theme::muted()),
    );
    let inner = float(frame, area, block);
    let [path, filter, list] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(1),
    ])
    .areas(inner);

    frame.render_widget(
        Line::from(vec![
            " 📁 ".into(),
            clip_start(&picker::tilde(&picker.dir), (path.width as usize).saturating_sub(6)).bold(),
        ]),
        path,
    );
    let filter_line = if picker.filter.is_empty() {
        Line::from(vec![" 🔍 ".into(), "type to filter · . shows hidden files".fg(theme::faint())])
    } else {
        Line::from(vec![" 🔍 ".into(), picker.filter.clone().fg(theme::gold())])
    };
    frame.render_widget(filter_line, filter);

    if let Some(error) = &picker.error {
        frame.render_widget(Line::from(format!("  {error}").fg(err())), list);
        return;
    }
    let visible = picker.visible();
    if visible.is_empty() {
        frame.render_widget(Line::from("  nothing here".fg(theme::muted())), list);
        return;
    }
    let height = list.height as usize;
    let start = picker.selected.saturating_sub(height.saturating_sub(1));
    let width = list.width as usize;
    let lines: Vec<Line> = visible
        .iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(i, entry)| {
            let mark = if picker.is_marked(entry) { "◆ ".fg(theme::sage()) } else { "  ".into() };
            let (icon, name, size) = if entry.dir {
                ("📁".to_string(), format!("{}/", entry.name), String::new())
            } else {
                (file_icon(&entry.name).to_string(), entry.name.clone(), human_size(entry.size))
            };
            let name = if entry.dir { clip(&name, width.saturating_sub(18)) } else { clip_middle(&name, width.saturating_sub(18)) };
            // Mark and icon before, a margin after: the row fills the width.
            let pad = width.saturating_sub(5 + name.width() + size.width() + 1);
            let line = Line::from(vec![
                mark,
                format!("{icon} ").into(),
                if entry.dir { name.fg(theme::foam()).bold() } else { name.into() },
                " ".repeat(pad).into(),
                size.fg(theme::muted()),
                " ".into(),
            ]);
            if i == picker.selected {
                let mut line = line;
                line.spans[0] = if picker.is_marked(entry) { "◆ ".fg(theme::sage()) } else { "▎ ".fg(theme::iris()) };
                line.style(Style::new().bg(selected_bg()))
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list);
}

/// Keys and commands, scrolled `at` rows down. Gives back how far down it
/// can go, for the scroll to stop there.
fn draw_help(frame: &mut Frame, at: usize) -> usize {
    let screen = frame.area();
    let wide = screen.width >= 120;
    let w = if wide { screen.width.saturating_sub(8).min(150) } else { screen.width.saturating_sub(4).min(86) };
    // Clear of the strip at the top and the keys at the bottom, shadow too.
    let h = screen.height.saturating_sub(3).max(10).min(screen.height);
    let area = Rect::new(screen.x + (screen.width - w.min(screen.width)) / 2, screen.y + 1, w.min(screen.width), h);
    let key_width = 19;
    let key = |k: &str, what: &str, width: usize| -> Vec<Line<'static>> {
        wrap(what, width.saturating_sub(key_width + 1))
            .into_iter()
            .enumerate()
            .map(|(i, row)| {
                let k = if i == 0 { format!("  {k:<16} ") } else { " ".repeat(key_width) };
                Line::from(vec![k.fg(key_color()).bold(), row.fg(theme::text())])
            })
            .collect()
    };
    let section = |title: &str, rows: &[(&str, &str)], width: usize| -> Vec<Line<'static>> {
        let mut out = vec![Line::from(vec!["  ".into(), title.to_string().fg(theme::rose()).bold()])];
        for (k, what) in rows {
            out.extend(key(k, what, width));
        }
        out.push(Line::from(""));
        out
    };
    let chatting: &[(&str, &str)] = &[
        ("Enter", "send (with any attached files)"),
        ("↑ · ↓", "what you sent here, to edit or send again"),
        ("Shift/Alt-Enter", "new line (Ctrl-J works everywhere)"),
        ("Tab · Shift-Tab", "next / previous conversation (also Ctrl-N/P)"),
        ("Alt-1 … 9", "jump to a conversation"),
        ("PgUp · PgDn", "scroll history (or the mouse wheel)"),
        ("Esc", "cancel reply · jump to newest"),
    ];
    let files: &[(&str, &str)] = &[
        ("Ctrl-O", "browse for files to attach"),
        ("drag & drop", "drop files on the window to attach them"),
        ("/file PATH", "send a file by path"),
    ];
    let messages: &[(&str, &str)] = &[
        ("r · Enter", "reply"),
        ("o · f", "open a file · show it in its folder"),
        ("y", "copy text to the clipboard"),
        ("R", "retry a failed send"),
        ("t", "make it a todo, or a reminder"),
    ];
    let editing: &[(&str, &str)] = &[
        ("Ctrl-A · Ctrl-E", "start / end of line"),
        ("Ctrl-W · Ctrl-U", "delete word / to start of line"),
        ("Alt-← · Alt-→", "move by word"),
    ];
    let elsewhere: &[(&str, &str)] = &[
        ("Lobby", "everyone nearby, always at the top · @home your devices · @ai your model"),
        ("Me", "just you: notes, and @home and @ai that never leave this machine"),
        ("Ctrl-T", "todos & reminders: type \"buy milk\" or \"call mom at 5pm\", Enter · reminders ring in Me"),
        ("Ctrl-G", "new group: name it, pick people, Enter"),
        ("Ctrl-D", "devices: find, pair and test them"),
        ("Ctrl-E", "protocol event log"),
        ("Ctrl-C · Ctrl-Q", "quit"),
    ];
    let first = |width: usize| -> Vec<Line<'static>> {
        let mut out = vec![Line::from("")];
        out.extend(section("Chatting", chatting, width));
        out.extend(section("Files", files, width));
        out.extend(section("Messages  (click one, or Alt-↑)", messages, width));
        out.extend(section("Editing", editing, width));
        out
    };
    let second = |width: usize, lead: bool| -> Vec<Line<'static>> {
        let mut out = if lead { vec![Line::from("")] } else { Vec::new() };
        out.extend(section("Elsewhere", elsewhere, width));
        out.extend(section("Commands  (type / to see them)", COMMANDS, width));
        out.extend(wrap_line(
            Line::from("  Drag over messages to copy them. To select anything else, hold Shift (Option in iTerm/Terminal).".fg(theme::faint())),
            width,
        ));
        out
    };

    let inner = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), area.height.saturating_sub(2));
    let width = inner.width as usize;
    let intro = wrap_line(
        Line::from(vec![
            "  Everything is clickable too: ".fg(theme::muted()),
            "chats".fg(theme::text()).bold(),
            " on the left, ".fg(theme::muted()),
            "messages".fg(theme::text()).bold(),
            ", and the ".fg(theme::muted()),
            "keys".fg(key_color()).bold(),
            " along the bottom.".fg(theme::muted()),
        ]),
        width.saturating_sub(2),
    );
    let columns: Vec<Vec<Line<'static>>> = if wide {
        let half = (width - 2) / 2;
        let (left, right) = (first(half), second(width - 2 - half, true));
        vec![left, right]
    } else {
        let mut all = first(width);
        all.extend(second(width, false));
        vec![all]
    };
    let height = (inner.height as usize).saturating_sub(intro.len());
    let longest = columns.iter().map(Vec::len).max().unwrap_or(0);
    let max = longest.saturating_sub(height);
    let at = at.min(max);

    let mut block = panel_block("hopchat · help").title_bottom(Line::from(vec![
        " ✓ on the wire · ".fg(theme::muted()),
        "✓✓".fg(theme::sage()),
        " delivered · ".fg(theme::muted()),
        "✓✓".fg(read()),
        " read ".fg(theme::muted()),
    ]));
    if max > 0 {
        let more = if at < max { " ↓ more " } else { " ↑ top " };
        block = block.title_bottom(Line::from(more.fg(key_color())).alignment(Alignment::Right));
    }
    let inner = float(frame, area, block);
    let [top, body] =
        Layout::vertical([Constraint::Length(intro.len() as u16), Constraint::Min(1)]).areas(inner);
    frame.render_widget(Paragraph::new(intro), top);
    let areas: Vec<Rect> = if wide {
        let half = (body.width.saturating_sub(2)) / 2;
        vec![
            Rect::new(body.x, body.y, half, body.height),
            Rect::new(body.x + half + 2, body.y, body.width.saturating_sub(half + 2), body.height),
        ]
    } else {
        vec![body]
    };
    for (lines, rect) in columns.into_iter().zip(areas) {
        let lines: Vec<Line> = lines.into_iter().skip(at).collect();
        frame.render_widget(Paragraph::new(lines), rect);
    }
    max
}

/// What an empty lobby says: who's here, and what you can say to it.
fn lobby_hint(others: bool, open: bool) -> Vec<Line<'static>> {
    let who = if others {
        "Everyone on your network is here. Say hello."
    } else {
        "Just you so far. People on your network appear here by themselves."
    };
    let private = if others {
        Line::from(vec![
            "Click someone on the left".fg(theme::text()).bold(),
            " (or Tab) for a private chat, end-to-end encrypted.".fg(theme::muted()),
        ])
    } else {
        Line::from("")
    };
    vec![
        Line::from(who.fg(theme::muted())).centered(),
        private.centered(),
        Line::from(""),
        Line::from(vec!["@home".fg(theme::gold()).bold(), " turn the lights down".fg(theme::muted())]).centered(),
        Line::from(vec!["@ai".fg(theme::gold()).bold(), " ask your model anything".fg(theme::muted())]).centered(),
        Line::from(""),
        Line::from(
            if open {
                "You've let everyone here use them too (Ctrl-D to change)."
            } else {
                "Only you can use them here until you say otherwise (Ctrl-D)."
            }
            .fg(theme::faint()),
        )
        .centered(),
    ]
}

/// What an empty Me says: what it is, and what you can say to it.
fn me_hint(view: &home::View) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from("◇".fg(theme::iris())).centered(),
        Line::from(""),
        Line::from("A space that's only yours.".fg(theme::text())).centered(),
        Line::from("Jot things down, talk to your devices, ask your model.".fg(theme::muted())).centered(),
        Line::from("Nothing written here leaves this machine.".fg(theme::faint())).centered(),
        Line::from(""),
    ];
    let names: Vec<&str> = view.devices.iter().filter(|d| d.added).map(|d| d.name.as_str()).collect();
    let example = match view.devices.iter().find(|d| d.added) {
        Some(d) if d.category == home::Category::Tv => format!(" turn the {} down a bit", d.name.to_lowercase()),
        Some(d) => format!(" dim the {} to 30%", d.name.to_lowercase()),
        None => " turn the lights down".to_string(),
    };
    lines.push(Line::from(vec!["@home".fg(theme::gold()), example.fg(theme::muted())]).centered());
    lines.push(Line::from(vec!["@ai".fg(theme::gold()), " what should I cook tonight?".fg(theme::muted())]).centered());
    lines.push(Line::from(vec!["/remind".fg(theme::gold()), " stretch in 20m".fg(theme::muted()), "  ·  reminders ring here".fg(theme::faint())]).centered());
    lines.push(Line::from(""));
    lines.push(
        if names.is_empty() {
            Line::from(vec!["No devices yet: ".fg(theme::faint()), "Ctrl-D".fg(key_color()), " finds them.".fg(theme::faint())])
        } else {
            Line::from(format!("Your devices: {}", names.join(", ")).fg(theme::faint()))
        }
        .centered(),
    );
    lines
}

/// How wide a panel of at most `max` columns can be on `screen`.
fn fit_width(screen: Rect, max: u16) -> u16 {
    max.min(screen.width.saturating_sub(4)).max(40).min(screen.width)
}

/// A panel up to `max` wide and `rows` tall, centred, never past the
/// edges.
fn fit(screen: Rect, max: u16, rows: u16) -> Rect {
    let w = fit_width(screen, max);
    let h = rows.min(screen.height.saturating_sub(4)).max(10).min(screen.height);
    Rect::new(screen.x + (screen.width - w) / 2, screen.y + (screen.height - h) / 2, w, h)
}

fn progress_bar(fraction: f64, width: usize) -> Vec<Span<'static>> {
    let filled = ((fraction.clamp(0.0, 1.0)) * width as f64).round() as usize;
    vec!["━".repeat(filled).fg(theme::iris()), "─".repeat(width - filled).fg(theme::faint())]
}

fn highlight_line(line: Line<'static>, width: usize) -> Line<'static> {
    let used = line.width();
    let mut line = line;
    if used < width {
        line.spans.push(" ".repeat(width - used).into());
    }
    line.style(Style::new().bg(selected_bg()))
}

fn tick(status: &Status) -> Span<'static> {
    match status {
        Status::Queued => "◷".fg(theme::faint()),
        Status::Waiting(_) => "◷ waiting".fg(theme::faint()),
        Status::Retrying(n) => format!("↻ retry {n}").fg(warn()),
        Status::Sent => "✓".fg(theme::muted()),
        Status::Delivered { hops, ms } if *hops > 1 => format!("✓✓ {hops} hops").fg(ok()),
        Status::Delivered { ms, .. } if *ms > 0 => format!("✓✓ {ms}ms").fg(ok()),
        Status::Delivered { .. } => "✓✓".fg(ok()),
        Status::Read => "✓✓ read".fg(read()),
        Status::Failed(reason) => format!("✗ {}", clip(reason, 30)).fg(err()),
    }
}

/// A one-line description of an entry, for previews and quotes.
pub fn entry_summary(entry: &Entry) -> String {
    match &entry.body {
        Body::Text(text) => text.clone(),
        Body::File { name, .. } => format!("{} {name}", file_icon(name)),
        Body::Notice(text) => text.clone(),
        Body::Reminder { text, .. } => format!("⏰ {text}"),
    }
}

fn file_icon(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "bmp" | "svg" => "🖼",
        "mp4" | "mov" | "mkv" | "webm" | "avi" => "🎬",
        "mp3" | "m4a" | "wav" | "flac" | "ogg" | "opus" | "aac" => "🎵",
        "zip" | "tar" | "gz" | "tgz" | "xz" | "7z" | "rar" | "zst" => "📦",
        "pdf" => "📕",
        "rs" | "py" | "js" | "ts" | "go" | "c" | "h" | "cpp" | "java" | "sh" | "toml" | "json"
        | "yaml" | "yml" => "📜",
        _ => "📄",
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

fn local(ms: i64) -> DateTime<Local> {
    Local.timestamp_millis_opt(ms).single().unwrap_or_else(Local::now)
}

pub fn clock(ms: i64) -> String {
    local(ms).format("%H:%M").to_string()
}

fn same_day(a: i64, b: i64) -> bool {
    local(a).date_naive() == local(b).date_naive()
}

fn day_separator(ms: i64, width: usize) -> Line<'static> {
    let day = local(ms).date_naive();
    let today = Local::now().date_naive();
    let label = if day == today {
        "Today".to_string()
    } else if today.pred_opt() == Some(day) {
        "Yesterday".to_string()
    } else if day.year() == today.year() {
        local(ms).format("%a %-d %b").to_string()
    } else {
        local(ms).format("%-d %b %Y").to_string()
    };
    let label = format!(" {label} ");
    let label_width = label.width();
    let side = width.saturating_sub(label_width) / 2;
    Line::from(vec![
        "─".repeat(side).fg(theme::line()),
        label.fg(theme::muted()),
        "─".repeat(width.saturating_sub(side + label_width)).fg(theme::line()),
    ])
}

/// Soft-wraps on spaces where it can, hard-wraps words wider than a row.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let mut rows = Vec::new();
    for paragraph in text.split('\n') {
        let mut row = String::new();
        let mut row_width = 0;
        for word in paragraph.split_inclusive(' ') {
            let w = word.width();
            if row_width + w.saturating_sub(word.ends_with(' ') as usize) > width && !row.is_empty() {
                rows.push(row.trim_end().to_string());
                row = String::new();
                row_width = 0;
            }
            let bare = word.trim_end_matches(' ');
            if bare.width() > width {
                for c in bare.chars() {
                    let cw = c.width().unwrap_or(0);
                    if row_width + cw > width {
                        rows.push(std::mem::take(&mut row));
                        row_width = 0;
                    }
                    row.push(c);
                    row_width += cw;
                }
                // The space after it never starts a row.
                if bare.len() < word.len() && row_width < width {
                    row.push(' ');
                    row_width += 1;
                }
            } else {
                row.push_str(word);
                row_width += w;
            }
        }
        rows.push(row.trim_end().to_string());
    }
    rows
}

/// Shortens to `max` columns with an ellipsis.
pub fn clip(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in text.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Shortens to `max` columns by cutting the middle, so both ends stay:
/// a file keeps its extension, `report-final-…-v2.pdf`.
pub fn clip_middle(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let room = max.saturating_sub(1);
    let tail_room = room / 3;
    let head = clip(text, room - tail_room + 1);
    let head = head.trim_end_matches('…');
    let mut tail: Vec<char> = Vec::new();
    let mut w = 0;
    for c in text.chars().rev() {
        let cw = c.width().unwrap_or(0);
        if w + cw > room.saturating_sub(head.width()) {
            break;
        }
        tail.push(c);
        w += cw;
    }
    tail.reverse();
    format!("{head}…{}", tail.into_iter().collect::<String>())
}

/// Shortens to `max` columns by dropping the start: for paths, where the
/// end is where you are.
pub fn clip_start(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut w = 0;
    for c in text.chars().rev() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        tail.push(c);
        w += cw;
    }
    tail.reverse();
    format!("…{}", tail.into_iter().collect::<String>())
}

/// A name someone else chose, made safe to lay out: one line, no control
/// characters, no runs of spaces, at most `max` characters. `None` when
/// nothing is left.
pub fn display_name(raw: &str, max: usize) -> Option<String> {
    let name: String = raw
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect();
    let name = name.trim_end().to_string();
    (!name.is_empty()).then_some(name)
}

/// Styled spans re-flowed into rows at most `width` wide, breaking between
/// words; a row's leading indent is kept, the spaces at a break are not.
fn wrap_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(4);
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0;
    let trim = |row: &mut Vec<Span<'static>>| {
        while let Some(last) = row.last_mut() {
            let kept = last.content.trim_end().to_string();
            if kept.is_empty() {
                row.pop();
            } else {
                last.content = kept.into();
                break;
            }
        }
    };
    for span in spans {
        for word in span.content.split_inclusive(' ') {
            let bare = word.trim_end().width();
            if used > 0 && used + bare > width {
                trim(rows.last_mut().unwrap());
                rows.push(Vec::new());
                used = 0;
            }
            if used == 0 && rows.len() > 1 && bare == 0 {
                continue;
            }
            if bare > width {
                for c in word.trim_end().chars() {
                    let cw = c.width().unwrap_or(0);
                    if used + cw > width {
                        rows.push(Vec::new());
                        used = 0;
                    }
                    rows.last_mut().unwrap().push(Span::styled(c.to_string(), span.style));
                    used += cw;
                }
                continue;
            }
            rows.last_mut().unwrap().push(Span::styled(word.to_string(), span.style));
            used += word.width();
        }
    }
    for row in &mut rows {
        trim(row);
    }
    rows
}

/// A line wrapped to `width`, each row keeping its alignment, its style
/// and the first row's indent.
fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let (alignment, style) = (line.alignment, line.style);
    // Take the indent off, wrap what's left, and put it back on every row.
    let mut spans = line.spans;
    let mut indent = 0;
    while let Some(first) = spans.first_mut() {
        let rest = first.content.trim_start_matches(' ');
        indent += first.content.len() - rest.len();
        if rest.is_empty() {
            spans.remove(0);
        } else {
            first.content = rest.to_string().into();
            break;
        }
    }
    let mut rows = wrap_spans(spans, width.saturating_sub(indent));
    if indent > 0 {
        for row in &mut rows {
            row.insert(0, " ".repeat(indent).into());
        }
    }
    rows.into_iter()
        .map(|spans| {
            let mut row = Line::from(spans).style(style);
            row.alignment = alignment;
            row
        })
        .collect()
}

/// A centred line wrapped into rows of even length: as few rows as at
/// `width`, but no lone word left on the last.
fn balanced(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let rows = wrap_line(line.clone(), width).len();
    if rows < 2 {
        return wrap_line(line, width);
    }
    let narrowest = (width / 2..width)
        .find(|&w| wrap_line(line.clone(), w).len() == rows)
        .unwrap_or(width);
    wrap_line(line, narrowest)
}

/// Spans cut to `width`: the trailing hints (those that start "  ·")
/// go first, then the end of what's left is clipped.
fn squeeze(mut spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let total = |s: &[Span]| s.iter().map(Span::width).sum::<usize>();
    while total(&spans) > width
        && spans.len() > 1
        && spans.last().is_some_and(|s| s.content.starts_with("  ·"))
    {
        spans.pop();
    }
    while total(&spans) > width {
        let before = total(&spans[..spans.len() - 1]);
        if before + 2 > width && spans.len() > 1 {
            spans.pop();
            continue;
        }
        let last = spans.last_mut().unwrap();
        last.content = clip(&last.content, width - before).into();
        break;
    }
    spans
}

/// As many of `segments`, in order, as fit in `room` columns: the first
/// that doesn't fit and everything after it are left out.
fn fitting(segments: Vec<Vec<Span<'static>>>, room: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for segment in segments {
        let w: usize = segment.iter().map(Span::width).sum();
        if used + w > room {
            break;
        }
        used += w;
        out.extend(segment);
    }
    out
}

/// `off1qz8…k3d` style: enough to tell addresses apart at a glance.
pub fn short(address: &str) -> String {
    let n = address.chars().count();
    if n <= 12 {
        return address.to_string();
    }
    let head: String = address.chars().take(7).collect();
    let tail: String = address.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

pub fn log_line(at: &str, color: Color, text: String) -> Line<'static> {
    Line::from(vec![
        format!("{at} ").fg(theme::faint()),
        Span::styled(text, Style::new().fg(color)),
    ])
}

/// One protocol-log line per event worth watching; the rest are noise here.
pub fn describe(event: &Event, nick: impl Fn(&str) -> String) -> Option<Line<'static>> {
    let (color, text) = match event {
        Event::MessageSent { message_id, .. } => (Color::Gray, format!("sent       {}", id8(message_id))),
        Event::MessageReceived {
            sender,
            hop_count,
            transport,
            encrypted,
            ..
        } => (
            theirs(),
            format!(
                "recv       {} {}hop {transport}{}",
                nick(sender),
                hop_count,
                if *encrypted { " 🔒" } else { "" }
            ),
        ),
        Event::MessageDelivered {
            message_id,
            latency_ms,
            ..
        } => (ok(), format!("ack        {} {latency_ms}ms", id8(message_id))),
        Event::MessageDeferred { message_id, reason, .. } => {
            (theme::muted(), format!("deferred   {} {reason}", id8(message_id)))
        }
        Event::MessageRetrying {
            message_id,
            retry_count,
            ..
        } => (warn(), format!("retry #{retry_count}   {}", id8(message_id))),
        Event::MessageFailed { message_id, reason, .. }
        | Event::MessageUndeliverable { message_id, reason, .. } => {
            (err(), format!("failed     {} {reason}", id8(message_id)))
        }
        Event::MessageRelayed {
            sender,
            recipient,
            hop_count,
            ..
        } => (
            Color::Magenta,
            format!("relay      {}→{} hop {hop_count}", nick(sender), nick(recipient)),
        ),
        Event::SecureSessionFailed { peer_id, reason } => {
            (err(), format!("mls failed {} {reason}", nick(peer_id)))
        }
        Event::SecurityWarning { peer_id, reason, .. } => {
            (warn(), format!("warning    {} {reason}", nick(peer_id)))
        }
        _ => return None,
    };
    let at = Local::now().format("%H:%M:%S").to_string();
    Some(log_line(&at, color, text))
}

fn id8(id: &str) -> String {
    id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_words() {
        assert_eq!(wrap("hello there world", 11), ["hello there", "world"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\n\nb", 10), ["a", "", "b"]);
        assert_eq!(wrap("abcd efg", 4), ["abcd", "efg"]);
        assert_eq!(wrap("abcdefgh ij", 4), ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn clips_keep_the_useful_end() {
        assert_eq!(clip_middle("short.pdf", 20), "short.pdf");
        let file = clip_middle("quarterly-report-final-version-2.pdf", 20);
        assert_eq!(file.width(), 20);
        assert!(file.ends_with(".pdf") && file.starts_with("quarterly"), "{file}");
        assert_eq!(clip_start("~/a/b/c/projects", 10), "…/projects");
        assert_eq!(clip_start("~/a", 10), "~/a");
    }

    #[test]
    fn names_are_one_tidy_line() {
        assert_eq!(display_name("  bob\n\x1b[2J  the\tbuilder ", 32).as_deref(), Some("bob [2J the builder"));
        assert_eq!(display_name("\n\t", 32), None);
        assert_eq!(display_name("abcdefgh", 4).as_deref(), Some("abcd"));
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(25 * 1024 * 1024), "25 MB");
    }
}
