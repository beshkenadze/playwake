use serde_json::{Value, json};
use std::{
    io::{self, Write},
    time::{SystemTime, UNIX_EPOCH},
};

// Only pass stage names, status codes, counts, and other non-sensitive metadata.
pub fn record(event: &str, details: Value) {
    let time_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let entry = json!({"time_ms": time_ms, "event": event, "details": details});
    let _ = writeln!(io::stderr().lock(), "{entry}");
}
