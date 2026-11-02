use std::net::TcpListener;
use std::sync::Arc;
use std::thread;

use native_tls::TlsAcceptor;
use rsa::BigUint;

use super::*;

fn hex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
}

/// protoc's encoding of each message, from the schema in androidtvremote2
/// 0.3.2, so these prove the bytes match what a real TV expects.
#[test]
fn sends_what_protoc_encodes() {
    let cases = [
        (polo(10, Pb::default().text(1, "atvremote").text(2, "hopchat")), "080210c80152140a0961747672656d6f74651207686f7063686174"),
        (polo(20, Pb::default().msg(1, hex_encoding()).int(3, 1)), "080210c801a201080a04080310061801"),
        (polo(30, Pb::default().msg(1, hex_encoding()).int(2, 1)), "080210c801f201080a04080310061001"),
        (polo(40, Pb::default().bytes(1, &[1, 2, 0xFF])), "080210c801c202050a030102ff"),
        (
            remote(1, Pb::default().int(1, 611).msg(2, Pb::default().int(3, 1).text(4, "1").text(5, "atvremote").text(6, "1.0.0"))),
            "0a1c08e304121718012201312a0961747672656d6f74653205312e302e30",
        ),
        (remote(2, Pb::default().int(1, 611)), "120308e304"),
        (remote(9, Pb::default().int(1, 7)), "4a020807"),
        (remote(10, Pb::default().int(1, KEY_POWER).int(2, SHORT)), "5204081a1003"),
        (
            remote(90, Pb::default().text(1, "market://launch?id=com.netflix.ninja")),
            "d205260a246d61726b65743a2f2f6c61756e63683f69643d636f6d2e6e6574666c69782e6e696e6a61",
        ),
    ];
    for (built, want) in cases {
        assert_eq!(built.0, hex(want));
    }
}

#[test]
fn reads_what_a_tv_sends() {
    let hello = Fields::parse(&hex("0a4e08ee0412490a054d5a54553012065869616f6d6918012201312a24636f6d2e676f6f676c652e616e64726f69642e74762e72656d6f74652e73657276696365320d352e322e343733323534313333")).unwrap();
    let configure = hello.msg(1).unwrap();
    assert_eq!(configure.int(1), Some(622));
    let info = configure.msg(2).unwrap();
    assert_eq!((info.text(1).as_deref(), info.text(2).as_deref()), (Some("MZTU0"), Some("Xiaomi")));
    let volume = Fields::parse(&hex("92030f08041a054d5a5455303064380f4001")).unwrap().msg(50).unwrap();
    assert_eq!((volume.int(6), volume.int(7), volume.int(8)), (Some(100), Some(15), Some(1)));
    assert_eq!(Fields::parse(&hex("c202020801")).unwrap().msg(40).unwrap().int(1), Some(1));
}

#[test]
fn broken_messages_are_refused_not_trusted() {
    assert!(Fields::parse(&[0x0a, 0x05, 1]).is_err(), "a length past the end");
    assert!(Fields::parse(&[0x08]).is_err(), "a key with no value");
    assert!(Fields::parse(&[0x0b]).is_err(), "a group, which nothing here sends");
    assert!(take_varint(&[0xFF; 11]).is_err());
    assert_eq!(take_varint(&[0x96, 0x01]), Ok(Some((150, 2))));
    assert_eq!(take_varint(&[0x96]), Ok(None));
}

/// The reference's hash of two keys and a code, worked with Python's
/// hashlib: n1 = 0xc1f1…f3, n2 = 0xd2e2…e5, e = 65537, code xxA1B2.
#[test]
fn the_pairing_secret_matches_the_reference() {
    let key = |head: &str, body: &str, tail: &str| {
        let n = BigUint::parse_bytes(format!("{head}{}{tail}", body.repeat(127)).as_bytes(), 16).unwrap();
        RsaPublicKey::new(n, BigUint::from(65537u32)).unwrap()
    };
    let (client, server) = (key("c", "1f", "3"), key("d", "2e", "5"));
    let want = hex("1f4923a08e7c2e3e44615994403788aca7194e654bc571efaf0147a5ceee4bd4");
    assert_eq!(secret(&client, &server, "1FA1B2").unwrap().to_vec(), want);
    assert_eq!(secret(&client, &server, " 1fa1b2 ").unwrap().to_vec(), want, "typed loosely");
    assert!(secret(&client, &server, "20A1B2").unwrap_err().contains("isn't the code"));
    assert!(secret(&client, &server, "1FA1B").unwrap_err().contains("six"));
    assert!(secret(&client, &server, "1FA1BZ").unwrap_err().contains("six"));
}

#[test]
fn apps_inputs_and_keys_by_spoken_name() {
    assert_eq!(app_link("Netflix").as_deref(), Some("https://www.netflix.com/title"));
    assert_eq!(app_link("open YouTube please").as_deref(), Some("https://www.youtube.com"));
    assert_eq!(app_link("youtube music").as_deref(), Some("https://music.youtube.com"));
    assert_eq!(app_link("in.startv.hotstar").as_deref(), Some("market://launch?id=in.startv.hotstar"));
    assert_eq!(app_link("https://www.netflix.com/title").as_deref(), Some("https://www.netflix.com/title"));
    assert_eq!(app_link("my favourite thing"), None);
    assert_eq!(input_key("HDMI_1"), Some(243));
    assert_eq!(input_key("hdmi 4"), Some(246));
    assert_eq!(input_key("HDMI 5"), None);
    assert_eq!(input_key("Live TV"), Some(KEY_TV));
}

#[test]
fn a_code_with_no_tv_waiting_says_so() {
    assert!(give_code(None).contains("/home code CODE"));
    assert!(give_code(Some("1FA1B2")).contains("no TV is waiting"));
    assert!(give_code_to("127.0.0.1:9".parse().unwrap(), "1FA1B2").is_err());
}

// ----- a stand-in TV -------------------------------------------------------------

/// One identity per role, made once: RSA keys take a while.
fn tv_identity() -> &'static (String, String) {
    static ID: OnceLock<(String, String)> = OnceLock::new();
    ID.get_or_init(|| new_identity().unwrap())
}

fn our_identity() -> &'static (String, String) {
    static ID: OnceLock<(String, String)> = OnceLock::new();
    ID.get_or_init(|| new_identity().unwrap())
}

#[test]
fn a_new_identity_is_a_usable_rsa_certificate() {
    let (cert, key) = our_identity();
    assert!(cert.starts_with("-----BEGIN CERTIFICATE-----") && key.starts_with("-----BEGIN PRIVATE KEY-----"));
    let public = public_key(&pem_to_der(cert).unwrap()).unwrap();
    assert_eq!(public.e(), &BigUint::from(65537u32));
    assert_eq!(public.n().bits(), 2048);
    assert!(identity(cert, key).is_ok());
    // A key-usage extension that leaves out digital signatures makes TVs
    // refuse the certificate; this one has none at all.
    let der = pem_to_der(cert).unwrap();
    let parsed = x509_cert::Certificate::from_der(&der).unwrap();
    let extensions = parsed.tbs_certificate.extensions.unwrap_or_default();
    let key_usage = x509_cert::spki::ObjectIdentifier::new_unwrap("2.5.29.15");
    assert!(extensions.iter().all(|e| e.extn_id != key_usage));
}

/// What the stand-in TV is doing, and what it was asked.
#[derive(Default)]
struct Screen {
    on: bool,
    level: u64,
    muted: bool,
    launched: Vec<String>,
    keys: Vec<u64>,
    pings_answered: usize,
}

/// Each TLS connection made to `listener`, for as long as it listens.
fn accept_tls(listener: TcpListener) -> impl Iterator<Item = TlsStream<TcpStream>> {
    let (cert, key) = tv_identity();
    let acceptor = TlsAcceptor::new(Identity::from_pkcs8(cert.as_bytes(), key.as_bytes()).unwrap()).unwrap();
    std::iter::from_fn(move || loop {
        let (tcp, _) = listener.accept().ok()?;
        tcp.set_read_timeout(Some(TICK)).unwrap();
        if let Ok(tls) = acceptor.accept(tcp) {
            return Some(tls);
        }
    })
}

/// A TV's pairing port: shows a code (sent to `codes`) and accepts the
/// secret only if it was worked from that code and `client`'s key.
fn fake_pairing(client: &'static str) -> (SocketAddr, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let to = listener.local_addr().unwrap();
    let (codes, shown) = mpsc::channel();
    thread::spawn(move || {
        for tls in accept_tls(listener) {
            let mut wire = Wire { stream: tls, buf: Vec::new() };
            let next = |wire: &mut Wire<_>| wire.next(Instant::now() + HELLO).ok().flatten();
            let ok = |field, inner| polo(field, inner);
            let Some(m) = next(&mut wire) else { continue };
            assert!(m.has(10));
            wire.send(&ok(11, Pb::default().text(1, "Mi TV"))).unwrap();
            assert!(next(&mut wire).unwrap().has(20));
            wire.send(&ok(20, Pb::default().msg(1, hex_encoding()).int(3, 1))).unwrap();
            assert!(next(&mut wire).unwrap().has(30));
            wire.send(&ok(31, Pb::default())).unwrap();
            // Work out a code the way a TV does, and show it.
            let ours = public_key(&pem_to_der(&tv_identity().0).unwrap()).unwrap();
            let theirs = public_key(&pem_to_der(client).unwrap()).unwrap();
            let tail: [u8; 2] = random();
            let mut hash = Sha256::new();
            for k in [&theirs, &ours] {
                hash.update(k.n().to_bytes_be());
                hash.update(k.e().to_bytes_be());
            }
            hash.update(tail);
            let want: [u8; 32] = hash.finalize().into();
            codes.send(format!("{:02X}{:02X}{:02X}", want[0], tail[0], tail[1])).unwrap();
            let got = next(&mut wire).unwrap();
            let sent = got.msg(40).and_then(|s| s.bytes(1).map(<[u8]>::to_vec));
            if sent.as_deref() == Some(want.as_slice()) {
                wire.send(&ok(41, Pb::default())).unwrap();
            } else {
                wire.send(&Pb::default().int(1, 2).int(2, 402)).unwrap();
            }
        }
    });
    (to, shown)
}

/// The remote side of a TV, on any stream.
fn serve_remote<S: Read + Write>(stream: S, screen: &Mutex<Screen>) -> Option<()> {
    let mut wire = Wire { stream, buf: Vec::new() };
    let next = |wire: &mut Wire<S>| wire.next(Instant::now() + HELLO).ok().flatten();
    let info = Pb::default().text(1, "MZTU0").text(2, "Xiaomi").int(3, 1);
    wire.send(&remote(1, Pb::default().int(1, 622).msg(2, info))).ok()?;
    let configured = next(&mut wire)?.msg(1)?;
    assert_eq!(configured.int(1), Some(622 & FEATURES));
    wire.send(&remote(2, Pb::default())).ok()?;
    assert_eq!(next(&mut wire)?.msg(2)?.int(1), Some(622 & FEATURES), "set-active echoes what was agreed");
    wire.send(&remote(8, Pb::default().int(1, 7).int(2, 9))).ok()?;
    assert_eq!(next(&mut wire)?.msg(9)?.int(1), Some(7));
    screen.lock().unwrap().pings_answered += 1;
    let report = |wire: &mut Wire<S>, s: &Screen| -> Option<()> {
        wire.send(&remote(40, Pb::default().int(1, u64::from(s.on)))).ok()?;
        let volume = Pb::default().int(6, 50).int(7, s.level).int(8, u64::from(s.muted));
        wire.send(&remote(50, volume)).ok()
    };
    report(&mut wire, &screen.lock().unwrap())?;
    loop {
        let m = next(&mut wire)?;
        let mut s = screen.lock().unwrap();
        if let Some(key) = m.msg(10) {
            assert_eq!(key.int(2), Some(SHORT));
            let code = key.int(1)?;
            s.keys.push(code);
            match code {
                KEY_POWER => s.on = !s.on,
                KEY_VOLUME_UP => s.level = (s.level + 1).min(50),
                KEY_VOLUME_DOWN => s.level = s.level.saturating_sub(1),
                KEY_MUTE => s.muted = !s.muted,
                _ => continue,
            }
            report(&mut wire, &s)?;
        } else if let Some(link) = m.msg(90) {
            let link = link.text(1)?;
            if link.starts_with("market://") {
                // As a Mi TV does: an error carrying the request, then it hangs up.
                wire.send(&remote(3, Pb::default().int(1, 7).bytes(2, &m_bytes(&link)))).ok()?;
                return None;
            }
            s.launched.push(link);
        }
    }
}

/// The request a refused link came in, as a TV's error echoes it.
fn m_bytes(link: &str) -> Vec<u8> {
    remote(90, Pb::default().text(1, link)).0
}

fn fake_remote(screen: Arc<Mutex<Screen>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let to = listener.local_addr().unwrap();
    thread::spawn(move || {
        for tls in accept_tls(listener) {
            let screen = screen.clone();
            thread::spawn(move || serve_remote(tls, &screen));
        }
    });
    to
}

/// A remote session over plain TCP, for what doesn't need TLS to test.
fn plain_remote(screen: Arc<Mutex<Screen>>) -> Remote<TcpStream> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let to = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        tcp.set_read_timeout(Some(TICK)).unwrap();
        serve_remote(tcp, &screen);
    });
    let tcp = TcpStream::connect(to).unwrap();
    tcp.set_read_timeout(Some(TICK)).unwrap();
    Remote::start(tcp).unwrap()
}

#[test]
fn a_session_answers_the_tv_and_learns_its_state() {
    let screen = Arc::new(Mutex::new(Screen { on: true, level: 10, ..Default::default() }));
    let r = plain_remote(screen.clone());
    assert_eq!(r.model.as_deref(), Some("Xiaomi MZTU0"));
    assert_eq!(r.on, Some(true));
    assert_eq!(r.describe(), "on, volume 20");
    assert_eq!(screen.lock().unwrap().pings_answered, 1);
}

#[test]
fn power_presses_the_button_only_when_needed() {
    let screen = Arc::new(Mutex::new(Screen { on: true, level: 10, ..Default::default() }));
    let mut r = plain_remote(screen.clone());
    assert_eq!(r.power(true), Ok("already on".into()));
    assert!(screen.lock().unwrap().keys.is_empty());
    assert_eq!(r.power(false), Ok("turned off".into()));
    assert_eq!(r.describe(), "off");
    assert_eq!(screen.lock().unwrap().keys, vec![KEY_POWER]);
}

#[test]
fn volume_steps_to_a_level_and_mutes() {
    let screen = Arc::new(Mutex::new(Screen { on: true, level: 10, ..Default::default() }));
    let mut r = plain_remote(screen.clone());
    // 30% of a TV that goes to 50 is 15: five steps up from 10.
    assert_eq!(r.volume(Some(30), None, None), Ok("volume 30".into()));
    assert_eq!(screen.lock().unwrap().level, 15);
    assert_eq!(r.volume(None, Some(-3), None), Ok("volume 24".into()));
    assert_eq!(screen.lock().unwrap().level, 12);
    assert_eq!(r.volume(None, None, Some(true)), Ok("volume 24, muted".into()));
    // Already muted: mute isn't pressed again, which would unmute it.
    r.volume(None, None, Some(true)).unwrap();
    assert_eq!(screen.lock().unwrap().keys.iter().filter(|k| **k == KEY_MUTE).count(), 1);
}

fn paired_device() -> Device {
    let (cert, key) = our_identity();
    Device::new("androidtv", "Living room".into(), Ipv4Addr::LOCALHOST, None, json!({"client_cert": cert, "client_key": key}))
}

#[test]
fn pairs_with_the_code_the_tv_shows() {
    let (cert, _) = our_identity();
    let (pair, shown) = fake_pairing(cert.as_str());
    let screen = Arc::new(Mutex::new(Screen { on: true, level: 25, ..Default::default() }));
    let remote = fake_remote(screen.clone());
    let mut d = paired_device();
    assert!(!AndroidTv.ready(&d));
    let pairing = thread::spawn(move || {
        let result = pair_at(&mut d, pair, remote);
        (d, result)
    });
    let code = shown.recv_timeout(Duration::from_secs(10)).unwrap();
    // A mistyped code is caught here, and the TV still waits for the right one.
    let wrong = format!("{:02X}{}", (u8::from_str_radix(&code[..2], 16).unwrap()).wrapping_add(1), &code[2..]);
    let until = Instant::now() + Duration::from_secs(5);
    let refused = loop {
        match give_code_to(pair, &wrong) {
            Err(e) if e.contains("isn't waiting") && Instant::now() < until => thread::sleep(Duration::from_millis(20)),
            other => break other,
        }
    };
    assert!(refused.unwrap_err().contains("isn't the code"));
    assert_eq!(give_code_to(pair, &code.to_lowercase()), Ok(()));
    let (d, result) = pairing.join().unwrap();
    assert_eq!(result, Ok("on, volume 50".into()));
    assert!(AndroidTv.ready(&d));
    assert_eq!(field(&d, "model"), Some("Xiaomi MZTU0"));
    assert_eq!(field(&d, "pin").map(str::len), Some(64));
    assert!(give_code_to(pair, &code).is_err(), "nothing waits once it's paired");

    // Commands go through the pinned connection.
    let mut r = remote_at(&d, remote).unwrap();
    r.launch(&app_link("youtube").unwrap()).unwrap();
    assert_eq!(screen.lock().unwrap().launched, vec!["https://www.youtube.com".to_string()]);
    // A link the TV refuses is reported, not taken as done.
    let mut r = remote_at(&d, remote).unwrap();
    let refused = r.launch(&app_link("com.example.missing").unwrap()).unwrap_err();
    assert!(refused.contains("couldn't open"), "{refused}");
}

#[test]
fn a_tv_with_another_certificate_is_refused() {
    let screen = Arc::new(Mutex::new(Screen { on: true, ..Default::default() }));
    let remote = fake_remote(screen);
    let mut d = paired_device();
    d.data["pin"] = json!("00".repeat(32));
    let error = remote_at(&d, remote).err().unwrap();
    assert!(error.contains("different certificate"), "{error}");
}

#[test]
fn an_unpaired_tv_says_how_to_pair() {
    let d = Device::new("androidtv", "Living room".into(), Ipv4Addr::LOCALHOST, None, json!({"name": "Living room"}));
    assert!(!AndroidTv.ready(&d));
    assert_eq!(AndroidTv.name(&d).as_deref(), Some("Living room"));
    assert!(remote_of(&d).err().unwrap().contains("isn't paired yet"));
}

/// Finds the Android TVs on this network and takes each as far through
/// pairing as it goes without a person: the TV shows its code, which
/// proves the certificate, the framing and the handshake against real
/// firmware. The TV drops the code when this ends.
/// `cargo test scan_for_android_tvs -- --ignored --nocapture`.
#[test]
#[ignore = "needs an Android TV on the network, and shows a pairing code on it"]
fn scan_for_android_tvs() {
    let found = Mutex::new(Vec::new());
    AndroidTv.discover(&|f| found.lock().unwrap().push(f));
    let found = found.into_inner().unwrap();
    assert!(!found.is_empty(), "no Android TV announced itself");
    let (cert, key) = our_identity();
    for f in found {
        let pair = SocketAddr::from((f.ip, PAIR_PORT));
        println!("{} {:?} {}", f.ip, f.mac, f.data);
        let attempt = thread::spawn(move || pair_with(pair, cert, key));
        let until = Instant::now() + Duration::from_secs(10);
        let shown = loop {
            if waiting().lock().unwrap().contains_key(&pair) {
                break true;
            }
            if attempt.is_finished() || Instant::now() > until {
                break false;
            }
            thread::sleep(Duration::from_millis(50));
        };
        if shown {
            println!("  {} is showing a pairing code: handshake accepted", f.ip);
        } else {
            panic!("{} never got to showing a code: {:?}", f.ip, attempt.join().unwrap());
        }
    }
}
