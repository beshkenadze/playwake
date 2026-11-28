//! The C ABI of the Playwake core for the iOS and Mac client.
//!
//! All the work is in `playwake-core`, shared with the SteamOS app; this
//! crate only turns it into C calls and JSON events for Swift, whose
//! libwebrtc and VideoToolbox replace the Linux GStreamer worker.

use playwake_core::{cloud, handshake, library, route, sdp, session_store, wire, xbox};

mod bridge;
mod input_bridge;
mod input_route;
mod overlay_bridge;
mod pc_bridge;

pub use bridge::*;
pub use input_bridge::*;
pub use input_route::*;
pub use overlay_bridge::*;
pub use pc_bridge::*;
