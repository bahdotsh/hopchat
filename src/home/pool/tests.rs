use super::*;
use crate::engine::Update;
use std::fs;
use std::path::Path;
use std::sync::mpsc::Receiver;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hopchat-{name}-{}", crate::engine::new_file_id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn has_python() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// One machine's home, with what it tells its engine and itself.
struct Side {
    home: Home,
    engine: Receiver<Input>,
    me: Receiver<Request>,
    _ui: Receiver<Update>,
}

fn side(root: &Path, name: &str, bin: &Path) -> Side {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    let (engine_tx, engine) = mpsc::channel();
    let (ui_tx, ui) = mpsc::channel();
    let (me_tx, me) = mpsc::channel();
    let mut home = Home::open(&dir, engine_tx, ui_tx, me_tx);
    home.config.llama_dir = Some(bin.to_path_buf());
    Side { home, engine, me, _ui: ui }
}

fn peer(name: &str) -> Peer {
    Peer { address: name.into(), who: name.into(), ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)) }
}

/// The pool messages `side` asked its engine to send, and to whom.
fn sent(side: &Side) -> Vec<(String, Wire)> {
    side.engine
        .try_iter()
        .filter_map(|input| match input {
            Input::Ui(Command::PoolSend { to, wire }) => Some((to, wire)),
            _ => None,
        })
        .collect()
}

fn configure(side: &mut Side, group: Option<&str>, args: &str) {
    side.home.handle(Request::Configure { group: group.map(str::to_string), args: args.into() });
}

fn join(group: &str) -> Wire {
    let key = base64::engine::general_purpose::STANDARD.encode(tunnel::secret());
    Wire::Join { id: "p1".into(), group: group.into(), model: "m".into(), key }
}

#[test]
fn wire_round_trips_and_hides_garble() {
    let join = join("g");
    assert_eq!(Wire::decode(&join.encode()), Some(Some(join)));
    assert_eq!(Wire::decode(&format!("{MARK}{{nonsense")), Some(None));
    assert_eq!(Wire::decode("an ordinary message"), None);
    // The model lending marker is a different message.
    assert_eq!(Wire::decode(&super::super::lend::Wire::Gone.encode()), None);
}

#[test]
fn the_engine_turns_away_invitations_from_outside_the_group() {
    let invite = join("g");
    assert_eq!(admit(invite.clone(), |g| g == "g"), Ok(invite.clone()));
    let refused = admit(invite, |g| g == "other");
    assert_eq!(refused, Err(Wire::Refuse { id: "p1".into(), error: "isn't in that group with you".into() }));
    // Everything but an invitation goes through: it names no group to check.
    let leave = Wire::Leave { id: "p1".into() };
    assert_eq!(admit(leave.clone(), |_| false), Ok(leave));
}

#[test]
fn only_those_told_a_machine_gives_hear_it_stopped() {
    let mut told = HashSet::new();
    // Never gave: nothing is said, so older clients see no stray message.
    assert_eq!(give_wire(None, &mut told, "old"), None);
    assert_eq!(give_wire(Some(8), &mut told, "a"), Some(Wire::Give { mem: 8 }));
    assert_eq!(give_wire(None, &mut told, "a"), Some(Wire::Keep));
    // Told once is enough.
    assert_eq!(give_wire(None, &mut told, "a"), None);
    assert!(told.is_empty());
}

#[test]
fn a_pool_only_writes_plain_answers() {
    assert!(takes(None));
    assert!(takes(Some(&Value::Null)));
    assert!(takes(Some(&json!([]))));
    assert!(!takes(Some(&json!([{"type": "function"}]))));
}

#[test]
fn paths_keep_their_spaces() {
    assert_eq!(after_words("pool bin /Applications/llama cpp/bin ", 2), "/Applications/llama cpp/bin");
    assert_eq!(after_words("  pool   start   ~/My Models/big one.gguf", 2), "~/My Models/big one.gguf");
    assert_eq!(after_words("pool give on", 2), "on");
    assert_eq!(after_words("pool", 2), "");
}

#[test]
fn giving_is_switched_on_one_conversation_at_a_time() {
    let root = scratch("pool-give");
    let mut gina = side(&root, "gina", &root);

    configure(&mut gina, None, "pool give on");
    assert!(gina.home.config.pool_groups.is_empty(), "on needs a conversation to give in");
    configure(&mut gina, Some("g"), "pool give maybe");
    assert!(gina.home.config.pool_groups.is_empty(), "only on switches giving on");
    configure(&mut gina, Some("g"), "pool give on");
    assert_eq!(gina.home.config.pool_groups, ["g"]);
    let _ = sent(&gina);

    // A pool started anywhere else, the lobby included, is refused.
    for group in ["other", LOBBY] {
        gina.home.handle(Request::PoolWire { peer: peer("hank"), wire: join(group) });
        let refused = sent(&gina);
        assert!(
            matches!(refused.as_slice(), [(to, Wire::Refuse { error, .. })] if to == "hank" && error.contains("isn't giving")),
            "{group}: {refused:?}"
        );
        assert!(gina.home.pools.serving.is_none());
    }

    configure(&mut gina, Some("g"), "pool give off");
    assert!(gina.home.config.pool_groups.is_empty());
    let _ = fs::remove_dir_all(root);
}

/// A host asking `gina` in group `g` to pool `model`, waiting for answers.
fn hosting(root: &Path) -> Side {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("llama-server"), "").unwrap();
    let gguf = root.join("big.gguf");
    fs::write(&gguf, b"GGUF").unwrap();
    let mut hank = side(root, "hank", &bin);
    configure(&mut hank, Some("g"), &format!("pool start {}", gguf.display()));
    for input in hank.engine.try_iter() {
        if let Input::Ui(Command::PoolGivers { reply, .. }) = input {
            let gina = Giver { address: "gina".into(), nick: "gina".into(), mem: 8 << 30, ip: peer("gina").ip };
            reply.send(vec![gina]).unwrap();
        }
    }
    let found = hank.me.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(found, Request::PoolFound { .. }));
    hank.home.handle(found);
    let invited = sent(&hank);
    assert!(matches!(invited.as_slice(), [(to, Wire::Join { .. })] if to == "gina"), "{invited:?}");
    hank
}

#[test]
fn only_those_asked_can_answer_and_a_vanished_giver_isnt_waited_for() {
    let root = scratch("pool-ask");
    let mut hank = hosting(&root);
    let id = hank.home.pools.hosting.as_ref().unwrap().id.clone();

    // Someone who wasn't asked can't slip into the pool.
    hank.home.handle(Request::PoolWire { peer: peer("mallory"), wire: Wire::Ready { id: id.clone(), port: 9, mem: 1 } });
    let h = hank.home.pools.hosting.as_ref().unwrap();
    assert_eq!(h.phase, Phase::Gathering);
    assert!(h.members.iter().all(|m| m.address == "gina" && m.ready.is_none()));
    assert!(sent(&hank).is_empty());

    // The only giver drops off before answering: the pool ends now, not in 20 seconds.
    hank.home.handle(Request::PoolPeerGone { address: "gina".into(), who: "gina".into() });
    assert!(hank.home.pools.hosting.is_none());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn a_giver_doesnt_host_and_help_at_once() {
    let root = scratch("pool-both");
    let mut hank = hosting(&root);
    hank.home.config.pool_groups.push("g".into());
    hank.home.handle(Request::PoolWire { peer: peer("carol"), wire: join("g") });
    let refused = sent(&hank);
    assert!(
        matches!(refused.as_slice(), [(_, Wire::Refuse { error, .. })] if error.contains("hosting")),
        "{refused:?}"
    );
    let _ = fs::remove_dir_all(root);
}

/// Stand-ins for llama.cpp: an RPC server that echoes, and a llama-server
/// that answers once every `--rpc` worker has echoed a ping through its
/// tunnel.
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
/// A llama-server that never finishes loading, and says where it runs.
const SLOW_SERVER: &str = r#"#!/bin/sh
echo $$ > "$(dirname "$0")/server.pid"
exec sleep 60
"#;

fn fake_llama_cpp(dir: &Path, server: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(dir).unwrap();
    for (name, script) in [("ggml-rpc-server", FAKE_RPC), ("llama-server", server)] {
        let path = dir.join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Pool messages from `from` go to `to`, and each side handles what it
/// told itself.
fn route(from: &mut Side, to: &mut Side, name: &str) {
    while let Ok(input) = from.engine.try_recv() {
        if let Input::Ui(Command::PoolSend { wire, .. }) = input {
            to.home.handle(Request::PoolWire { peer: peer(name), wire });
        }
    }
    while let Ok(request) = from.me.try_recv() {
        from.home.handle(request);
    }
}

/// A host and a giver with `server` standing in for llama-server, the host
/// pooling a model with the giver until `done` says it has gone far enough.
fn pool_until(server: &str, done: impl Fn(&Side, &Path) -> bool) -> Option<(Side, Side, PathBuf)> {
    if !has_python() {
        eprintln!("skipped: needs python3 to stand in for llama.cpp");
        return None;
    }
    let root = scratch("pool-e2e");
    let bin = root.join("bin");
    fake_llama_cpp(&bin, server);
    let gguf = root.join("big.gguf");
    fs::write(&gguf, b"GGUF").unwrap();
    let mut host = side(&root, "hank", &bin);
    let mut giver = side(&root, "gina", &bin);
    giver.home.config.pool_groups.push("g".into());

    // Answer the givers question from another thread, as the engine would.
    let (givers_tx, givers_rx) = mpsc::channel::<Input>();
    let engine = std::mem::replace(&mut host.engine, givers_rx);
    thread::spawn(move || {
        for input in engine {
            if let Input::Ui(Command::PoolGivers { reply, .. }) = input {
                let gina = Giver { address: "gina".into(), nick: "gina".into(), mem: 8 << 30, ip: peer("gina").ip };
                let _ = reply.send(vec![gina]);
            } else if givers_tx.send(input).is_err() {
                break;
            }
        }
    });

    configure(&mut host, Some("g"), &format!("pool start {}", gguf.display()));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done(&host, &bin) {
        assert!(Instant::now() < deadline, "the pool never got that far");
        route(&mut host, &mut giver, "hank");
        route(&mut giver, &mut host, "gina");
        thread::sleep(Duration::from_millis(20));
    }
    Some((host, giver, root))
}

/// A host and a giver arrange a pool with each other's hidden messages,
/// and the host's answer crosses the encrypted tunnel to the giver.
#[test]
fn a_host_and_a_giver_pool_a_model() {
    let Some((mut host, mut giver, root)) = pool_until(FAKE_SERVER, |host, _| host.home.pools.url().is_some()) else {
        return;
    };
    assert!(giver.home.pools.serving.is_some());
    let (url, _) = host.home.pools.url().unwrap();
    let answer = ask(&url, &[json!({"role": "user", "content": "hi"})]).unwrap();
    assert_eq!(answer["content"], "pooled over 1");

    configure(&mut host, None, "pool stop");
    assert!(host.home.pools.hosting.is_none());
    // The host's messages reach the giver through the stand-in engine's
    // thread, so give them a moment.
    let deadline = Instant::now() + Duration::from_secs(5);
    while giver.home.pools.serving.is_some() {
        assert!(Instant::now() < deadline, "the giver should leave when the host stops");
        route(&mut host, &mut giver, "hank");
        thread::sleep(Duration::from_millis(20));
    }
    let _ = fs::remove_dir_all(root);
}

/// Stopping a pool while it loads stops llama-server, even though a thread
/// is still waiting for it to finish loading.
#[test]
fn stopping_while_loading_stops_llama_server() {
    let loading = |host: &Side, bin: &Path| {
        host.home.pools.hosting.as_ref().is_some_and(|h| h.phase == Phase::Loading) && bin.join("server.pid").exists()
    };
    let Some((mut host, _giver, root)) = pool_until(SLOW_SERVER, loading) else { return };
    let pid = fs::read_to_string(root.join("bin/server.pid")).unwrap().trim().to_string();
    let alive = || {
        std::process::Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    assert!(alive());
    configure(&mut host, None, "pool stop");
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive() {
        assert!(Instant::now() < deadline, "llama-server outlived its pool");
        thread::sleep(Duration::from_millis(20));
    }
    let _ = fs::remove_dir_all(root);
}
