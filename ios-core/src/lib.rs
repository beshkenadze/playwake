//! Portable Rust core for the Playwake iOS client.
//!
//! The original desktop engine splits into a UI process and a Linux-only
//! GStreamer worker. iOS has no subprocesses, X11, Vulkan or evdev, so the
//! media pipeline is replaced by libwebrtc + VideoToolbox in Swift. Everything
//! that is pure protocol work (OAuth/xHome/xCloud signaling, session
//! provisioning, SDP exchange, RTP wire formats) is reused here unchanged and
//! exposed over a small C ABI.

#[path = "../../src/journal.rs"]
mod journal;
// Desktop preferences live in this module too; the iOS client does not use
// them, but the session/cache helpers still come from the same source file.
#[allow(dead_code)]
#[path = "../../src/session_store.rs"]
mod session_store;
#[path = "../../src/xbox.rs"]
mod xbox;
// The cover cache (`cloud::COVERS`) is the desktop's; SwiftUI loads covers.
#[allow(dead_code)]
#[path = "../../src/cloud.rs"]
mod cloud;
#[allow(dead_code)]
#[path = "../../src/covers.rs"]
mod covers;
#[allow(dead_code)]
#[path = "../../src/library.rs"]
mod library;
#[path = "../../media-worker/src/route.rs"]
mod route;

mod bridge;
mod handshake;
mod input_route;
#[allow(dead_code)]
mod sdp;
mod wire;

pub use bridge::*;
pub use input_route::*;
