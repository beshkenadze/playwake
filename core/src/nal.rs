//! Encoded video from the Annex B form a PC sends to the form Apple's
//! decoders take: the parameter sets apart, every other NAL unit behind its
//! 4-byte big-endian length.

use gamestream::Codec;

/// One access unit, ready for a decoder that takes length-prefixed NAL units.
#[derive(Debug, Default, PartialEq)]
pub struct AccessUnit {
    /// H.264: SPS, PPS. HEVC: VPS, SPS, PPS. Only in a unit that has them,
    /// which a key frame does.
    pub parameter_sets: Vec<Vec<u8>>,
    /// The other NAL units, each behind its length.
    pub data: Vec<u8>,
}

/// The NAL units of an Annex B stream, without their start codes.
fn units(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
    // Positions just after each 00 00 01; a 4-byte start code ends the same way.
    let starts: Vec<usize> = stream.windows(3).enumerate()
        .filter(|(_, window)| *window == [0, 0, 1]).map(|(at, _)| at + 3).collect();
    let ends: Vec<usize> = starts.iter().skip(1).map(|&next| next - 3).chain([stream.len()]).collect();
    starts.into_iter().zip(ends).map(move |(start, end)| {
        // The zero that begins a 4-byte start code is not part of the unit before it.
        let mut end = end;
        while end > start && stream[end - 1] == 0 { end -= 1; }
        &stream[start..end]
    }).filter(|unit| !unit.is_empty())
}

fn is_parameter_set(codec: Codec, unit: &[u8]) -> bool {
    match codec {
        Codec::H264 => matches!(unit[0] & 0x1f, 7 | 8),
        Codec::Hevc => matches!((unit[0] >> 1) & 0x3f, 32..=34),
        Codec::Av1 => false,
    }
}

/// Splits an Annex B access unit of `codec` for a length-prefixed decoder.
pub fn access_unit(stream: &[u8], codec: Codec) -> AccessUnit {
    let mut unit = AccessUnit::default();
    for nal in units(stream) {
        if is_parameter_set(codec, nal) {
            unit.parameter_sets.push(nal.to_vec());
        } else {
            unit.data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            unit.data.extend_from_slice(nal);
        }
    }
    unit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_h264_key_frame_gives_its_parameter_sets_and_a_length_prefixed_slice() {
        // 4-byte start codes before SPS and PPS, a 3-byte one before the IDR slice.
        let stream = [0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1f, 0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80,
            0, 0, 1, 0x65, 0x88, 0x84, 0x00];
        let unit = access_unit(&stream, Codec::H264);
        assert_eq!(unit.parameter_sets, [vec![0x67, 0x64, 0x00, 0x1f], vec![0x68, 0xee, 0x3c, 0x80]]);
        // A unit never ends in a zero byte; trailing zeros are padding.
        assert_eq!(unit.data, [0, 0, 0, 3, 0x65, 0x88, 0x84]);
    }

    #[test]
    fn an_hevc_frame_without_parameter_sets_is_only_its_slices() {
        let stream = [0, 0, 0, 1, 0x02, 0x01, 0xd0, 0, 0, 0, 1, 0x02, 0x01, 0xd1, 0xaa];
        let unit = access_unit(&stream, Codec::Hevc);
        assert!(unit.parameter_sets.is_empty());
        assert_eq!(unit.data, [0, 0, 0, 3, 0x02, 0x01, 0xd0, 0, 0, 0, 4, 0x02, 0x01, 0xd1, 0xaa]);
    }

    #[test]
    fn hevc_parameter_sets_are_vps_sps_and_pps() {
        let stream = [0, 0, 0, 1, 0x40, 0x01, 0x0c, 0, 0, 0, 1, 0x42, 0x01, 0x01, 0, 0, 0, 1, 0x44, 0x01, 0xc1,
            0, 0, 0, 1, 0x26, 0x01, 0xaf];
        let unit = access_unit(&stream, Codec::Hevc);
        assert_eq!(unit.parameter_sets.len(), 3);
        assert_eq!(unit.data, [0, 0, 0, 3, 0x26, 0x01, 0xaf]);
    }
}
