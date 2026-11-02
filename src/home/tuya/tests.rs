use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

use super::*;

const KEY: Key = *b"0123456789abcdef";
const ID: &str = "bf0123456789abcdefghij";
const IV: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
/// tinytuya 1.20.0's own encoding of each frame, so these prove the driver
/// speaks what real bulbs do rather than only agreeing with itself.
const SEND_33: &str = "000055aa000000010000000700000037332e330000000000000000000000006c43740537c46421392c83bfa222a5ee31836d84bd28cb4b20619eb84882ca2ad1a187750000aa55";
const SEND_34: &str = "000055aa000000020000000d000000544490b05d74be9368c24a038cbaeded8e7dafb7b4370dab264878a7dbed69289164215f365b0dad6b2ac8fab15b1d40bdb8af3b99368110e9b0316a215e4d36405fd47bb571fa547ef0feddf10860bf1c0000aa55";
const SEND_35: &str = "000066990000000000030000000d0000003e000102030405060708090a0bce11f11d1e743b36e826071ac09bb05622b32789258931694559c8c99bd9de09236acd3b659258a0430f4332da8d9e18583400009966";
const QUERY_35: &str = "00006699000000000004000000100000001e000102030405060708090a0b8642e2774a4eb08c125444272bc2cbf631d400009966";
const RECV_33: &str = "000055aa000000010000000a0000002c000000006c43740537c46421392c83bfa222a5ee23a505a5f798f0d93e34296ee6a53506282487cf0000aa55";
const RECV_34: &str = "000055aa000000010000001000000048000000006c43740537c46421392c83bfa222a5ee23a505a5f798f0d93e34296ee6a5350600e6404f51322af4b866f53feb1b24aee11477907ec45a099468a3567eb8ac660000aa55";
const RECV_35: &str = "00006699000000000001000000100000003c000102030405060708090a0bfd3fc41d65565f469b043d61e2a9800f3aa3258f629f6879454bd0c6df9bd61180b827c429863b0c54c0c252e5b53cb800009966";

fn hex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
}

#[test]
fn sends_what_tinytuya_sends() {
    let body = br#"{"dps":{"20":true}}"#;
    assert_eq!(seal(Version::V33, &KEY, 1, CONTROL, body, IV), hex(SEND_33));
    assert_eq!(seal(Version::V34, &KEY, 2, CONTROL_NEW, body, IV), hex(SEND_34));
    assert_eq!(seal(Version::V35, &KEY, 3, CONTROL_NEW, body, IV), hex(SEND_35));
    assert_eq!(seal(Version::V35, &KEY, 4, DP_QUERY_NEW, b"{}", IV), hex(QUERY_35));
}

#[test]
fn reads_what_a_bulb_sends() {
    for (version, cmd, frame) in
        [(Version::V33, DP_QUERY, RECV_33), (Version::V34, DP_QUERY_NEW, RECV_34), (Version::V35, DP_QUERY_NEW, RECV_35)]
    {
        let frame = open_frame(&mut hex(frame).as_slice(), version, &KEY).unwrap();
        assert_eq!((frame.cmd, frame.retcode), (cmd, 0));
        assert_eq!(frame.dps(), Some(json!({"20": true, "22": 500})), "{version:?}");
    }
}

#[test]
fn a_frame_under_the_wrong_key_is_refused() {
    let other = *b"fedcba9876543210";
    assert!(open_frame(&mut hex(RECV_34).as_slice(), Version::V34, &other).is_err());
    assert!(open_frame(&mut hex(RECV_35).as_slice(), Version::V35, &other).is_err());
    // 3.3's check isn't keyed: it decrypts to something that isn't JSON.
    let frame = open_frame(&mut hex(RECV_33).as_slice(), Version::V33, &other).unwrap();
    assert_eq!(frame.dps(), None);
}

#[test]
fn a_corrupt_frame_is_refused() {
    for (version, text) in [(Version::V33, RECV_33), (Version::V34, RECV_34), (Version::V35, RECV_35)] {
        let mut frame = hex(text);
        frame[25] ^= 1;
        assert!(open_frame(&mut frame.as_slice(), version, &KEY).is_err(), "{version:?}");
    }
    let mut huge = hex(RECV_33);
    huge[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(open_frame(&mut huge.as_slice(), Version::V33, &KEY).is_err());
}

#[test]
fn a_frame_too_short_for_its_status_word_is_refused() {
    // Three bytes pass every length check, and 3.3's check takes no key.
    let head = |tag: usize| {
        let mut frame = PREFIX_55AA.to_vec();
        frame.extend_from_slice(&1u32.to_be_bytes());
        frame.extend_from_slice(&DP_QUERY.to_be_bytes());
        frame.extend_from_slice(&((3 + tag + SUFFIX_55AA.len()) as u32).to_be_bytes());
        frame.extend_from_slice(b"abc");
        frame
    };
    let mut crc = head(4);
    crc.extend_from_slice(&crc32fast::hash(&crc).to_be_bytes());
    crc.extend_from_slice(&SUFFIX_55AA);
    assert!(open_frame(&mut crc.as_slice(), Version::V33, &KEY).is_err());
    let mut tagged = head(32);
    tagged.extend_from_slice(&hmac_of(&KEY, &[&tagged]));
    tagged.extend_from_slice(&SUFFIX_55AA);
    assert!(open_frame(&mut tagged.as_slice(), Version::V34, &KEY).is_err());
}

#[test]
fn session_keys_match_tinytuya() {
    let ours: [u8; 16] = std::array::from_fn(|i| i as u8);
    let theirs: [u8; 16] = std::array::from_fn(|i| i as u8 + 16);
    assert_eq!(session_key(Version::V34, &KEY, &ours, &theirs).to_vec(), hex("377222e061a924c591cd9c27ea163ed4"));
    assert_eq!(session_key(Version::V35, &KEY, &ours, &theirs).to_vec(), hex("ed2fd40d0e642b26f836170ad08ba03d"));
}

#[test]
fn colours_match_tinytuya() {
    let text = |layout, rgb| {
        let (h, s, v) = to_hsv(rgb);
        colour_text(layout, h, s, v)
    };
    assert_eq!(text(&LAYOUT_B, (255, 0, 0)), "000003e803e8");
    assert_eq!(text(&LAYOUT_B, (255, 128, 0)), "001e03e803e8");
    assert_eq!(text(&LAYOUT_A, (0, 0, 255)), "0000ff00f0ffff");
    assert_eq!(text(&LAYOUT_A, (255, 128, 0)), "ff8000001effff");
}

#[test]
fn colours_survive_the_trip() {
    for rgb in [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 255), (128, 64, 200)] {
        let (h, s, v) = to_hsv(rgb);
        let (r, g, b) = to_rgb(h, s, v);
        assert_eq!((r, g, b), rgb);
        let (h2, s2, v2) = colour_read(&LAYOUT_B, &colour_text(&LAYOUT_B, h, s, v)).unwrap();
        assert!((h - h2).abs() <= 0.5 && (s - s2).abs() < 0.001 && (v - v2).abs() < 0.001);
    }
}

#[test]
fn layouts_come_from_what_a_bulb_answers() {
    let b = json!({"20": true, "21": "white", "22": 500, "23": 0, "24": "000003e803e8"});
    assert_eq!(layout_of(&b), Some(&LAYOUT_B));
    assert_eq!(caps_in(&LAYOUT_B, &b), Caps { colour: true, white: true });
    let white = json!({"20": true, "21": "white", "22": 500, "23": 0});
    assert_eq!(caps_in(&LAYOUT_B, &white), Caps { colour: false, white: true });
    let dim = json!({"20": true, "22": 500});
    assert_eq!(caps_in(&LAYOUT_B, &dim), Caps { colour: false, white: false });
    let a = json!({"1": true, "2": "white", "3": 255, "4": 0, "5": "ff0000000003ff"});
    assert_eq!(layout_of(&a), Some(&LAYOUT_A));
    let c = json!({"1": true, "2": 255, "3": 0});
    assert_eq!(layout_of(&c), Some(&LAYOUT_C));
    // A plug: a switch, a timer, and power readings.
    assert_eq!(layout_of(&json!({"1": true, "9": 0, "18": 120})), None);
}

fn dps(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

#[test]
fn plans_what_a_pilot_asks() {
    let none = Value::Null;
    let off = Pilot { on: Some(false), ..Default::default() };
    assert_eq!(plan(&LAYOUT_B, &off, &none), dps(json!({"20": false})));
    assert_eq!(plan(&LAYOUT_A, &off, &none), dps(json!({"1": false})));

    let dim = Pilot { brightness: Some(50), ..Default::default() };
    assert_eq!(plan(&LAYOUT_B, &dim, &none), dps(json!({"20": true, "22": 500})));
    let lowest = Pilot { brightness: Some(0), ..Default::default() };
    assert_eq!(plan(&LAYOUT_B, &lowest, &none), dps(json!({"20": true, "22": 10})));
    assert_eq!(plan(&LAYOUT_A, &lowest, &none), dps(json!({"1": true, "3": 25})));

    let warm = Pilot { kelvin: Some(2000), ..Default::default() };
    assert_eq!(plan(&LAYOUT_B, &warm, &none), dps(json!({"20": true, "21": "white", "23": 0})));
    let cool = Pilot { kelvin: Some(6500), brightness: Some(100), ..Default::default() };
    assert_eq!(plan(&LAYOUT_B, &cool, &none), dps(json!({"20": true, "21": "white", "22": 1000, "23": 1000})));

    let red = Pilot { rgb: Some((255, 0, 0)), ..Default::default() };
    assert_eq!(plan(&LAYOUT_B, &red, &none), dps(json!({"20": true, "21": "colour", "24": "000003e803e8"})));
    let dim_red = Pilot { rgb: Some((255, 0, 0)), brightness: Some(50), kelvin: Some(3000), ..Default::default() };
    assert_eq!(plan(&LAYOUT_B, &dim_red, &none), dps(json!({"20": true, "21": "colour", "24": "000003e801f4"})));

    assert_eq!(plan(&LAYOUT_B, &Pilot::default(), &none), Map::new());
}

#[test]
fn dimming_a_coloured_bulb_keeps_its_colour() {
    let now = json!({"20": true, "21": "colour", "22": 1000, "23": 0, "24": "007803e803e8"});
    let dim = Pilot { brightness: Some(25), ..Default::default() };
    assert_eq!(plan(&LAYOUT_B, &dim, &now), dps(json!({"20": true, "24": "007803e800fa"})));
    let white = json!({"20": true, "21": "white", "22": 1000, "24": "007803e803e8"});
    assert_eq!(plan(&LAYOUT_B, &dim, &white), dps(json!({"20": true, "22": 250})));
    // Going to white from a colour sets brightness on the white channel.
    let both = Pilot { brightness: Some(25), kelvin: Some(2700), ..Default::default() };
    assert_eq!(plan(&LAYOUT_B, &both, &now), dps(json!({"20": true, "21": "white", "22": 250, "23": 0})));
}

#[test]
fn describes_what_it_is_doing() {
    assert_eq!(describe(&LAYOUT_B, &json!({"20": false, "22": 500})), "off");
    assert_eq!(describe(&LAYOUT_B, &json!({"20": true, "21": "white", "22": 500, "23": 0})), "on · 50% · 2700K");
    assert_eq!(describe(&LAYOUT_B, &json!({"20": true, "21": "white", "22": 1000, "23": 1000})), "on · 100% · 6500K");
    assert_eq!(describe(&LAYOUT_B, &json!({"20": true, "21": "colour", "24": "000003e801f4"})), "on · 50% · #800000");
    assert_eq!(describe(&LAYOUT_B, &json!({"20": true, "22": 1000})), "on · 100%");
}

#[test]
fn checks_what_it_was_typed() {
    let args = |ip, id, key| tuya_args(Some(ip), Some(id), Some(key));
    assert_eq!(args("192.168.0.10", ID, "0123456789abcdef"), Ok((Ipv4Addr::new(192, 168, 0, 10), ID.into(), "0123456789abcdef".into())));
    assert!(args("192.168.0.10", ID, "short").unwrap_err().contains("16 characters"));
    assert!(args("bulb", ID, "0123456789abcdef").unwrap_err().contains("IPv4"));
    assert!(args("192.168.0.10", "no/slashes", "0123456789abcdef").is_err());
    assert_eq!(tuya_args(None, None, None), Err(USAGE.into()));
}

// ----- a stand-in bulb -------------------------------------------------------

/// A bulb on a loopback port speaking one version, that remembers what it
/// was told. It hangs up on a frame it can't read, as a real one does.
struct Fake {
    to: SocketAddr,
    state: Arc<Mutex<Value>>,
    /// Refuse a plain query until asked for data points by name.
    named_only: bool,
}

impl Fake {
    fn start(version: Version, dps: Value) -> Fake {
        Fake::spawn(version, dps, false)
    }

    fn spawn(version: Version, dps: Value, named_only: bool) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let to = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(dps));
        let shared = state.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = shared.clone();
                thread::spawn(move || serve(stream, version, state, named_only));
            }
        });
        Fake { to, state, named_only }
    }

    fn state(&self) -> Value {
        self.state.lock().unwrap().clone()
    }
}

fn serve(mut stream: TcpStream, version: Version, state: Arc<Mutex<Value>>, named_only: bool) -> Option<()> {
    let mut key = KEY;
    if version != Version::V33 {
        let start = open_frame(&mut stream, version, &key).ok()?;
        (start.cmd == SESS_KEY_NEG_START).then_some(())?;
        let ours: [u8; 16] = start.body.get(..16)?.try_into().ok()?;
        let theirs = *b"bulb-nonce-16byt";
        let reply = [theirs.as_slice(), &hmac_of(&key, &[&ours])].concat();
        stream.write_all(&seal(version, &key, 1, SESS_KEY_NEG_RESP, &reply, IV)).ok()?;
        let finish = open_frame(&mut stream, version, &key).ok()?;
        (finish.cmd == SESS_KEY_NEG_FINISH && finish.body == hmac_of(&key, &[&theirs])).then_some(())?;
        key = session_key(version, &key, &ours, &theirs);
    }
    let mut seq = 2;
    loop {
        let frame = open_frame(&mut stream, version, &key).ok()?;
        let json = frame.json()?;
        let mut reply = |cmd, body: Value| {
            seq += 1;
            let text = if body.is_null() { String::new() } else { body.to_string() };
            stream.write_all(&seal(version, &key, seq, cmd, text.as_bytes(), [seq as u8; 12])).ok()
        };
        match frame.cmd {
            DP_QUERY | DP_QUERY_NEW if named_only => reply(frame.cmd, json!({"error": "json obj data unvalid"}))?,
            DP_QUERY => reply(DP_QUERY, json!({"devId": ID, "dps": state.lock().unwrap().clone()}))?,
            DP_QUERY_NEW => reply(DP_QUERY_NEW, json!({"protocol": 4, "data": {"dps": state.lock().unwrap().clone()}}))?,
            CONTROL_NEW if json.get("t").is_some_and(Value::is_string) => {
                reply(STATUS, json!({"dps": state.lock().unwrap().clone()}))?
            }
            CONTROL | CONTROL_NEW => {
                let told = json.pointer("/dps").or_else(|| json.pointer("/data/dps"))?;
                let mut now = state.lock().unwrap();
                for (k, v) in told.as_object()? {
                    now[k] = v.clone();
                }
                reply(frame.cmd, Value::Null)?;
            }
            _ => return None,
        }
    }
}

fn unpaired(key: &str) -> Device {
    Device::new("tuya", "Light".into(), Ipv4Addr::LOCALHOST, None, json!({"id": ID, "key": key}))
}

fn colour_bulb() -> Value {
    json!({"20": true, "21": "white", "22": 500, "23": 0, "24": "000003e803e8"})
}

#[test]
fn pairs_whichever_version_a_bulb_speaks() {
    for version in VERSIONS {
        let fake = Fake::start(version, colour_bulb());
        let mut d = unpaired("0123456789abcdef");
        assert!(!Tuya.ready(&d));
        assert_eq!(pair_at(&mut d, fake.to), Ok("on · 50% · 2700K".into()), "{version:?}");
        assert!(Tuya.ready(&d));
        assert_eq!(field(&d, "version"), Some(version.text()));
        assert_eq!(field(&d, "layout"), Some("b"));
        assert_eq!(Tuya.caps(&d), Caps { colour: true, white: true });
    }
}

#[test]
fn drives_a_bulb_on_every_version() {
    for version in VERSIONS {
        let fake = Fake::start(version, colour_bulb());
        let mut bulb = Bulb::connect(fake.to, version, ID, &KEY).unwrap();
        // More than one action on a connection, as a blink does.
        bulb.set(&dps(json!({"20": false}))).unwrap();
        bulb.set(&dps(json!({"22": 900, "20": true}))).unwrap();
        assert_eq!(bulb.dps().unwrap()["22"], json!(900));
        assert_eq!(fake.state()["22"], json!(900), "{version:?}");
    }
}

#[test]
fn a_wrong_key_pairs_nothing() {
    for version in VERSIONS {
        let fake = Fake::start(version, colour_bulb());
        let mut d = unpaired("fedcba9876543210");
        let error = pair_at(&mut d, fake.to).unwrap_err();
        assert!(error.contains("would not take"), "{version:?}: {error}");
        assert!(!Tuya.ready(&d));
    }
}

#[test]
fn a_plug_is_not_a_light() {
    let fake = Fake::start(Version::V33, json!({"1": true, "9": 0}));
    let mut d = unpaired("0123456789abcdef");
    assert!(pair_at(&mut d, fake.to).unwrap_err().contains("isn't a dimmable light"));
    assert!(!Tuya.ready(&d));
}

#[test]
fn pairs_firmware_that_wants_data_points_named() {
    let fake = Fake::spawn(Version::V33, colour_bulb(), true);
    assert!(fake.named_only);
    let mut d = unpaired("0123456789abcdef");
    assert_eq!(pair_at(&mut d, fake.to), Ok("on · 50% · 2700K".into()));
}

#[test]
fn an_unpaired_bulb_says_how_to_add_it() {
    let mut d = Device::new("tuya", "Light".into(), Ipv4Addr::new(192, 168, 0, 10), None, Value::Null);
    assert_eq!(Tuya.found_detail(&d), "Tuya · needs a key: /home tuya 192.168.0.10 ID KEY");
    let error = Tuya.pair(&mut d).unwrap_err();
    assert!(error.contains("/home tuya IP DEVICE_ID LOCAL_KEY") && error.contains("192.168.0.10"));
    assert!(reach(&d).err().is_some_and(|e| e.contains("hasn't got its key")));
}
