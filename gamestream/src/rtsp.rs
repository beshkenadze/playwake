//! The RTSP handshake that follows `launch`: it names the three streams and
//! tells the host what to encode. The host starts the session on ANNOUNCE.
//! Plain text, one TCP connection per request, answer read until the host
//! closes.

use crate::{Codec, Config, Result};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);
/// Bytes of video payload per packet, plus the 16-byte packet header.
pub(crate) const PACKET_SIZE: u32 = 1392;

#[derive(Debug, PartialEq)]
pub(crate) struct Ports {
    pub audio: u16,
    pub video: u16,
    pub control: u16,
    /// Sent in every audio and video ping so the host can match the session.
    pub ping_payload: [u8; 16],
    /// Sent when the control stream connects, for the same reason.
    pub connect_data: u32,
}

struct Client {
    address: SocketAddr,
    host: String,
    sequence: u32,
}

impl Client {
    /// Sends one request and returns the status line and headers.
    fn request(&mut self, line: &str, headers: &str, body: &str) -> Result<String> {
        self.sequence += 1;
        // The host reads the body length only from this exact spelling.
        let length = if body.is_empty() { String::new() } else { format!("Content-length: {}\r\n", body.len()) };
        let request = format!("{line} RTSP/1.0\r\nCSeq: {}\r\nX-GS-ClientVersion: 14\r\nHost: {}\r\n{headers}{length}\r\n{body}",
            self.sequence, self.host);
        let failed = |error: std::io::Error| format!("Stream handshake failed: {error}");
        let mut socket = TcpStream::connect_timeout(&self.address, TIMEOUT).map_err(failed)?;
        socket.set_read_timeout(Some(TIMEOUT)).map_err(failed)?;
        socket.write_all(request.as_bytes()).map_err(failed)?;
        let mut answer = String::new();
        socket.read_to_string(&mut answer).map_err(failed)?;
        match answer.split_whitespace().nth(1) {
            Some("200") => Ok(answer),
            Some(status) => Err(format!("The PC refused the stream ({} answered {status})", line.split(' ').next().unwrap_or_default())),
            None => Err("The PC closed the stream handshake".into()),
        }
    }
}

fn header<'a>(answer: &'a str, name: &str) -> Option<&'a str> {
    answer.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix(':')).map(str::trim)
}

fn server_port(answer: &str, fallback: u16) -> u16 {
    header(answer, "Transport").and_then(|value| value.split_once("server_port=")?.1.parse().ok()).unwrap_or(fallback)
}

/// The session description: what the client wants the host to send.
fn description(address: &SocketAddr, config: &Config, video_port: u16) -> String {
    let codec = match config.codec { Codec::H264 => 0, Codec::Hevc => 1, Codec::Av1 => 2 };
    let attributes = [
        // Match the session by ping payload and connect data, not by address.
        ("x-ml-general.featureFlags", 3),
        // The control stream uses the 12-byte nonce; video and audio stay clear.
        ("x-ss-general.encryptionEnabled", 1),
        ("x-nv-general.useReliableUdp", 13),
        ("x-nv-video[0].clientViewportWd", config.width),
        ("x-nv-video[0].clientViewportHt", config.height),
        ("x-nv-video[0].maxFPS", config.fps),
        ("x-nv-video[0].packetSize", PACKET_SIZE),
        ("x-nv-vqos[0].bw.maximumBitrateKbps", config.bitrate_kbps * 4 / 5),
        ("x-ml-video.configuredBitrateKbps", config.bitrate_kbps),
        ("x-nv-vqos[0].fec.minRequiredFecPackets", 2),
        ("x-nv-video[0].videoEncoderSlicesPerFrame", 1),
        ("x-nv-vqos[0].bitStreamFormat", codec),
        ("x-nv-video[0].dynamicRangeMode", u32::from(config.hdr)),
        ("x-nv-video[0].maxNumReferenceFrames", 1),
        // Rec. 709, limited range.
        ("x-nv-video[0].encoderCscMode", 2),
        ("x-nv-audio.surround.numChannels", 2),
        ("x-nv-audio.surround.channelMask", 3),
        ("x-nv-audio.surround.AudioQuality", 0),
        ("x-nv-aqos.packetDuration", 5),
    ];
    let family = if address.is_ipv6() { "IPv6" } else { "IPv4" };
    let mut text = format!("v=0\r\no=android 0 14 IN {family} {}\r\ns=NVIDIA Streaming Client\r\n", address.ip());
    for (name, value) in attributes {
        text.push_str(&format!("a={name}:{value} \r\n"));
    }
    text.push_str(&format!("t=0 0\r\nm=video {video_port}  \r\n"));
    text
}

/// Runs the handshake. `on_audio_port` is called as soon as the audio port is
/// known, because the host stops the session without an early audio ping.
pub(crate) fn handshake(address: SocketAddr, config: &Config) -> Result<Ports> {
    let mut client = Client { address, host: address.ip().to_string(), sequence: 0 };
    let transport = "Transport: unicast;X-GS-ClientPort=50000-50001\r\n";

    let audio = client.request("SETUP streamid=audio/0/0", transport, "")?;
    let session = header(&audio, "Session").and_then(|value| value.split(';').next())
        .ok_or("The PC sent no stream session")?.to_owned();
    let ping_payload: [u8; 16] = header(&audio, "X-SS-Ping-Payload").and_then(|value| value.as_bytes().try_into().ok())
        .ok_or("The PC sent no ping payload")?;
    let session = format!("Session: {session}\r\n");
    let video = client.request("SETUP streamid=video/0/0", &format!("{session}{transport}"), "")?;
    let control = client.request("SETUP streamid=control/13/0", &format!("{session}{transport}"), "")?;
    let ports = Ports {
        audio: server_port(&audio, 48000),
        video: server_port(&video, 47998),
        control: server_port(&control, 47999),
        ping_payload,
        connect_data: header(&control, "X-SS-Connect-Data").and_then(|value| value.parse().ok()).unwrap_or(0),
    };
    client.request("ANNOUNCE streamid=control/13/0", &format!("{session}Content-type: application/sdp\r\n"),
        &description(&address, config, ports.video))?;
    client.request("PLAY /", &session, "")?;
    Ok(ports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ports_and_session_values_from_setup_answers() {
        let answer = "RTSP/1.0 200 OK\r\nCSeq: 3\r\nSession: DEADBEEFCAFE;timeout = 90\r\n\
            Transport: server_port=48000\r\nX-SS-Ping-Payload: 0123456789ABCDEF\r\n\r\n";
        assert_eq!(header(answer, "Session"), Some("DEADBEEFCAFE;timeout = 90"));
        assert_eq!(header(answer, "X-SS-Ping-Payload"), Some("0123456789ABCDEF"));
        assert_eq!(header(answer, "X-SS-Connect-Data"), None);
        assert_eq!(server_port(answer, 1), 48000);
        assert_eq!(server_port("RTSP/1.0 200 OK\r\n\r\n", 47998), 47998);
    }

    #[test]
    fn describes_the_stream_the_way_the_host_parses_it() {
        let config = Config { width: 1920, height: 1080, fps: 60, bitrate_kbps: 20000, codec: Codec::H264, hdr: false };
        let text = description(&"192.168.1.159:48010".parse().unwrap(), &config, 47998);
        assert!(text.starts_with("v=0\r\no=android 0 14 IN IPv4 192.168.1.159\r\n"));
        // One trailing space, which the host strips, then CRLF.
        assert!(text.contains("a=x-nv-video[0].clientViewportWd:1920 \r\n"));
        assert!(text.contains("a=x-nv-vqos[0].bw.maximumBitrateKbps:16000 \r\n"));
        assert!(text.contains("a=x-ml-video.configuredBitrateKbps:20000 \r\n"));
        assert!(text.contains("a=x-nv-general.useReliableUdp:13 \r\n"));
        assert!(text.contains("a=x-nv-vqos[0].bitStreamFormat:0 \r\n"));
        assert!(text.ends_with("t=0 0\r\nm=video 47998  \r\n"));
        // The host rejects a request over 2048 bytes; the headers need room too.
        assert!(text.len() < 1500);
    }
}
