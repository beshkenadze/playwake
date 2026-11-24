//! Finds hosts on the local network: they announce the `_nvstream._tcp`
//! service over multicast DNS.

use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::time::{Duration, Instant};

/// One question: the PTR records of `_nvstream._tcp.local`, asking each
/// responder to answer this socket directly instead of the whole network.
fn question() -> Vec<u8> {
    let mut query = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in ["_nvstream", "_tcp", "local"] {
        query.push(label.len() as u8);
        query.extend_from_slice(label.as_bytes());
    }
    query.extend_from_slice(&[0, 0, 12, 0x80, 1]);
    query
}

/// Whether a datagram is a DNS response that carries at least one answer.
fn answers(datagram: &[u8]) -> bool {
    datagram.len() >= 12 && datagram[2] & 0x80 != 0 && u16::from_be_bytes([datagram[6], datagram[7]]) > 0
}

/// The addresses that answered within `wait`. An answer only shows that the
/// address runs the service; ask the host itself for its name and state.
pub fn discover(wait: Duration) -> Vec<IpAddr> {
    let mut found = Vec::new();
    let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else { return found };
    let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
    if socket.send_to(&question(), (Ipv4Addr::new(224, 0, 0, 251), 5353)).is_err() { return found; }
    let deadline = Instant::now() + wait;
    let mut datagram = [0u8; 2048];
    while Instant::now() < deadline {
        let Ok((length, from)) = socket.recv_from(&mut datagram) else { continue };
        if answers(&datagram[..length]) && !found.contains(&from.ip()) { found.push(from.ip()); }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_for_the_stream_service_with_a_direct_answer() {
        let query = question();
        assert_eq!(&query[..12], &[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&query[12..34], b"\x09_nvstream\x04_tcp\x05local\0");
        assert_eq!(&query[34..], &[0, 12, 0x80, 1]);
    }

    #[test]
    fn takes_only_responses_that_carry_an_answer() {
        let mut response = [0u8; 12];
        response[2] = 0x84;
        assert!(!answers(&response));
        response[7] = 1;
        assert!(answers(&response));
        response[2] = 0;
        assert!(!answers(&response));
        assert!(!answers(&response[..8]));
    }
}
