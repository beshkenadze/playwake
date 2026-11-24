//! A running stream: the handshake, then one thread each for the control
//! stream, video and audio. Events reach the caller through a channel.

use crate::control::{self, Cipher, Gamepad};
use crate::enet::{self, Enet};
use crate::video::{Assembler, Frame};
use crate::{Host, Result, Session, rtsp};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
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
/// How long to wait for a key frame before asking the host again.
const KEY_FRAME_REQUEST_GAP: Duration = Duration::from_millis(250);
const GRACEFUL: u32 = 0x8003_0023;
/// Control stream channels: general, urgent, keyboard, and the first gamepad.
const CHANNEL_GENERAL: u8 = 0;
const CHANNEL_URGENT: u8 = 1;
const CHANNEL_KEYBOARD: u8 = 2;
const CHANNEL_GAMEPAD: u8 = 0x10;

struct Control {
    enet: Enet,
    cipher: Mutex<Cipher>,
}

impl Control {
    fn send(&self, channel: u8, kind: u16, payload: &[u8]) {
        let message = self.cipher.lock().unwrap().seal(kind, payload);
        self.enet.send(channel, &message);
    }
}

pub struct Stream {
    control: Arc<Control>,
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
        let mut audio = Media::open(on_port(ports.audio), &ports.ping_payload, AUDIO_PING_OFFSET)?;
        let control = Arc::new(Control {
            enet: Enet::connect(on_port(ports.control), ports.connect_data, CONNECT_TIMEOUT)?,
            cipher: Mutex::new(Cipher::new(&session.key)),
        });
        control.send(CHANNEL_GENERAL, control::REQUEST_IDR, &[0, 0]);
        control.send(CHANNEL_GENERAL, control::START_B, &[0]);
        // The host makes its virtual gamepad on the first state it gets, which
        // can take seconds; start that now, not on the player's first press.
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
                for event in link.enet.service(next_ping.saturating_duration_since(Instant::now())) {
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

        Ok((Self { control, stop, threads: Mutex::new(threads) }, receiver))
    }

    /// Sends the gamepad state to the host at once.
    pub fn gamepad(&self, state: &Gamepad) {
        self.control.send(CHANNEL_GAMEPAD, control::INPUT, &state.packet());
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
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (running, finished) = (stop.clone(), Arc::new(AtomicBool::new(false)));
        let done = finished.clone();
        let worker = thread::spawn(move || {
            while !running.load(Ordering::Relaxed) { thread::sleep(Duration::from_millis(1)); }
            done.store(true, Ordering::Relaxed);
        });
        drop(Stream { control, stop, threads: Mutex::new(vec![worker]) });
        assert!(finished.load(Ordering::Relaxed));
        host.join().unwrap();
    }
}
