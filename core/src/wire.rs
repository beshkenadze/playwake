//! Wire formats shared with the Xbox streaming transport, byte-identical to
//! the GStreamer worker and the Python reference implementation.

/// 38-byte input packet:
/// `struct.pack('<HIdBBHhhhhHHII', 2, seq, time, 1, 0, buttons, lx, -ly, rx, -ry, lt, rt, 0, 0)`.
pub fn input_packet(sequence: u32, time: f64, buttons: u16, axes: [i16; 4], lt: u16, rt: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(38);
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&time.to_le_bytes());
    out.push(1);
    out.push(0);
    out.extend_from_slice(&buttons.to_le_bytes());
    out.extend_from_slice(&axes[0].to_le_bytes());
    out.extend_from_slice(&(-axes[1]).to_le_bytes());
    out.extend_from_slice(&axes[2].to_le_bytes());
    out.extend_from_slice(&(-axes[3]).to_le_bytes());
    out.extend_from_slice(&lt.to_le_bytes());
    out.extend_from_slice(&rt.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

/// 15-byte channel-open packet: `struct.pack('<HIdB', 8, 0, start_time, 2)`.
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
    use super::*;

    #[test]
    fn packet_layout_matches_python_struct() {
        let bytes = input_packet(7, 12.5, 16, [100, -200, 300, -400], 1000, 2000);
        assert_eq!(bytes.len(), 38);
        assert_eq!(&bytes[0..2], &2u16.to_le_bytes());
        assert_eq!(&bytes[2..6], &7u32.to_le_bytes());
        assert_eq!(&bytes[6..14], &12.5f64.to_le_bytes());
        assert_eq!(&bytes[14..16], &[1, 0]);
        assert_eq!(&bytes[16..18], &16u16.to_le_bytes());
        assert_eq!(&bytes[18..20], &100i16.to_le_bytes());
        assert_eq!(&bytes[20..22], &200i16.to_le_bytes());
        assert_eq!(&bytes[22..24], &300i16.to_le_bytes());
        assert_eq!(&bytes[24..26], &400i16.to_le_bytes());
        assert_eq!(&bytes[26..28], &1000u16.to_le_bytes());
        assert_eq!(&bytes[28..30], &2000u16.to_le_bytes());
        assert_eq!(&bytes[30..38], &[0; 8]);
    }

    #[test]
    fn connect_packet_layout() {
        let bytes = connect_packet(1234.5);
        assert_eq!(bytes.len(), 15);
        assert_eq!(&bytes[0..2], &8u16.to_le_bytes());
        assert_eq!(&bytes[2..6], &0u32.to_le_bytes());
        assert_eq!(&bytes[6..14], &1234.5f64.to_le_bytes());
        assert_eq!(bytes[14], 2);
    }
}
