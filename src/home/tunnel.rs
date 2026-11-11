//! The encrypted pipe between a pool's host and the machines lending it
//! their memory (see `pool`).
//!
//! llama.cpp's RPC protocol has no authentication and no encryption, so
//! its worker only ever listens on 127.0.0.1. Each end of a pool runs one
//! of these instead: the host's llama-server talks plain RPC to a local
//! [`forward`] listener, which carries it to the worker's [`serve`]
//! listener sealed with ChaCha20-Poly1305, and that hands it to the worker
//! on loopback.
//!
//! The 32-byte key is minted by the host for one worker and one pool, and
//! travels inside the MLS session like any hidden message, so only that
//! worker learns it. Every TCP connection then derives its own pair of
//! keys from it and a fresh nonce from each side, so frames from one
//! connection or direction can't be replayed into another. Nothing reaches
//! the worker's RPC server until a frame has been opened with the key.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;

pub type Secret = [u8; 32];

const MAGIC: &[u8; 14] = b"hopchat-pool/1";
const NONCE_BYTES: usize = 32;
/// Plaintext carried per frame: big enough that the tag is noise when
/// weights stream to a worker, small enough to stay cheap per token.
const CHUNK: usize = 256 * 1024;
const TAG: usize = 16;
/// How long the other end may take to say hello.
const HANDSHAKE_WAIT: Duration = Duration::from_secs(10);
const CONNECT_WAIT: Duration = Duration::from_secs(5);
/// How often a listener looks up from accepting to see if it was closed.
const ACCEPT_POLL: Duration = Duration::from_millis(100);
/// Streams a listener holds open at once. llama.cpp keeps one connection per
/// worker, so this only bounds someone holding half-open handshakes.
const MAX_OPEN: usize = 64;

/// A fresh random key.
pub fn secret() -> Secret {
    let mut key = [0u8; 32];
    let _ = getrandom::fill(&mut key);
    key
}

/// A listener and the connections it carries. Dropping it closes them all.
pub struct Listener {
    pub port: u16,
    conns: Arc<Conns>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.conns.close();
    }
}

/// The streams a listener carries, and whether it was closed.
#[derive(Default)]
struct Conns {
    closed: AtomicBool,
    /// Each with the connection it belongs to.
    open: Mutex<Vec<(u64, TcpStream)>>,
}

impl Conns {
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        for (_, stream) in self.open.lock().unwrap().drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    /// Holds a copy of `stream` for connection `id` so closing the listener
    /// closes it too. Checked under the same lock `close` drains with, so a
    /// stream kept while the listener closes is shut rather than missed.
    fn keep(&self, id: u64, stream: &TcpStream) -> io::Result<()> {
        let mut open = self.open.lock().unwrap();
        if self.closed.load(Ordering::SeqCst) {
            let _ = stream.shutdown(Shutdown::Both);
            return Err(io::Error::other("pool tunnel closed"));
        }
        open.push((id, stream.try_clone()?));
        Ok(())
    }

    /// Connection `id` is over: shuts and lets go of every stream kept for
    /// it. The socket itself can't be asked whether it has closed: one the
    /// other end closed first still names its peer.
    fn forget(&self, id: u64) {
        self.open.lock().unwrap().retain(|(of, stream)| {
            if *of == id {
                let _ = stream.shutdown(Shutdown::Both);
            }
            *of != id
        });
    }
}

/// One accepted connection, as its handler sees the listener.
struct Conn<'a> {
    conns: &'a Conns,
    id: u64,
}

impl Conn<'_> {
    /// Holds a copy of a stream this connection opened (see [`Conns::keep`]).
    fn keep(&self, stream: &TcpStream) -> io::Result<()> {
        self.conns.keep(self.id, stream)
    }
}

/// The worker's end: accepts sealed connections from `allow` only, on any
/// interface, and opens each onto `target` once it has proved the key.
pub fn serve(key: Secret, allow: IpAddr, target: SocketAddr) -> io::Result<Listener> {
    listen(Ipv4Addr::UNSPECIFIED, move |stream, conn| {
        if stream.peer_addr().ok().map(|a| a.ip()) != Some(allow) {
            return Ok(());
        }
        let (mut send, mut recv) = accept_handshake(&stream, &key)?;
        // The first frame proves the key before anything reaches the target.
        let mut reader = stream.try_clone()?;
        let first = read_frame(&mut reader, &mut recv)?;
        // An empty one is a probe: prove the key back, and reach nothing.
        // Real traffic never sends one (see `pipe`).
        if first.is_empty() {
            return write_frame(&mut reader, &mut send, &[]);
        }
        // After it, RPC may sit idle as long as it likes.
        stream.set_read_timeout(None)?;
        let mut local = TcpStream::connect_timeout(&target, CONNECT_WAIT)?;
        let _ = local.set_nodelay(true);
        conn.keep(&local)?;
        local.write_all(&first)?;
        pipe(stream, local, send, recv);
        Ok(())
    })
}

/// The host's end: accepts plain connections on loopback and carries each
/// to the worker at `remote`, sealed.
pub fn forward(key: Secret, remote: SocketAddr) -> io::Result<Listener> {
    listen(Ipv4Addr::LOCALHOST, move |local, conn| {
        let sealed = TcpStream::connect_timeout(&remote, CONNECT_WAIT)?;
        let _ = sealed.set_nodelay(true);
        conn.keep(&sealed)?;
        let (send, recv) = offer_handshake(&sealed, &key)?;
        pipe(sealed, local, send, recv);
        Ok(())
    })
}

/// Proves the worker's tunnel at `remote` is reachable from here, lets this
/// machine in, and holds the same key, without anything reaching its RPC
/// server: an empty sealed frame goes each way, and the worker opens the
/// RPC server only for a frame with something in it.
pub fn probe(key: Secret, remote: SocketAddr) -> io::Result<()> {
    let stream = TcpStream::connect_timeout(&remote, CONNECT_WAIT)?;
    let result = probe_on(&stream, &key);
    let _ = stream.shutdown(Shutdown::Both);
    result
}

fn probe_on(stream: &TcpStream, key: &Secret) -> io::Result<()> {
    let quiet = |e: io::Error| match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => io::Error::other("it didn't answer"),
        _ => e,
    };
    let (mut send, mut recv) = offer_handshake(stream, key).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset => {
            io::Error::other("it turned this machine away")
        }
        _ => quiet(e),
    })?;
    let mut s = stream.try_clone()?;
    s.set_read_timeout(Some(HANDSHAKE_WAIT))?;
    // Past the handshake, only a key that differs closes it.
    let keyed = |e: io::Error| match e.kind() {
        io::ErrorKind::UnexpectedEof
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::InvalidData => io::Error::other("its pool key doesn't match the one sent to it"),
        _ => quiet(e),
    };
    write_frame(&mut s, &mut send, &[]).map_err(keyed)?;
    read_frame(&mut s, &mut recv).map_err(keyed)?;
    Ok(())
}

fn listen(
    on: Ipv4Addr,
    handle: impl Fn(TcpStream, &Conn) -> io::Result<()> + Send + Sync + 'static,
) -> io::Result<Listener> {
    let listener = TcpListener::bind((on, 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let conns: Arc<Conns> = Arc::default();
    let handle = Arc::new(handle);
    {
        let conns = conns.clone();
        thread::spawn(move || {
            let mut id = 0u64;
            while !conns.closed.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        id += 1;
                        let full = conns.open.lock().unwrap().len() >= MAX_OPEN;
                        if full || conns.keep(id, &stream).is_err() {
                            let _ = stream.shutdown(Shutdown::Both);
                            continue;
                        }
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_nodelay(true);
                        let (handle, conns) = (handle.clone(), conns.clone());
                        thread::spawn(move || {
                            // Finished, refused or failed: either way it's over,
                            // and the copies kept for it mustn't hold it open.
                            let _ = handle(stream, &Conn { conns: &conns, id });
                            conns.forget(id);
                        });
                    }
                    Err(_) => thread::sleep(ACCEPT_POLL),
                }
            }
        });
    }
    Ok(Listener { port, conns })
}

/// One direction's key and frame counter.
struct Seal {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl Seal {
    fn new(key: &[u8; 32]) -> Self {
        Self { cipher: ChaCha20Poly1305::new(Key::from_slice(key)), counter: 0 }
    }

    /// Each key is used for one connection and direction only, so a
    /// counter is a nonce that never repeats.
    fn nonce(&mut self) -> io::Result<Nonce> {
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&self.counter.to_be_bytes());
        self.counter = self.counter.checked_add(1).ok_or_else(|| io::Error::other("pool frame counter ran out"))?;
        Ok(*Nonce::from_slice(&nonce))
    }

    fn seal(&mut self, plain: &[u8]) -> io::Result<Vec<u8>> {
        let nonce = self.nonce()?;
        self.cipher.encrypt(&nonce, plain).map_err(|_| io::Error::other("pool frame not sealed"))
    }

    fn open(&mut self, sealed: &[u8]) -> io::Result<Vec<u8>> {
        let nonce = self.nonce()?;
        self.cipher
            .decrypt(&nonce, sealed)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "pool frame failed to open: wrong key or tampered"))
    }
}

/// The keys for host-to-worker and worker-to-host frames.
fn keys(key: &Secret, host: &[u8], worker: &[u8]) -> (Seal, Seal) {
    let salt = [host, worker].concat();
    let hk = Hkdf::<Sha256>::new(Some(&salt), key);
    let mut up = [0u8; 32];
    let mut down = [0u8; 32];
    // 32 bytes is far under HKDF's limit, so expand can't fail.
    let _ = hk.expand(b"hopchat-pool host->worker", &mut up);
    let _ = hk.expand(b"hopchat-pool worker->host", &mut down);
    (Seal::new(&up), Seal::new(&down))
}

/// The host says hello; returns its sending and receiving seals.
fn offer_handshake(stream: &TcpStream, key: &Secret) -> io::Result<(Seal, Seal)> {
    let mut host = [0u8; NONCE_BYTES];
    getrandom::fill(&mut host).map_err(|e| io::Error::other(e.to_string()))?;
    let mut s = stream;
    s.write_all(&[MAGIC.as_slice(), &host].concat())?;
    let mut worker = [0u8; NONCE_BYTES];
    stream.set_read_timeout(Some(HANDSHAKE_WAIT))?;
    s.read_exact(&mut worker)?;
    stream.set_read_timeout(None)?;
    let (up, down) = keys(key, &host, &worker);
    Ok((up, down))
}

/// The worker answers hello; returns its sending and receiving seals.
fn accept_handshake(stream: &TcpStream, key: &Secret) -> io::Result<(Seal, Seal)> {
    stream.set_read_timeout(Some(HANDSHAKE_WAIT))?;
    let mut s = stream;
    let mut hello = [0u8; MAGIC.len() + NONCE_BYTES];
    s.read_exact(&mut hello)?;
    if &hello[..MAGIC.len()] != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a hopchat pool connection"));
    }
    let mut worker = [0u8; NONCE_BYTES];
    getrandom::fill(&mut worker).map_err(|e| io::Error::other(e.to_string()))?;
    s.write_all(&worker)?;
    let (up, down) = keys(key, &hello[MAGIC.len()..], &worker);
    // The read timeout stays: the host's first frame must come soon too.
    Ok((down, up))
}

fn read_frame(stream: &mut TcpStream, seal: &mut Seal) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if !(TAG..=CHUNK + TAG).contains(&len) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "pool frame of a wrong size"));
    }
    let mut sealed = vec![0u8; len];
    stream.read_exact(&mut sealed)?;
    seal.open(&sealed)
}

fn write_frame(stream: &mut TcpStream, seal: &mut Seal, plain: &[u8]) -> io::Result<()> {
    let sealed = seal.seal(plain)?;
    let mut frame = Vec::with_capacity(4 + sealed.len());
    frame.extend_from_slice(&(sealed.len() as u32).to_be_bytes());
    frame.extend_from_slice(&sealed);
    stream.write_all(&frame)
}

/// Carries bytes both ways until either side closes: plain from `local`
/// sealed onto `sealed`, and frames from `sealed` opened onto `local`.
fn pipe(sealed: TcpStream, local: TcpStream, mut send: Seal, mut recv: Seal) {
    let (Ok(mut sealed_in), Ok(mut local_out)) = (sealed.try_clone(), local.try_clone()) else {
        let _ = sealed.shutdown(Shutdown::Both);
        let _ = local.shutdown(Shutdown::Both);
        return;
    };
    let close = |a: &TcpStream, b: &TcpStream| {
        let _ = a.shutdown(Shutdown::Both);
        let _ = b.shutdown(Shutdown::Both);
    };
    let inbound = thread::spawn(move || {
        while let Ok(plain) = read_frame(&mut sealed_in, &mut recv) {
            if local_out.write_all(&plain).is_err() {
                break;
            }
        }
        close(&sealed_in, &local_out);
    });
    let (mut sealed_out, mut local_in) = (sealed, local);
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match local_in.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if write_frame(&mut sealed_out, &mut send, &buf[..n]).is_err() {
            break;
        }
    }
    close(&sealed_out, &local_in);
    let _ = inbound.join();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for the RPC server: echoes everything back, reversed in
    /// chunks so the test sees it really went through.
    fn echo() -> SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let at = listener.local_addr().unwrap();
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = stream.read(&mut buf) {
                        if n == 0 || stream.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        at
    }

    fn worker(key: Secret) -> (Listener, SocketAddr) {
        let worker = serve(key, IpAddr::V4(Ipv4Addr::LOCALHOST), echo()).unwrap();
        let at = SocketAddr::from((Ipv4Addr::LOCALHOST, worker.port));
        (worker, at)
    }

    fn round_trip(port: u16, payload: &[u8]) -> io::Result<Vec<u8>> {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut writer = stream.try_clone()?;
        let payload_owned = payload.to_vec();
        let writing = thread::spawn(move || writer.write_all(&payload_owned));
        let mut back = vec![0u8; payload.len()];
        stream.read_exact(&mut back)?;
        writing.join().unwrap()?;
        Ok(back)
    }

    #[test]
    fn carries_bytes_both_ways_over_several_connections() {
        let key = secret();
        let (_worker, at) = worker(key);
        let host = forward(key, at).unwrap();
        // Bigger than one frame, so it is split and put back together.
        let payload: Vec<u8> = (0..(CHUNK * 2 + 123)).map(|i| (i % 251) as u8).collect();
        assert_eq!(round_trip(host.port, &payload).unwrap(), payload);
        assert_eq!(round_trip(host.port, b"second connection").unwrap(), b"second connection");
    }

    #[test]
    fn a_wrong_key_reaches_nothing() {
        let (_worker, at) = worker(secret());
        let host = forward(secret(), at).unwrap();
        assert!(round_trip(host.port, b"let me in").is_err());
    }

    #[test]
    fn plain_rpc_is_refused() {
        let (_worker, at) = worker(secret());
        let mut stream = TcpStream::connect(at).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = stream.write_all(&[0u8; 64]);
        let mut buf = [0u8; 64];
        // Closed without a word of RPC coming back.
        assert!(matches!(stream.read(&mut buf), Ok(0) | Err(_)));
    }

    #[test]
    fn only_the_host_may_connect() {
        let key = secret();
        let elsewhere = IpAddr::V4(Ipv4Addr::new(10, 255, 255, 1));
        let worker = serve(key, elsewhere, echo()).unwrap();
        let host = forward(key, SocketAddr::from((Ipv4Addr::LOCALHOST, worker.port))).unwrap();
        assert!(round_trip(host.port, b"not from the host").is_err());
    }

    #[test]
    fn dropping_a_listener_closes_its_connections() {
        let key = secret();
        let (worker, at) = worker(key);
        let host = forward(key, at).unwrap();
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, host.port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).unwrap();
        drop(worker);
        let _ = stream.write_all(b"ping");
        assert!(matches!(stream.read(&mut buf), Ok(0) | Err(_)));
    }

    /// llama-server opens a connection to each worker again and again over
    /// a pool's life. Ones that have closed mustn't use up the listener,
    /// whichever end closed them first.
    #[test]
    fn closed_connections_dont_fill_a_listener() {
        let key = secret();
        let (worker, at) = worker(key);
        let host = forward(key, at).unwrap();
        // A few at a time, so the listeners are never full of open ones.
        for round in 0..MAX_OPEN / 4 {
            thread::scope(|scope| {
                for i in 0..8 {
                    let port = host.port;
                    scope.spawn(move || {
                        let payload = format!("connection {round}.{i}");
                        let back = round_trip(port, payload.as_bytes()).unwrap_or_else(|e| panic!("{payload}: {e}"));
                        assert_eq!(back, payload.as_bytes());
                    });
                }
            });
        }
        // And once they have all closed, nothing is still held for them.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !host.conns.open.lock().unwrap().is_empty() || !worker.conns.open.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < deadline, "closed connections are still held");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn frames_cannot_be_replayed_across_directions() {
        let key = secret();
        let (mut up, mut down) = keys(&key, &[1; 32], &[2; 32]);
        let sealed = up.seal(b"hello").unwrap();
        assert!(down.open(&sealed).is_err());
    }

    #[test]
    fn frames_cannot_be_replayed_across_connections() {
        let key = secret();
        // The same direction under the same pool key, one nonce apart.
        let (mut first, _) = keys(&key, &[1; 32], &[2; 32]);
        let (mut second, _) = keys(&key, &[1; 32], &[3; 32]);
        let sealed = first.seal(b"hello").unwrap();
        assert!(second.open(&sealed).is_err());
        // Whereas the right connection opens it, which shows the test can fail.
        let (_, mut worker_recv) = {
            let (up, down) = keys(&key, &[1; 32], &[2; 32]);
            (down, up)
        };
        assert_eq!(worker_recv.open(&sealed).unwrap(), b"hello");
    }

    #[test]
    fn a_probe_reaches_the_tunnel_but_not_the_rpc_server() {
        let key = secret();
        let rpc = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        rpc.set_nonblocking(true).unwrap();
        let worker = serve(key, IpAddr::V4(Ipv4Addr::LOCALHOST), rpc.local_addr().unwrap()).unwrap();
        probe(key, SocketAddr::from((Ipv4Addr::LOCALHOST, worker.port))).unwrap();
        thread::sleep(Duration::from_millis(200));
        assert!(rpc.accept().is_err(), "a probe must not reach the RPC server");
    }

    #[test]
    fn a_probe_from_the_wrong_address_is_turned_away() {
        let key = secret();
        let worker = serve(key, IpAddr::V4(Ipv4Addr::new(10, 255, 255, 1)), echo()).unwrap();
        let error = probe(key, SocketAddr::from((Ipv4Addr::LOCALHOST, worker.port))).unwrap_err();
        assert!(error.to_string().contains("turned this machine away"), "{error}");
    }

    #[test]
    fn a_probe_with_another_key_is_told_so() {
        let (_worker, at) = worker(secret());
        let error = probe(secret(), at).unwrap_err();
        assert!(error.to_string().contains("key doesn't match"), "{error}");
    }

    #[test]
    fn frames_of_a_wrong_size_are_refused() {
        let key = secret();
        // Both ends of the host-to-worker direction.
        let (mut send, _) = keys(&key, &[1; 32], &[2; 32]);
        let (mut recv, _) = keys(&key, &[1; 32], &[2; 32]);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut writer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut reader, _) = listener.accept().unwrap();
        // Too short to hold a tag, and too long for a chunk.
        for len in [TAG as u32 - 1, (CHUNK + TAG) as u32 + 1] {
            writer.write_all(&len.to_be_bytes()).unwrap();
            let error = read_frame(&mut reader, &mut recv).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
        // Whereas the biggest frame there is gets through whole.
        let big = vec![7u8; CHUNK];
        let writing = thread::spawn(move || write_frame(&mut writer, &mut send, &big));
        assert_eq!(read_frame(&mut reader, &mut recv).unwrap(), vec![7u8; CHUNK]);
        writing.join().unwrap().unwrap();
    }

    #[test]
    fn a_probe_to_nothing_fails() {
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        assert!(probe(secret(), SocketAddr::from((Ipv4Addr::LOCALHOST, port))).is_err());
    }
}
