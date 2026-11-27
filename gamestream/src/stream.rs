//! A running stream: the handshake, then one thread each for the control
//! stream, video and audio. Events reach the caller through a channel.

use crate::control::{self, Cipher, Gamepad, MouseButton, Touch};
use crate::enet::{self, Enet};
use crate::video::{Assembler, Frame};
use crate::{Host, Result, Session, rtsp};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Codec { H264, Hevc, Av1 }

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Config {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub codec: Codec,
    pub hdr: bool,
    /// Ask for full-range video: 1023 codes for each 10-bit sample, not 876.
    pub full_range: bool,
}

#[derive(Debug, PartialEq)]
pub enum Event {
    Video(Frame),
    /// One Opus packet: 48 kHz stereo, 5 ms.
    Audio(Vec<u8>),
    /// Motor strengths for the gamepad, low and high frequency.
    Rumble(u16, u16),
    /// The host now sends HDR for this display, or SDR for `None`. A stream
    /// that asked for HDR still gets SDR when the host could not turn HDR on.
    Hdr(Option<control::HdrDisplay>),
    /// The stream is over; `Ok` when the host ended it on purpose.
    Ended(Result<()>),
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_PING: Duration = Duration::from_millis(100);
const MEDIA_PING: Duration = Duration::from_millis(500);
/// How long after the video pings the audio pings go out. The host keeps the
/// sender of a ping in one place for both streams, so two pings that arrive
/// together can leave it sending video to the audio port.
const AUDIO_PING_OFFSET: Duration = Duration::from_millis(250);
const RECEIVE_WAIT: Duration = Duration::from_millis(100);
/// Room for 2048 video packets, as the reference client asks. A key frame
/// arrives in one burst that is larger than the system's default buffer.
const VIDEO_RECEIVE_BUFFER: usize = 2048 * (rtsp::PACKET_SIZE as usize + 16);
/// How long to wait for a key frame before asking the host again.
const KEY_FRAME_REQUEST_GAP: Duration = Duration::from_millis(250);
const GRACEFUL: u32 = 0x8003_0023;
/// Control stream channels: general, urgent, keyboard, mouse, touch, and the
/// first gamepad.
const CHANNEL_GENERAL: u8 = 0;
const CHANNEL_URGENT: u8 = 1;
const CHANNEL_KEYBOARD: u8 = 2;
const CHANNEL_MOUSE: u8 = 3;
const CHANNEL_TOUCH: u8 = 5;
const CHANNEL_GAMEPAD: u8 = 0x10;
/// How many gamepad states or mouse moves may wait for the host's
/// acknowledgement. More wait here, merged, so that a stalled link sends only
/// what still matters.
const IN_FLIGHT: usize = 8;
/// Button changes kept while the link is stalled.
const GAMEPAD_QUEUE: usize = 64;

struct Control {
    enet: Enet,
    cipher: Mutex<Cipher>,
    /// Gamepad states not sent yet, oldest first.
    gamepads: Mutex<VecDeque<Gamepad>>,
    /// Mouse motion not sent yet. Every send on the mouse channel holds this
    /// lock, so the mouse packets reach the host in the order they were made.
    motion: Mutex<Option<Motion>>,
}

impl Control {
    fn send(&self, channel: u8, kind: u16, payload: &[u8]) {
        let message = self.cipher.lock().unwrap().seal(kind, payload);
        self.enet.send(channel, &message);
    }

    /// Sends the gamepad states that the link has room for.
    fn send_gamepads(&self) {
        let mut waiting = self.gamepads.lock().unwrap();
        while !waiting.is_empty() && self.enet.in_flight(CHANNEL_GAMEPAD) < IN_FLIGHT {
            let state = waiting.pop_front().unwrap();
            self.send(CHANNEL_GAMEPAD, control::INPUT, &state.packet());
        }
    }

    /// Sends the waiting mouse motion if the link has room for it, or at once
    /// when `now`.
    fn send_motion(&self, waiting: &mut Option<Motion>, now: bool) {
        if !now && self.enet.in_flight(CHANNEL_MOUSE) >= IN_FLIGHT { return; }
        for packet in waiting.take().map(Motion::packets).unwrap_or_default() {
            self.send(CHANNEL_MOUSE, control::INPUT, &packet);
        }
    }

    fn move_mouse(&self, motion: Motion) {
        let mut waiting = self.motion.lock().unwrap();
        if let Some(earlier) = merge_motion(&mut waiting, motion) {
            self.send_motion(&mut Some(earlier), true);
        }
        self.send_motion(&mut waiting, false);
    }

    /// Sends a mouse packet that is not motion, after the motion made before it.
    fn send_mouse(&self, packet: &[u8]) {
        let mut waiting = self.motion.lock().unwrap();
        self.send_motion(&mut waiting, true);
        self.send(CHANNEL_MOUSE, control::INPUT, packet);
    }
}

/// Mouse motion as the host takes it: moves by deltas, or a position on an
/// area of a size.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Motion {
    Relative(i32, i32),
    Absolute { x: i16, y: i16, width: i16, height: i16 },
}

impl Motion {
    fn packets(self) -> Vec<Vec<u8>> {
        match self {
            Motion::Relative(dx, dy) => control::mouse_move_packets(dx, dy).iter().map(|packet| packet.to_vec()).collect(),
            Motion::Absolute { x, y, width, height } => vec![control::mouse_position_packet(x, y, width, height).to_vec()],
        }
    }
}

/// Merges `motion` into the motion that waits, as the reference client does:
/// moves add up, and a position replaces the last one. Motion of the other
/// kind does not merge; the motion that waited is returned, to go out first.
fn merge_motion(waiting: &mut Option<Motion>, motion: Motion) -> Option<Motion> {
    match (waiting.as_mut(), motion) {
        (Some(Motion::Relative(x, y)), Motion::Relative(dx, dy)) => {
            (*x, *y) = (x.saturating_add(dx), y.saturating_add(dy));
            None
        }
        (Some(last @ Motion::Absolute { .. }), Motion::Absolute { .. }) => {
            *last = motion;
            None
        }
        _ => waiting.replace(motion),
    }
}

/// Puts `state` behind the gamepad states that wait. A state that changes
/// only sticks and triggers replaces the last one: the host needs where they
/// are now, not each step. A button change is kept, so a short press is not
/// lost; past `GAMEPAD_QUEUE` the oldest goes.
fn queue_gamepad(waiting: &mut VecDeque<Gamepad>, state: Gamepad) {
    match waiting.back_mut() {
        Some(last) if last.buttons == state.buttons => *last = state,
        _ => {
            if waiting.len() == GAMEPAD_QUEUE { waiting.pop_front(); }
            waiting.push_back(state);
        }
    }
}

pub struct Stream {
    control: Arc<Control>,
    /// The host takes touches: it said so in its stream description.
    touch: bool,
    stop: Arc<AtomicBool>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

/// A socket that keeps telling the host where to send one media stream.
struct Media {
    socket: UdpSocket,
    ping: [u8; 20],
    pings: u32,
    next_ping: Instant,
}

impl Media {
    /// `first_ping` is how long after opening the first ping goes out.
    fn open(host: SocketAddr, payload: &[u8; 16], first_ping: Duration) -> Result<Self> {
        let failed = |error: std::io::Error| format!("Media stream failed: {error}");
        let local: SocketAddr = if host.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }.parse().unwrap();
        let socket = UdpSocket::bind(local).map_err(failed)?;
        socket.connect(host).map_err(failed)?;
        socket.set_read_timeout(Some(RECEIVE_WAIT)).map_err(failed)?;
        let mut ping = [0u8; 20];
        ping[..16].copy_from_slice(payload);
        Ok(Self { socket, ping, pings: 0, next_ping: Instant::now() + first_ping })
    }

    /// Receives one datagram, pinging the host on the way when it is time.
    fn receive<'a>(&mut self, buffer: &'a mut [u8]) -> Option<&'a [u8]> {
        if Instant::now() >= self.next_ping {
            self.pings += 1;
            self.ping[16..].copy_from_slice(&self.pings.to_be_bytes());
            let _ = self.socket.send(&self.ping);
            // A fixed beat, so the two streams' pings stay apart.
            self.next_ping += MEDIA_PING;
        }
        let length = self.socket.recv(buffer).ok()?;
        Some(&buffer[..length])
    }
}

/// Whether a packet with this sequence number comes after the last one played.
fn follows(sequence: u16, last: Option<u16>) -> bool {
    last.is_none_or(|last| (1..0x8000).contains(&sequence.wrapping_sub(last)))
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Stream {
    /// Starts streaming the session that `Host::launch` returned.
    pub fn start(host: &Host, session: &Session, config: &Config) -> Result<(Self, mpsc::Receiver<Event>)> {
        let port = session.rtsp_url.rsplit_once(':').and_then(|(_, port)| port.parse().ok()).unwrap_or(48010);
        let address = (host.address.as_str(), port).to_socket_addrs().ok().and_then(|mut found| found.next())
            .ok_or_else(|| format!("Cannot resolve {}", host.address))?;
        let on_port = |port| SocketAddr::new(address.ip(), port);

        let ports = rtsp::handshake(address, config)?;
        // The host ends the session unless both media streams ping it soon.
        let mut video = Media::open(on_port(ports.video), &ports.ping_payload, Duration::ZERO)?;
        // The system grants what its limit allows; a smaller buffer still works.
        let _ = socket2::SockRef::from(&video.socket).set_recv_buffer_size(VIDEO_RECEIVE_BUFFER);
        let mut audio = Media::open(on_port(ports.audio), &ports.ping_payload, AUDIO_PING_OFFSET)?;
        let control = Arc::new(Control {
            enet: Enet::connect(on_port(ports.control), ports.connect_data, CONNECT_TIMEOUT)?,
            cipher: Mutex::new(Cipher::new(&session.key)),
            gamepads: Mutex::new(VecDeque::new()),
            motion: Mutex::new(None),
        });
        control.send(CHANNEL_GENERAL, control::REQUEST_IDR, &[0, 0]);
        control.send(CHANNEL_GENERAL, control::START_B, &[0]);
        // The host makes its virtual gamepad on the first state it gets, which
        // can take seconds; start that now, not on the player's first press.
        // A pad left from an earlier stream is unplugged first: the new one
        // arrives on the PC as a new controller, which wakes Steam from its
        // screensaver as Moonlight's does. Stick moves below the games' dead
        // zone, triggers and keys do not.
        control.send(CHANNEL_GAMEPAD, control::INPUT, &Gamepad::unplug_packet());
        control.send(CHANNEL_GAMEPAD, control::INPUT, &Gamepad::default().packet());

        let stop = Arc::new(AtomicBool::new(false));
        let (events, receiver) = mpsc::channel();
        let mut threads = Vec::new();

        let (running, sender, link) = (stop.clone(), events.clone(), control.clone());
        threads.push(thread::spawn(move || {
            let mut next_ping = Instant::now();
            let mut ended = None;
            while !running.load(Ordering::Relaxed) && ended.is_none() {
                if Instant::now() >= next_ping {
                    link.send(CHANNEL_GENERAL, control::PING, &[4, 0, 0, 0, 0, 0, 0, 0]);
                    next_ping += CONTROL_PING;
                }
                let events = link.enet.service(next_ping.saturating_duration_since(Instant::now()));
                // Acknowledgements may have made room for gamepad states and
                // mouse motion that wait.
                link.send_gamepads();
                link.send_motion(&mut link.motion.lock().unwrap(), false);
                for event in events {
                    let packet = match event {
                        enet::Event::Packet(packet) => packet,
                        enet::Event::Disconnected => { ended = Some(Err("The PC closed the stream".to_owned())); break }
                    };
                    match link.cipher.lock().unwrap().open(&packet) {
                        Ok((control::TERMINATION, reason)) => {
                            let code = reason.get(..4).map(|code| u32::from_be_bytes(code.try_into().unwrap()));
                            ended = Some(if code == Some(GRACEFUL) { Ok(()) }
                                else { Err(format!("The PC ended the stream ({:#x})", code.unwrap_or(0))) });
                        }
                        Ok((control::RUMBLE, motors)) if motors.len() >= 10 => {
                            let motor = |at: usize| u16::from_le_bytes([motors[at], motors[at + 1]]);
                            let _ = sender.send(Event::Rumble(motor(6), motor(8)));
                        }
                        Ok((control::HDR_MODE, mode)) => {
                            if let Some(display) = control::hdr_mode(&mode) { let _ = sender.send(Event::Hdr(display)); }
                        }
                        Ok(_) => {}
                        Err(error) => ended = Some(Err(error)),
                    }
                }
            }
            running.store(true, Ordering::Relaxed);
            link.enet.disconnect();
            if let Some(result) = ended { let _ = sender.send(Event::Ended(result)); }
        }));

        let (running, sender, link) = (stop.clone(), events.clone(), control.clone());
        threads.push(thread::spawn(move || {
            let mut assembler = Assembler::new();
            let mut buffer = vec![0u8; 2048];
            // `start` asked for the first key frame.
            let mut asked_at = Instant::now();
            while !running.load(Ordering::Relaxed) {
                let frame = video.receive(&mut buffer).and_then(|datagram| assembler.push(datagram));
                if let Some(frame) = frame { let _ = sender.send(Event::Video(frame)); }
                // The answer to a request can be lost as well, so ask again
                // for as long as the picture needs a key frame.
                if assembler.needs_key_frame() && asked_at.elapsed() >= KEY_FRAME_REQUEST_GAP {
                    asked_at = Instant::now();
                    link.send(CHANNEL_URGENT, control::REQUEST_IDR, &[0, 0]);
                }
            }
        }));

        let running = stop.clone();
        threads.push(thread::spawn(move || {
            const RTP_HEADER: usize = 12;
            const OPUS_DATA: u8 = 97;
            let mut buffer = vec![0u8; 2048];
            let mut last = None;
            while !running.load(Ordering::Relaxed) {
                let Some(datagram) = audio.receive(&mut buffer) else { continue };
                if datagram.len() <= RTP_HEADER || datagram[1] != OPUS_DATA { continue; }
                let sequence = u16::from_be_bytes([datagram[2], datagram[3]]);
                // Sound is played as it arrives, so a late packet or a repeat
                // has no place left to go.
                if !follows(sequence, last) { continue; }
                last = Some(sequence);
                let _ = events.send(Event::Audio(datagram[RTP_HEADER..].to_vec()));
            }
        }));

        Ok((Self { control, touch: ports.touch, stop, threads: Mutex::new(threads) }, receiver))
    }

    /// Sends the gamepad state to the host at once.
    pub fn gamepad(&self, state: &Gamepad) {
        queue_gamepad(&mut self.control.gamepads.lock().unwrap(), *state);
        self.control.send_gamepads();
    }

    /// Presses or releases a key on the host. `key` is a Windows virtual-key
    /// code and `modifiers` the `MODIFIER_*` keys held with it.
    pub fn key(&self, key: u8, down: bool, modifiers: u8) {
        self.control.send(CHANNEL_KEYBOARD, control::INPUT, &control::key_packet(key, down, modifiers));
    }

    /// Types text on the host, whatever its keyboard layout.
    pub fn text(&self, text: &str) {
        for packet in control::text_packets(text) { self.control.send(CHANNEL_KEYBOARD, control::INPUT, &packet); }
    }

    /// Moves the mouse by `dx`, `dy`. Moves made while the link is busy go
    /// out as one.
    pub fn mouse_move(&self, dx: i16, dy: i16) {
        if (dx, dy) != (0, 0) { self.control.move_mouse(Motion::Relative(dx.into(), dy.into())); }
    }

    /// Puts the pointer at `x`, `y` of a `width` by `height` area, which the
    /// host scales to its screen. While the link is busy only the last
    /// position goes out.
    pub fn mouse_position(&self, x: i16, y: i16, width: i16, height: i16) {
        self.control.move_mouse(Motion::Absolute { x, y, width, height });
    }

    pub fn mouse_button(&self, button: MouseButton, down: bool) {
        self.control.send_mouse(&control::mouse_button_packet(button, down));
    }

    /// Turns the wheel: 120 is one notch, positive away from the user.
    pub fn scroll(&self, amount: i16) {
        if amount != 0 { self.control.send_mouse(&control::scroll_packet(amount)); }
    }

    /// Turns the horizontal wheel: 120 is one notch, positive to the right.
    pub fn hscroll(&self, amount: i16) {
        if amount != 0 { self.control.send_mouse(&control::hscroll_packet(amount)); }
    }

    /// Sends a touch of the host's screen. `false`, and nothing sent, when the
    /// host does not take touches.
    pub fn touch(&self, event: Touch) -> bool {
        if self.touch { self.control.send(CHANNEL_TOUCH, control::INPUT, &event.packet()); }
        self.touch
    }

    /// Whether the host takes touches; without them, use the mouse position.
    pub fn host_supports_touch(&self) -> bool {
        self.touch
    }

    /// Whether the stream is over, by either side.
    pub fn ended(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Ends the stream. The app on the host keeps running until `Host::cancel`.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.lock().unwrap().drain(..) { let _ = thread.join(); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_streams_never_ping_the_host_at_the_same_moment() {
        let host = UdpSocket::bind("127.0.0.1:0").unwrap();
        host.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let address = host.local_addr().unwrap();
        let video = Media::open(address, b"0123456789ABCDEF", Duration::ZERO).unwrap();
        let audio = Media::open(address, b"0123456789ABCDEF", AUDIO_PING_OFFSET).unwrap();
        let (video_port, audio_port) = (video.socket.local_addr().unwrap().port(), audio.socket.local_addr().unwrap().port());
        // Each stream's thread calls `receive` in a loop; nothing arrives here.
        let stop = Arc::new(AtomicBool::new(false));
        let threads: Vec<_> = [(video, stop.clone()), (audio, stop.clone())].into_iter().map(|(mut media, stop)| {
            thread::spawn(move || {
                let mut buffer = [0u8; 64];
                while !stop.load(Ordering::Relaxed) { media.receive(&mut buffer); }
            })
        }).collect();
        let started = Instant::now();
        let mut pings = Vec::new();
        let mut datagram = [0u8; 64];
        while pings.len() < 6 {
            let (length, from) = host.recv_from(&mut datagram).unwrap();
            assert_eq!((length, &datagram[..16]), (20, &b"0123456789ABCDEF"[..]));
            pings.push((from.port(), started.elapsed()));
        }
        stop.store(true, Ordering::Relaxed);
        for thread in threads { thread.join().unwrap(); }
        // Video pings first, and the streams take turns with room between.
        assert_eq!(pings[0].0, video_port);
        for pair in pings.windows(2) {
            assert_ne!(pair[0].0, pair[1].0, "{pings:?}");
            assert!(pair[1].1 - pair[0].1 >= Duration::from_millis(100), "{pings:?}");
        }
        assert!(pings.iter().any(|(port, _)| *port == audio_port));
    }

    #[test]
    fn merges_waiting_stick_moves_but_keeps_every_button_change() {
        let pad = |buttons: u32, x: i16| Gamepad { buttons, left_stick: (x, 0), ..Gamepad::default() };
        let mut waiting = VecDeque::new();
        for x in 1..=50 { queue_gamepad(&mut waiting, pad(0, x)); }
        assert_eq!(waiting, [pad(0, 50)]);
        // A press and its release while the link is stalled both reach the host.
        queue_gamepad(&mut waiting, pad(control::BUTTON_UP, 50));
        queue_gamepad(&mut waiting, pad(0, 51));
        queue_gamepad(&mut waiting, pad(0, 52));
        assert_eq!(waiting, [pad(0, 50), pad(control::BUTTON_UP, 50), pad(0, 52)]);
        // Past the limit the oldest change goes.
        for press in 0..(GAMEPAD_QUEUE as u32 * 2) { queue_gamepad(&mut waiting, pad(press % 2, 0)); }
        assert_eq!(waiting.len(), GAMEPAD_QUEUE);
        assert_eq!(waiting.back(), Some(&pad(1, 0)));
    }

    #[test]
    fn adds_up_mouse_moves_and_keeps_only_the_last_position() {
        let mut waiting = None;
        assert_eq!(merge_motion(&mut waiting, Motion::Relative(1, 2)), None);
        assert_eq!(merge_motion(&mut waiting, Motion::Relative(-4, 5)), None);
        assert_eq!(merge_motion(&mut waiting, Motion::Relative(i16::MAX.into(), 0)), None);
        assert_eq!(waiting, Some(Motion::Relative(32764, 7)));
        // A position does not merge with moves: the moves go out first.
        let at = |x| Motion::Absolute { x, y: 5, width: 1920, height: 1080 };
        assert_eq!(merge_motion(&mut waiting, at(10)), Some(Motion::Relative(32764, 7)));
        assert_eq!(merge_motion(&mut waiting, at(20)), None);
        assert_eq!(waiting, Some(at(20)));
        assert_eq!(merge_motion(&mut waiting, Motion::Relative(1, 0)), Some(at(20)));
        assert_eq!(waiting, Some(Motion::Relative(1, 0)));
        // Merged moves past what one packet holds go out in parts.
        assert_eq!(Motion::Relative(40000, 0).packets().len(), 2);
    }

    /// The type and payload of a control message the client sealed with key [7; 16].
    fn opened(message: &[u8]) -> (u16, Vec<u8>) {
        use aes_gcm::aead::AeadInOut;
        use aes_gcm::{Aes128Gcm, KeyInit};
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&message[4..8]);
        nonce[10..].copy_from_slice(b"CC");
        let tag: [u8; 16] = message[8..24].try_into().unwrap();
        let mut text = message[24..].to_vec();
        Aes128Gcm::new_from_slice(&[7; 16]).unwrap()
            .decrypt_inout_detached((&nonce).into(), &[], text.as_mut_slice().into(), (&tag).into()).unwrap();
        (u16::from_le_bytes([text[0], text[1]]), text.split_off(4))
    }

    #[test]
    fn holds_mouse_moves_while_the_link_is_busy_and_sends_them_before_a_click() {
        let (received, packets) = mpsc::channel();
        let (address, host) = crate::enet::tests::host(move |host, _| {
            host.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            let mut datagram = [0u8; 2048];
            // Nothing is acknowledged, so the link stays busy. A reliable send
            // is the datagram header, command, channel, sequence and length,
            // then the data. Reads until the client has been quiet a while.
            while let Ok((length, _)) = host.recv_from(&mut datagram) {
                if length > 10 && datagram[4] & 0x0f == 6 && [CHANNEL_MOUSE, CHANNEL_TOUCH].contains(&datagram[5]) {
                    received.send((datagram[5], opened(&datagram[10..length]))).unwrap();
                }
            }
        });
        let control = Arc::new(Control {
            enet: Enet::connect(address, 0, CONNECT_TIMEOUT).unwrap(),
            cipher: Mutex::new(Cipher::new(&[7; 16])),
            gamepads: Mutex::new(VecDeque::new()),
            motion: Mutex::new(None),
        });
        let mut stream = Stream { control, touch: false, stop: Arc::new(AtomicBool::new(false)), threads: Mutex::new(Vec::new()) };
        for _ in 0..IN_FLIGHT + 3 { stream.mouse_move(1, -1); }
        stream.mouse_move(0, 0);
        stream.scroll(0);
        stream.mouse_button(MouseButton::Left, true);
        let touch = |id| Touch { id, phase: control::TouchPhase::Down, x: 0.5, y: 0.5, pressure: 1.0 };
        assert!(!stream.host_supports_touch());
        assert!(!stream.touch(touch(1)));
        stream.touch = true;
        assert!(stream.touch(touch(2)));
        host.join().unwrap();

        let mouse = |packet: &[u8]| (CHANNEL_MOUSE, (control::INPUT, packet.to_vec()));
        let mut want = vec![mouse(&control::mouse_move_packets(1, -1)[0]); IN_FLIGHT];
        want.push(mouse(&control::mouse_move_packets(3, -3)[0]));
        want.push(mouse(&control::mouse_button_packet(MouseButton::Left, true)));
        want.push((CHANNEL_TOUCH, (control::INPUT, touch(2).packet().to_vec())));
        assert_eq!(packets.try_iter().collect::<Vec<_>>(), want);
    }

    #[test]
    fn plays_sound_packets_only_forward() {
        assert!(follows(7, None));
        assert!(follows(101, Some(100)));
        assert!(follows(103, Some(100)));
        assert!(!follows(100, Some(100)));
        assert!(!follows(99, Some(100)));
        // The counter wraps.
        assert!(follows(2, Some(65534)));
        assert!(!follows(65534, Some(2)));
    }

    #[test]
    fn dropping_a_stream_stops_its_threads() {
        let (address, host) = crate::enet::tests::host(|_, _| {});
        let control = Arc::new(Control {
            enet: Enet::connect(address, 0, CONNECT_TIMEOUT).unwrap(),
            cipher: Mutex::new(Cipher::new(&[0; 16])),
            gamepads: Mutex::new(VecDeque::new()),
            motion: Mutex::new(None),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (running, finished) = (stop.clone(), Arc::new(AtomicBool::new(false)));
        let done = finished.clone();
        let worker = thread::spawn(move || {
            while !running.load(Ordering::Relaxed) { thread::sleep(Duration::from_millis(1)); }
            done.store(true, Ordering::Relaxed);
        });
        drop(Stream { control, touch: false, stop, threads: Mutex::new(vec![worker]) });
        assert!(finished.load(Ordering::Relaxed));
        host.join().unwrap();
    }
}
