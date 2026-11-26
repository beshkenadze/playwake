//! The Playwake core: everything but the UI and the platform's own media
//! decode and present. The SteamOS app, its media worker and the iOS/Mac
//! client all build on it.

pub mod cloud;
pub mod covers;
pub mod device;
pub mod handshake;
pub mod journal;
pub mod library;
pub mod pc;
pub mod route;
pub mod sdp;
pub mod session_store;
pub mod wire;
pub mod xbox;
