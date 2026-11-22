//! Xbox remote-play control handshake, byte-identical to the Python worker
//! (media/gstreamer_player.py:825-857).

use crate::protocol;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_webrtc as gst_webrtc;
use serde_json::{json, Value};
use std::collections::HashMap;

pub type Channels = HashMap<String, gst_webrtc::WebRTCDataChannel>;

pub fn send_json(channels: &Channels, label: &str, data: &Value) {
    if let Some(channel) = channels.get(label) {
        let text = serde_json::to_string(data).unwrap_or_default();
        channel.emit_by_name::<()>("send-data", &[&gst::glib::Bytes::from(text.as_bytes())]);
    }
}

pub fn handshake_open(channels: &Channels) {
    send_json(channels, "message", &json!({
        "type": "Handshake", "version": "messageV1",
        "id": uuid::Uuid::new_v4().to_string(), "cv": "",
    }));
}

pub fn is_handshake_ack(data: &str) -> bool {
    serde_json::from_str::<Value>(data).ok()
        .and_then(|v| v.get("type").and_then(Value::as_str).map(|t| t == "HandshakeAck"))
        .unwrap_or(false)
}

pub fn handshake_ack(channels: &Channels, width: u32, height: u32, start_time: f64) {
    let dimensions = json!({ "width": width, "height": height });
    send_json(channels, "control", &json!({
        "message": "authorizationRequest",
        "accessKey": "4BDB3609-C1F1-4195-9B37-FEFF45DA8B8E",
    }));
    send_json(channels, "control", &json!({
        "message": "gamepadChanged", "gamepadIndex": 0, "wasAdded": true,
    }));
    send_json(channels, "control", &json!({
        "message": "VideoPreference", "type": "VideoPreference",
        "resolution": dimensions, "dimensions": dimensions, "maxFPS": 60,
    }));
    send_json(channels, "control", &json!({
        "message": "userRequestedResolutionUpdate", "resolutionAlias": height.to_string(),
    }));
    for (target, content) in [
        ("systemUi/configuration", json!({ "version": [0, 1, 0], "systemUis": [] })),
        ("properties/clientappinstallidchanged", json!({ "clientAppInstallId": uuid::Uuid::new_v4().to_string() })),
        ("characteristics/orientationchanged", json!({ "orientation": 0 })),
        ("characteristics/touchinputenabledchanged", json!({ "touchInputEnabled": false })),
        ("characteristics/clientdevicecapabilities", json!({})),
        ("characteristics/dimensionschanged", json!({
            "horizontal": width, "vertical": height,
            "preferredWidth": width, "preferredHeight": height,
            "safeAreaLeft": 0, "safeAreaTop": 0,
            "safeAreaRight": width, "safeAreaBottom": height,
            "supportsCustomResolution": true,
        })),
    ] {
        send_json(channels, "message", &json!({
            "type": "Message", "target": format!("/streaming/{target}"),
            "content": content.to_string(), "id": uuid::Uuid::new_v4().to_string(), "cv": "",
        }));
    }
    if let Some(channel) = channels.get("input") {
        channel.emit_by_name::<()>("send-data", &[&gst::glib::Bytes::from(&connect_packet(start_time))]);
    }
    protocol::event("input-ready", json!({}));
}

/// struct.pack('<HIdB', 8, 0, start_time, 2)
pub fn connect_packet(start_time: f64) -> Vec<u8> {
    let mut out = Vec::with_capacity(15);
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&start_time.to_le_bytes());
    out.push(2);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn connect_packet_layout() {
        let bytes = super::connect_packet(1234.5);
        assert_eq!(bytes.len(), 15);
        assert_eq!(&bytes[0..2], &8u16.to_le_bytes());
        assert_eq!(&bytes[2..6], &0u32.to_le_bytes());
        assert_eq!(&bytes[6..14], &1234.5f64.to_le_bytes());
        assert_eq!(bytes[14], 2);
    }

    #[test]
    fn ack_detection() {
        assert!(super::is_handshake_ack(r#"{"type":"HandshakeAck"}"#));
        assert!(!super::is_handshake_ack(r#"{"type":"Handshake"}"#));
        assert!(!super::is_handshake_ack("garbage"));
    }
}