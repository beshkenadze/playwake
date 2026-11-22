//! webrtcbin setup, offer/answer/ICE glue and SDP munging, parity with
//! media/gstreamer_player.py:508-820.

use crate::{handshake::Channels, protocol, transport::{self, Transport}};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_webrtc as gst_webrtc;
use gstreamer_sdp as gst_sdp;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

pub struct Rtc {
    pub element: gst::Element,
    pub channels: Channels,
    pub candidates: Vec<Value>,
    pub transport: Arc<Mutex<Transport>>,
    pub capture_diagnostics: bool,
}

pub fn build(pipeline: &gst::Pipeline, jitter_ms: u32, transport: Arc<Mutex<Transport>>,
             capture_diagnostics: bool) -> Result<Rtc, String> {
    let element = gst::ElementFactory::make("webrtcbin").name("webrtc").build()
        .map_err(|e| format!("webrtcbin: {e}"))?;
    element.set_property("bundle-policy", gst_webrtc::WebRTCBundlePolicy::MaxBundle);
    element.set_property("latency", jitter_ms);
    pipeline.add(&element).map_err(|e| format!("pipeline add: {e}"))?;

    let audio_caps = gst::Caps::builder("application/x-rtp")
        .field("media", "audio").field("encoding-name", "OPUS").field("clock-rate", 48000i32)
        .field("payload", 111i32).field("encoding-params", "2")
        .build();
    let video_caps = gst::Caps::builder("application/x-rtp")
        .field("media", "video").field("encoding-name", "H264").field("clock-rate", 90000i32)
        .field("payload", 103i32).field("packetization-mode", "1").field("profile-level-id", "42001f")
        .build();
    for caps in [&audio_caps, &video_caps] {
        // Raw signal emission: the typed emit_by_name return-value check rejects
        // webrtcbin's action-signal returns on this GStreamer build.
        unsafe {
            let mut ret: *mut gst::glib::gobject_ffi::GObject = std::ptr::null_mut();
            gst::glib::gobject_ffi::g_signal_emit_by_name(
                element.as_ptr() as *mut gst::glib::gobject_ffi::GObject,
                b"add-transceiver\0".as_ptr() as *const i8,
                gst::glib::translate::IntoGlib::into_glib(gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly),
                caps.as_ptr() as *mut std::ffi::c_void,
                &mut ret as *mut *mut gst::glib::gobject_ffi::GObject as *mut std::ffi::c_void,
            );
            if !ret.is_null() {
                gst::glib::gobject_ffi::g_object_unref(ret);
            }
        }
    }

    Ok(Rtc { element, channels: Channels::new(), candidates: Vec::new(), transport, capture_diagnostics })
}

/// Datachannels must be created after the pipeline reaches PLAYING (Python parity).
pub fn create_channels(rtc: &Arc<Mutex<Rtc>>) -> Result<(), String> {
    let element = rtc.lock().unwrap().element.clone();
    let mut channels = Channels::new();
    for (label, protocol_name) in [("input", "1.0"), ("control", "controlV1"), ("message", "messageV1"), ("chat", "chatV1")] {
        let options = gst::Structure::builder("application/data-channel")
            .field("ordered", true)
            .field("protocol", protocol_name)
            .build();
        let channel = unsafe { create_data_channel(&element, label, &options) }
            .ok_or_else(|| format!("create-data-channel {label} failed"))?;
        channels.insert(label.to_string(), channel);
    }
    rtc.lock().unwrap().channels = channels;
    Ok(())
}

/// Raw `create-data-channel` action signal; returns the owned channel object.
unsafe fn create_data_channel(element: &gst::Element, label: &str, options: &gst::Structure)
    -> Option<gst_webrtc::WebRTCDataChannel>
{
    
    let c_label = std::ffi::CString::new(label).ok()?;
    let mut ret: *mut gst::glib::gobject_ffi::GObject = std::ptr::null_mut();
    gst::glib::gobject_ffi::g_signal_emit_by_name(
        element.as_ptr() as *mut gst::glib::gobject_ffi::GObject,
        b"create-data-channel\0".as_ptr() as *const i8,
        c_label.as_ptr(),
        options.as_ptr() as *mut std::ffi::c_void,
        &mut ret as *mut *mut gst::glib::gobject_ffi::GObject as *mut std::ffi::c_void,
    );
    if ret.is_null() { return None; }
    Some(gst::glib::translate::from_glib_full(
        ret as *mut gstreamer_webrtc::ffi::GstWebRTCDataChannel))
}

pub fn wire_signals(rtc: &Arc<Mutex<Rtc>>, on_pad: impl Fn(&gst::Pad) + Send + Sync + 'static) {
    let element = rtc.lock().unwrap().element.clone();
    let buffered = rtc.clone();
    element.connect("on-ice-candidate", false, move |values| {
        let index = values[1].get::<u32>().unwrap_or(0);
        let candidate = values[2].get::<String>().unwrap_or_default();
        buffered.lock().unwrap().candidates.push(json!({
            "candidate": candidate, "sdpMLineIndex": index,
        }));
        None
    });
    let buffered = rtc.clone();
    element.connect_notify(Some("ice-gathering-state"), move |element, _| {
        if element.property::<gst_webrtc::WebRTCICEGatheringState>("ice-gathering-state")
            == gst_webrtc::WebRTCICEGatheringState::Complete
        {
            let candidates = buffered.lock().unwrap().candidates.clone();
            protocol::event("candidates", json!({ "candidates": candidates }));
        }
    });
    element.connect_notify(Some("connection-state"), move |element, _| {
        let state = element.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
        let nick = match state {
            gst_webrtc::WebRTCPeerConnectionState::New => "new",
            gst_webrtc::WebRTCPeerConnectionState::Connecting => "connecting",
            gst_webrtc::WebRTCPeerConnectionState::Connected => "connected",
            gst_webrtc::WebRTCPeerConnectionState::Disconnected => "disconnected",
            gst_webrtc::WebRTCPeerConnectionState::Failed => "failed",
            gst_webrtc::WebRTCPeerConnectionState::Closed => "closed",
            _ => "unknown",
        };
        protocol::event("connection", json!({ "state": nick }));
    });
    element.connect_pad_added(move |_element, pad| on_pad(pad));
}

pub fn create_offer(rtc: &Arc<Mutex<Rtc>>, bitrate: u32) {
    let (element, capture, transport_arc) = {
        let rtc = rtc.lock().unwrap();
        (rtc.element.clone(), rtc.capture_diagnostics, rtc.transport.clone())
    };
    let element_inner = element.clone();
    let promise = gst::Promise::with_change_func(move |reply| {
        let Ok(Some(reply)) = reply else {
            protocol::error("create-offer failed");
            return;
        };
        let Ok(offer) = reply.get::<gst_webrtc::WebRTCSessionDescription>("offer") else {
            protocol::error("GStreamer returned no SDP offer");
            return;
        };
        let Ok(text) = offer.sdp().as_text() else {
            protocol::error("offer SDP serialization failed");
            return;
        };
        let (munged, extension) = transport::munge_offer(&text, bitrate, capture);
        if let Some(id) = extension {
            transport_arc.lock().unwrap().capture_extension = Some(id);
        }
        let Ok(sdp) = gst_sdp::SDPMessage::parse_buffer(munged.as_bytes()) else {
            protocol::error("munged offer did not parse");
            return;
        };
        let description = gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Offer, sdp);
        let element = element_inner.clone();
        let set_promise = gst::Promise::with_change_func(move |reply| {
            // Void actions reply REPLIED with a NULL structure on success
            // (Python checked(): REPLIED and no "error" field).
            match reply {
                Ok(Some(reply)) if reply.has_field("error") => {
                    protocol::error("set-local-description failed");
                }
                Ok(_) => protocol::event("offer", json!({ "sdp": munged })),
                Err(_) => protocol::error("set-local-description failed"),
            }
        });
        element.emit_by_name::<()>("set-local-description", &[&description, &set_promise]);
    });
    element.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
}

pub fn set_answer(rtc: &Arc<Mutex<Rtc>>, sdp_text: &str) {
    let element = rtc.lock().unwrap().element.clone();
    let (capture, send_time) = transport::inspect_answer(sdp_text);
    {
        let rtc = rtc.lock().unwrap();
        let mut transport = rtc.transport.lock().unwrap();
        transport.capture_extension = capture;
        transport.send_time_offered = send_time;
    }
    if rtc.lock().unwrap().capture_diagnostics {
        protocol::event("capture-negotiation", json!({ "capture": capture.is_some(), "sendTime": send_time }));
    }
    let Ok(sdp) = gst_sdp::SDPMessage::parse_buffer(sdp_text.as_bytes()) else {
        protocol::error("answer SDP did not parse");
        return;
    };
    let description = gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Answer, sdp);
    let promise = gst::Promise::with_change_func(move |reply| {
        match reply {
            Ok(Some(reply)) if reply.has_field("error") => {
                protocol::error("set-remote-description failed");
            }
            Ok(_) => protocol::event("answer-set", json!({})),
            Err(_) => protocol::error("set-remote-description failed"),
        }
    });
    element.emit_by_name::<()>("set-remote-description", &[&description, &promise]);
}

pub fn add_ice(rtc: &Arc<Mutex<Rtc>>, candidates: &[Value]) {
    let element = rtc.lock().unwrap().element.clone();
    for entry in candidates {
        let Some(candidate) = entry.get("candidate").and_then(Value::as_str) else { continue };
        let candidate = candidate.strip_prefix("a=").unwrap_or(candidate);
        let index = entry.get("sdpMLineIndex").and_then(Value::as_u64).unwrap_or(0) as u32;
        for line in candidate.lines() {
            if line.starts_with("candidate:") {
                element.emit_by_name::<()>("add-ice-candidate", &[&index, &line]);
            }
        }
    }
}

#[cfg(test)]
mod tests {

    const OFFER: &str = "v=0\r\no=- 1 0 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE audio0 video1\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:audio0\r\na=recvonly\r\nm=video 0 UDP/TLS/RTP/SAVPF 103\r\na=mid:video1\r\na=recvonly\r\na=fingerprint:sha-256 AA\r\n";

}
