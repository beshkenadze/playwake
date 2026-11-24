//! Client side of the GameStream protocol that Sunshine hosts speak.
//! Written from the protocol as the open-source Moonlight client and Sunshine
//! implement it; it shares no code with them.

mod control;
mod discovery;
mod enet;
mod fec;
mod http;
mod identity;
mod pairing;
mod rtsp;
mod stream;
mod video;

pub use control::*;
pub use discovery::discover;
pub use http::{App, Host, Launch, ServerInfo, Session};
pub use identity::Identity;
pub use pairing::{pair, pin};
pub use stream::{Codec, Config, Event, Stream};
pub use video::Frame;

pub type Result<T> = std::result::Result<T, String>;
