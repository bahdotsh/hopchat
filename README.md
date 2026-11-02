<div align="center">

# hopchat

**Offline, end-to-end encrypted terminal chat for your local network. No server, no internet, no accounts.**

[![crates.io](https://img.shields.io/crates/v/hopchat.svg)](https://crates.io/crates/hopchat)
[![Release](https://img.shields.io/github/v/release/bahdotsh/hopchat)](https://github.com/bahdotsh/hopchat/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org)
[![Built With Ratatui](https://ratatui.rs/built-with-ratatui/badge.svg)](https://ratatui.rs/)
[![MLS RFC 9420](https://img.shields.io/badge/encryption-MLS%20RFC%209420-success.svg)](https://www.rfc-editor.org/rfc/rfc9420)

![alice and bob find each other on the same network and start a private, end-to-end encrypted chat](demo/hopchat.gif)

</div>

hopchat is a terminal chat app built on the [Offline Protocol SDK](https://github.com/Offline-Protocol/offline-protocol-sdk) and [ratatui](https://ratatui.rs). Peers on the same network find each other on their own, and every conversation is end-to-end encrypted with MLS (RFC 9420).

It's also a remote control for the room you're in. Mention `@home` to dim the lights or put something on the TV, and `@ai` to ask a local model. One person's model can answer for the whole group, and nothing leaves your network.

## Table of contents

- [Features](#features)
- [Requirements](#requirements)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Usage](#usage)
  - [The lobby](#the-lobby)
  - [Me](#me)
  - [Todos and reminders](#todos-and-reminders)
  - [Direct messages and files](#direct-messages-and-files)
  - [Groups](#groups)
  - [Smart home with `@home`](#smart-home-with-home)
  - [A local model with `@ai`](#a-local-model-with-ai)
  - [Headless mode](#headless-mode)
- [Keyboard shortcuts](#keyboard-shortcuts)
- [Commands](#commands)
- [Configuration](#configuration)
- [Architecture](#architecture)
- [Security](#security)
- [Contributing](#contributing)
- [License](#license)

## Features

**Chat**
- **Serverless.** Peers on the same network discover each other over UDP multicast and DNS-SD. Your identity is a key on your disk, so there are no accounts.
- **End-to-end encrypted.** Direct messages, groups and the lobby all travel inside MLS (RFC 9420). History on disk is sealed with XChaCha20-Poly1305.
- **Lobby.** A room everyone nearby is in by default, pinned at the top of the sidebar.
- **Me.** A private space for notes and for talking to your own devices and model. Nothing in it goes on the network.
- **Todos and reminders.** Type "call mom at 5pm" or "standup every weekday at 9:30" and hopchat reminds you in Me. Any message becomes a todo with one key.
- **Groups.** MLS groups created from a panel in a few keystrokes.
- **File transfer.** Up to 100 MB per file, chunked and encrypted, with live progress, rate and time left. Pick files from a browser, drag them onto the window, or type a path.
- **Delivery and read receipts.** `◷` queued, `✓` sent, `✓✓` delivered, highlighted `✓✓ read`, and `✗` failed (press `R` to retry).
- **Replies, typing indicators and unread badges**, plus a bell and an unread count in the window title.
- **Presence.** It tells you when people come online, drop off the network, or join a group.
- **Offline delivery.** Messages written while a peer is away are delivered when they come back. History survives restarts.

**Smart home and AI**
- **`@home`.** Plain English becomes actions on Wiz and Tuya lights, LG webOS TVs and Android or Google TVs, and answers about network printers and copiers, through a local [Ollama](https://ollama.com) model.
- **`@ai`.** Ask a local model anything in any conversation. Follow-up questions work.
- **Shared models.** Only one person in a group needs a model. Everyone else borrows it over the encrypted session.
- **Device panel.** Discover, pair, name and test devices without editing any config.

**Interface**
- **Calm theme.** Soft pastels on a dusk background, with a stable colour for each person.
- **Mouse support.** The hints along the bottom are clickable buttons. You can click conversations and messages too.
- **Completion.** Slash commands and `@` mentions complete with Tab.
- **Multi-line composer** with word motions and readline-style editing keys.
- **Scriptable.** Headless modes for bots, tests and one-off sends.

## Requirements

- Rust 1.88 or newer, only when installing with cargo or building from source (the crate uses edition 2024)
- macOS or Linux, on a network that allows UDP multicast
- Optional, for `@home` and `@ai`: [Ollama](https://ollama.com) with a model that can call tools, such as `qwen2.5:7b`

## Installation

Prebuilt Linux binaries need glibc 2.35+, which means Debian 12, Ubuntu 22.04,
Fedora 36 or newer. The tarballs and AppImage bundle their own OpenSSL; the
`.deb` and `.rpm` use the system's OpenSSL 3. On older systems, use cargo below.

One-liner (Linux/macOS, installs the latest release to `~/.local/bin` after
checking the download for corruption against the release's `SHA256SUMS`):

```sh
curl -fsSL https://raw.githubusercontent.com/bahdotsh/hopchat/main/install.sh | sh
# options go after `sh -s --`, e.g. a system-wide install:
curl -fsSL https://raw.githubusercontent.com/bahdotsh/hopchat/main/install.sh | sudo sh -s -- --system
```

Arch Linux (AUR, community-maintained):

```sh
yay -S hopchat-git    # latest git
```

A stable `hopchat` AUR package for v0.1.0 is being updated by its maintainer.
Until it lands, use `hopchat-git`.

Debian/Ubuntu and Fedora/RHEL: grab the `.deb` / `.rpm` from the
[releases page](https://github.com/bahdotsh/hopchat/releases). Installing
through the package manager pulls in OpenSSL if it is missing:

```sh
sudo apt install ./hopchat_0.1.0-1_amd64.deb       # Debian/Ubuntu
sudo dnf install ./hopchat-0.1.0-1.x86_64.rpm      # Fedora/RHEL
```

AppImage (x86_64 Linux, glibc 2.35+, nothing else to install):

```sh
chmod +x hopchat-0.1.0-linux-x86_64.AppImage
./hopchat-0.1.0-linux-x86_64.AppImage --name alice
```

Every release lists the SHA-256 of each file in `SHA256SUMS`. To check a
manual download for corruption, run `sha256sum --ignore-missing -c SHA256SUMS`
in the folder that holds both. To check that a file was built by this
repository's release workflow, verify its build provenance with the
[GitHub CLI](https://cli.github.com):

```sh
gh attestation verify hopchat-0.1.0-linux-x86_64.tar.gz --repo bahdotsh/hopchat
```

From [crates.io](https://crates.io/crates/hopchat) with cargo (builds the
latest release on any glibc, and links your system's OpenSSL):

```sh
cargo install hopchat --locked
```

The latest unreleased code on `main`:

```sh
cargo install --git https://github.com/bahdotsh/hopchat --locked
```

Check what you have installed with `hopchat --version`.

From source:

```sh
git clone https://github.com/bahdotsh/hopchat
cd hopchat
cargo build --release
./target/release/hopchat --name alice
```

Optional model setup for `@home` and `@ai`:

```sh
ollama serve
ollama pull qwen2.5:7b
```

## Quick start

Run hopchat on two machines on the same network:

```sh
hopchat --name alice    # on one machine
hopchat --name bob      # on another
```

Two terminals on one machine work too, because each name gets its own identity under `~/.hopchat/<name>`. Both of you start in the lobby. Click the other person's name in the sidebar to talk privately, and press **F1** at any time to see every key and command.

## Usage

### The lobby

hopchat opens in the **Lobby**. Everyone on your network who runs hopchat is in it automatically, so there's nothing to create and nobody to invite.

```
anyone up for lunch?
@home turn the lights down
@ai what's a good name for a sourdough starter?
```

A lobby message goes, end-to-end encrypted, to each person who is connected when you send it. It works like a room, not a mailbox: people who join later don't see earlier messages, and there are no delivery ticks. You can't leave the lobby, and files are sent one-to-one.

Your own `@home` and `@ai` always work in the lobby. Only you can use them there until you run `/home on` or `/ai on`, or press `g` or `i` in the Devices panel. `off` makes them yours again.

### Me

**Me** sits right under the lobby. It's a conversation with nobody else in it: write notes to yourself, and talk to your own devices and model.

```
remember to call the landlord
@home dim the desk lamp to 30%
@ai what can I make with eggs, spinach and feta?
```

Nothing written in Me goes on the network. It's kept in local history, and nobody else can ever reach your devices or model through it.

### Todos and reminders

Press **Ctrl-T** (or type `/todo`) for your list. Type what to do and press Enter. If it says when, it becomes a reminder, and the line under it shows how it was read before you commit:

```
buy milk                              ☐ todo
call mom at 5pm                       ◷ reminder today 17:00 · in 3h
stretch in 20m                        ◷ reminder today 14:20 · in 20m
dentist tomorrow at 9                 ◷ reminder tomorrow 09:00
pay rent on nov 1                     ◷ reminder 1 Nov 09:00
standup every weekday at 9:30         ◷ reminder · ↻ weekdays
drink water every 2 hours             ◷ reminder · ↻ every 2h
```

The time can come before or after the task. Text that doesn't read as a time stays part of the task, so "meet at the cafe" is a todo. A bare "at 5" means 5 pm.

- **From anywhere.** `/todo TEXT` and `/remind TEXT` add one without opening the list. Select any message (click it, or ↑) and press `t` to turn it into a todo, with its text filled in so you can add a time.
- **When it's time**, the reminder appears in Me with a bell, an unread badge and a note in the footer, whichever chat you're in. The strip at the top shows what's due, or what's next. iTerm2, WezTerm and Ghostty also show a desktop notification.
- **On the reminder in Me:** `x` checks it off, `s` snoozes it 10 minutes, and `o` goes back to the chat it was set in.
- **In the list:** Space checks off, `s` snoozes, Enter edits, `d` deletes (`u` undoes), `o` goes to the chat it came from, and `c` clears what's done.

A one-off reminder waits under **Due** until you check it off or snooze it. A repeating one moves on to its next time. Reminders missed while hopchat was closed go off when it next starts. The list stays on this machine, sealed like your history, and is never sent to anyone.

### Direct messages and files

Click a person in the sidebar to open a private chat. A secure session is set up as soon as you see each other.

- **Send** with Enter. Shift-Enter, Alt-Enter or Ctrl-J starts a new line.
- **Reply, open, copy or retry** a message by clicking it, or by pressing ↑ on an empty line.
- **Send files** with Ctrl-O, by dragging them onto the window, or with `/file PATH`. Up to two transfers run per peer at once, and the rest wait in a queue. A transfer interrupted by quitting resumes on the next run.
- **Received files** are saved to `~/Downloads/hopchat`. They never overwrite an existing file and never end up outside that folder.

### Groups

Press **Ctrl-G**, click **+ New group**, or type `/group`. Name the group, press ↓, tick people with Space, and press Enter. Only people who are online can be invited, because an invite needs their key package.

- `/invite` opens the same picker in an existing group. `/invite bob` adds one person.
- `/group house` creates a group straight away.
- `/members` lists who's in the group, and `/leave` leaves it.

Groups are MLS groups carried over the same peer-to-peer links, so they need no server either. Files are still sent one-to-one.

### Smart home with `@home`

Mention `@home` in Me, the lobby or any group you're in:

```
@home dim the lights to 20% and put on netflix
@home make the lamp purple
@home turn the tv down a bit
@home is the light on?
@home turn on the kettle
```

A local model turns the request into device actions, and home replies in the conversation with 🏠.

**Setup on the machine that owns the devices:**

1. Start Ollama with a tool-calling model (`qwen2.5:7b` by default; `/home model NAME` picks another). You can skip this if someone in the group shares a model (see [Shared models](#shared-models)).
2. Press **Ctrl-D** (or type `/devices`). hopchat scans the network for Wiz and Tuya lights, LG and Android TVs, and printers.
   - **Light:** Enter adds it, `t` blinks it so you can tell which one it is, and `n` names it ("Desk lamp"). A Tuya light needs its key first; see [below](#tuya-lights).
   - **LG TV:** Enter pairs it. Choose *Allow* on the TV with the remote. For "turn the TV on" to work, enable *Turn on via Wi-Fi* (or *Mobile TV On*) in the TV's settings. A TV only shows up while it's on.
   - **Android or Google TV:** Enter pairs it, and the TV shows a six-character code. Type it with `/home code CODE` within a minute. A mistyped code is caught before anything is sent, so you can type it again. This covers Sony, TCL, Xiaomi, Philips and other Google TV and Android TV sets. `t` sends the TV to its home screen.
   - **Printer or copier:** Enter adds it, `t` makes it beep or flash if it supports that, and `n` names it. Home can then say whether it is printing, how much ink or toner is left, and whether it is jammed or out of paper. It never prints anything. A printer a computer shares through CUPS isn't listed, since a network printer is found on its own; a USB printer behind a print server therefore doesn't show up.
   - The panel also shows whether the model is ready, and the exact command to run if it isn't.
3. To let others in a group or the lobby use your devices, press `g` in the panel or type `/home on` there. `/home off` revokes it.

If a device is on another subnet, add it with `/home add 192.168.1.40`. `/home scan` searches again, and a bare `/home` shows the current status.

**Tuya lights.** Wipro bulbs and most other white-label Wi-Fi bulbs are Tuya devices, and hopchat drives them directly over your LAN. Each bulb encrypts with a local key issued by its Tuya cloud account, so getting that key once is the only step that needs the internet. Link the bulb's app to a project on the [Tuya IoT platform](https://platform.tuya.com) and run the [tinytuya](https://github.com/jasonacox/tinytuya) wizard, which lists each device's ID and local key. Then add each bulb with `/home tuya IP DEVICE_ID LOCAL_KEY`. Scanning finds any device with Tuya's port 6668 open and shows it in the panel with its address, and plugs and other devices that aren't dimmable lights are refused when you add them. Protocol versions 3.3, 3.4 and 3.5 work, and hopchat works out which one a bulb speaks. Resetting a bulb or re-adding it in its app changes its key: press `x` on it in `/devices` and add it again. The key is kept in `home.json`, like the Home Assistant token below.

**Home Assistant.** If you run Home Assistant, hopchat can drive anything it has an integration for. Make a long-lived access token under your HA profile (Security tab), then type `/home ha http://IP:8123 TOKEN`. `/home ha find` looks for an HA announcing itself over mDNS and tells you its address; the token is only ever sent to an address you typed, since anything on the LAN can announce itself as HA. Its devices show up in `/devices` under their HA names, to add like any other. Hidden, disabled and settings entities are left out, as are Wiz, LG and Android TVs, which hopchat drives directly. `https://` works too, with a certificate this machine trusts. An HA integration may itself go through the cloud. hopchat talks to one Home Assistant at a time: connecting again with a new token or address updates every HA device. `/home ha forget` disconnects HA and removes its devices.

The token is kept in `home.json` in hopchat's settings directory, readable only by you on macOS and Linux and in plain text elsewhere; anyone who can read that file can drive your Home Assistant, so revoke the token in HA if the file leaks. Once HA devices are added, don't go back to a hopchat from before this feature: it drops their entity ids when it saves, and they can't be driven afterwards.

**Supported devices**

| Device | Protocol |
| --- | --- |
| Wiz bulbs | local UDP, port 38899 |
| Tuya bulbs, including Wipro | local TCP, port 6668, protocol 3.3 to 3.5, with a device ID and local key |
| LG webOS TVs | WebSocket API on port 3001 over TLS (3000 on older sets) |
| Android and Google TVs | Android TV Remote protocol v2: pairing on port 6467, commands on 6466, TLS with a certificate per TV, found over mDNS (`_androidtvremote2._tcp`) |
| Printers and copiers | IPP 2.0 or 1.1 on port 631, found over Bonjour (`_ipp._tcp`) |
| Home Assistant lights, TVs and switches | HA's REST API, port 8123 by default |

Devices are tracked by MAC address, and printers by the UUID they report over IPP, so a new IP from the router doesn't lose them. Each light's capabilities come from its module name: a white-tones bulb is never asked for a colour, and home says so.

**Permissions.** You can always use your own devices. Beyond that, membership is the permission: everyone MLS has admitted to a group where home is on can use the devices, and nobody else can. In the lobby, that means everyone with a secure session to you, once you switch home on there. Home only answers while the host's hopchat is running.

### A local model with `@ai`

```
@ai what's a good name for a sourdough starter?
@ai summarise what alice just said
@ai and in French?
```

You can ask your own model in any conversation. To let the rest of a group ask it, type `/ai on` there or press `i` in the Devices panel. Answers come back as 🤖 replies. Each conversation remembers its recent questions, so follow-ups work. `/ai forget` clears that memory, and `/ai off` stops answering others.

#### Shared models

Only one person in a group needs to run a model.

- A machine whose model is ready offers it to the people it's connected to. It answers only people it shares a group with (the lobby counts), and handles at most two requests at a time.
- If the device owner has no model, home sends each reasoning step to a shared model and still runs the device actions itself. The Devices panel shows something like `borrowing bob (qwen2.5:7b)`.
- The lender sees a note when someone starts using its model. `/ai share off` stops lending, and `/ai share on` resumes it.
- Requests travel inside the same end-to-end encrypted one-to-one session as chat, and are never shown as messages.

### Headless mode

Run hopchat without the UI, for bots, tests and scripts:

```sh
hopchat --name bot --echo                         # answer every message and file
hopchat --name alice --say "hello" --to bot       # send one message
hopchat --name alice --file ./notes.pdf --to bot  # send one file
hopchat --name alice --lobby --to bob --say "hi all"
hopchat --name alice --group house --home --to bob --say "@home is the light on?"
hopchat --name alice --group quiz --ai --to bob --say "@ai what is 2+2?"
```

Without `--to`, `--say` and `--file` go to the first peer whose secure session comes up. `--lobby` and `--group` can't be combined.

## Keyboard shortcuts

| Key | Action |
| --- | --- |
| Enter | send (with any attached files) |
| Shift-Enter, Alt-Enter, Ctrl-J | new line |
| Tab / Shift-Tab, Ctrl-N / Ctrl-P, Alt-1…9 | switch conversation |
| Tab after `/` or `@` | complete a command or mention |
| Ctrl-O | attach files |
| ↑ on an empty line, or click | select a message: `r` reply, `o` open, `f` show in folder, `y` copy, `t` todo, `R` retry, `g`/`G` first/last, Esc back |
| PgUp / PgDn, mouse wheel | scroll |
| Ctrl-T | todos and reminders |
| Ctrl-G | new group |
| Ctrl-D | devices panel |
| Ctrl-E | protocol event log |
| F1 | help |
| Ctrl-C / Ctrl-Q | quit |

**Editing:** ←/→ move; Ctrl/Alt-←/→ and Alt-B/F jump words; Home/End or Ctrl-A/Ctrl-E go to the start or end; Ctrl-W and Alt-Backspace delete a word; Ctrl-U clears the line.

**File browser:** ↑/↓ move; → or Enter opens a folder or picks the file; ← goes up; `~` jumps home; Space marks several files; typing filters the list.

## Commands

| Command | Description |
| --- | --- |
| `/file [PATH]` | send files (no path opens the browser) |
| `/downloads` | open the folder received files are saved in |
| `/info` | who this peer is and how they're connected |
| `/me` | show and copy your own address |
| `/todo [TEXT]` | add a todo, or a reminder if it says when (no text opens the list) |
| `/remind TEXT` | set a reminder: `/remind 20m stretch`, `/remind call mom at 5pm` |
| `/group [NAME]` | start a group |
| `/invite [NICK]` | add people to this group |
| `/members` | list who is in this group |
| `/leave` | leave this group |
| `/devices` | find, pair and test devices |
| `/home on\|off` | let everyone here use your devices, or stop |
| `/home add IP` | add a device by address |
| `/home scan` | scan for devices again |
| `/home tuya IP ID KEY` | add a Tuya light with its device ID and local key |
| `/home code CODE` | finish pairing an Android TV with the code it shows |
| `/home ha URL TOKEN` | connect Home Assistant and list its devices |
| `/home ha find` | look for a Home Assistant announcing itself, and say its address |
| `/home ha forget` | disconnect Home Assistant and remove its devices |
| `/home model NAME` | choose the Ollama model |
| `/ai on\|off` | let everyone here ask your model, or stop |
| `/ai forget` | clear this conversation's model memory |
| `/ai share on\|off` | lend your model to people you share a group with, or stop |
| `/clear` | delete this conversation from this device |
| `/log` | show or hide the protocol event log |
| `/theme` | switch between dark and light (`/theme dark`, `/theme light`) |
| `/help` | keys and commands |
| `/quit` | quit |

## Configuration

```
hopchat [--name NICK] [--dir PATH] [--downloads PATH] [--theme dark|light]
        [--echo | --say TEXT | --file PATH] [--to NICK]
        [--lobby | --group NAME [--home] [--ai]]
```

| Flag | Default | Description |
| --- | --- | --- |
| `--name`, `-n` | `$USER` | the name peers see |
| `--dir` | `~/.hopchat/<name>` | where identity, history and settings live |
| `--downloads` | `~/Downloads/hopchat` | where received files are saved |
| `--theme` | last `/theme`, else the terminal's | `dark` or `light` |
| `--echo` | | no UI: answer every message |
| `--say TEXT` | | no UI: send one message |
| `--file PATH` | | no UI: send one file |
| `--to NICK` | | with `--say`/`--file`: only send to this peer |
| `--lobby` | | with `--say`: say it in the lobby |
| `--group NAME` | | no UI: create a group and invite peers as they connect |
| `--home` | | with `--group`: let the group use this machine's devices |
| `--ai` | | with `--group`: let the group ask this machine's model |
| `--version`, `-V` | | print the version and exit |
| `--help`, `-h` | | print usage and exit |

**Data directory** (`~/.hopchat/<name>`):

| Path | Contents |
| --- | --- |
| `store.key` | the key that seals everything else |
| `history/` | each conversation, and your todos and reminders (`tasks.box`), sealed with XChaCha20-Poly1305 |
| `home.json` | devices, the model, where home and `@ai` are on, and each TV's client key and pinned certificate |
| everything else | the SDK's identity and protocol stores |

If there's no `~/Downloads` folder, received files go to `<dir>/downloads`.

## Architecture

The SDK does no I/O itself, so hopchat supplies the platform bridge for its peer-stream (`wifi_direct`) transport.

| Component | Where | Role |
| --- | --- | --- |
| Discovery | `src/net.rs` | UDP multicast beacon on `239.255.42.99:47999` announcing `address, tcp port, nick` every 2 s. The lower address dials. |
| DNS-SD | `src/dnssd.rs` | Publishes and browses `_offlineprotocol._tcp` with TXT `txtvers=1`, `addr=<address>`, the record every Offline Protocol SDK app uses, so SDK apps on iOS, Android and Python find hopchat too. A record is a hint; the stream's preamble must prove `addr`. |
| Streams | `src/net.rs` | One TCP connection per peer. The first frame each way is the SDK's signed identity assertion, checked with `verify_identity_assertion`. |
| Engine | `src/engine.rs` | Owns `OfflineProtocol` on one thread. It feeds frames in, routes outgoing messages to streams, queues file transfers, and forwards events to the UI. |
| Lobby | `src/lobby.rs` | Not an MLS group. Each message is a hidden one-to-one message to every peer with a session, sharing one id so replies line up. |
| Me | `src/me.rs` | A local-only conversation. Messages go straight back to the UI and to home, never onto the wire. |
| History | `src/history.rs` | Encrypted per-conversation history. |
| Tasks | `src/tasks.rs` | Todos and reminders: reading times from plain English, repeats, and what is due. They fire from the UI loop into Me. |
| Home | `src/home/` | Device discovery and drivers (`wiz.rs`, `tuya.rs`, `webos.rs`, `androidtv.rs`, `ipp.rs`), the Ollama client, the `@home` tool-calling loop (`assistant.rs`), `@ai` (`ai.rs`), and model lending (`lend.rs`). |
| UI | `src/ui/` | ratatui rendering, the composer, the file browser, and the group and device panels. |

## Security

- Every message is end-to-end encrypted with MLS (RFC 9420) through the Offline Protocol SDK.
- A peer is identified only by the address its signed identity assertion proves (`off1…`). The nick in the beacon is unauthenticated and only used for display.
- History and stores on disk are sealed with a per-profile key.
- Received file names are sanitised: files never overwrite existing ones or escape the downloads folder.
- LG and Android TVs use self-signed certificates. hopchat pins the certificate it sees when pairing and refuses a different one later. For an Android TV, hopchat also makes its own certificate, which the TV remembers in place of a password. It is kept in `home.json` with the rest.
- Model requests sent to a lender travel inside the encrypted one-to-one session, and lenders only answer people they share a group with.
- Nothing is sent to the internet, apart from whatever your own Ollama setup does.

If you find a security issue, please report it privately to the maintainer instead of opening a public issue.

## Contributing

Contributions are welcome. Open an issue to discuss a larger change before you start.

```sh
cargo build
cargo test
cargo clippy --all-targets
cargo fmt
```

To test without a second machine, run two copies with different names, or chat with the headless echo bot:

```sh
cargo run -- --name bot --echo
cargo run -- --name alice
```

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org) (`feat(ui): …`, `fix(home): …`).

## License

Licensed under the [MIT license](LICENSE).

## Acknowledgements

- [Offline Protocol SDK](https://github.com/Offline-Protocol/offline-protocol-sdk) for identity, MLS sessions and transport
- [ratatui](https://ratatui.rs) for the terminal UI
- [Ollama](https://ollama.com) for running local models
