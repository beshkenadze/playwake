//! Monotonic millisecond clock matching the Python worker's
//! `time.monotonic_ns() / 1e6` contract (CLOCK_MONOTONIC, absolute).

#[cfg(target_os = "linux")]
pub fn now_ms() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as f64) * 1000.0 + (ts.tv_nsec as f64) / 1e6
}

#[cfg(not(target_os = "linux"))]
pub fn now_ms() -> f64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as f64 / 1e6
}

pub fn wall_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as f64 / 1e6)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn monotonic_advances() {
        let a = super::now_ms();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = super::now_ms();
        assert!(b >= a + 4.0, "{a} -> {b}");
    }
}