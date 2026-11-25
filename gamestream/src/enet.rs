//! The part of the ENet protocol the control stream needs: connecting,
//! reliable packets in both directions, acknowledgements and disconnecting.
//! One peer, no fragments, no unreliable sends: control messages are small.
//! All fields are big-endian.

use crate::Result;
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const CHANNELS: u32 = 48;
/// A connection-level command uses this channel number.
const NO_CHANNEL: u8 = 0xff;
const FLAG_SENT_TIME: u16 = 0x8000;
const FLAG_COMPRESSED: u16 = 0x4000;
const FLAG_ACKNOWLEDGE: u8 = 0x80;
/// How long to wait for an answer while connecting or leaving.
const HANDSHAKE_RESEND: Duration = Duration::from_millis(200);
/// How far ahead of the next expected packet a packet is kept.
const MOST_EARLY: u16 = 64;
/// The round trip assumed until the host has acknowledged something.
const FIRST_ROUND_TRIP: Duration = Duration::from_millis(50);
/// The host drops a session that is silent this long; so does the client.
const DEAD_AFTER: Duration = Duration::from_secs(10);
/// The longest wait between two sends of one packet. Each send of a packet
/// that is not acknowledged waits twice as long as the one before, as in
/// ENet: a link that stalls for seconds must not come back to a flood.
const LONGEST_RESEND: Duration = Duration::from_secs(1);

const ACKNOWLEDGE: u8 = 1;
const CONNECT: u8 = 2;
const VERIFY_CONNECT: u8 = 3;
const DISCONNECT: u8 = 4;
const SEND_RELIABLE: u8 = 6;

#[derive(Debug, PartialEq)]
pub(crate) enum Event {
    Packet(Vec<u8>),
    Disconnected,
}

struct Unacknowledged {
    channel: u8,
    sequence: u16,
    command: Vec<u8>,
    first_sent: Instant,
    last_sent: Instant,
    /// How many times it was sent again.
    resent: u32,
}

struct State {
    /// The host's slot and session bits, as they go in each header.
    peer: u16,
    incoming_session: u16,
    /// Last sequence number sent, by channel.
    sent: HashMap<u8, u16>,
    /// Last sequence number delivered, by channel.
    delivered: HashMap<u8, u16>,
    /// Packets that arrived before an earlier one of their channel.
    early: HashMap<(u8, u16), Vec<u8>>,
    unacknowledged: Vec<Unacknowledged>,
    /// Smoothed time from a send to its acknowledgement.
    round_trip: Duration,
}

impl State {
    /// How long a packet may stay unacknowledged before it is sent again:
    /// long enough for a normal answer, short enough that a lost input packet
    /// does not hold the ones behind it for long.
    fn resend_after(&self) -> Duration {
        (self.round_trip * 2 + Duration::from_millis(10)).clamp(Duration::from_millis(20), HANDSHAKE_RESEND)
    }

    /// How long a packet that was sent again `resent` times waits for the next send.
    fn next_resend(&self, resent: u32) -> Duration {
        (self.resend_after() * 2u32.saturating_pow(resent.min(16))).min(LONGEST_RESEND)
    }

    /// Takes a reliable packet and returns the packets that are now next in
    /// order on its channel. `None` for a packet too far ahead to keep, which
    /// the host must send again.
    fn deliver(&mut self, channel: u8, sequence: u16, data: &[u8]) -> Option<Vec<Vec<u8>>> {
        let delivered = self.delivered.entry(channel).or_insert(0);
        let ahead = sequence.wrapping_sub(*delivered);
        let mut ready = Vec::new();
        // Zero or "behind" is a repeat of a packet already delivered.
        if ahead == 0 || ahead >= 0x8000 { return Some(ready); }
        if ahead > MOST_EARLY { return None; }
        self.early.insert((channel, sequence), data.to_vec());
        while let Some(packet) = self.early.remove(&(channel, delivered.wrapping_add(1))) {
            *delivered = delivered.wrapping_add(1);
            ready.push(packet);
        }
        Some(ready)
    }

    /// Takes one measured round trip into the smoothed value.
    fn measured(&mut self, round_trip: Duration) {
        self.round_trip = (self.round_trip * 7 + round_trip) / 8;
    }
}

pub(crate) struct Enet {
    socket: UdpSocket,
    started: Instant,
    state: Mutex<State>,
}

/// A command from a datagram, borrowed from it.
struct Command<'a> {
    number: u8,
    acknowledge: bool,
    channel: u8,
    sequence: u16,
    body: &'a [u8],
    data: &'a [u8],
}

/// The size of each command's fixed part, header included.
fn fixed_size(number: u8) -> Option<usize> {
    Some(match number {
        ACKNOWLEDGE | DISCONNECT => 8,
        CONNECT => 48,
        VERIFY_CONNECT => 44,
        5 => 4,
        SEND_RELIABLE => 6,
        7 | 9 => 8,
        8 | 12 => 24,
        10 => 12,
        11 => 16,
        _ => return None,
    })
}

/// Splits a datagram into its session bits, sent time and commands.
fn parse(datagram: &[u8]) -> Option<(u16, u16, Vec<Command<'_>>)> {
    let peer = u16::from_be_bytes(datagram.get(..2)?.try_into().ok()?);
    if peer & FLAG_COMPRESSED != 0 { return None; }
    let (sent_time, mut rest) = if peer & FLAG_SENT_TIME != 0 {
        (u16::from_be_bytes(datagram.get(2..4)?.try_into().ok()?), &datagram[4..])
    } else {
        (0, &datagram[2..])
    };
    let mut commands = Vec::new();
    while rest.len() >= 4 {
        let number = rest[0] & 0x0f;
        let Some(fixed) = fixed_size(number) else { break };
        if rest.len() < fixed { break; }
        // Every data-carrying command has its length in the two bytes that
        // end its fixed part, except fragments, where it follows the start.
        let length = match number {
            SEND_RELIABLE => u16::from_be_bytes([rest[4], rest[5]]) as usize,
            7 | 9 => u16::from_be_bytes([rest[6], rest[7]]) as usize,
            8 | 12 => u16::from_be_bytes([rest[6], rest[7]]) as usize,
            _ => 0,
        };
        if rest.len() < fixed + length { break; }
        commands.push(Command {
            number,
            acknowledge: rest[0] & FLAG_ACKNOWLEDGE != 0,
            channel: rest[1],
            sequence: u16::from_be_bytes([rest[2], rest[3]]),
            body: &rest[4..fixed],
            data: &rest[fixed..fixed + length],
        });
        rest = &rest[fixed + length..];
    }
    Some(((peer >> 12) & 3, sent_time, commands))
}

fn command(number: u8, channel: u8, sequence: u16, body: &[u8]) -> Vec<u8> {
    let mut command = vec![number, channel];
    command.extend_from_slice(&sequence.to_be_bytes());
    command.extend_from_slice(body);
    command
}

fn acknowledgement(of: &Command, sent_time: u16) -> Vec<u8> {
    let mut body = of.sequence.to_be_bytes().to_vec();
    body.extend_from_slice(&sent_time.to_be_bytes());
    command(ACKNOWLEDGE, of.channel, of.sequence, &body)
}

impl Enet {
    fn clock(&self) -> u16 {
        self.started.elapsed().as_millis() as u16
    }

    /// Sends commands in one datagram. Reliable commands carry the send time,
    /// which the host echoes in its acknowledgement.
    fn transmit(&self, peer: u16, commands: &[u8], reliable: bool) {
        let mut datagram = Vec::with_capacity(4 + commands.len());
        if reliable {
            datagram.extend_from_slice(&(peer | FLAG_SENT_TIME).to_be_bytes());
            datagram.extend_from_slice(&self.clock().to_be_bytes());
        } else {
            datagram.extend_from_slice(&peer.to_be_bytes());
        }
        datagram.extend_from_slice(commands);
        // A lost datagram is sent again by the resend timer.
        let _ = self.socket.send(&datagram);
    }

    /// Connects to the host's control port. `data` lets the host match the
    /// connection to the session.
    pub(crate) fn connect(address: SocketAddr, data: u32, timeout: Duration) -> Result<Self> {
        let failed = |error: std::io::Error| format!("Control stream failed: {error}");
        let local: SocketAddr = if address.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }.parse().unwrap();
        let socket = UdpSocket::bind(local).map_err(failed)?;
        socket.connect(address).map_err(failed)?;
        socket.set_read_timeout(Some(HANDSHAKE_RESEND)).map_err(failed)?;
        let enet = Self {
            socket,
            started: Instant::now(),
            state: Mutex::new(State { peer: 0x0fff, incoming_session: 0, sent: HashMap::new(),
                delivered: HashMap::new(), early: HashMap::new(), unacknowledged: Vec::new(), round_trip: FIRST_ROUND_TRIP }),
        };
        let connect_id: [u8; 4] = rand::random();
        // Our peer slot 0, both session IDs unset, MTU 900, window 65536, no
        // bandwidth limits, the default throttle settings.
        let mut body = vec![0, 0, 0xff, 0xff];
        for value in [900u32, 65536, CHANNELS, 0, 0, 5000, 2, 2] { body.extend_from_slice(&value.to_be_bytes()); }
        body.extend_from_slice(&connect_id);
        body.extend_from_slice(&data.to_be_bytes());
        let connect = command(CONNECT | FLAG_ACKNOWLEDGE, NO_CHANNEL, 1, &body);

        let deadline = Instant::now() + timeout;
        let mut datagram = [0u8; 2048];
        while Instant::now() < deadline {
            enet.transmit(0x0fff, &connect, true);
            let Ok(length) = enet.socket.recv(&mut datagram) else { continue };
            let Some((_, sent_time, commands)) = parse(&datagram[..length]) else { continue };
            for verify in commands.iter().filter(|c| c.number == VERIFY_CONNECT && c.body[36..40] == connect_id) {
                let slot = u16::from_be_bytes([verify.body[0], verify.body[1]]) & 0x0fff;
                let (incoming_session, outgoing_session) = (verify.body[2] as u16 & 3, verify.body[3] as u16 & 3);
                let mut state = enet.state.lock().unwrap();
                state.peer = slot | outgoing_session << 12;
                state.incoming_session = incoming_session;
                // The host reports the connection only once this arrives.
                enet.transmit(state.peer, &acknowledgement(verify, sent_time), false);
                drop(state);
                return Ok(enet);
            }
            if commands.iter().any(|c| c.number == DISCONNECT) {
                return Err("The PC refused the control stream".into());
            }
        }
        Err("The PC did not answer on the control stream".into())
    }

    /// Queues a packet for reliable delivery on `channel` and sends it now.
    pub(crate) fn send(&self, channel: u8, packet: &[u8]) {
        let mut state = self.state.lock().unwrap();
        let sequence = state.sent.entry(channel).or_insert(0);
        *sequence = sequence.wrapping_add(1);
        let sequence = *sequence;
        let mut body = (packet.len() as u16).to_be_bytes().to_vec();
        body.extend_from_slice(packet);
        let command = command(SEND_RELIABLE | FLAG_ACKNOWLEDGE, channel, sequence, &body);
        self.transmit(state.peer, &command, true);
        let now = Instant::now();
        state.unacknowledged.push(Unacknowledged { channel, sequence, command, first_sent: now, last_sent: now, resent: 0 });
    }

    /// How many packets of `channel` the host has not acknowledged yet.
    pub(crate) fn in_flight(&self, channel: u8) -> usize {
        self.state.lock().unwrap().unacknowledged.iter().filter(|sent| sent.channel == channel).count()
    }

    /// Waits up to `wait` for a datagram, answers it, and resends what the
    /// host has not acknowledged. Returns the packets the host sent.
    pub(crate) fn service(&self, wait: Duration) -> Vec<Event> {
        let mut events = Vec::new();
        let mut datagram = [0u8; 2048];
        // Wake in time for the next resend.
        let wait = {
            let state = self.state.lock().unwrap();
            state.unacknowledged.iter().map(|sent| state.next_resend(sent.resent).saturating_sub(sent.last_sent.elapsed())).min()
                .map_or(wait, |until_resend| wait.min(until_resend))
        };
        let _ = self.socket.set_read_timeout(Some(wait.max(Duration::from_millis(1))));
        let received = self.socket.recv(&mut datagram).ok();
        let mut state = self.state.lock().unwrap();
        if let Some((session, sent_time, commands)) = received.and_then(|length| parse(&datagram[..length]))
            && session == state.incoming_session
        {
            let mut replies = Vec::new();
            for command in &commands {
                let mut acknowledge = command.acknowledge;
                match command.number {
                    ACKNOWLEDGE => {
                        let sequence = u16::from_be_bytes([command.body[0], command.body[1]]);
                        // The host echoes the send time of the datagram it answers.
                        let echoed = u16::from_be_bytes([command.body[2], command.body[3]]);
                        let round_trip = self.clock().wrapping_sub(echoed);
                        if round_trip < 1000 { state.measured(Duration::from_millis(round_trip.into())); }
                        state.unacknowledged.retain(|sent| (sent.channel, sent.sequence) != (command.channel, sequence));
                    }
                    SEND_RELIABLE => match state.deliver(command.channel, command.sequence, command.data) {
                        Some(ready) => events.extend(ready.into_iter().map(Event::Packet)),
                        None => acknowledge = false,
                    },
                    DISCONNECT => events.push(Event::Disconnected),
                    _ => {}
                }
                if acknowledge { replies.extend(acknowledgement(command, sent_time)); }
            }
            if !replies.is_empty() { self.transmit(state.peer, &replies, false); }
        }
        let now = Instant::now();
        if state.unacknowledged.iter().any(|sent| now.duration_since(sent.first_sent) > DEAD_AFTER) {
            state.unacknowledged.clear();
            events.push(Event::Disconnected);
        }
        let due: Vec<usize> = state.unacknowledged.iter().enumerate()
            .filter(|(_, sent)| now.duration_since(sent.last_sent) >= state.next_resend(sent.resent))
            .map(|(index, _)| index).collect();
        for index in due {
            let sent = &mut state.unacknowledged[index];
            sent.last_sent = now;
            sent.resent += 1;
            self.transmit(state.peer, &state.unacknowledged[index].command, true);
        }
        events
    }

    /// Tells the host the client is leaving and waits briefly for its answer.
    pub(crate) fn disconnect(&self) {
        let (peer, sequence) = {
            let mut state = self.state.lock().unwrap();
            state.unacknowledged.clear();
            // CONNECT was the first connection-level command.
            (state.peer, 2u16)
        };
        let leave = command(DISCONNECT | FLAG_ACKNOWLEDGE, NO_CHANNEL, sequence, &[0; 4]);
        let mut datagram = [0u8; 2048];
        let _ = self.socket.set_read_timeout(Some(HANDSHAKE_RESEND));
        for _ in 0..10 {
            self.transmit(peer, &leave, true);
            let Ok(length) = self.socket.recv(&mut datagram) else { continue };
            let acknowledged = parse(&datagram[..length]).is_some_and(|(_, _, commands)| commands.iter()
                .any(|c| c.number == ACKNOWLEDGE && c.channel == NO_CHANNEL && c.sequence == sequence));
            if acknowledged { return; }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn state() -> State {
        State { peer: 0, incoming_session: 0, sent: HashMap::new(), delivered: HashMap::new(), early: HashMap::new(),
            unacknowledged: Vec::new(), round_trip: FIRST_ROUND_TRIP }
    }

    #[test]
    fn delivers_reliable_packets_in_order_whatever_order_they_arrive_in() {
        let mut state = state();
        // The host's last message overtakes the one before it.
        assert_eq!(state.deliver(0, 2, b"end"), Some(vec![]));
        assert_eq!(state.deliver(0, 1, b"ping"), Some(vec![b"ping".to_vec(), b"end".to_vec()]));
        // Repeats are acknowledged again and not delivered again.
        assert_eq!(state.deliver(0, 2, b"end"), Some(vec![]));
        assert_eq!(state.deliver(0, 3, b"next"), Some(vec![b"next".to_vec()]));
        // Channels count separately.
        assert_eq!(state.deliver(1, 1, b"other"), Some(vec![b"other".to_vec()]));
        // A packet far ahead is not kept; the host sends it again.
        assert_eq!(state.deliver(0, 4 + MOST_EARLY, b"far"), None);
        assert!(state.early.is_empty());
    }

    #[test]
    fn splits_a_datagram_into_commands() {
        // Session 1, sent time 0x1234; a ping that wants an acknowledgement,
        // then a reliable packet "hi" on channel 0 with sequence 7.
        let datagram = [0x90, 0x00, 0x12, 0x34, 0x85, 0xff, 0x00, 0x03,
            0x86, 0x00, 0x00, 0x07, 0x00, 0x02, b'h', b'i'];
        let (session, sent_time, commands) = parse(&datagram).unwrap();
        assert_eq!((session, sent_time, commands.len()), (1, 0x1234, 2));
        assert_eq!((commands[0].number, commands[0].acknowledge, commands[0].channel, commands[0].sequence), (5, true, 0xff, 3));
        assert_eq!((commands[1].number, commands[1].channel, commands[1].sequence, commands[1].data), (6, 0, 7, &b"hi"[..]));
        assert_eq!(acknowledgement(&commands[1], sent_time), [0x01, 0x00, 0x00, 0x07, 0x00, 0x07, 0x12, 0x34]);
    }

    #[test]
    fn resends_sooner_on_a_fast_link_and_never_later_than_the_handshake() {
        let mut state = state();
        assert_eq!(state.resend_after(), Duration::from_millis(110));
        for _ in 0..64 { state.measured(Duration::from_millis(2)); }
        assert_eq!(state.resend_after(), Duration::from_millis(20));
        for _ in 0..64 { state.measured(Duration::from_millis(900)); }
        assert_eq!(state.resend_after(), HANDSHAKE_RESEND);
    }

    #[test]
    fn each_resend_of_a_packet_waits_twice_as_long_up_to_a_second() {
        let mut state = state();
        for _ in 0..64 { state.measured(Duration::from_millis(2)); }
        let waits: Vec<u64> = (0..8).map(|resent| state.next_resend(resent).as_millis() as u64).collect();
        assert_eq!(waits, [20, 40, 80, 160, 320, 640, 1000, 1000]);
        assert_eq!(state.next_resend(u32::MAX), LONGEST_RESEND);
    }

    #[test]
    fn stops_at_a_truncated_or_unknown_command() {
        // No sent time; a reliable packet that claims more data than there is.
        let (_, _, commands) = parse(&[0x00, 0x00, 0x86, 0x00, 0x00, 0x01, 0x00, 0x09, b'x']).unwrap();
        assert!(commands.is_empty());
        let (_, _, commands) = parse(&[0x00, 0x00, 0x0f, 0x00, 0x00, 0x01]).unwrap();
        assert!(commands.is_empty());
        assert!(parse(&[0x40, 0x00, 0x05, 0xff, 0x00, 0x01]).is_none());
    }

    /// A loopback "host" that completes the handshake as slot 5 with sessions
    /// 1 (host to client) and 2 (client to host), then hands over to `then`.
    pub(crate) fn host(then: impl FnOnce(&UdpSocket, SocketAddr) + Send + 'static) -> (SocketAddr, std::thread::JoinHandle<()>) {
        let host = UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = host.local_addr().unwrap();
        let thread = std::thread::spawn(move || {
            let mut buffer = [0u8; 2048];
            let (length, client) = host.recv_from(&mut buffer).unwrap();
            let (_, _, commands) = parse(&buffer[..length]).unwrap();
            assert_eq!(commands[0].number, CONNECT);
            let mut body = vec![0, 5, 1, 2];
            body.extend_from_slice(&commands[0].body[4..40]);
            let mut verify = vec![0x80, 0x00, 0x00, 0x09];
            verify.extend(command(VERIFY_CONNECT | FLAG_ACKNOWLEDGE, NO_CHANNEL, 1, &body));
            host.send_to(&verify, client).unwrap();
            then(&host, client);
        });
        (address, thread)
    }

    #[test]
    fn connects_sends_and_receives_over_loopback() {
        let (address, server) = host(|host, client| {
            let mut buffer = [0u8; 2048];
            // The acknowledgement of VERIFY_CONNECT, then the client's packet.
            let mut seen = Vec::new();
            while seen.len() < 2 {
                let (length, _) = host.recv_from(&mut buffer).unwrap();
                let header = u16::from_be_bytes([buffer[0], buffer[1]]);
                assert_eq!(header & 0x3fff, 5 | 2 << 12);
                let (_, sent_time, commands) = parse(&buffer[..length]).unwrap();
                for c in &commands {
                    if c.number == CONNECT { continue; }
                    if c.number == ACKNOWLEDGE { assert_eq!((c.channel, c.sequence, &c.body[2..4]), (NO_CHANNEL, 1, &[0, 9][..])); }
                    if c.number == SEND_RELIABLE {
                        assert_eq!((c.channel, c.sequence, c.data), (0x10, 1, &b"input"[..]));
                        let mut reply = (1u16 << 12 | FLAG_SENT_TIME).to_be_bytes().to_vec();
                        reply.extend_from_slice(&[0, 1]);
                        reply.extend(acknowledgement(c, sent_time));
                        reply.extend(command(SEND_RELIABLE | FLAG_ACKNOWLEDGE, 0, 1, &[0, 5, b'h', b'e', b'l', b'l', b'o']));
                        host.send_to(&reply, client).unwrap();
                    }
                    if seen.last() != Some(&c.number) { seen.push(c.number); }
                }
            }
        });
        let enet = Enet::connect(address, 0xdead_beef, Duration::from_secs(5)).unwrap();
        enet.send(0x10, b"input");
        let events = enet.service(Duration::from_secs(2));
        assert_eq!(events, [Event::Packet(b"hello".to_vec())]);
        assert!(enet.state.lock().unwrap().unacknowledged.is_empty());
        server.join().unwrap();
    }
}
