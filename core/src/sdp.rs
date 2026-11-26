//! Portable SDP helpers shared with the desktop worker.

pub const ABS_CAPTURE_TIME_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time";
pub const ABS_SEND_TIME_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time";

/// Insert `b=AS` and (when capturing) the abs-capture-time extmap into the
/// `m=video` section. Returns the munged SDP and the extension id, if added.
pub fn munge_offer(text: &str, bitrate: u32, capture: bool) -> (String, Option<u8>) {
    let mut lines: Vec<String> = text.split("\r\n").map(str::to_string).collect();
    let Some(start) = lines.iter().position(|line| line.starts_with("m=video ")) else {
        return (text.to_string(), None);
    };
    let end = lines.iter().skip(start + 1).position(|line| line.starts_with("m="))
        .map(|offset| start + 1 + offset)
        .unwrap_or(lines.len());
    let first_attr = lines[start..end].iter().position(|line| line.starts_with("a="))
        .map(|offset| start + offset)
        .unwrap_or(end);
    lines.insert(first_attr, format!("b=AS:{bitrate}"));
    let mut extension = None;
    if capture {
        let mut used: Vec<u8> = Vec::new();
        for line in &lines[start..end + 1] {
            if let Some(rest) = line.strip_prefix("a=extmap:")
                && let Some(id) = rest.split(|c: char| c == '/' || c.is_whitespace()).next()
                    .and_then(|id| id.parse::<u8>().ok())
            {
                used.push(id);
            }
        }
        if let Some(id) = (1..=14).rev().find(|id| !used.contains(id)) {
            lines.insert(first_attr + 1, format!("a=extmap:{id}/recvonly {ABS_CAPTURE_TIME_URI}"));
            extension = Some(id);
        }
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    let mut out = lines.join("\r\n");
    out.push_str("\r\n");
    (out, extension)
}

/// Which capture/send-time extensions the console agreed to.
pub fn inspect_answer(text: &str) -> (Option<u8>, bool) {
    let mut in_video = false;
    let mut capture = None;
    let mut send_time = false;
    for line in text.split("\r\n") {
        if line.starts_with("m=") {
            in_video = line.starts_with("m=video ");
            continue;
        }
        if !in_video { continue; }
        if let Some(rest) = line.strip_prefix("a=extmap:") {
            let mut parts = rest.splitn(2, |c: char| c.is_whitespace());
            let id_part = parts.next().unwrap_or("");
            let uri = parts.next().unwrap_or("");
            let id = id_part.split('/').next().and_then(|id| id.parse::<u8>().ok());
            if uri == ABS_CAPTURE_TIME_URI { capture = id; }
            if uri == ABS_SEND_TIME_URI { send_time = true; }
        }
    }
    (capture, send_time)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 98\r\na=mid:0\r\na=recvonly\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:1\r\na=recvonly\r\n";

    #[test]
    fn inserts_bitrate_only_into_video() {
        let (sdp, ext) = munge_offer(OFFER, 20000, false);
        assert!(ext.is_none());
        assert!(sdp.contains("m=video 9 UDP/TLS/RTP/SAVPF 98\r\nb=AS:20000\r\na=mid:0"));
        let audio = sdp.split("m=audio").nth(1).unwrap();
        assert!(!audio.contains("b=AS"));
    }

    #[test]
    fn adds_capture_extension() {
        let (sdp, ext) = munge_offer(OFFER, 12000, true);
        assert_eq!(ext, Some(14));
        assert!(sdp.contains("a=extmap:14/recvonly http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time"));
    }

    #[test]
    fn inspects_answer_extensions() {
        let answer = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 98\r\na=extmap:3 urn:ietf:params:rtp-hdrext:sdes:mid\r\na=extmap:11 http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time\r\na=extmap:9 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n";
        assert_eq!(inspect_answer(answer), (Some(11), true));
    }
}
