//! Control-channel handshake sequence for the Xbox stream.

use serde_json::{Value, json};
use uuid::Uuid;

pub fn is_ack(data: &str) -> bool {
    serde_json::from_str::<Value>(data).ok()
        .and_then(|value| value.get("type").and_then(Value::as_str).map(|kind| kind == "HandshakeAck"))
        .unwrap_or(false)
}

/// JSON messages (label, payload) sent after the console acknowledges the
/// message-channel handshake.
pub fn messages(width: u32, height: u32) -> Vec<(&'static str, Value)> {
    let dimensions = json!({ "width": width, "height": height });
    let mut out = vec![
        ("control", json!({
            "message": "authorizationRequest",
            "accessKey": "4BDB3609-C1F1-4195-9B37-FEFF45DA8B8E",
        })),
        ("control", json!({
            "message": "gamepadChanged", "gamepadIndex": 0, "wasAdded": true,
        })),
        ("control", json!({
            "message": "VideoPreference", "type": "VideoPreference",
            "resolution": dimensions, "dimensions": dimensions, "maxFPS": 60,
        })),
        ("control", json!({
            "message": "userRequestedResolutionUpdate", "resolutionAlias": height.to_string(),
        })),
    ];
    for (target, content) in [
        ("systemUi/configuration", json!({ "version": [0, 1, 0], "systemUis": [] })),
        ("properties/clientappinstallidchanged", json!({ "clientAppInstallId": Uuid::new_v4().to_string() })),
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
        out.push(("message", json!({
            "type": "Message", "target": format!("/streaming/{target}"),
            "content": content.to_string(), "id": Uuid::new_v4().to_string(), "cv": "",
        })));
    }
    out
}

pub fn open_message() -> Value {
    json!({
        "type": "Handshake", "version": "messageV1",
        "id": Uuid::new_v4().to_string(), "cv": "",
    })
}
