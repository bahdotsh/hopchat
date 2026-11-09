//! llama.cpp as pools run it: finding its programs and a model's weights,
//! and starting its servers so that none outlives hopchat.

use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

pub const LLAMA_SERVERS: &[&str] = &["llama-server"];
/// The worker was called `rpc-server` before llama.cpp renamed it.
pub const RPC_SERVERS: &[&str] = &["ggml-rpc-server", "rpc-server"];
/// The log is started over when it grows past this.
const LOG_LIMIT: u64 = 16 << 20;
const RPC_START: Duration = Duration::from_secs(15);
/// Loading streams every layer to its worker the first time, which is
/// gigabytes over Wi-Fi.
const LOAD: Duration = Duration::from_secs(45 * 60);

/// Runs the program in `$@` and kills it once its input closes. hopchat
/// holds the other end of that input, and the kernel closes it however
/// hopchat ends, a crash or SIGKILL included, so llama.cpp never outlives
/// it. A program that ignores SIGTERM gets SIGKILL ten seconds later. The
/// shell exits with the program's status, so a crash is still seen.
const GUARD: &str = r#"exec 3<&0 0</dev/null
"$@" 3<&- &
child=$!
{ read -r line <&3; kill "$child"; sleep 10; kill -9 "$child"; } 2>/dev/null &
watch=$!
exec 3<&-
wait "$child"
status=$?
kill "$watch" 2>/dev/null
exit "$status"
"#;

/// A llama.cpp process a pool started. Dropping it stops the process.
pub struct Process {
    /// Taken when it is dropped, to be waited for.
    child: Mutex<Option<Child>>,
    input: Mutex<Option<ChildStdin>>,
    /// Where this process's output starts in the log.
    log_from: u64,
}

impl Process {
    /// Starts `program` with `args` and `env`, its output appended to `log`.
    pub fn start<I, S>(program: &Path, args: I, env: &[(&str, &Path)], log: &Path) -> std::io::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let file = fs::OpenOptions::new().create(true).append(true).open(log)?;
        if file.metadata()?.len() > LOG_LIMIT {
            file.set_len(0)?;
        }
        let log_from = file.metadata()?.len();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", GUARD, "hopchat-pool"]).arg(program).args(args);
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command.stdin(Stdio::piped()).stdout(file.try_clone()?).stderr(file).spawn()?;
        let input = child.stdin.take();
        Ok(Self { child: Mutex::new(Some(child)), input: Mutex::new(input), log_from })
    }

    /// Stops it, even while other threads still hold it: closing its input
    /// is what stops it, and the shell does the rest.
    pub fn stop(&self) {
        self.input.lock().unwrap().take();
    }

    /// How it ended, once it has.
    pub fn exited(&self) -> Option<String> {
        let status = self.child.lock().unwrap().as_mut()?.try_wait().ok()??;
        Some(status.to_string())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
        if let Some(child) = self.child.lock().unwrap().take() {
            reap(child);
        }
    }
}

/// Waits for `child` off the caller's thread, so stopping never blocks home.
fn reap(mut child: Child) {
    thread::spawn(move || {
        let _ = child.wait();
    });
}

/// The first of `names` in `dir`, or on the PATH.
pub fn find(dir: &Option<PathBuf>, names: &[&str]) -> Result<PathBuf, String> {
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

pub fn expand_home(path: &str) -> PathBuf {
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
pub fn free_port() -> std::io::Result<u16> {
    Ok(TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?.local_addr()?.port())
}

/// Waits for the RPC server `rpc` to listen on `port`.
pub fn wait_for_port(port: u16, rpc: &Process, log: &Path) -> Result<(), String> {
    let deadline = Instant::now() + RPC_START;
    while Instant::now() < deadline {
        if let Some(status) = rpc.exited() {
            let why = last_error(log, rpc.log_from).unwrap_or_else(|| format!("llama.cpp's RPC server stopped ({status})"));
            return Err(format!("{why} (more in {})", log.display()));
        }
        if TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), Duration::from_millis(200)).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err("llama.cpp's RPC server didn't start".into())
}

/// Waits for `server` to finish loading and answer on `url`.
pub fn wait_for_health(url: &str, server: &Process, log: &Path) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let deadline = Instant::now() + LOAD;
    while Instant::now() < deadline {
        if let Some(status) = server.exited() {
            let why = last_error(log, server.log_from).unwrap_or_else(|| format!("llama-server stopped ({status})"));
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

/// How often a running server is checked for having stopped.
const WATCH: Duration = Duration::from_secs(1);

/// Waits until `process` stops, however long that is, and says why: its
/// last error in `log`, or how `name` ended.
pub fn watch(process: &Process, name: &str, log: &Path) -> String {
    loop {
        if let Some(status) = process.exited() {
            let why = last_error(log, process.log_from).unwrap_or_else(|| format!("{name} stopped ({status})"));
            return format!("{why} (more in {})", log.display());
        }
        thread::sleep(WATCH);
    }
}

/// The last line of `log` past byte `from` that sounds like an error, so
/// an earlier run's errors are never blamed for this one.
fn last_error(log: &Path, from: u64) -> Option<String> {
    let mut file = fs::File::open(log).ok()?;
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let line = text.lines().rev().find(|l| {
        let l = l.to_ascii_lowercase();
        l.contains("error") || l.contains("failed")
    })?;
    Some(line.trim().chars().take(200).collect())
}

/// How many bytes are in `dir` and everything under it.
pub fn size_of(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => size_of(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

/// This machine's memory, in bytes, or 0 when it can't tell.
pub fn memory() -> u64 {
    #[cfg(target_os = "macos")]
    {
        Command::new("sysctl")
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

/// A machine's memory as "16 GB RAM". It is all the machine has, not what
/// is free: llama.cpp splits layers by what it finds free when it loads.
pub fn ram(bytes: u64) -> String {
    format!("{} RAM", size(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hopchat-{name}-{}", crate::engine::new_file_id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ollama_folder() -> (PathBuf, PathBuf) {
        let root = scratch("pool");
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
    fn a_gguf_path_is_taken_as_it_is_spaces_and_all() {
        let (root, blob) = ollama_folder();
        let gguf = root.join("My Models").join("big model.gguf");
        fs::create_dir_all(gguf.parent().unwrap()).unwrap();
        fs::copy(&blob, &gguf).unwrap();
        assert_eq!(resolve(gguf.to_str().unwrap()).unwrap(), gguf);
        assert!(resolve(root.join("missing.gguf").to_str().unwrap()).is_err());
        let _ = fs::remove_dir_all(root);
    }

    fn alive(pid: &str) -> bool {
        Command::new("kill").args(["-0", pid]).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    }

    /// Closing a process's input is what happens when hopchat exits, however
    /// it exits; the program must go with it.
    #[test]
    fn a_process_dies_with_its_input() {
        let dir = scratch("guard");
        let pid_file = dir.join("pid");
        let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
        let process = Process::start(Path::new("/bin/sh"), ["-c", script.as_str()], &[], &dir.join("log")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let pid = loop {
            if let Ok(pid) = fs::read_to_string(&pid_file)
                && !pid.trim().is_empty()
            {
                break pid.trim().to_string();
            }
            assert!(Instant::now() < deadline, "the program never started");
            thread::sleep(Duration::from_millis(20));
        };
        assert!(alive(&pid));
        drop(process);
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(&pid) {
            assert!(Instant::now() < deadline, "the program outlived its input");
            thread::sleep(Duration::from_millis(20));
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_crash_is_seen_with_this_runs_error_only() {
        let dir = scratch("crash");
        let log = dir.join("log");
        fs::write(&log, "error: from a run long ago\n").unwrap();
        let process = Process::start(Path::new("/bin/sh"), ["-c", "echo loading; exit 3"], &[], &log).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while process.exited().is_none() {
            assert!(Instant::now() < deadline, "the crash was never seen");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(process.exited().unwrap().contains('3'));
        assert_eq!(last_error(&log, process.log_from), None);
        assert_eq!(last_error(&log, 0).as_deref(), Some("error: from a run long ago"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_watched_process_that_stops_says_why() {
        let dir = scratch("watch");
        let log = dir.join("log");
        let process = Process::start(Path::new("/bin/sh"), ["-c", "echo 'error: out of memory'; exit 1"], &[], &log).unwrap();
        let why = watch(&process, "llama-server", &log);
        assert!(why.starts_with("error: out of memory"), "{why}");
        let quiet = Process::start(Path::new("/bin/sh"), ["-c", "exit 2"], &[], &log).unwrap();
        assert!(watch(&quiet, "llama-server", &log).starts_with("llama-server stopped"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn sizes_read_like_people_say_them() {
        assert_eq!(size(17_179_869_184), "16 GB");
        assert_eq!(size(0), "? GB");
        assert_eq!(ram(17_179_869_184), "16 GB RAM");
    }
}
