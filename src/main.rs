//! hopchat: offline terminal chat over an Offline Protocol mesh.
//!
//! Peers on the same network find each other with no server and no
//! internet; every conversation is end-to-end encrypted with MLS.

mod dnssd;
mod engine;
mod history;
mod home;
mod lobby;
mod me;
mod net;
mod tasks;
mod ui;

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

const USAGE: &str = "usage: hopchat [--name NICK] [--dir PATH] [--downloads PATH] [--theme dark|light]
               [--echo | --say TEXT | --file PATH] [--to NICK]
               [--lobby | --group NAME [--home] [--ai]]

  --name NICK       what peers see you as (default: $USER)
  --dir PATH        where your identity and history live (default: ~/.hopchat/NICK)
  --downloads PATH  where received files are saved (default: ~/Downloads/hopchat)
  --theme NAME      dark or light (default: the last /theme, else the terminal's)
  --echo            no UI: answer every message (a bot to test against)
  --say TEXT        no UI: send TEXT to the first peer that connects
  --file PATH       no UI: send the file at PATH to the first peer that connects
  --to NICK         with --say/--file: only send to a peer with this nick
  --lobby           with --say: say it in the lobby, everyone nearby's room,
                    once the first peer (or --to NICK) has a session
  --group NAME      no UI: create group NAME, invite peers as they connect
                    (only --to NICK, if given), and --say into the group
  --home            with --group: let the group use this machine's devices
  --ai              with --group: let the group ask this machine's model with @ai
  --version         print the version and exit

Run two copies with different names to chat with yourself on one machine.";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut nick = std::env::var("USER").unwrap_or_else(|_| "anon".into());
    let mut dir: Option<PathBuf> = None;
    let mut downloads: Option<PathBuf> = None;
    let mut theme: Option<bool> = None;
    let mut echo = false;
    let mut say: Option<String> = None;
    let mut file: Option<PathBuf> = None;
    let mut to: Option<String> = None;
    let mut group: Option<String> = None;
    let mut lobby = false;
    let mut home_on = false;
    let mut ai_on = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--name" | "-n" => nick = args.next().ok_or(USAGE)?,
            "--dir" => dir = Some(args.next().ok_or(USAGE)?.into()),
            "--downloads" => downloads = Some(args.next().ok_or(USAGE)?.into()),
            "--theme" => theme = Some(args.next().as_deref().and_then(ui::theme::parse).ok_or(USAGE)?),
            "--echo" => echo = true,
            "--say" => say = Some(args.next().ok_or(USAGE)?),
            "--file" => file = Some(args.next().ok_or(USAGE)?.into()),
            "--to" => to = Some(args.next().ok_or(USAGE)?),
            "--group" => group = Some(args.next().ok_or(USAGE)?),
            "--lobby" => lobby = true,
            "--home" => home_on = true,
            "--ai" => ai_on = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--version" | "-V" => {
                println!("hopchat {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => return Err(USAGE.into()),
        }
    }
    let nick: String = nick
        .chars()
        .filter(|c| !c.is_whitespace())
        .take(24)
        .collect();
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let dir = dir.unwrap_or_else(|| home.join(".hopchat").join(&nick));
    let downloads = downloads.unwrap_or_else(|| {
        let folder = home.join("Downloads");
        if folder.is_dir() {
            folder.join("hopchat")
        } else {
            dir.join("downloads")
        }
    });
    if let Some(path) = &file
        && !path.is_file()
    {
        return Err(format!("{} is not a file", path.display()).into());
    }

    let (update_tx, update_rx) = mpsc::channel();
    let (input_tx, input_rx) = mpsc::channel();

    let (mut engine, me, store_key) =
        engine::open(&dir, &nick, downloads.clone(), update_tx.clone())?;

    // Home answers @home in the groups it is switched on for.
    let (home_tx, home_rx) = mpsc::channel();
    engine.set_home(home_tx.clone());
    {
        let home = home::Home::open(&dir, input_tx.clone(), update_tx, home_tx.clone());
        thread::spawn(move || home.run(home_rx));
    }

    // The network reports into the engine's input queue.
    let (net_tx, net_rx) = mpsc::channel();
    net::start(me.clone(), net_tx)?;
    {
        let input_tx = input_tx.clone();
        thread::spawn(move || {
            for event in net_rx {
                if input_tx.send(engine::Input::Net(event)).is_err() {
                    break;
                }
            }
        });
    }
    thread::spawn(move || engine.run(input_rx));

    if lobby && group.is_some() {
        return Err(USAGE.into());
    }
    if echo || say.is_some() || file.is_some() || group.is_some() {
        let script = Script {
            echo,
            say,
            file,
            to,
            group,
            lobby,
            home_on,
            ai_on,
        };
        headless(&me, update_rx, input_tx, home_tx, script);
        return Ok(());
    }

    let store = match history::Store::open(&dir, &store_key) {
        Ok(store) => Some(store),
        Err(e) => {
            eprintln!("history disabled: {e}");
            None
        }
    };
    ui::theme::init(dir.join("theme"), theme);
    let app = ui::App::new(me.nick, me.address, store, downloads, home_tx);
    let mut terminal = ratatui::init();
    enable_extras();
    let result = app.run(&mut terminal, update_rx, input_tx);
    disable_extras();
    ratatui::restore();
    result?;
    Ok(())
}

/// Mouse, bracketed paste (so dropped files arrive as one paste), focus
/// reports (for read receipts), and unambiguous keys where the terminal
/// offers them (so Shift-Enter is distinct from Enter).
fn enable_extras() {
    use ratatui::crossterm::event::{
        EnableBracketedPaste, EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    };
    use ratatui::crossterm::execute;
    let mut out = std::io::stdout();
    let _ = execute!(out, EnableMouseCapture, EnableBracketedPaste, EnableFocusChange);
    if matches!(ratatui::crossterm::terminal::supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
}

fn disable_extras() {
    use ratatui::crossterm::event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, PopKeyboardEnhancementFlags,
    };
    use ratatui::crossterm::execute;
    let mut out = std::io::stdout();
    if matches!(ratatui::crossterm::terminal::supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableMouseCapture, DisableBracketedPaste, DisableFocusChange);
}

/// What a headless run does.
struct Script {
    echo: bool,
    say: Option<String>,
    file: Option<PathBuf>,
    to: Option<String>,
    group: Option<String>,
    lobby: bool,
    home_on: bool,
    ai_on: bool,
}

/// No TUI: prints what happens. `--echo` answers every message it gets;
/// `--say TEXT` and `--file PATH` send to the first peer whose secure
/// session is up (and, with `--to NICK`, whose beacon carries that nick).
/// With `--group NAME`, that peer is invited to a new group instead and
/// `--say` goes to the group once they have joined. With `--lobby` it
/// goes to the lobby instead, once that peer is there.
fn headless(
    me: &net::LocalIdentity,
    updates: mpsc::Receiver<engine::Update>,
    commands: mpsc::Sender<engine::Input>,
    home: mpsc::Sender<home::Request>,
    script: Script,
) {
    let Script {
        echo,
        mut say,
        mut file,
        to,
        group,
        lobby,
        home_on,
        ai_on,
    } = script;
    use std::collections::{HashMap, HashSet};

    use engine::{Command, Input, Update};
    use offline_protocol::Event;

    println!("{} is {}", me.nick, me.address);
    let send = |c: Command| {
        let _ = commands.send(Input::Ui(c));
    };
    let mut nicks: HashMap<String, String> = HashMap::new();
    let mut ready: HashSet<String> = HashSet::new();
    // With --group: the group once it exists, and who to invite into it.
    let mut group_id: Option<String> = None;
    let mut invite: Vec<String> = Vec::new();
    let mut invited: HashSet<String> = HashSet::new();
    let mut say_group = if group.is_some() { say.take() } else { None };
    if let Some(name) = &group {
        send(Command::CreateGroup {
            name: name.clone(),
            members: Vec::new(),
        });
    }
    // Sends once a ready peer is known to be the wanted one; the beacon
    // with its nick and its session can arrive in either order.
    let mut deliver = |address: &str, nicks: &HashMap<String, String>, invite: &mut Vec<String>| {
        let wanted = to
            .as_ref()
            .is_none_or(|n| nicks.get(address) == Some(n));
        if !wanted {
            return;
        }
        if group.is_some() {
            invite.push(address.to_string());
            return;
        }
        if lobby && let Some(text) = say.take() {
            send(Command::SendGroup {
                group: crate::lobby::LOBBY.into(),
                text,
                reply: None,
                from_home: false,
            });
        }
        if let Some(text) = say.take() {
            send(Command::Send {
                to: address.to_string(),
                text,
                reply: None,
            });
        }
        if let Some(path) = file.take() {
            send(Command::SendFile {
                to: address.to_string(),
                path,
                file_id: engine::new_file_id(),
            });
        }
    };
    for update in updates {
        if let Some(id) = &group_id {
            for member in invite.drain(..) {
                if invited.insert(member.clone()) {
                    send(Command::Invite {
                        group: id.clone(),
                        member,
                    });
                }
            }
        }
        match update {
            Update::Typing { .. } => {}
            Update::PeerSeen { address, nick } => {
                nicks.insert(address.clone(), nick);
                if ready.contains(&address) {
                    deliver(&address, &nicks, &mut invite);
                }
            }
            Update::PeerUp { address } => println!("link up {address}"),
            Update::PeerDown { address } => println!("link down {address}"),
            Update::Queued { to, id, text, .. } => println!("queued {id} to {to}: {text}"),
            Update::Incoming {
                from, text, hops, ..
            } => {
                println!("message from {from} ({hops} hop): {text}");
                if echo && !text.starts_with("echo: ") {
                    send(Command::Send {
                        to: from,
                        text: format!("echo: {text}"),
                        reply: None,
                    });
                }
            }
            Update::SessionReady { address } => {
                println!("secure session with {address}");
                ready.insert(address.clone());
                deliver(&address, &nicks, &mut invite);
            }
            Update::FileQueued {
                file_id, name, size, ..
            } => println!("file queued {file_id}: {name} ({size} bytes)"),
            Update::FileProgress {
                file_id,
                done,
                total,
                ..
            } => println!("file progress {file_id} {done}/{total}"),
            Update::FileSent { file_id } => println!("file sent {file_id}"),
            Update::FileFailed { file_id, reason } => println!("file failed {file_id}: {reason}"),
            Update::FileReceived {
                from,
                name,
                size,
                path,
                ..
            } => {
                println!("file from {from}: {name} ({size} bytes) saved to {}", path.display());
                if echo {
                    send(Command::Send {
                        to: from,
                        text: format!("echo: got {name} ({size} bytes)"),
                        reply: None,
                    });
                }
            }
            Update::FileLost {
                from, name, reason, ..
            } => println!("file from {from} lost: {name}: {reason}"),
            Update::ResendWanted { file_id, .. } => println!("resend wanted {file_id}"),
            Update::Read { from, ids } => println!("read by {from}: {}", ids.join(",")),
            Update::Group { id, name, members } => {
                println!("group {name} ({id}): {} members", members.len());
                if group_id.as_ref() == Some(&id)
                    && members.len() > 1
                    && let Some(text) = say_group.take()
                {
                    send(Command::SendGroup {
                        group: id,
                        text,
                        reply: None,
                        from_home: false,
                    });
                }
            }
            Update::GroupLeft { id } => println!("left group {id}"),
            // The group this run made, not an older one with the same name.
            Update::GroupCreated { id } => {
                if group_id.is_none() && group.is_some() {
                    group_id = Some(id.clone());
                    if home_on {
                        let _ = home.send(home::Request::Configure {
                            group: Some(id.clone()),
                            args: "on".into(),
                        });
                    }
                    if ai_on {
                        let _ = home.send(home::Request::Configure {
                            group: Some(id),
                            args: "ai on".into(),
                        });
                    }
                }
            }
            Update::GroupQueued { group, id, text, .. } => {
                println!("queued {id} to group {group}: {text}")
            }
            Update::GroupIncoming {
                group, from, text, ..
            } => {
                println!("group {group} message from {from}: {text}");
                if echo && from != me.address && !text.starts_with("echo: ") {
                    send(Command::SendGroup {
                        group,
                        text: format!("echo: {text}"),
                        reply: None,
                        from_home: false,
                    });
                }
            }
            Update::Home(text) => println!("{text}"),
            Update::HomeView(_) => {}
            Update::Error(e) => println!("error {e}"),
            Update::Protocol(event) => match *event {
                Event::MessageDelivered {
                    message_id,
                    latency_ms,
                    ..
                } => println!("delivered {message_id} in {latency_ms}ms"),
                Event::MessageReceived {
                    message_id,
                    encrypted,
                    ..
                } => println!("received {message_id} encrypted={encrypted}"),
                Event::MessageFailed {
                    message_id, reason, ..
                } => println!("failed {message_id}: {reason}"),
                _ => {}
            },
        }
    }
}
