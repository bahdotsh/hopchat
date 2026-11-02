//! Tuya lights (Wipro, and most other white-label Wi-Fi bulbs): AES-framed
//! messages on TCP 6668, LAN only.
//!
//! Nothing here reaches Tuya's cloud, but each bulb encrypts with a local
//! key its cloud account issued, so a person has to bring that key and the
//! device id: `/home tuya IP ID KEY`. A bulb speaks one of three versions
//! of the protocol and doesn't say which, so pairing tries each and keeps
//! the one that answered. 3.1 is not supported: bulbs that old are rare.
//!
//! Most of them take one connection at a time, so each action connects,
//! does its work and hangs up.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use aes_gcm::aead::{Aead, Payload};
use aes_gcm::Aes128Gcm;
use hmac::{Hmac, Mac};
use serde_json::{json, Map, Value};
use sha2::Sha256;

use super::discover::{self, format_mac, mac_of};
use super::driver::{Backend, Category, Found, PairHelp};
use super::light::{Caps, LightDriver, Pilot};
use super::{Device, Home, State};

const PORT: u16 = 6668;
const WAIT: Duration = Duration::from_secs(3);
/// Between tries at pairing: a bulb that just dropped a bad frame may not
/// take the next connection at once.
const SETTLE: Duration = Duration::from_millis(300);
/// More than any bulb sends; a bigger claim is a stream gone wrong.
const MAX_FRAME: usize = 8192;
/// Frames read for one answer: a bulb pushes status between them.
const MAX_FRAMES: usize = 8;
const BLINK: Duration = Duration::from_millis(350);

const PREFIX_55AA: [u8; 4] = [0, 0, 0x55, 0xAA];
const SUFFIX_55AA: [u8; 4] = [0, 0, 0xAA, 0x55];
const PREFIX_6699: [u8; 4] = [0, 0, 0x66, 0x99];
const SUFFIX_6699: [u8; 4] = [0, 0, 0x99, 0x66];

const SESS_KEY_NEG_START: u32 = 3;
const SESS_KEY_NEG_RESP: u32 = 4;
const SESS_KEY_NEG_FINISH: u32 = 5;
const CONTROL: u32 = 7;
const STATUS: u32 = 8;
const DP_QUERY: u32 = 10;
const CONTROL_NEW: u32 = 13;
const DP_QUERY_NEW: u32 = 16;

const USAGE: &str = "add a Tuya light with /home tuya IP DEVICE_ID LOCAL_KEY; \
                     the Tuya IoT platform or tinytuya's wizard shows both";

type Key = [u8; 16];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Version {
    V33,
    V34,
    V35,
}

const VERSIONS: [Version; 3] = [Version::V33, Version::V34, Version::V35];

impl Version {
    fn text(self) -> &'static str {
        match self {
            Version::V33 => "3.3",
            Version::V34 => "3.4",
            Version::V35 => "3.5",
        }
    }

    fn parse(text: &str) -> Option<Version> {
        VERSIONS.into_iter().find(|v| v.text() == text)
    }

    fn query(self) -> u32 {
        if self == Version::V33 { DP_QUERY } else { DP_QUERY_NEW }
    }

    fn control(self) -> u32 {
        if self == Version::V33 { CONTROL } else { CONTROL_NEW }
    }
}

// ----- which data point means what -----------------------------------------

/// Where a kind of bulb keeps its state. The same data points mean
/// different things on different firmware, and only the bulb's own answer
/// tells them apart.
#[derive(Debug, PartialEq)]
struct Layout {
    id: &'static str,
    switch: &'static str,
    mode: Option<&'static str>,
    bright: &'static str,
    temp: &'static str,
    colour: Option<&'static str>,
    min: u32,
    max: u32,
    /// `hhhhssssvvvv` scaled to 1000, rather than `rrggbbhhhhssvv`.
    hsv16: bool,
}

/// Newer firmware, Wipro's among it.
const LAYOUT_B: Layout = Layout {
    id: "b",
    switch: "20",
    mode: Some("21"),
    bright: "22",
    temp: "23",
    colour: Some("24"),
    min: 10,
    max: 1000,
    hsv16: true,
};
const LAYOUT_A: Layout = Layout {
    id: "a",
    switch: "1",
    mode: Some("2"),
    bright: "3",
    temp: "4",
    colour: Some("5"),
    min: 25,
    max: 255,
    hsv16: false,
};
/// White tones only.
const LAYOUT_C: Layout =
    Layout { id: "c", switch: "1", mode: None, bright: "2", temp: "3", colour: None, min: 25, max: 255, hsv16: false };

const LAYOUTS: [&Layout; 3] = [&LAYOUT_B, &LAYOUT_A, &LAYOUT_C];

/// The layout of a bulb answering `dps`; `None` for something that isn't a
/// dimmable light, as a Tuya plug isn't. Most specific first: a bulb on
/// layout A would also pass the test for layout C if its data point 2 held
/// a number, and only a string there tells them apart.
fn layout_of(dps: &Value) -> Option<&'static Layout> {
    let number = |k: &str| dps.get(k).is_some_and(Value::is_number);
    let string = |k: &str| dps.get(k).is_some_and(Value::is_string);
    let boolean = |k: &str| dps.get(k).is_some_and(Value::is_boolean);
    if boolean("20") && number("22") {
        Some(&LAYOUT_B)
    } else if boolean("1") && string("2") && number("3") {
        Some(&LAYOUT_A)
    } else if boolean("1") && number("2") {
        Some(&LAYOUT_C)
    } else {
        None
    }
}

/// `dps` as a Tuya bulb answered, which [`Bulb::dps`] gets from the frame.
fn caps_in(layout: &Layout, dps: &Value) -> Caps {
    Caps {
        colour: layout.colour.is_some_and(|k| dps.get(k).is_some()),
        white: dps.get(layout.temp).is_some(),
    }
}

/// Percent on a bulb's own scale: never below what it takes.
fn scaled(layout: &Layout, percent: u8) -> u32 {
    (layout.max * u32::from(percent.min(100)) / 100).max(layout.min)
}

fn percent_of(layout: &Layout, value: u64) -> u64 {
    (value * 100 / u64::from(layout.max)).min(100)
}

/// What Wipro's bulbs span; a bulb only takes a position between its ends.
const WARMEST: u16 = 2700;
const COOLEST: u16 = 6500;

fn temp_of(layout: &Layout, kelvin: u16) -> u32 {
    let k = u32::from(kelvin.clamp(WARMEST, COOLEST) - WARMEST);
    k * layout.max / u32::from(COOLEST - WARMEST)
}

fn kelvin_of(layout: &Layout, temp: u64) -> u64 {
    u64::from(WARMEST) + temp.min(u64::from(layout.max)) * u64::from(COOLEST - WARMEST) / u64::from(layout.max)
}

/// Hue in degrees, saturation and value from 0 to 1.
fn to_hsv((r, g, b): (u8, u8, u8)) -> (f64, f64, f64) {
    let (r, g, b) = (f64::from(r) / 255.0, f64::from(g) / 255.0, f64::from(b) / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let delta = max - min;
    let hue = if delta == 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    (hue, if max == 0.0 { 0.0 } else { delta / max }, max)
}

fn to_rgb(h: f64, s: f64, v: f64) -> (u8, u8, u8) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match (h.rem_euclid(360.0) / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let byte = |n: f64| ((n + v - c) * 255.0).round() as u8;
    (byte(r), byte(g), byte(b))
}

fn colour_text(layout: &Layout, h: f64, s: f64, v: f64) -> String {
    if layout.hsv16 {
        let scale = |n: f64| (n * 1000.0).round() as u32;
        format!("{:04x}{:04x}{:04x}", h.round() as u32, scale(s), scale(v))
    } else {
        let (r, g, b) = to_rgb(h, s, v);
        let scale = |n: f64| (n * 255.0).round() as u32;
        format!("{r:02x}{g:02x}{b:02x}{:04x}{:02x}{:02x}", h.round() as u32, scale(s), scale(v))
    }
}

/// Hue, saturation and value out of a bulb's colour text.
fn colour_read(layout: &Layout, text: &str) -> Option<(f64, f64, f64)> {
    let hex = |from: usize, to: usize| u32::from_str_radix(text.get(from..to)?, 16).ok().map(f64::from);
    if layout.hsv16 {
        Some((hex(0, 4)?, hex(4, 8)? / 1000.0, hex(8, 12)? / 1000.0))
    } else {
        Some((hex(6, 10)?, hex(10, 12)? / 255.0, hex(12, 14)? / 255.0))
    }
}

/// The data points that carry out `pilot`, on a bulb showing `now`. Colour
/// wins over white temperature, as the bulb holds one. A coloured bulb's
/// brightness is the colour's own value, so dimming one rewrites it.
fn plan(layout: &Layout, pilot: &Pilot, now: &Value) -> Map<String, Value> {
    let mut dps = Map::new();
    let percent = pilot.brightness.map(|b| f64::from(b.clamp(1, 100)) / 100.0);
    if let (Some(rgb), Some(key)) = (pilot.rgb, layout.colour) {
        let (h, s, v) = to_hsv(rgb);
        dps.insert(key.into(), json!(colour_text(layout, h, s, percent.unwrap_or(v).max(0.01))));
        if let Some(mode) = layout.mode {
            dps.insert(mode.into(), json!("colour"));
        }
    } else {
        let coloured = layout.mode.and_then(|m| now.get(m)).and_then(Value::as_str) == Some("colour");
        let shown = layout.colour.and_then(|k| now.get(k)).and_then(Value::as_str);
        match (percent, shown) {
            (Some(v), Some(text)) if coloured && pilot.kelvin.is_none() => {
                if let (Some((h, s, _)), Some(key)) = (colour_read(layout, text), layout.colour) {
                    dps.insert(key.into(), json!(colour_text(layout, h, s, v)));
                }
            }
            _ => {
                if let Some(b) = pilot.brightness {
                    dps.insert(layout.bright.into(), json!(scaled(layout, b)));
                }
            }
        }
        if let Some(k) = pilot.kelvin {
            dps.insert(layout.temp.into(), json!(temp_of(layout, k)));
            if let Some(mode) = layout.mode {
                dps.insert(mode.into(), json!("white"));
            }
        }
    }
    // Changing anything but power implies the light should be on.
    if !dps.is_empty() || pilot.on.is_some() {
        dps.insert(layout.switch.into(), json!(pilot.on.unwrap_or(true)));
    }
    dps
}

fn describe(layout: &Layout, dps: &Value) -> String {
    if dps.get(layout.switch) == Some(&Value::Bool(false)) {
        return "off".into();
    }
    let coloured = layout.mode.and_then(|m| dps.get(m)).and_then(Value::as_str) == Some("colour");
    let colour = layout
        .colour
        .and_then(|k| dps.get(k))
        .and_then(Value::as_str)
        .filter(|_| coloured)
        .and_then(|text| colour_read(layout, text));
    if let Some((h, s, v)) = colour {
        let (r, g, b) = to_rgb(h, s, v);
        return format!("on · {}% · #{r:02x}{g:02x}{b:02x}", (v * 100.0).round() as u32);
    }
    let bright = dps.get(layout.bright).and_then(Value::as_u64).map_or(100, |b| percent_of(layout, b));
    match dps.get(layout.temp).and_then(Value::as_u64) {
        Some(temp) => format!("on · {bright}% · {}K", kelvin_of(layout, temp)),
        None => format!("on · {bright}%"),
    }
}

// ----- the wire -------------------------------------------------------------

fn block(key: &Key) -> Aes128 {
    Aes128::new(GenericArray::from_slice(key))
}

/// ECB with PKCS#7 padding, which is what Tuya chose.
fn encrypt_ecb(key: &Key, plain: &[u8]) -> Vec<u8> {
    let cipher = block(key);
    let pad = 16 - plain.len() % 16;
    let mut out = plain.to_vec();
    out.resize(plain.len() + pad, pad as u8);
    for chunk in out.as_chunks_mut::<16>().0 {
        cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
    }
    out
}

/// Some firmware pads badly, so bad padding stays in; a wrong key shows
/// up as text that isn't JSON.
fn decrypt_ecb(key: &Key, sealed: &[u8]) -> Result<Vec<u8>, String> {
    if sealed.is_empty() || !sealed.len().is_multiple_of(16) {
        return Err("the bulb sent a broken frame".into());
    }
    let cipher = block(key);
    let mut out = sealed.to_vec();
    for chunk in out.as_chunks_mut::<16>().0 {
        cipher.decrypt_block(GenericArray::from_mut_slice(chunk));
    }
    let pad = usize::from(*out.last().unwrap_or(&0));
    if (1..=16).contains(&pad) && out[out.len() - pad..].iter().all(|b| usize::from(*b) == pad) {
        out.truncate(out.len() - pad);
    }
    Ok(out)
}

fn hmac_of(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

fn gcm(key: &Key) -> Aes128Gcm {
    Aes128Gcm::new(GenericArray::from_slice(key))
}

/// Whether a command's plaintext starts with the version and twelve zeros.
fn headed(cmd: u32) -> bool {
    !matches!(cmd, DP_QUERY | DP_QUERY_NEW | SESS_KEY_NEG_START | SESS_KEY_NEG_RESP | SESS_KEY_NEG_FINISH)
}

fn version_header(version: Version) -> Vec<u8> {
    let mut out = version.text().as_bytes().to_vec();
    out.resize(15, 0);
    out
}

/// One frame to send, `iv` only used by 3.5.
fn seal(version: Version, key: &Key, seq: u32, cmd: u32, body: &[u8], iv: [u8; 12]) -> Vec<u8> {
    let header = if headed(cmd) { version_header(version) } else { Vec::new() };
    if version == Version::V35 {
        let plain = [header.as_slice(), body].concat();
        // The nonce, the text and the tag, and neither prefix nor suffix.
        let length = 12 + plain.len() + 16;
        let mut frame = PREFIX_6699.to_vec();
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(&seq.to_be_bytes());
        frame.extend_from_slice(&cmd.to_be_bytes());
        frame.extend_from_slice(&(length as u32).to_be_bytes());
        let aad = frame[4..].to_vec();
        let sealed = gcm(key)
            .encrypt(GenericArray::from_slice(&iv), Payload { msg: &plain, aad: &aad })
            .expect("a frame is well under GCM's limit");
        frame.extend_from_slice(&iv);
        frame.extend_from_slice(&sealed);
        frame.extend_from_slice(&SUFFIX_6699);
        return frame;
    }
    let payload = match version {
        Version::V33 => [header.as_slice(), &encrypt_ecb(key, body)].concat(),
        _ => encrypt_ecb(key, &[header.as_slice(), body].concat()),
    };
    let trailer = if version == Version::V33 { 4 } else { 32 } + SUFFIX_55AA.len();
    let mut frame = PREFIX_55AA.to_vec();
    frame.extend_from_slice(&seq.to_be_bytes());
    frame.extend_from_slice(&cmd.to_be_bytes());
    frame.extend_from_slice(&((payload.len() + trailer) as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    if version == Version::V33 {
        let crc = crc32fast::hash(&frame);
        frame.extend_from_slice(&crc.to_be_bytes());
    } else {
        let tag = hmac_of(key, &[&frame]);
        frame.extend_from_slice(&tag);
    }
    frame.extend_from_slice(&SUFFIX_55AA);
    frame
}

#[derive(Debug, PartialEq)]
struct Frame {
    cmd: u32,
    retcode: u32,
    body: Vec<u8>,
}

impl Frame {
    /// The JSON in the body, which may follow a version header.
    fn json(&self) -> Option<Value> {
        let from = self.body.iter().position(|b| *b == b'{')?;
        let to = self.body.iter().rposition(|b| *b == b'}')?;
        serde_json::from_slice(self.body.get(from..=to)?).ok()
    }

    /// The data points in the body: 3.4 and later nest them.
    fn dps(&self) -> Option<Value> {
        let json = self.json()?;
        json.pointer("/dps").or_else(|| json.pointer("/data/dps")).cloned()
    }
}

fn read_exact(source: &mut impl Read, buf: &mut [u8]) -> Result<(), String> {
    source.read_exact(buf).map_err(|e| match e.kind() {
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset => "the bulb hung up".to_string(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => "the bulb did not answer".to_string(),
        _ => e.to_string(),
    })
}

fn word(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// The next frame from `source`, checked and decrypted.
fn open_frame(source: &mut impl Read, version: Version, key: &Key) -> Result<Frame, String> {
    let mut prefix = [0u8; 4];
    read_exact(source, &mut prefix)?;
    if prefix == PREFIX_6699 && version == Version::V35 {
        return open_6699(source, key);
    }
    if prefix != PREFIX_55AA || version == Version::V35 {
        return Err("the bulb speaks another version of the protocol".into());
    }
    let mut head = [0u8; 12];
    read_exact(source, &mut head)?;
    let (cmd, length) = (word(&head[4..8]), word(&head[8..12]) as usize);
    let tag = if version == Version::V33 { 4 } else { 32 };
    if length < tag + SUFFIX_55AA.len() || length > MAX_FRAME {
        return Err("the bulb sent a broken frame".into());
    }
    let mut rest = vec![0u8; length];
    read_exact(source, &mut rest)?;
    let (payload, end) = rest.split_at(length - tag - SUFFIX_55AA.len());
    let (sent, suffix) = end.split_at(tag);
    if suffix != SUFFIX_55AA {
        return Err("the bulb sent a broken frame".into());
    }
    let good = if version == Version::V33 {
        let mut crc = crc32fast::Hasher::new();
        crc.update(&prefix);
        crc.update(&head);
        crc.update(payload);
        crc.finalize().to_be_bytes().as_slice() == sent
    } else {
        hmac_of(key, &[&prefix, &head, payload]).as_slice() == sent
    };
    if !good {
        return Err("that frame failed its check, which usually means a wrong key".into());
    }
    // A bulb's own frames start with a status word, unless they're pushed.
    // Anything shorter than the word has none, and fails as a broken frame.
    let (retcode, payload) = match (payload.len() % 16, payload.split_first_chunk::<4>()) {
        (3 | 4, Some((status, rest))) => (u32::from_be_bytes(*status), rest),
        _ => (0, payload),
    };
    let payload = payload.strip_prefix(version_header(version).as_slice()).unwrap_or(payload);
    let body = if payload.is_empty() { Vec::new() } else { decrypt_ecb(key, payload)? };
    Ok(Frame { cmd, retcode, body })
}

fn open_6699(source: &mut impl Read, key: &Key) -> Result<Frame, String> {
    let mut head = [0u8; 14];
    read_exact(source, &mut head)?;
    let (cmd, length) = (word(&head[6..10]), word(&head[10..14]) as usize);
    if !(12 + 16..=MAX_FRAME).contains(&length) {
        return Err("the bulb sent a broken frame".into());
    }
    let mut rest = vec![0u8; length + SUFFIX_6699.len()];
    read_exact(source, &mut rest)?;
    let (body, suffix) = rest.split_at(length);
    if suffix != SUFFIX_6699 {
        return Err("the bulb sent a broken frame".into());
    }
    let (iv, sealed) = body.split_at(12);
    let plain = gcm(key)
        .decrypt(GenericArray::from_slice(iv), Payload { msg: sealed, aad: &head })
        .map_err(|_| "that frame failed its check, which usually means a wrong key")?;
    // A status word comes first, except in the frames that set up a session:
    // those are nonces, and [`Bulb::negotiate`] finds its own way in them.
    let setup = matches!(cmd, SESS_KEY_NEG_START | SESS_KEY_NEG_RESP | SESS_KEY_NEG_FINISH);
    let status = !setup && plain.len() >= 4 && plain[0] != b'{' && !plain.starts_with(b"3.5");
    let (retcode, body) = if status { (word(&plain), plain[4..].to_vec()) } else { (0, plain) };
    Ok(Frame { cmd, retcode, body })
}

/// The key a 3.4 or 3.5 session runs under: both sides' nonces mixed, and
/// sealed with the bulb's own key.
fn session_key(version: Version, key: &Key, ours: &[u8; 16], theirs: &[u8; 16]) -> Key {
    let mut mixed: Key = std::array::from_fn(|i| ours[i] ^ theirs[i]);
    if version == Version::V35 {
        let sealed = gcm(key).encrypt(GenericArray::from_slice(&ours[..12]), mixed.as_slice());
        mixed.copy_from_slice(&sealed.expect("sixteen bytes is under GCM's limit")[..16]);
    } else {
        block(key).encrypt_block(GenericArray::from_mut_slice(&mut mixed));
    }
    mixed
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    let _ = getrandom::fill(&mut bytes);
    bytes
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |t| t.as_secs())
}

// ----- one bulb ------------------------------------------------------------

/// A connection to a bulb, with a session once one is negotiated.
struct Bulb {
    stream: TcpStream,
    version: Version,
    id: String,
    key: Key,
    seq: u32,
}

impl Bulb {
    fn connect(to: SocketAddr, version: Version, id: &str, key: &Key) -> Result<Bulb, String> {
        let stream = TcpStream::connect_timeout(&to, WAIT).map_err(|_| format!("the light at {} did not answer", to.ip()))?;
        stream.set_read_timeout(Some(WAIT)).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(WAIT)).map_err(|e| e.to_string())?;
        let _ = stream.set_nodelay(true);
        let mut bulb = Bulb { stream, version, id: id.into(), key: *key, seq: 1 };
        if version != Version::V33 {
            bulb.negotiate()?;
        }
        Ok(bulb)
    }

    fn send(&mut self, cmd: u32, body: &[u8]) -> Result<(), String> {
        let frame = seal(self.version, &self.key, self.seq, cmd, body, random());
        self.seq += 1;
        self.stream.write_all(&frame).map_err(|e| e.to_string())
    }

    fn negotiate(&mut self) -> Result<(), String> {
        let ours: [u8; 16] = random();
        self.send(SESS_KEY_NEG_START, &ours)?;
        let answer = open_frame(&mut self.stream, self.version, &self.key)?;
        if answer.cmd != SESS_KEY_NEG_RESP {
            return Err("the bulb would not start a session".into());
        }
        // Its nonce, then proof it knows the key: maybe after a status word.
        let proof = hmac_of(&self.key, &[&ours]);
        let theirs = [0, 4]
            .into_iter()
            .filter_map(|at| answer.body.get(at..at + 48))
            .find(|part| part[16..] == proof)
            .ok_or("the bulb has a different local key")?;
        let theirs: [u8; 16] = theirs[..16].try_into().map_err(|_| "the bulb sent a broken frame")?;
        self.send(SESS_KEY_NEG_FINISH, &hmac_of(&self.key, &[&theirs]))?;
        self.key = session_key(self.version, &self.key, &ours, &theirs);
        Ok(())
    }

    /// Reads until a frame `wanted` says is the answer.
    fn answer(&mut self, wanted: impl Fn(&Frame) -> bool) -> Result<Frame, String> {
        for _ in 0..MAX_FRAMES {
            let frame = open_frame(&mut self.stream, self.version, &self.key)?;
            if wanted(&frame) {
                return Ok(frame);
            }
        }
        Err("the bulb did not answer".into())
    }

    /// What every data point holds now.
    fn dps(&mut self) -> Result<Value, String> {
        let (query, id) = (self.version.query(), self.id.clone());
        let t = now().to_string();
        let body = match self.version {
            Version::V33 => json!({"gwId": id, "devId": id, "uid": id, "t": t}),
            _ => json!({}),
        };
        self.send(query, body.to_string().as_bytes())?;
        let frame = self.answer(|f| f.cmd == query || f.cmd == STATUS)?;
        if let Some(dps) = frame.dps() {
            return Ok(dps);
        }
        // Some firmware only answers when asked for data points by name.
        if String::from_utf8_lossy(&frame.body).contains("data unvalid") {
            let named: Map<String, Value> = (1..=5).chain(20..=24).map(|dp| (dp.to_string(), Value::Null)).collect();
            let body = json!({"devId": id, "uid": id, "t": t, "dps": named});
            self.send(CONTROL_NEW, body.to_string().as_bytes())?;
            let frame = self.answer(|f| f.dps().is_some())?;
            return frame.dps().ok_or_else(|| "the bulb sent no state".into());
        }
        Err("the bulb has a different device ID or local key".into())
    }

    fn set(&mut self, dps: &Map<String, Value>) -> Result<(), String> {
        let (control, id) = (self.version.control(), self.id.clone());
        let body = match self.version {
            Version::V33 => json!({"devId": id, "uid": id, "t": now().to_string(), "dps": dps}),
            _ => json!({"protocol": 5, "t": now(), "data": {"dps": dps}}),
        };
        self.send(control, body.to_string().as_bytes())?;
        // Some firmware skips the acknowledgement and only reports the change.
        let frame = self.answer(|f| f.cmd == control || f.cmd == STATUS)?;
        match frame.retcode {
            0 => Ok(()),
            code => Err(format!("the bulb refused that (code {code})")),
        }
    }
}

// ----- home ------------------------------------------------------------------

/// Tuya lights, as a home backend.
pub struct Tuya;

fn field<'a>(d: &'a Device, name: &str) -> Option<&'a str> {
    d.data.get(name).and_then(Value::as_str)
}

fn key_of(d: &Device) -> Option<Key> {
    field(d, "key")?.as_bytes().try_into().ok()
}

fn layout_saved(d: &Device) -> Option<&'static Layout> {
    let id = field(d, "layout")?;
    LAYOUTS.into_iter().find(|l| l.id == id)
}

fn address(d: &Device) -> SocketAddr {
    SocketAddr::from((d.ip, PORT))
}

/// A paired bulb's connection and layout.
fn reach(d: &Device) -> Result<(Bulb, &'static Layout), String> {
    let (Some(id), Some(key), Some(version), Some(layout)) =
        (field(d, "id"), key_of(d), field(d, "version").and_then(Version::parse), layout_saved(d))
    else {
        return Err(format!("{} hasn't got its key yet: {USAGE}", d.name));
    };
    Ok((Bulb::connect(address(d), version, id, &key)?, layout))
}

fn status(d: &Device) -> Result<String, String> {
    let (mut bulb, layout) = reach(d)?;
    Ok(describe(layout, &bulb.dps()?))
}

/// Flicks it twice and leaves it as it was, so you can tell which is which.
fn blink(d: &Device) -> Result<(), String> {
    let (mut bulb, layout) = reach(d)?;
    let was_on = bulb.dps()?.get(layout.switch) != Some(&Value::Bool(false));
    let mut blinked = Ok(());
    for on in [!was_on, was_on, !was_on] {
        blinked = bulb.set(&Map::from_iter([(layout.switch.to_string(), json!(on))]));
        if blinked.is_err() {
            break;
        }
        std::thread::sleep(BLINK);
    }
    // Put it back even if a flick got lost.
    let restored = bulb.set(&Map::from_iter([(layout.switch.to_string(), json!(was_on))]));
    blinked.and(restored)
}

/// Tries each version of the protocol on the bulb at `to`, keeping in `d`
/// what the one that answered showed.
fn pair_at(d: &mut Device, to: SocketAddr) -> Result<String, String> {
    let (Some(id), Some(key)) = (field(d, "id").map(str::to_string), key_of(d)) else {
        return Err(format!("{USAGE} (this one is at {})", d.ip));
    };
    let mut last = String::new();
    for (at, version) in VERSIONS.into_iter().enumerate() {
        if at > 0 {
            std::thread::sleep(SETTLE);
        }
        match Bulb::connect(to, version, &id, &key).and_then(|mut bulb| bulb.dps()) {
            Ok(dps) => {
                let Some(layout) = layout_of(&dps) else {
                    return Err(format!("the device at {} isn't a dimmable light", d.ip));
                };
                let caps = caps_in(layout, &dps);
                let data = d.data.as_object_mut().ok_or("no key to keep")?;
                data.insert("version".into(), json!(version.text()));
                data.insert("layout".into(), json!(layout.id));
                data.insert("colour".into(), json!(caps.colour));
                data.insert("white".into(), json!(caps.white));
                return Ok(describe(layout, &dps));
            }
            Err(e) => last = e,
        }
    }
    Err(format!("the light at {} would not take that device ID and key: {last}", d.ip))
}

const PAIR_HELP: PairHelp = PairHelp {
    before: [
        " says what it needs. Tuya lights take a key from their cloud account.",
        "Type /home tuya IP DEVICE_ID LOCAL_KEY to add one with its key.",
    ],
    during: ["Trying that device ID and key…", "The light itself needs nothing done to it."],
    takes: Duration::from_secs(60),
};

fn found(ip: Ipv4Addr) -> Found {
    let mac = mac_of(ip).map(|m| format_mac(&m));
    Found { backend: "tuya", ip, mac, unit: None, data: Value::Null, checked: None }
}

impl Backend for Tuya {
    fn id(&self) -> &'static str {
        "tuya"
    }

    fn category(&self) -> Category {
        Category::Light
    }

    fn label(&self) -> &'static str {
        "Tuya lights"
    }

    // Swept for by port: the broadcasts Tuya documents are often not sent.
    fn discover(&self, each: &dyn Fn(Found)) {
        for ip in discover::sweep(&[PORT]) {
            each(found(ip));
        }
    }

    fn probe(&self, ip: Ipv4Addr) -> Option<Found> {
        TcpStream::connect_timeout(&SocketAddr::from((ip, PORT)), WAIT).ok().map(|_| found(ip))
    }

    fn about(&self, d: &Device) -> String {
        self.caps(d).describe().into()
    }

    fn found_detail(&self, d: &Device) -> String {
        if self.ready(d) {
            format!("Tuya · {}", self.about(d))
        } else {
            format!("Tuya · needs a key: /home tuya {} ID KEY", d.ip)
        }
    }

    fn check(&self, d: &Device) -> (State, String) {
        if !self.ready(d) {
            return (State::Ready, self.found_detail(d));
        }
        match status(d) {
            Ok(status) => (State::Ready, status),
            Err(_) => (State::Offline, "not answering".into()),
        }
    }

    fn test(&self, d: &mut Device) -> Result<String, String> {
        blink(d).map(|()| format!("{} blinked", d.name))
    }

    fn test_help(&self) -> &'static str {
        " blinks it so you can find it"
    }

    fn pairing(&self) -> Option<&'static PairHelp> {
        Some(&PAIR_HELP)
    }

    fn pair(&self, d: &mut Device) -> Result<String, String> {
        pair_at(d, address(d))
    }

    /// One that has answered to its key, not one that's merely been given it.
    fn ready(&self, d: &Device) -> bool {
        layout_saved(d).is_some() && field(d, "version").is_some()
    }

    fn as_light(&self) -> Option<&dyn LightDriver> {
        Some(self)
    }
}

impl LightDriver for Tuya {
    fn caps(&self, d: &Device) -> Caps {
        let has = |name: &str| d.data.get(name).and_then(Value::as_bool).unwrap_or(false);
        Caps { colour: has("colour"), white: has("white") }
    }

    fn set(&self, d: &Device, pilot: &Pilot) -> Result<Option<String>, String> {
        let (mut bulb, layout) = reach(d)?;
        // Only dimming needs to know what it is showing.
        let dimming = pilot.brightness.is_some() && pilot.rgb.is_none() && pilot.kelvin.is_none();
        let now = if dimming && layout.mode.is_some() { bulb.dps()? } else { Value::Null };
        bulb.set(&plan(layout, pilot, &now)).map(|()| None)
    }

    fn status(&self, d: &Device) -> Result<String, String> {
        status(d)
    }
}

/// What `/home tuya` was given, checked: an address, a device id, a key.
fn tuya_args(ip: Option<&str>, id: Option<&str>, key: Option<&str>) -> Result<(Ipv4Addr, String, String), String> {
    let (Some(ip), Some(id), Some(key)) = (ip, id, key) else { return Err(USAGE.into()) };
    let ip = ip.parse().map_err(|_| format!("{ip} isn't an IPv4 address"))?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(format!("that device ID doesn't look right; {USAGE}"));
    }
    if key.len() != 16 {
        return Err("a Tuya local key is 16 characters".into());
    }
    Ok((ip, id.to_string(), key.to_string()))
}

impl Home {
    /// `/home tuya IP ID KEY`: gives a light its key and pairs it.
    pub(super) fn tuya_configure(&mut self, ip: Option<&str>, id: Option<&str>, key: Option<&str>) -> String {
        let (ip, id, key) = match tuya_args(ip, id, key) {
            Ok(args) => args,
            Err(e) => return e,
        };
        let here = |d: &&mut Device| d.backend == "tuya" && d.ip == ip;
        if let Some(d) = self.config.devices.iter().find(|d| d.backend == "tuya" && d.ip == ip && Tuya.ready(d)) {
            return format!("{} is already added; press x on it in /devices first if its key changed", d.name);
        }
        self.on_found(found(ip));
        let data = json!({"id": id, "key": key});
        let waiting = self.found.iter_mut().find(here);
        let Some(d) = waiting.or_else(|| self.config.devices.iter_mut().find(here)) else {
            return format!("nothing to add at {ip}");
        };
        d.data = data;
        let device = d.id();
        self.add(&device);
        format!("trying that key on the light at {ip}…")
    }
}

#[cfg(test)]
mod tests;
