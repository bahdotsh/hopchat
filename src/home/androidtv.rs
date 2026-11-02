//! Android TV and Google TV sets (Sony, TCL, Xiaomi, Philips and the
//! rest): the remote-control protocol the Google TV phone app speaks,
//! protobuf messages over TLS. Pairing runs on port 6467: the TV shows a
//! six-character code, the person types it with `/home code`, and the TV
//! remembers this machine's certificate. Commands then go to port 6466.
//!
//! Both sides present self-signed certificates. hopchat makes one per TV
//! when it pairs, and pins the certificate the TV shows then; a later
//! connection presenting a different one is refused.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::str::FromStr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use native_tls::{Identity, TlsConnector, TlsStream};
use rsa::pkcs8::{DecodePublicKey, EncodePrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use x509_cert::builder::{Builder, CertificateBuilder, Profile};
use x509_cert::der::{Decode, Encode, EncodePem};
use x509_cert::ext::pkix::BasicConstraints;
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::spki::SubjectPublicKeyInfoOwned;
use x509_cert::time::Validity;

use super::discover::{self, format_mac, mac_of, parse_mac};
use super::driver::{Backend, Category, Found, PairHelp};
use super::tv::TvDriver;
use super::{Device, State};

const REMOTE_PORT: u16 = 6466;
const PAIR_PORT: u16 = 6467;
const SERVICE: &str = "_androidtvremote2._tcp.local.";
const CONNECT: Duration = Duration::from_secs(3);
/// How long a TV takes to say hello on a fresh connection.
const HELLO: Duration = Duration::from_secs(5);
/// Each read waits this long, so a session can keep an eye on its deadline.
const TICK: Duration = Duration::from_millis(200);
/// How long someone has to read the code off the TV and type it.
const PAIRING: Duration = Duration::from_secs(60);
/// All of a pairing: the code, then the TV taking its time to trust us.
const PAIRING_TAKES: Duration = Duration::from_secs(90);
/// How long a scan listens for TVs announcing themselves.
const DISCOVER: Duration = Duration::from_secs(3);
/// How long to wait for the TV to report a change it was asked for.
const SETTLE: Duration = Duration::from_secs(4);
/// How long after saying it's ready a TV takes to report its volume.
const VOLUME_REPORT: Duration = Duration::from_millis(700);
/// Between volume steps: a TV drops key presses that come too fast.
const STEP: Duration = Duration::from_millis(60);
/// The longest message any TV sends; a bigger length is a broken stream.
const MAX_MESSAGE: usize = 64 * 1024;

// What this remote can do, in the bits the TV uses: ping, keys, power,
// volume and app links.
const FEATURES: u64 = 1 | 2 | 32 | 64 | 512;

const KEY_POWER: u64 = 26;
const KEY_VOLUME_UP: u64 = 24;
const KEY_VOLUME_DOWN: u64 = 25;
const KEY_MUTE: u64 = 164;
const KEY_HOME: u64 = 3;
const KEY_TV: u64 = 170;
const KEY_HDMI_1: u64 = 243;
/// A press and release, as opposed to holding a key.
const SHORT: u64 = 3;

/// Spoken app names to the links that open them. Apps register web links
/// with the TV, and those open them; some firmware (Xiaomi's among it)
/// refuses the `market://launch` links that name a package. Anything else
/// that looks like a package name is still tried as one.
const APPS: &[(&str, &str)] = &[
    ("youtube music", "https://music.youtube.com"),
    ("youtube", "https://www.youtube.com"),
    ("netflix", "https://www.netflix.com/title"),
    ("prime video", "https://app.primevideo.com"),
    ("amazon", "https://app.primevideo.com"),
    ("disney+", "https://www.disneyplus.com"),
    ("disney", "https://www.disneyplus.com"),
    ("spotify", "spotify://"),
];
/// How long the TV takes to refuse a link it can't open.
const REFUSAL: Duration = Duration::from_millis(1500);

const MEDIA: &[(&str, u64)] = &[("play", 126), ("pause", 127), ("stop", 86), ("rewind", 89), ("fastForward", 90)];

// ----- protobuf, by hand ------------------------------------------------------

/// A protobuf message being written: fields in the order they're added.
#[derive(Default, Clone, Debug, PartialEq)]
struct Pb(Vec<u8>);

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

impl Pb {
    fn int(mut self, field: u32, v: u64) -> Pb {
        put_varint(&mut self.0, u64::from(field) << 3);
        put_varint(&mut self.0, v);
        self
    }

    fn bytes(mut self, field: u32, v: &[u8]) -> Pb {
        put_varint(&mut self.0, (u64::from(field) << 3) | 2);
        put_varint(&mut self.0, v.len() as u64);
        self.0.extend_from_slice(v);
        self
    }

    fn text(self, field: u32, v: &str) -> Pb {
        self.bytes(field, v.as_bytes())
    }

    fn msg(self, field: u32, m: Pb) -> Pb {
        self.bytes(field, &m.0)
    }

    /// As sent: its length first.
    fn framed(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.0.len() + 3);
        put_varint(&mut out, self.0.len() as u64);
        out.extend_from_slice(&self.0);
        out
    }
}

/// A protobuf message read back: each field's number and value.
#[derive(Debug, Default, PartialEq)]
struct Fields(Vec<(u32, Field)>);

#[derive(Debug, PartialEq)]
enum Field {
    Int(u64),
    Bytes(Vec<u8>),
}

/// A varint at the start of `b`, and how many bytes it took; `None` if
/// `b` ends first.
fn take_varint(b: &[u8]) -> Result<Option<(u64, usize)>, String> {
    let mut v = 0u64;
    for (i, byte) in b.iter().enumerate() {
        if i >= 10 {
            return Err("the TV sent a broken message".into());
        }
        v |= u64::from(byte & 0x7F) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(Some((v, i + 1)));
        }
    }
    Ok(None)
}

impl Fields {
    fn parse(mut b: &[u8]) -> Result<Fields, String> {
        let broken = || "the TV sent a broken message".to_string();
        let mut out = Vec::new();
        while !b.is_empty() {
            let (key, n) = take_varint(b)?.ok_or_else(broken)?;
            b = &b[n..];
            let field = (key >> 3) as u32;
            match key & 7 {
                0 => {
                    let (v, n) = take_varint(b)?.ok_or_else(broken)?;
                    out.push((field, Field::Int(v)));
                    b = &b[n..];
                }
                2 => {
                    let (len, n) = take_varint(b)?.ok_or_else(broken)?;
                    let end = n.checked_add(len as usize).filter(|e| *e <= b.len()).ok_or_else(broken)?;
                    out.push((field, Field::Bytes(b[n..end].to_vec())));
                    b = &b[end..];
                }
                1 => b = b.get(8..).ok_or_else(broken)?,
                5 => b = b.get(4..).ok_or_else(broken)?,
                _ => return Err(broken()),
            }
        }
        Ok(Fields(out))
    }

    fn int(&self, field: u32) -> Option<u64> {
        self.0.iter().rev().find_map(|(f, v)| match v {
            Field::Int(i) if *f == field => Some(*i),
            _ => None,
        })
    }

    fn bytes(&self, field: u32) -> Option<&[u8]> {
        self.0.iter().rev().find_map(|(f, v)| match v {
            Field::Bytes(b) if *f == field => Some(b.as_slice()),
            _ => None,
        })
    }

    fn text(&self, field: u32) -> Option<String> {
        self.bytes(field).map(|b| String::from_utf8_lossy(b).into_owned())
    }

    fn msg(&self, field: u32) -> Option<Fields> {
        self.bytes(field).and_then(|b| Fields::parse(b).ok())
    }

    fn has(&self, field: u32) -> bool {
        self.0.iter().any(|(f, _)| *f == field)
    }
}

// ----- certificates --------------------------------------------------------------

/// A new identity for this machine to present: a self-signed RSA
/// certificate and its key, both PEM, the key PKCS#8 as native-tls wants.
/// It carries no key-usage extension: TVs refuse a client certificate
/// whose key is marked for signing certificates only, which is what a
/// root profile would mark it.
fn new_identity() -> Result<(String, String), String> {
    let mut rng = rand_core::OsRng;
    let key = RsaPrivateKey::new(&mut rng, 2048).map_err(|e| e.to_string())?;
    let signer = rsa::pkcs1v15::SigningKey::<Sha256>::new(key.clone());
    let public = SubjectPublicKeyInfoOwned::from_key(key.to_public_key()).map_err(|e| e.to_string())?;
    let subject = Name::from_str("CN=hopchat").map_err(|e| e.to_string())?;
    let validity = Validity::from_now(Duration::from_secs(20 * 365 * 24 * 60 * 60)).map_err(|e| e.to_string())?;
    let serial = SerialNumber::from(u32::from_be_bytes(random()) >> 1);
    let mut builder = CertificateBuilder::new(Profile::Manual { issuer: None }, serial, validity, subject, public, &signer)
        .map_err(|e| e.to_string())?;
    builder
        .add_extension(&BasicConstraints { ca: true, path_len_constraint: None })
        .map_err(|e| e.to_string())?;
    let cert = builder.build::<rsa::pkcs1v15::Signature>().map_err(|e| e.to_string())?;
    let cert = cert.to_pem(LineEnding::LF).map_err(|e| e.to_string())?;
    let key = key.to_pkcs8_pem(LineEnding::LF).map_err(|e| e.to_string())?;
    Ok((cert, key.to_string()))
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    let _ = getrandom::fill(&mut bytes);
    bytes
}

/// The RSA key inside a DER certificate.
fn public_key(der: &[u8]) -> Result<RsaPublicKey, String> {
    let cert = x509_cert::Certificate::from_der(der).map_err(|e| format!("unreadable certificate: {e}"))?;
    let spki = cert.tbs_certificate.subject_public_key_info.to_der().map_err(|e| e.to_string())?;
    RsaPublicKey::from_public_key_der(&spki).map_err(|_| "the certificate's key isn't RSA".to_string())
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    base64::engine::general_purpose::STANDARD.decode(body).map_err(|e| e.to_string())
}

/// What proves to the TV that the person typed the code it showed: both
/// sides' keys and the code's last four characters, hashed. The code's
/// first two characters are the hash's first byte, so a mistyped code is
/// caught before anything is sent.
fn secret(client: &RsaPublicKey, server: &RsaPublicKey, code: &str) -> Result<[u8; 32], String> {
    let code = code.trim().to_ascii_uppercase();
    let bytes = (code.len() == 6 && code.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| (0..3).map(|i| u8::from_str_radix(&code[i * 2..i * 2 + 2], 16).unwrap()).collect::<Vec<u8>>())
        .ok_or("the code is six letters and digits, 0-9 and A-F")?;
    let mut hash = Sha256::new();
    for key in [client, server] {
        hash.update(key.n().to_bytes_be());
        hash.update(key.e().to_bytes_be());
    }
    hash.update(&bytes[1..]);
    let hash: [u8; 32] = hash.finalize().into();
    if hash[0] != bytes[0] {
        return Err("that isn't the code on the TV; check it and type it again".into());
    }
    Ok(hash)
}

fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der).iter().map(|b| format!("{b:02x}")).collect()
}

/// native-tls on macOS builds a keychain for each identity, so each is
/// made once.
fn identity(cert: &str, key: &str) -> Result<Identity, String> {
    static MADE: OnceLock<Mutex<HashMap<String, Identity>>> = OnceLock::new();
    let made = MADE.get_or_init(Default::default);
    let mut made = made.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(id) = made.get(cert) {
        return Ok(id.clone());
    }
    let id = Identity::from_pkcs8(cert.as_bytes(), key.as_bytes()).map_err(|e| format!("can't use this machine's certificate: {e}"))?;
    made.insert(cert.to_string(), id.clone());
    Ok(id)
}

/// A TLS connection presenting `cert`. The TV's certificate is
/// self-signed, so it isn't checked against anything here; the caller
/// pins it.
fn connect(to: SocketAddr, cert: &str, key: &str) -> Result<TlsStream<TcpStream>, String> {
    let tcp = TcpStream::connect_timeout(&to, CONNECT).map_err(|_| format!("the TV at {} isn't answering", to.ip()))?;
    tcp.set_read_timeout(Some(HELLO)).map_err(|e| e.to_string())?;
    tcp.set_write_timeout(Some(CONNECT)).map_err(|e| e.to_string())?;
    let _ = tcp.set_nodelay(true);
    let connector = TlsConnector::builder()
        .identity(identity(cert, key)?)
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
        .map_err(|e| e.to_string())?;
    let tls = connector.connect(&to.ip().to_string(), tcp).map_err(|e| match e {
        native_tls::HandshakeError::Failure(e) => format!("the TV refused the connection: {e}"),
        native_tls::HandshakeError::WouldBlock(_) => "the TV did not finish connecting".into(),
    })?;
    tls.get_ref().set_read_timeout(Some(TICK)).map_err(|e| e.to_string())?;
    Ok(tls)
}

fn peer_der(tls: &TlsStream<TcpStream>) -> Result<Vec<u8>, String> {
    let cert = tls.peer_certificate().map_err(|e| e.to_string())?.ok_or("the TV showed no certificate")?;
    cert.to_der().map_err(|e| e.to_string())
}

/// Reads length-prefixed messages off a stream that times out every
/// [`TICK`], keeping what arrives in between.
struct Wire<S> {
    stream: S,
    buf: Vec<u8>,
}

impl<S: Read + Write> Wire<S> {
    fn send(&mut self, m: &Pb) -> Result<(), String> {
        self.stream.write_all(&m.framed()).and_then(|()| self.stream.flush()).map_err(|e| e.to_string())
    }

    /// The next message, or `None` once `until` passes.
    fn next(&mut self, until: Instant) -> Result<Option<Fields>, String> {
        loop {
            if let Some((len, n)) = take_varint(&self.buf)? {
                let len = usize::try_from(len).ok().filter(|l| *l <= MAX_MESSAGE).ok_or("the TV sent a broken message")?;
                if self.buf.len() >= n + len {
                    let message = Fields::parse(&self.buf[n..n + len])?;
                    self.buf.drain(..n + len);
                    return Ok(Some(message));
                }
            }
            if Instant::now() >= until {
                return Ok(None);
            }
            let mut chunk = [0u8; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err("the TV hung up".into()),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

// ----- pairing -------------------------------------------------------------------------

/// The pairing protocol's envelope, with what goes inside at `field`.
fn polo(field: u32, inner: Pb) -> Pb {
    Pb::default().int(1, 2).int(2, 200).msg(field, inner)
}

/// Six hexadecimal characters, the encoding every TV accepts.
fn hex_encoding() -> Pb {
    Pb::default().int(1, 3).int(2, 6)
}

/// Pairings waiting for a code, by the TV's address: where to send it, and
/// where the pairing says whether it was the right one.
type Waiting = HashMap<SocketAddr, Sender<(String, Sender<Result<(), String>>)>>;

fn waiting() -> &'static Mutex<Waiting> {
    static WAITING: OnceLock<Mutex<Waiting>> = OnceLock::new();
    WAITING.get_or_init(Default::default)
}

/// Takes a pairing off the waiting list however it ends.
struct Unwait(SocketAddr);

impl Drop for Unwait {
    fn drop(&mut self) {
        waiting().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

/// Hands `code` to the TV pairing at `to`, and says whether it was right.
fn give_code_to(to: SocketAddr, code: &str) -> Result<(), String> {
    let send = waiting().lock().unwrap_or_else(|e| e.into_inner()).get(&to).cloned();
    let send = send.ok_or("that TV isn't waiting for a code")?;
    let (answer, verdict) = mpsc::channel();
    send.send((code.to_string(), answer)).map_err(|_| "that TV stopped pairing")?;
    verdict.recv_timeout(Duration::from_secs(2)).map_err(|_| "the TV did not answer".to_string())?
}

/// `/home code CODE`: the code goes to the one TV waiting for it.
pub fn give_code(code: Option<&str>) -> String {
    let Some(code) = code else { return "type the code the TV shows: /home code CODE".into() };
    let pairing: Vec<SocketAddr> = waiting().lock().unwrap_or_else(|e| e.into_inner()).keys().copied().collect();
    match pairing.as_slice() {
        [] => "no TV is waiting for a code; press Enter on it in /devices first".into(),
        [to] => match give_code_to(*to, code) {
            Ok(()) => "code accepted; finishing pairing…".into(),
            Err(e) => e,
        },
        _ => "more than one TV is pairing; pair them one at a time".into(),
    }
}

/// Runs the pairing protocol with the TV at `to`, presenting `cert`. Waits
/// for the code the TV shows to come through [`give_code`].
fn pair_with(to: SocketAddr, cert: &str, key: &str) -> Result<(), String> {
    let tls = connect(to, cert, key)?;
    let server = public_key(&peer_der(&tls)?)?;
    let client = public_key(&pem_to_der(cert)?)?;
    let mut wire = Wire { stream: tls, buf: Vec::new() };
    let answer = |wire: &mut Wire<TlsStream<TcpStream>>, field: u32| -> Result<Fields, String> {
        let message = wire.next(Instant::now() + HELLO)?.ok_or("the TV stopped answering")?;
        match message.int(2) {
            Some(200) => {}
            Some(402) => return Err("the TV didn't accept the code".into()),
            Some(code) => return Err(format!("the TV refused to pair (status {code})")),
            None => return Err("the TV sent a broken message".into()),
        }
        if !message.has(field) {
            return Err("the TV answered something unexpected".into());
        }
        Ok(message)
    };
    wire.send(&polo(10, Pb::default().text(1, "atvremote").text(2, "hopchat")))?;
    answer(&mut wire, 11)?;
    wire.send(&polo(20, Pb::default().msg(1, hex_encoding()).int(3, 1)))?;
    answer(&mut wire, 20)?;
    wire.send(&polo(30, Pb::default().msg(1, hex_encoding()).int(2, 1)))?;
    answer(&mut wire, 31)?;

    // The TV shows its code now.
    let (codes, inbox): (Sender<_>, Receiver<_>) = mpsc::channel();
    waiting().lock().unwrap_or_else(|e| e.into_inner()).insert(to, codes);
    let _unwait = Unwait(to);
    let until = Instant::now() + PAIRING;
    let hash = loop {
        let left = until.checked_duration_since(Instant::now()).ok_or("nobody typed the code in time")?;
        let (code, verdict): (String, Sender<Result<(), String>>) =
            inbox.recv_timeout(left).map_err(|_| "nobody typed the code in time")?;
        match secret(&client, &server, &code) {
            Ok(hash) => {
                let _ = verdict.send(Ok(()));
                break hash;
            }
            Err(e) => {
                let _ = verdict.send(Err(e));
            }
        }
    };
    wire.send(&polo(40, Pb::default().bytes(1, &hash)))?;
    answer(&mut wire, 41)?;
    Ok(())
}

// ----- the remote --------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
struct Volume {
    level: u64,
    max: u64,
    muted: bool,
}

/// A connection on the remote port, keeping up with what the TV reports.
struct Remote<S> {
    wire: Wire<S>,
    /// Whether the screen is on, once the TV has said.
    on: Option<bool>,
    volume: Option<Volume>,
    /// Maker and model, from the TV's hello.
    model: Option<String>,
    /// What both sides can do, as agreed in the hello.
    features: u64,
}

fn remote(field: u32, inner: Pb) -> Pb {
    Pb::default().msg(field, inner)
}

impl<S: Read + Write> Remote<S> {
    /// Answers the TV's hello until it says it's ready.
    fn start(stream: S) -> Result<Remote<S>, String> {
        let mut r = Remote { wire: Wire { stream, buf: Vec::new() }, on: None, volume: None, model: None, features: FEATURES };
        r.pump(Instant::now() + HELLO, |r| r.on.is_some())?;
        if r.on.is_none() {
            return Err("the TV never said it was ready".into());
        }
        // Its volume follows in a message of its own; one that's off may
        // never send it.
        r.pump(Instant::now() + VOLUME_REPORT, |r| r.volume.is_some())?;
        Ok(r)
    }

    /// Handles what the TV sends until `done` or `until`; whether `done`.
    fn pump(&mut self, until: Instant, done: impl Fn(&Self) -> bool) -> Result<bool, String> {
        while !done(self) {
            let Some(message) = self.wire.next(until)? else { return Ok(false) };
            self.handle(&message)?;
        }
        Ok(true)
    }

    fn handle(&mut self, m: &Fields) -> Result<(), String> {
        if let Some(configure) = m.msg(1) {
            if let Some(info) = configure.msg(2) {
                let said: Vec<String> = [info.text(2), info.text(1)].into_iter().flatten().filter(|s| !s.is_empty()).collect();
                self.model = Some(said.join(" ")).filter(|s| !s.is_empty());
            }
            self.features = configure.int(1).unwrap_or(FEATURES) & FEATURES;
            let info = Pb::default().int(3, 1).text(4, "1").text(5, "atvremote").text(6, "1.0.0");
            self.wire.send(&remote(1, Pb::default().int(1, self.features).msg(2, info)))?;
        } else if m.has(2) {
            self.wire.send(&remote(2, Pb::default().int(1, self.features)))?;
        } else if let Some(ping) = m.msg(8) {
            self.wire.send(&remote(9, Pb::default().int(1, ping.int(1).unwrap_or(0))))?;
        } else if let Some(start) = m.msg(40) {
            self.on = Some(start.int(1).unwrap_or(0) != 0);
        } else if let Some(volume) = m.msg(50) {
            self.volume = Some(Volume {
                level: volume.int(7).unwrap_or(0),
                max: volume.int(6).unwrap_or(0),
                muted: volume.int(8).unwrap_or(0) != 0,
            });
        } else if m.has(3) {
            return Err("the TV reported an error".into());
        }
        Ok(())
    }

    fn key(&mut self, code: u64) -> Result<(), String> {
        self.wire.send(&remote(10, Pb::default().int(1, code).int(2, SHORT)))
    }

    /// Opens `link`, and waits long enough to hear if the TV refuses it:
    /// it answers a link it can't open with an error and hangs up.
    fn launch(&mut self, link: &str) -> Result<(), String> {
        self.wire.send(&remote(90, Pb::default().text(1, link)))?;
        match self.pump(Instant::now() + REFUSAL, |_| false) {
            Ok(_) => Ok(()),
            Err(_) => Err("the TV couldn't open that; is the app installed?".into()),
        }
    }

    fn describe(&self) -> String {
        if self.on == Some(false) {
            return "off".into();
        }
        match self.volume {
            Some(v) if v.max > 0 => {
                let muted = if v.muted { ", muted" } else { "" };
                format!("on, volume {}{muted}", v.level * 100 / v.max)
            }
            _ => "on".into(),
        }
    }

    fn power(&mut self, on: bool) -> Result<String, String> {
        if self.on == Some(on) {
            return Ok(format!("already {}", if on { "on" } else { "off" }));
        }
        self.key(KEY_POWER)?;
        if self.pump(Instant::now() + SETTLE, |r| r.on == Some(on))? {
            Ok(format!("turned {}", if on { "on" } else { "off" }))
        } else {
            Err("pressed power, but the TV didn't say it changed".into())
        }
    }

    fn volume(&mut self, level: Option<u8>, change: Option<i8>, mute: Option<bool>) -> Result<String, String> {
        if let Some(mute) = mute
            && self.volume.is_none_or(|v| v.muted != mute)
        {
            self.key(KEY_MUTE)?;
            let _ = self.pump(Instant::now() + SETTLE, |r| r.volume.is_some_and(|v| v.muted == mute));
        }
        let steps: i64 = match (level, change) {
            (Some(level), _) => {
                let v = self.volume.filter(|v| v.max > 0).ok_or("this TV doesn't report its volume, so it can only go up or down")?;
                (u64::from(level) * v.max).div_ceil(100) as i64 - v.level as i64
            }
            (None, Some(change)) => i64::from(change),
            (None, None) => 0,
        };
        let key = if steps > 0 { KEY_VOLUME_UP } else { KEY_VOLUME_DOWN };
        for _ in 0..steps.unsigned_abs().min(100) {
            self.key(key)?;
            std::thread::sleep(STEP);
        }
        // The TV reports each step; wait for the last.
        if let Some(v) = self.volume.filter(|v| v.max > 0 && steps != 0) {
            let target = (v.level as i64 + steps).clamp(0, v.max as i64) as u64;
            let _ = self.pump(Instant::now() + SETTLE, |r| r.volume.is_some_and(|v| v.level == target));
        }
        Ok(match self.volume {
            Some(v) if v.max > 0 => format!("volume {}{}", v.level * 100 / v.max, if v.muted { ", muted" } else { "" }),
            _ => "done".into(),
        })
    }
}

/// The app link for something spoken: an app named in [`APPS`], a package
/// name, or a link.
fn app_link(app: &str) -> Option<String> {
    let spoken = app.trim().to_ascii_lowercase();
    if spoken.contains("://") {
        return Some(app.trim().to_string());
    }
    let known = APPS
        .iter()
        .find(|(name, _)| *name == spoken)
        .or_else(|| APPS.iter().find(|(name, _)| spoken.contains(name)));
    if let Some((_, link)) = known {
        return Some(link.to_string());
    }
    let parts: Vec<&str> = spoken.split('.').collect();
    (parts.len() >= 2 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')))
        .then(|| format!("market://launch?id={}", app.trim()))
}

fn input_key(input: &str) -> Option<u64> {
    let spoken: String = input.to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    match spoken.as_str() {
        "tv" | "livetv" | "antenna" => Some(KEY_TV),
        "home" => Some(KEY_HOME),
        _ => {
            let n: u64 = spoken.strip_prefix("hdmi")?.parse().ok()?;
            (1..=4).contains(&n).then_some(KEY_HDMI_1 + n - 1)
        }
    }
}

// ----- home ------------------------------------------------------------------------------

/// Android TV and Google TV sets, as a home backend. A paired one keeps
/// this machine's certificate and key for it, the TV's pinned certificate
/// and its model in [`Device::data`].
pub struct AndroidTv;

fn field<'a>(d: &'a Device, name: &str) -> Option<&'a str> {
    d.data.get(name).and_then(Value::as_str)
}

fn remote_at(d: &Device, to: SocketAddr) -> Result<Remote<TlsStream<TcpStream>>, String> {
    let (Some(cert), Some(key), Some(pin)) = (field(d, "client_cert"), field(d, "client_key"), field(d, "pin")) else {
        return Err(format!("{} isn't paired yet; press Enter on it in /devices", d.name));
    };
    let tls = connect(to, cert, key).map_err(|e| {
        if e.starts_with("the TV refused") {
            format!("{e}. If the TV was reset or forgot hopchat, forget it in /devices and pair again")
        } else {
            e
        }
    })?;
    if fingerprint(&peer_der(&tls)?) != pin {
        return Err(format!("{} showed a different certificate than when it paired; if it was reset, pair it again", d.name));
    }
    Remote::start(tls)
}

fn remote_of(d: &Device) -> Result<Remote<TlsStream<TcpStream>>, String> {
    remote_at(d, SocketAddr::from((d.ip, REMOTE_PORT)))
}

/// A TV whose network sleeps in standby comes back with a magic packet,
/// when it allows that; tries for a few seconds.
fn wake_then_reach(d: &Device) -> Result<Remote<TlsStream<TcpStream>>, String> {
    let first = match remote_of(d) {
        Ok(r) => return Ok(r),
        Err(e) => e,
    };
    let Some(mac) = d.mac.as_deref().and_then(parse_mac) else { return Err(first) };
    discover::wake(mac)?;
    let until = Instant::now() + Duration::from_secs(12);
    while Instant::now() < until {
        std::thread::sleep(Duration::from_secs(2));
        if let Ok(r) = remote_of(d) {
            return Ok(r);
        }
    }
    Err(format!("{first}; a wake-up signal didn't help either. Keep the TV's network on in standby"))
}

/// Pairs `d` with the TV whose pairing port is `pair` and remote port is
/// `remote`, keeping in `d` what later connections need.
fn pair_at(d: &mut Device, pair: SocketAddr, remote: SocketAddr) -> Result<String, String> {
    let (cert, key) = match (field(d, "client_cert"), field(d, "client_key")) {
        (Some(c), Some(k)) => (c.to_string(), k.to_string()),
        _ => new_identity()?,
    };
    pair_with(pair, &cert, &key)?;
    // The TV takes a moment to trust the new certificate on its remote port.
    let mut last = String::new();
    for _ in 0..5 {
        match connect(remote, &cert, &key).and_then(|tls| {
            let pin = fingerprint(&peer_der(&tls)?);
            Remote::start(tls).map(|r| (pin, r))
        }) {
            Ok((pin, r)) => {
                let data = d.data.as_object_mut().ok_or("nothing to keep the pairing in")?;
                data.insert("client_cert".into(), json!(cert));
                data.insert("client_key".into(), json!(key));
                data.insert("pin".into(), json!(pin));
                if let Some(model) = &r.model {
                    data.insert("model".into(), json!(model));
                }
                return Ok(r.describe());
            }
            Err(e) => last = e,
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!("paired, but the TV won't take commands: {last}"))
}

const PAIR_HELP: PairHelp = PairHelp {
    before: [" pairs it. The TV will show a code to type here.", "Keep the TV on and in view while it pairs."],
    during: ["Type the code on the TV: /home code CODE", "It's six letters and digits; the TV shows it for a minute."],
    takes: PAIRING_TAKES,
};

fn found(ip: Ipv4Addr, name: Option<String>) -> Found {
    let mac = mac_of(ip).map(|m| format_mac(&m));
    let data = name.map(|n| json!({"name": n})).unwrap_or_else(|| json!({}));
    Found { backend: "androidtv", ip, mac, unit: None, data, checked: None }
}

impl Backend for AndroidTv {
    fn id(&self) -> &'static str {
        "androidtv"
    }

    fn category(&self) -> Category {
        Category::Tv
    }

    fn label(&self) -> &'static str {
        "Android TVs"
    }

    fn discover(&self, each: &dyn Fn(Found)) {
        let Ok(daemon) = mdns_sd::ServiceDaemon::new() else { return };
        let Ok(browse) = daemon.browse(SERVICE) else { return };
        let until = Instant::now() + DISCOVER;
        let mut seen = Vec::new();
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match browse.recv_timeout(left) {
                Ok(mdns_sd::ServiceEvent::ServiceResolved(service)) => {
                    let Some(ip) = service.get_addresses_v4().into_iter().next() else { continue };
                    if seen.contains(&ip) {
                        continue;
                    }
                    seen.push(ip);
                    let name = service.get_fullname().strip_suffix(&format!(".{SERVICE}")).map(str::to_string);
                    each(found(ip, name));
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = daemon.shutdown();
    }

    fn probe(&self, ip: Ipv4Addr) -> Option<Found> {
        TcpStream::connect_timeout(&SocketAddr::from((ip, PAIR_PORT)), CONNECT).ok().map(|_| found(ip, None))
    }

    fn name(&self, d: &Device) -> Option<String> {
        field(d, "name").map(str::to_string)
    }

    fn about(&self, d: &Device) -> String {
        match field(d, "model") {
            Some(model) => format!("Android TV ({model})"),
            None => "Android TV".into(),
        }
    }

    fn check(&self, d: &Device) -> (State, String) {
        if !self.ready(d) {
            return (State::Ready, "Android TV".into());
        }
        match remote_of(d) {
            Ok(r) => (State::Ready, r.describe()),
            Err(_) => (State::Offline, "off or in standby".into()),
        }
    }

    fn test(&self, d: &mut Device) -> Result<String, String> {
        let mut r = remote_of(d)?;
        r.key(KEY_HOME)?;
        Ok(format!("{} went to its home screen", d.name))
    }

    fn test_help(&self) -> &'static str {
        " shows its home screen so you can tell which one it is"
    }

    fn pairing(&self) -> Option<&'static PairHelp> {
        Some(&PAIR_HELP)
    }

    fn pair(&self, d: &mut Device) -> Result<String, String> {
        pair_at(d, SocketAddr::from((d.ip, PAIR_PORT)), SocketAddr::from((d.ip, REMOTE_PORT)))
    }

    fn ready(&self, d: &Device) -> bool {
        ["client_cert", "client_key", "pin"].iter().all(|k| field(d, k).is_some())
    }

    fn as_tv(&self) -> Option<&dyn TvDriver> {
        Some(self)
    }
}

impl TvDriver for AndroidTv {
    fn power(&self, d: &mut Device, on: bool) -> Result<String, String> {
        let mut r = if on { wake_then_reach(d)? } else { remote_of(d)? };
        r.power(on)
    }

    fn volume(&self, d: &mut Device, level: Option<u8>, change: Option<i8>, mute: Option<bool>) -> Result<String, String> {
        remote_of(d)?.volume(level, change, mute)
    }

    fn status(&self, d: &mut Device) -> Result<String, String> {
        Ok(remote_of(d)?.describe())
    }

    fn open_app(&self, d: &mut Device, app: &str) -> Result<String, String> {
        let link = app_link(app).ok_or_else(|| {
            let known: Vec<&str> = APPS.iter().map(|(name, _)| *name).collect();
            format!("I don't know the app {app}; I know {}, or give its package name", known.join(", "))
        })?;
        let mut r = remote_of(d)?;
        r.launch(&link)?;
        Ok(format!("opened {app}"))
    }

    fn input(&self, d: &mut Device, input: &str) -> Result<String, String> {
        let key = input_key(input).ok_or_else(|| format!("{} can switch to HDMI 1 to 4 or TV, not {input}", d.name))?;
        remote_of(d)?.key(key)?;
        Ok(format!("switched to {input}"))
    }

    fn media(&self, d: &mut Device, action: &str) -> Result<String, String> {
        let key = MEDIA.iter().find(|(name, _)| name.eq_ignore_ascii_case(action)).map(|(_, k)| *k);
        let key = key.ok_or_else(|| format!("{} can't {action}", d.name))?;
        remote_of(d)?.key(key)?;
        Ok(format!("sent {action}"))
    }
}

#[cfg(test)]
mod tests;
