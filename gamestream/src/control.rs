//! Control messages: the encrypted envelope every message travels in, and the
//! messages a client sends and understands.

use crate::Result;
use aes_gcm::aead::AeadInOut;
use aes_gcm::{Aes128Gcm, KeyInit};

pub(crate) const REQUEST_IDR: u16 = 0x0302;
pub(crate) const START_B: u16 = 0x0307;
pub(crate) const PING: u16 = 0x0200;
pub(crate) const INPUT: u16 = 0x0206;
pub(crate) const RUMBLE: u16 = 0x010b;
pub(crate) const TERMINATION: u16 = 0x0109;
pub(crate) const HDR_MODE: u16 = 0x010e;

/// The HDR display the host encodes for, as the host describes it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HdrDisplay {
    /// Chromaticity x, y of red, green and blue, in 1/50000.
    pub primaries: [[u16; 2]; 3],
    /// Chromaticity x, y of white, in 1/50000.
    pub white: [u16; 2],
    /// Brightest the display shows, in nits.
    pub max_luminance: u16,
    /// Darkest the display shows, in 1/10000 nit.
    pub min_luminance: u16,
    /// Brightest pixel of the content, in nits.
    pub max_content_light: u16,
    /// Brightest frame average of the content, in nits.
    pub max_frame_average_light: u16,
}

/// Reads an HDR mode message: `None` for a message cut short, `Some(None)`
/// when the host sends SDR.
pub(crate) fn hdr_mode(payload: &[u8]) -> Option<Option<HdrDisplay>> {
    if *payload.first()? == 0 { return Some(None); }
    let values = payload.get(1..27)?;
    let value = |at: usize| u16::from_le_bytes([values[at * 2], values[at * 2 + 1]]);
    Some(Some(HdrDisplay {
        primaries: [[value(0), value(1)], [value(2), value(3)], [value(4), value(5)]],
        white: [value(6), value(7)],
        max_luminance: value(8),
        min_luminance: value(9),
        max_content_light: value(10),
        max_frame_average_light: value(11),
    }))
}

const ENVELOPE: u16 = 0x0001;
const TAG: usize = 16;

/// Seals and opens control messages with the session key.
pub(crate) struct Cipher {
    key: Aes128Gcm,
    sequence: u32,
}

/// The nonce: the sequence number, then who sent it ("CC" client, "HC" host).
fn nonce(sequence: u32, from_host: bool) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&sequence.to_le_bytes());
    nonce[10] = if from_host { b'H' } else { b'C' };
    nonce[11] = b'C';
    nonce
}

impl Cipher {
    pub(crate) fn new(key: &[u8; 16]) -> Self {
        Self { key: Aes128Gcm::new_from_slice(key).unwrap(), sequence: 0 }
    }

    /// `[0x0001][length][sequence][tag][ciphertext of: type, payload length, payload]`.
    pub(crate) fn seal(&mut self, kind: u16, payload: &[u8]) -> Vec<u8> {
        let mut text = Vec::with_capacity(4 + payload.len());
        text.extend_from_slice(&kind.to_le_bytes());
        text.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        text.extend_from_slice(payload);
        let tag = self.key.encrypt_inout_detached((&nonce(self.sequence, false)).into(), &[], text.as_mut_slice().into())
            .expect("AES-GCM seals any message this short");
        let mut message = Vec::with_capacity(8 + TAG + text.len());
        message.extend_from_slice(&ENVELOPE.to_le_bytes());
        message.extend_from_slice(&((4 + TAG + text.len()) as u16).to_le_bytes());
        message.extend_from_slice(&self.sequence.to_le_bytes());
        message.extend_from_slice(&tag);
        message.extend_from_slice(&text);
        self.sequence = self.sequence.wrapping_add(1);
        message
    }

    /// The type and payload of a message from the host.
    pub(crate) fn open(&self, message: &[u8]) -> Result<(u16, Vec<u8>)> {
        let malformed = "The PC sent a malformed control message";
        if message.len() < 8 + TAG + 4 || message[..2] != ENVELOPE.to_le_bytes() { return Err(malformed.into()); }
        let sequence = u32::from_le_bytes(message[4..8].try_into().unwrap());
        let tag: [u8; TAG] = message[8..8 + TAG].try_into().unwrap();
        let mut text = message[8 + TAG..].to_vec();
        self.key.decrypt_inout_detached((&nonce(sequence, true)).into(), &[], text.as_mut_slice().into(), (&tag).into())
            .map_err(|_| "A control message from the PC failed its integrity check")?;
        let kind = u16::from_le_bytes([text[0], text[1]]);
        Ok((kind, text.split_off(4)))
    }
}

/// The state of one Xbox-style gamepad, in the host's units.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Gamepad {
    /// See the `BUTTON_*` constants.
    pub buttons: u32,
    pub left_trigger: u8,
    pub right_trigger: u8,
    /// Stick axes: X positive is right, Y positive is up.
    pub left_stick: (i16, i16),
    pub right_stick: (i16, i16),
}

pub const BUTTON_UP: u32 = 0x0001;
pub const BUTTON_DOWN: u32 = 0x0002;
pub const BUTTON_LEFT: u32 = 0x0004;
pub const BUTTON_RIGHT: u32 = 0x0008;
pub const BUTTON_START: u32 = 0x0010;
pub const BUTTON_BACK: u32 = 0x0020;
pub const BUTTON_LEFT_STICK: u32 = 0x0040;
pub const BUTTON_RIGHT_STICK: u32 = 0x0080;
pub const BUTTON_LEFT_BUMPER: u32 = 0x0100;
pub const BUTTON_RIGHT_BUMPER: u32 = 0x0200;
pub const BUTTON_GUIDE: u32 = 0x0400;
pub const BUTTON_A: u32 = 0x1000;
pub const BUTTON_B: u32 = 0x2000;
pub const BUTTON_X: u32 = 0x4000;
pub const BUTTON_Y: u32 = 0x8000;

impl Gamepad {
    /// The state packet for controller 0, the only one connected.
    pub(crate) fn packet(&self) -> [u8; 34] {
        let mut packet = [0u8; 34];
        packet[..4].copy_from_slice(&30u32.to_be_bytes());
        packet[4..8].copy_from_slice(&0x0000_000cu32.to_le_bytes());
        let fields: [(usize, u16); 10] = [
            (8, 0x001a), (10, 0), (12, 1), (14, 0x0014), (16, self.buttons as u16),
            (20, self.left_stick.0 as u16), (22, self.left_stick.1 as u16),
            (24, self.right_stick.0 as u16), (26, self.right_stick.1 as u16), (28, 0x009c),
        ];
        for (at, value) in fields { packet[at..at + 2].copy_from_slice(&value.to_le_bytes()); }
        packet[18] = self.left_trigger;
        packet[19] = self.right_trigger;
        packet[30..32].copy_from_slice(&((self.buttons >> 16) as u16).to_le_bytes());
        packet[32..34].copy_from_slice(&0x0055u16.to_le_bytes());
        packet
    }

    /// The packet that unplugs controller 0 on the host: its bit is clear in
    /// the mask of connected controllers.
    pub(crate) fn unplug_packet() -> [u8; 34] {
        let mut packet = Gamepad::default().packet();
        packet[12..14].copy_from_slice(&0u16.to_le_bytes());
        packet
    }
}

pub const MODIFIER_SHIFT: u8 = 0x01;
pub const MODIFIER_CTRL: u8 = 0x02;
pub const MODIFIER_ALT: u8 = 0x04;
pub const MODIFIER_META: u8 = 0x08;

/// A key going down or up. `key` is a Windows virtual-key code.
pub(crate) fn key_packet(key: u8, down: bool, modifiers: u8) -> [u8; 14] {
    let mut packet = [0u8; 14];
    packet[..4].copy_from_slice(&10u32.to_be_bytes());
    packet[4..8].copy_from_slice(&(if down { 3u32 } else { 4 }).to_le_bytes());
    // The high bit marks a virtual-key code, as the reference client sends it.
    packet[9..11].copy_from_slice(&(0x8000 | u16::from(key)).to_le_bytes());
    packet[11] = modifiers;
    packet
}

/// Text for the host to type, in packets of whole characters. The host takes
/// 32 bytes at most in one packet.
pub(crate) fn text_packets(text: &str) -> Vec<Vec<u8>> {
    const MOST: usize = 32;
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut filled = MOST;
    for character in text.chars() {
        if filled + character.len_utf8() > MOST {
            let mut header = 4u32.to_be_bytes().to_vec();
            header.extend_from_slice(&0x17u32.to_le_bytes());
            packets.push(header);
            filled = 0;
        }
        let packet = packets.last_mut().unwrap();
        packet.extend_from_slice(character.encode_utf8(&mut [0; 4]).as_bytes());
        filled += character.len_utf8();
        packet[..4].copy_from_slice(&(4 + filled as u32).to_be_bytes());
    }
    packets
}

/// An input packet of `N` bytes with its header filled in: the length after
/// the length field, big-endian, then the type, little-endian.
fn input_packet<const N: usize>(magic: u32) -> [u8; N] {
    let mut packet = [0u8; N];
    packet[..4].copy_from_slice(&(N as u32 - 4).to_be_bytes());
    packet[4..8].copy_from_slice(&magic.to_le_bytes());
    packet
}

/// Mouse motion moves the pointer by deltas, in big-endian. A delta larger
/// than one packet holds goes out in several.
pub(crate) fn mouse_move_packets(mut dx: i32, mut dy: i32) -> Vec<[u8; 12]> {
    let mut packets = Vec::new();
    while dx != 0 || dy != 0 {
        let (x, y) = (dx.clamp(i16::MIN.into(), i16::MAX.into()), dy.clamp(i16::MIN.into(), i16::MAX.into()));
        let mut packet = input_packet(0x07);
        packet[8..10].copy_from_slice(&(x as i16).to_be_bytes());
        packet[10..12].copy_from_slice(&(y as i16).to_be_bytes());
        packets.push(packet);
        (dx, dy) = (dx - x, dy - y);
    }
    packets
}

/// The pointer at `x`, `y` of a `width` by `height` area that the host scales
/// to its screen. The host is sent the largest coordinate, one less than the
/// size, as the reference client does so that the far edge is reachable.
pub(crate) fn mouse_position_packet(x: i16, y: i16, width: i16, height: i16) -> [u8; 18] {
    let mut packet = input_packet(0x05);
    let fields = [x, y, 0, width.saturating_sub(1), height.saturating_sub(1)];
    for (at, value) in fields.into_iter().enumerate() {
        packet[8 + at * 2..10 + at * 2].copy_from_slice(&value.to_be_bytes());
    }
    packet
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton { Left = 1, Middle = 2, Right = 3, X1 = 4, X2 = 5 }

pub(crate) fn mouse_button_packet(button: MouseButton, down: bool) -> [u8; 9] {
    let mut packet = input_packet(if down { 0x08 } else { 0x09 });
    packet[8] = button as u8;
    packet
}

/// Vertical wheel turn, 120 to a notch, positive away from the user.
pub(crate) fn scroll_packet(amount: i16) -> [u8; 14] {
    let mut packet = input_packet(0x0a);
    packet[8..10].copy_from_slice(&amount.to_be_bytes());
    packet[10..12].copy_from_slice(&amount.to_be_bytes());
    packet
}

/// Horizontal wheel turn, 120 to a notch, positive to the right.
pub(crate) fn hscroll_packet(amount: i16) -> [u8; 10] {
    let mut packet = input_packet(0x5500_0001);
    packet[8..10].copy_from_slice(&amount.to_be_bytes());
    packet
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchPhase { Hover = 0, Down = 1, Up = 2, Move = 3, Cancel = 4, HoverLeave = 6, CancelAll = 7 }

/// One finger on the screen. The host injects it as a touch of its own
/// screen, which only hosts that announce touch accept.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Touch {
    /// Stays the same from the finger's `Down` to its `Up`.
    pub id: u32,
    pub phase: TouchPhase,
    /// Position on the video, 0 to 1 from the left and from the top.
    pub x: f32,
    pub y: f32,
    /// 0 to 1; for `Hover`, the distance from the screen, 0 when unknown.
    pub pressure: f32,
}

impl Touch {
    /// Contact area 0 and rotation 0xffff tell the host neither is known.
    pub(crate) fn packet(&self) -> [u8; 36] {
        let mut packet = input_packet(0x5500_0002);
        packet[8] = self.phase as u8;
        packet[10..12].copy_from_slice(&0xffffu16.to_le_bytes());
        packet[12..16].copy_from_slice(&self.id.to_le_bytes());
        packet[16..20].copy_from_slice(&self.x.to_le_bytes());
        packet[20..24].copy_from_slice(&self.y.to_le_bytes());
        packet[24..28].copy_from_slice(&self.pressure.to_le_bytes());
        packet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_hdr_mode_message() {
        assert_eq!(hdr_mode(&[0; 27]), Some(None));
        assert_eq!(hdr_mode(&[]), None);
        // Rec. 2020 primaries, D65 white, 1000 nits down to 0.005, content up to 800 and 400 nits.
        let values: [u16; 13] = [35400, 14600, 8500, 39850, 6550, 2300, 15635, 16450, 1000, 50, 800, 400, 0];
        let mut message = vec![1];
        message.extend(values.iter().flat_map(|value| value.to_le_bytes()));
        assert_eq!(hdr_mode(&message), Some(Some(HdrDisplay {
            primaries: [[35400, 14600], [8500, 39850], [6550, 2300]], white: [15635, 16450],
            max_luminance: 1000, min_luminance: 50, max_content_light: 800, max_frame_average_light: 400 })));
        // HDR is on, but the display's description is cut short.
        assert_eq!(hdr_mode(&message[..20]), None);
    }

    #[test]
    fn seals_with_the_documented_layout_and_counts_messages() {
        let mut cipher = Cipher::new(&[7; 16]);
        let first = cipher.seal(PING, &[4, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(first.len(), 8 + 16 + 4 + 8);
        assert_eq!(&first[..2], &[1, 0]);
        assert_eq!(u16::from_le_bytes([first[2], first[3]]) as usize, first.len() - 4);
        assert_eq!(&first[4..8], &[0, 0, 0, 0]);
        let second = cipher.seal(PING, &[4, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&second[4..8], &[1, 0, 0, 0]);
        assert_ne!(first[8..], second[8..]);
    }

    #[test]
    fn opens_only_what_the_host_direction_sealed() {
        // A message sealed as the host would: same key, the host's nonce.
        let key = Aes128Gcm::new_from_slice(&[7; 16]).unwrap();
        let mut text = vec![0x0b, 0x01, 2, 0, 0xaa, 0xbb];
        let tag = key.encrypt_inout_detached((&nonce(9, true)).into(), &[], text.as_mut_slice().into()).unwrap();
        let mut message = vec![1, 0, 26, 0, 9, 0, 0, 0];
        message.extend_from_slice(&tag);
        message.extend_from_slice(&text);

        let cipher = Cipher::new(&[7; 16]);
        assert_eq!(cipher.open(&message), Ok((RUMBLE, vec![0xaa, 0xbb])));
        message[29] ^= 1;
        assert!(cipher.open(&message).is_err());
        // The client's own direction uses another nonce and must not open.
        let own = Cipher::new(&[7; 16]).seal(PING, &[0; 8]);
        assert!(cipher.open(&own).is_err());
        assert!(cipher.open(&[1, 0, 0]).is_err());
    }

    #[test]
    fn lays_out_key_and_text_packets() {
        assert_eq!(key_packet(0x0d, true, 0), [0, 0, 0, 10, 3, 0, 0, 0, 0, 0x0d, 0x80, 0, 0, 0]);
        assert_eq!(key_packet(0x41, false, MODIFIER_SHIFT | MODIFIER_CTRL), [0, 0, 0, 10, 4, 0, 0, 0, 0, 0x41, 0x80, 3, 0, 0]);
        assert_eq!(text_packets("12"), [vec![0, 0, 0, 6, 0x17, 0, 0, 0, b'1', b'2']]);
        assert!(text_packets("").is_empty());
        // Sixteen two-byte letters fill one packet; the seventeenth starts the next.
        let packets = text_packets(&"я".repeat(17));
        assert_eq!(packets.len(), 2);
        assert_eq!((packets[0].len(), &packets[0][..4]), (8 + 32, &[0, 0, 0, 36][..]));
        assert_eq!(&packets[1][..], &[0, 0, 0, 6, 0x17, 0, 0, 0, 0xd1, 0x8f][..]);
    }

    // The expected bytes below follow moonlight-common-c b618ef2 and Vibepollo
    // 1f9ea16. Every packet starts with the header of Input.h:11-14: the length
    // after the length field, big-endian, then the type, little-endian. The
    // host drops a packet whose length is not exactly its struct's size
    // (Vibepollo src/input.cpp:308-345).

    #[test]
    fn lays_out_the_relative_mouse_packet() {
        // NV_REL_MOUSE_MOVE_PACKET, Input.h:39-45: GEN5 type 7, big-endian deltas.
        assert_eq!(mouse_move_packets(1, -2), [[0, 0, 0, 8, 7, 0, 0, 0, 0x00, 0x01, 0xff, 0xfe]]);
        assert!(mouse_move_packets(0, 0).is_empty());
        // A sum past i16 goes out in parts, InputStream.c:419-458.
        assert_eq!(mouse_move_packets(40000, -5), [
            [0, 0, 0, 8, 7, 0, 0, 0, 0x7f, 0xff, 0xff, 0xfb],
            [0, 0, 0, 8, 7, 0, 0, 0, 0x1c, 0x41, 0x00, 0x00],
        ]);
        assert_eq!(mouse_move_packets(-32769, 0), [
            [0, 0, 0, 8, 7, 0, 0, 0, 0x80, 0x00, 0, 0],
            [0, 0, 0, 8, 7, 0, 0, 0, 0xff, 0xff, 0, 0],
        ]);
    }

    #[test]
    fn lays_out_the_absolute_mouse_packet() {
        // NV_ABS_MOUSE_MOVE_PACKET, Input.h:47-60: type 5; x, y, unused, width,
        // height, big-endian. The size goes less one, InputStream.c:872-873.
        assert_eq!(mouse_position_packet(100, 200, 1920, 1080),
            [0, 0, 0, 14, 5, 0, 0, 0, 0x00, 0x64, 0x00, 0xc8, 0, 0, 0x07, 0x7f, 0x04, 0x37]);
        assert_eq!(&mouse_position_packet(0, 0, i16::MIN, 1)[14..], &[0x80, 0x00, 0, 0]);
    }

    #[test]
    fn lays_out_the_mouse_button_packet() {
        // NV_MOUSE_BUTTON_PACKET, Input.h:62-67: GEN5 types 8 down and 9 up;
        // buttons 1 to 5 from Limelight.h:699-703.
        assert_eq!(mouse_button_packet(MouseButton::Left, true), [0, 0, 0, 5, 8, 0, 0, 0, 1]);
        assert_eq!(mouse_button_packet(MouseButton::X2, false), [0, 0, 0, 5, 9, 0, 0, 0, 5]);
        assert_eq!(mouse_button_packet(MouseButton::Middle, true)[8], 2);
        assert_eq!(mouse_button_packet(MouseButton::Right, true)[8], 3);
        assert_eq!(mouse_button_packet(MouseButton::X1, true)[8], 4);
    }

    #[test]
    fn lays_out_the_scroll_packets() {
        // NV_SCROLL_PACKET, Input.h:111-118: GEN5 type 0x0a, the amount twice,
        // big-endian, then zero (InputStream.c:1354-1356).
        assert_eq!(scroll_packet(-120), [0, 0, 0, 10, 0x0a, 0, 0, 0, 0xff, 0x88, 0xff, 0x88, 0, 0]);
        // SS_HSCROLL_PACKET, Input.h:120-124: type 0x55000001, InputStream.c:1400-1402.
        assert_eq!(hscroll_packet(120), [0, 0, 0, 6, 0x01, 0, 0, 0x55, 0x00, 0x78]);
    }

    #[test]
    fn lays_out_the_touch_packet() {
        // SS_TOUCH_PACKET, Input.h:126-138: type 0x55000002; event, a zero,
        // rotation, pointer id, then x, y, pressure and the contact area as
        // little-endian floats (InputStream.c:1443-1453). Events from
        // Limelight.h:663-671; 0xffff is LI_ROT_UNKNOWN, Limelight.h:671.
        let touch = Touch { id: 0x0102_0304, phase: TouchPhase::Move, x: 0.5, y: 0.25, pressure: 1.0 };
        assert_eq!(touch.packet(), [
            0, 0, 0, 32, 0x02, 0, 0, 0x55,
            3, 0, 0xff, 0xff, 0x04, 0x03, 0x02, 0x01,
            0, 0, 0, 0x3f, 0, 0, 0x80, 0x3e, 0, 0, 0x80, 0x3f,
            0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        let phases = [TouchPhase::Hover, TouchPhase::Down, TouchPhase::Up, TouchPhase::Move, TouchPhase::Cancel,
            TouchPhase::HoverLeave, TouchPhase::CancelAll];
        let events: Vec<u8> = phases.iter().map(|&phase| Touch { phase, ..touch }.packet()[8]).collect();
        assert_eq!(events, [0, 1, 2, 3, 4, 6, 7]);
    }

    #[test]
    fn lays_out_the_gamepad_state_packet() {
        let pad = Gamepad { buttons: BUTTON_A | BUTTON_GUIDE | 0x20_0000, left_trigger: 255, right_trigger: 1,
            left_stick: (-32768, 32767), right_stick: (1, -2) };
        let packet = pad.packet();
        assert_eq!(&packet[..10], &[0, 0, 0, 30, 0x0c, 0, 0, 0, 0x1a, 0]);
        assert_eq!(&packet[10..16], &[0, 0, 1, 0, 0x14, 0]);
        assert_eq!(&packet[16..20], &[0x00, 0x14, 255, 1]);
        assert_eq!(&packet[20..28], &[0x00, 0x80, 0xff, 0x7f, 1, 0, 0xfe, 0xff]);
        assert_eq!(&packet[28..34], &[0x9c, 0, 0x20, 0, 0x55, 0]);
    }

    #[test]
    fn unplugs_controller_zero_with_an_empty_mask() {
        let (unplug, neutral) = (Gamepad::unplug_packet(), Gamepad::default().packet());
        assert_eq!(&unplug[12..14], &[0, 0]);
        assert_eq!(&neutral[12..14], &[1, 0]);
        // Only the mask differs from a neutral state.
        assert_eq!(unplug[..12], neutral[..12]);
        assert_eq!(unplug[14..], neutral[14..]);
    }
}
