//! Finding devices on the local network: every backend looks in its own
//! way, all at once, and this holds what they share.

use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::driver::{Found, BACKENDS};

const PORT_WAIT: Duration = Duration::from_millis(400);
const SWEEPERS: usize = 64;

/// Every device any backend finds, reported through `each` as it is found.
pub fn scan(each: impl Fn(Found) + Send + Clone + 'static) {
    let scans: Vec<_> = BACKENDS
        .iter()
        .map(|backend| {
            let each = each.clone();
            thread::spawn(move || backend.discover(&each))
        })
        .collect();
    for scan in scans {
        let _ = scan.join();
    }
}

/// Addresses on this machine's /24 with any of `ports` open, for devices
/// that answer no broadcast.
pub fn sweep(ports: &[u16]) -> Vec<Ipv4Addr> {
    let Some(me) = local_ipv4() else { return Vec::new() };
    let [a, b, c, _] = me.octets();
    let hosts: Vec<Ipv4Addr> = (1..=254)
        .map(|d| Ipv4Addr::new(a, b, c, d))
        .filter(|ip| *ip != me)
        .collect();
    let (tx, rx) = mpsc::channel();
    let chunk = hosts.len().div_ceil(SWEEPERS);
    let workers: Vec<_> = hosts
        .chunks(chunk)
        .map(|part| {
            let part = part.to_vec();
            let ports = ports.to_vec();
            let tx = tx.clone();
            thread::spawn(move || {
                for ip in part {
                    let open = ports.iter().any(|port| {
                        TcpStream::connect_timeout(&SocketAddr::from((ip, *port)), PORT_WAIT).is_ok()
                    });
                    if open {
                        let _ = tx.send(ip);
                    }
                }
            })
        })
        .collect();
    drop(tx);
    for worker in workers {
        let _ = worker.join();
    }
    let mut found: Vec<Ipv4Addr> = rx.into_iter().collect();
    found.sort();
    found
}

/// The address this machine uses on the LAN. Connecting a UDP socket sends
/// nothing; it only makes the OS pick the outgoing interface.
pub fn local_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(addr) if !addr.ip().is_loopback() => Some(*addr.ip()),
        _ => None,
    }
}

/// Sends the magic packet that wakes a machine in standby, if it allows that.
pub fn wake(mac: [u8; 6]) -> Result<(), String> {
    let mut packet = vec![0xFFu8; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&mac);
    }
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    socket.set_broadcast(true).map_err(|e| e.to_string())?;
    for port in [9, 7] {
        socket.send_to(&packet, (Ipv4Addr::BROADCAST, port)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn mac_of(ip: Ipv4Addr) -> Option<[u8; 6]> {
    let out = std::process::Command::new("arp").arg("-n").arg(ip.to_string()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().find_map(parse_mac)
}

pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = text.split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (slot, part) in mac.iter_mut().zip(parts) {
        *slot = u8::from_str_radix(part, 16).ok()?;
    }
    Some(mac)
}

pub fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macs_from_arp() {
        // macOS drops leading zeros in arp output.
        assert_eq!(parse_mac("a4:36:c7:1:2:ff"), Some([0xa4, 0x36, 0xc7, 1, 2, 0xff]));
        assert_eq!(parse_mac("(incomplete)"), None);
        assert_eq!(format_mac(&[0xa4, 0x36, 0xc7, 1, 2, 0xff]), "a4:36:c7:01:02:ff");
    }
}
