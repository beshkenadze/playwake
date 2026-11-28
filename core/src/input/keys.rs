//! US-layout key tables: HID usages of page 0x07 (what `GCKeyboard` reports)
//! and Linux evdev `KEY_*` codes to the Windows virtual-key codes that
//! GameStream hosts take.

use super::{Device, Holds, Usage};
use gamestream::{MODIFIER_ALT, MODIFIER_CTRL, MODIFIER_META, MODIFIER_SHIFT};

impl Usage {
    /// The Windows virtual-key code of this key on a US keyboard.
    pub fn vk(self) -> Option<u8> {
        match self {
            Usage::Hid(usage) => hid_vk(usage),
            Usage::Evdev(code) => evdev_hid(code).and_then(hid_vk),
        }
    }
}

fn hid_vk(usage: u16) -> Option<u8> {
    // Every arm's result fits in a byte; the ranges are offsets into runs.
    let offset = |first: u16| (usage - first) as u8;
    Some(match usage {
        0x04..=0x1D => 0x41 + offset(0x04), // A…Z
        0x1E..=0x26 => 0x31 + offset(0x1E), // 1…9
        0x27 => 0x30,                       // 0
        0x28 => 0x0D,                       // Enter
        0x29 => 0x1B,                       // Escape
        0x2A => 0x08,                       // Backspace
        0x2B => 0x09,                       // Tab
        0x2C => 0x20,                       // Space
        0x2D => 0xBD,                       // -
        0x2E => 0xBB,                       // =
        0x2F => 0xDB,                       // [
        0x30 => 0xDD,                       // ]
        0x31 | 0x32 => 0xDC,                // \ and the ISO # in its place
        0x33 => 0xBA,                       // ;
        0x34 => 0xDE,                       // '
        0x35 => 0xC0,                       // `
        0x36 => 0xBC,                       // ,
        0x37 => 0xBE,                       // .
        0x38 => 0xBF,                       // /
        0x39 => 0x14,                       // Caps Lock
        0x3A..=0x45 => 0x70 + offset(0x3A), // F1…F12
        0x46 => 0x2C,                       // Print Screen
        0x47 => 0x91,                       // Scroll Lock
        0x48 => 0x13,                       // Pause
        0x49 => 0x2D,                       // Insert
        0x4A => 0x24,                       // Home
        0x4B => 0x21,                       // Page Up
        0x4C => 0x2E,                       // Delete
        0x4D => 0x23,                       // End
        0x4E => 0x22,                       // Page Down
        0x4F => 0x27,                       // Right
        0x50 => 0x25,                       // Left
        0x51 => 0x28,                       // Down
        0x52 => 0x26,                       // Up
        0x53 => 0x90,                       // Num Lock
        0x54 => 0x6F,                       // keypad /
        0x55 => 0x6A,                       // keypad *
        0x56 => 0x6D,                       // keypad -
        0x57 => 0x6B,                       // keypad +
        0x58 => 0x0D,                       // keypad Enter
        0x59..=0x61 => 0x61 + offset(0x59), // keypad 1…9
        0x62 => 0x60,                       // keypad 0
        0x63 => 0x6E,                       // keypad .
        0x64 => 0xE2,                       // ISO \ next to the left Shift
        0x65 => 0x5D,                       // Menu
        0x67 => 0x92,                       // keypad =
        0x68..=0x73 => 0x7C + offset(0x68), // F13…F24
        0xE0 => 0xA2,                       // left Ctrl
        0xE1 => 0xA0,                       // left Shift
        0xE2 => 0xA4,                       // left Alt
        0xE3 => 0x5B,                       // left Win (Cmd)
        0xE4 => 0xA3,                       // right Ctrl
        0xE5 => 0xA1,                       // right Shift
        0xE6 => 0xA5,                       // right Alt
        0xE7 => 0x5C,                       // right Win (Cmd)
        _ => return None,
    })
}

/// The HID usage of the key an evdev code names, from `input-event-codes.h`.
fn evdev_hid(code: u16) -> Option<u16> {
    let letter = |row: &[u8], first: u16| 0x04 + u16::from(row[usize::from(code - first)] - b'A');
    Some(match code {
        1 => 0x29,                           // KEY_ESC
        2..=10 => 0x1E + (code - 2),         // KEY_1…KEY_9
        11 => 0x27,                          // KEY_0
        12 => 0x2D,                          // KEY_MINUS
        13 => 0x2E,                          // KEY_EQUAL
        14 => 0x2A,                          // KEY_BACKSPACE
        15 => 0x2B,                          // KEY_TAB
        16..=25 => letter(b"QWERTYUIOP", 16),
        26 => 0x2F,                          // KEY_LEFTBRACE
        27 => 0x30,                          // KEY_RIGHTBRACE
        28 => 0x28,                          // KEY_ENTER
        29 => 0xE0,                          // KEY_LEFTCTRL
        30..=38 => letter(b"ASDFGHJKL", 30),
        39 => 0x33,                          // KEY_SEMICOLON
        40 => 0x34,                          // KEY_APOSTROPHE
        41 => 0x35,                          // KEY_GRAVE
        42 => 0xE1,                          // KEY_LEFTSHIFT
        43 => 0x31,                          // KEY_BACKSLASH
        44..=50 => letter(b"ZXCVBNM", 44),
        51 => 0x36,                          // KEY_COMMA
        52 => 0x37,                          // KEY_DOT
        53 => 0x38,                          // KEY_SLASH
        54 => 0xE5,                          // KEY_RIGHTSHIFT
        55 => 0x55,                          // KEY_KPASTERISK
        56 => 0xE2,                          // KEY_LEFTALT
        57 => 0x2C,                          // KEY_SPACE
        58 => 0x39,                          // KEY_CAPSLOCK
        59..=68 => 0x3A + (code - 59),       // KEY_F1…KEY_F10
        69 => 0x53,                          // KEY_NUMLOCK
        70 => 0x47,                          // KEY_SCROLLLOCK
        // KEY_KP7 KP8 KP9 KPMINUS KP4 KP5 KP6 KPPLUS KP1 KP2 KP3 KP0 KPDOT
        71..=83 => [0x5F, 0x60, 0x61, 0x56, 0x5C, 0x5D, 0x5E, 0x57, 0x59, 0x5A, 0x5B, 0x62, 0x63]
            [usize::from(code - 71)],
        86 => 0x64,                          // KEY_102ND
        87 => 0x44,                          // KEY_F11
        88 => 0x45,                          // KEY_F12
        96 => 0x58,                          // KEY_KPENTER
        97 => 0xE4,                          // KEY_RIGHTCTRL
        98 => 0x54,                          // KEY_KPSLASH
        99 => 0x46,                          // KEY_SYSRQ
        100 => 0xE6,                         // KEY_RIGHTALT
        102 => 0x4A,                         // KEY_HOME
        103 => 0x52,                         // KEY_UP
        104 => 0x4B,                         // KEY_PAGEUP
        105 => 0x50,                         // KEY_LEFT
        106 => 0x4F,                         // KEY_RIGHT
        107 => 0x4D,                         // KEY_END
        108 => 0x51,                         // KEY_DOWN
        109 => 0x4E,                         // KEY_PAGEDOWN
        110 => 0x49,                         // KEY_INSERT
        111 => 0x4C,                         // KEY_DELETE
        117 => 0x67,                         // KEY_KPEQUAL
        119 => 0x48,                         // KEY_PAUSE
        125 => 0xE3,                         // KEY_LEFTMETA
        126 => 0xE7,                         // KEY_RIGHTMETA
        127 => 0x65,                         // KEY_COMPOSE
        183..=194 => 0x68 + (code - 183),    // KEY_F13…KEY_F24
        _ => return None,
    })
}

/// The `gamestream::MODIFIER_*` bit a modifier key's virtual-key code
/// stands for, or 0 for any other key.
pub fn modifier(vk: u8) -> u8 {
    match vk {
        0xA0 | 0xA1 => MODIFIER_SHIFT,
        0xA2 | 0xA3 => MODIFIER_CTRL,
        0xA4 | 0xA5 => MODIFIER_ALT,
        0x5B | 0x5C => MODIFIER_META,
        _ => 0,
    }
}

/// The HID usage that types `c` on a US keyboard, and whether it takes Shift.
pub fn char_usage(c: char) -> Option<(u16, bool)> {
    /// Usages with their plain and shifted characters.
    const PUNCTUATION: [(u16, char, char); 11] = [
        (0x2D, '-', '_'), (0x2E, '=', '+'), (0x2F, '[', '{'), (0x30, ']', '}'), (0x31, '\\', '|'),
        (0x33, ';', ':'), (0x34, '\'', '"'), (0x35, '`', '~'), (0x36, ',', '<'), (0x37, '.', '>'),
        (0x38, '/', '?'),
    ];
    /// Shift with 1…9 and 0, the usages 0x1E…0x27.
    const SHIFTED_DIGITS: [char; 10] = ['!', '@', '#', '$', '%', '^', '&', '*', '(', ')'];
    let offset = |first: char| c as u16 - first as u16;
    Some(match c {
        'a'..='z' => (0x04 + offset('a'), false),
        'A'..='Z' => (0x04 + offset('A'), true),
        '1'..='9' => (0x1E + offset('1'), false),
        '0' => (0x27, false),
        ' ' => (0x2C, false),
        _ => match SHIFTED_DIGITS.iter().position(|&digit| digit == c) {
            Some(index) => (0x1E + index as u16, true),
            None => PUNCTUATION.iter().find_map(|&(usage, plain, shifted)| {
                (c == plain || c == shifted).then_some((usage, c == shifted))
            })?,
        },
    })
}

/// The keys held down, for the modifier mask each key packet carries.
#[derive(Default)]
pub(super) struct Keyboard {
    held: Holds<u8>,
}

impl Keyboard {
    /// The key's virtual-key code and the modifiers held with it, if the
    /// host is to hear it: a press always, a release once no device holds
    /// the key. A modifier counts itself while it goes down and no longer
    /// once it goes up.
    pub(super) fn key(&mut self, device: Device, usage: Usage, down: bool) -> Option<(u8, u8)> {
        let vk = usage.vk()?;
        self.held.change(vk, device, down).then(|| (vk, self.held.keys().fold(0, |mask, held| mask | modifier(held))))
    }

    /// Every key still held, now forgotten as released.
    pub(super) fn release_all(&mut self) -> Vec<u8> {
        self.held.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn both(hid: u16, evdev: u16) -> (Option<u8>, Option<u8>) {
        (Usage::Hid(hid).vk(), Usage::Evdev(evdev).vk())
    }

    #[test]
    fn letters_and_digits() {
        assert_eq!(both(0x04, 30), (Some(0x41), Some(0x41)));   // A
        assert_eq!(both(0x1D, 44), (Some(0x5A), Some(0x5A)));   // Z
        assert_eq!(both(0x1A, 17), (Some(0x57), Some(0x57)));   // W
        assert_eq!(both(0x10, 50), (Some(0x4D), Some(0x4D)));   // M
        assert_eq!(both(0x1E, 2), (Some(0x31), Some(0x31)));    // 1
        assert_eq!(both(0x26, 10), (Some(0x39), Some(0x39)));   // 9
        assert_eq!(both(0x27, 11), (Some(0x30), Some(0x30)));   // 0
    }

    #[test]
    fn named_keys() {
        assert_eq!(both(0x28, 28), (Some(0x0D), Some(0x0D)));   // Enter
        assert_eq!(both(0x29, 1), (Some(0x1B), Some(0x1B)));    // Escape
        assert_eq!(both(0x2C, 57), (Some(0x20), Some(0x20)));   // Space
        assert_eq!(both(0x3A, 59), (Some(0x70), Some(0x70)));   // F1
        assert_eq!(both(0x45, 88), (Some(0x7B), Some(0x7B)));   // F12
        assert_eq!(both(0x68, 183), (Some(0x7C), Some(0x7C)));  // F13
        assert_eq!(both(0x35, 41), (Some(0xC0), Some(0xC0)));   // `
        assert_eq!(both(0x2D, 12), (Some(0xBD), Some(0xBD)));   // -
        assert_eq!(both(0x5F, 71), (Some(0x67), Some(0x67)));   // keypad 7
        assert_eq!(both(0x63, 83), (Some(0x6E), Some(0x6E)));   // keypad .
        assert_eq!(both(0x54, 98), (Some(0x6F), Some(0x6F)));   // keypad /
        assert_eq!(both(0x58, 96), (Some(0x0D), Some(0x0D)));   // keypad Enter
        assert_eq!(both(0x67, 117), (Some(0x92), Some(0x92)));  // keypad =
    }

    #[test]
    fn arrows() {
        assert_eq!(both(0x52, 103), (Some(0x26), Some(0x26)));  // Up
        assert_eq!(both(0x51, 108), (Some(0x28), Some(0x28)));  // Down
        assert_eq!(both(0x50, 105), (Some(0x25), Some(0x25)));  // Left
        assert_eq!(both(0x4F, 106), (Some(0x27), Some(0x27)));  // Right
    }

    #[test]
    fn modifiers_left_and_right() {
        assert_eq!(both(0xE1, 42), (Some(0xA0), Some(0xA0)));   // left Shift
        assert_eq!(both(0xE5, 54), (Some(0xA1), Some(0xA1)));   // right Shift
        assert_eq!(both(0xE0, 29), (Some(0xA2), Some(0xA2)));   // left Ctrl
        assert_eq!(both(0xE4, 97), (Some(0xA3), Some(0xA3)));   // right Ctrl
        assert_eq!(both(0xE2, 56), (Some(0xA4), Some(0xA4)));   // left Alt
        assert_eq!(both(0xE6, 100), (Some(0xA5), Some(0xA5)));  // right Alt
        assert_eq!(both(0xE3, 125), (Some(0x5B), Some(0x5B)));  // left Win
        assert_eq!(both(0xE7, 126), (Some(0x5C), Some(0x5C)));  // right Win
        assert_eq!(modifier(0xA1), MODIFIER_SHIFT);
        assert_eq!(modifier(0x5B), MODIFIER_META);
        assert_eq!(modifier(0x41), 0);
    }

    #[test]
    fn unknown_codes_have_no_key() {
        assert_eq!(both(0x00, 0), (None, None));
        assert_eq!(both(0x66, 84), (None, None));
        assert_eq!(Usage::Hid(0xFFFF).vk(), None);
        assert_eq!(Usage::Evdev(0xFFFF).vk(), None);
    }

    #[test]
    fn rows_keep_their_order() {
        let rows: [(u16, &[u8]); 4] = [(16, b"QWERTYUIOP"), (30, b"ASDFGHJKL"), (44, b"ZXCVBNM"), (2, b"1234567890")];
        for (first, row) in rows {
            for (code, &vk) in (first..).zip(row) {
                assert_eq!(Usage::Evdev(code).vk(), Some(vk), "evdev {code}");
            }
        }
        for (usage, &vk) in (0x04..).zip(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890") {
            assert_eq!(Usage::Hid(usage).vk(), Some(vk), "HID {usage:#x}");
        }
        // Keypad 7 8 9 - 4 5 6 + 1 2 3 0 .
        let keypad = [0x67, 0x68, 0x69, 0x6D, 0x64, 0x65, 0x66, 0x6B, 0x61, 0x62, 0x63, 0x60, 0x6E];
        for (code, vk) in (71..).zip(keypad) {
            assert_eq!(Usage::Evdev(code).vk(), Some(vk), "evdev {code}");
        }
        // Keypad 1…9 0 .
        let keypad = [0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x60, 0x6E];
        for (usage, vk) in (0x59..).zip(keypad) {
            assert_eq!(Usage::Hid(usage).vk(), Some(vk), "HID {usage:#x}");
        }
        for (code, vk) in (59..=68).chain([87, 88]).chain(183..=194).zip(0x70..=0x87) {
            assert_eq!(Usage::Evdev(code).vk(), Some(vk), "evdev F-key {code}");
        }
        for (usage, vk) in (0x3A..=0x45).chain(0x68..=0x73).zip(0x70..=0x87) {
            assert_eq!(Usage::Hid(usage).vk(), Some(vk), "HID F-key {usage:#x}");
        }
        assert_eq!(Usage::Hid(0x32).vk(), Some(0xDC), "ISO # sits where \\ is");
    }

    /// Every key lands on a virtual key of its own, but for the keys that
    /// share one on purpose, so a lost or misdirected row shows.
    fn distinct_keys(usage: fn(u16) -> Usage, shared: &[u8]) -> usize {
        let mut seen = std::collections::BTreeMap::new();
        for code in 0..=u16::MAX {
            if let Some(vk) = usage(code).vk()
                && let Some(before) = seen.insert(vk, code)
            {
                assert!(shared.contains(&vk), "{before} and {code} both give {vk:#x}");
            }
        }
        seen.len()
    }

    #[test]
    fn each_hid_key_has_its_own_virtual_key() {
        assert_eq!(distinct_keys(Usage::Hid, &[0x0D, 0xDC]), 117);
    }

    #[test]
    fn each_evdev_key_has_its_own_virtual_key() {
        assert_eq!(distinct_keys(Usage::Evdev, &[0x0D]), 117);
    }

    #[test]
    fn modifier_mask_follows_the_held_keys() {
        let mut keyboard = Keyboard::default();
        let mut key = |usage, down| keyboard.key(Device::Keyboard, usage, down);
        assert_eq!(key(Usage::Hid(0xE1), true), Some((0xA0, MODIFIER_SHIFT)));
        assert_eq!(key(Usage::Hid(0x04), true), Some((0x41, MODIFIER_SHIFT)));
        assert_eq!(key(Usage::Evdev(29), true), Some((0xA2, MODIFIER_SHIFT | MODIFIER_CTRL)));
        assert_eq!(key(Usage::Hid(0xE1), false), Some((0xA0, MODIFIER_CTRL)));
        assert_eq!(key(Usage::Hid(0x04), false), Some((0x41, MODIFIER_CTRL)));
        assert_eq!(key(Usage::Hid(0xE0), false), Some((0xA2, 0)));
        assert_eq!(key(Usage::Hid(0x00), true), None);
    }

    #[test]
    fn a_key_two_devices_hold_goes_up_with_the_last() {
        let mut keyboard = Keyboard::default();
        let ctrl = Usage::Hid(0xE0);
        assert_eq!(keyboard.key(Device::Keyboard, ctrl, true), Some((0xA2, MODIFIER_CTRL)));
        assert_eq!(keyboard.key(Device::Panel, ctrl, true), Some((0xA2, MODIFIER_CTRL)));
        assert_eq!(keyboard.key(Device::Panel, ctrl, false), None);
        assert_eq!(keyboard.key(Device::Keyboard, Usage::Hid(0x06), true), Some((0x43, MODIFIER_CTRL)));
        assert_eq!(keyboard.key(Device::Keyboard, ctrl, false), Some((0xA2, 0)));
        // A release of a key nobody holds still goes out.
        assert_eq!(keyboard.key(Device::Panel, ctrl, false), Some((0xA2, 0)));
    }

    #[test]
    fn characters_type_with_their_us_keys() {
        let typed = |text: &str| text.chars().map(|c| char_usage(c).unwrap()).collect::<Vec<_>>();
        assert_eq!(typed("aZ0 9"), [(0x04, false), (0x1D, true), (0x27, false), (0x2C, false), (0x26, false)]);
        assert_eq!(typed("!@)"), [(0x1E, true), (0x1F, true), (0x27, true)]);
        assert_eq!(typed("-_\\|'\"`~/?"), [
            (0x2D, false), (0x2D, true), (0x31, false), (0x31, true), (0x34, false), (0x34, true),
            (0x35, false), (0x35, true), (0x38, false), (0x38, true),
        ]);
        for c in ['é', '\n', '\t', '£', '\u{0}'] { assert_eq!(char_usage(c), None, "{c:?}"); }
    }

    #[test]
    fn every_printable_ascii_character_types_with_a_key_that_has_a_virtual_key() {
        for c in ' '..='~' {
            let (usage, _) = char_usage(c).unwrap_or_else(|| panic!("{c:?} has no key"));
            assert!(Usage::Hid(usage).vk().is_some(), "{c:?}");
        }
    }

    #[test]
    fn release_all_forgets_the_held_keys() {
        let mut keyboard = Keyboard::default();
        keyboard.key(Device::Keyboard, Usage::Hid(0xE3), true);
        keyboard.key(Device::Panel, Usage::Hid(0x04), true);
        assert_eq!(keyboard.release_all(), vec![0x41, 0x5B]);
        assert_eq!(keyboard.key(Device::Keyboard, Usage::Hid(0x05), true), Some((0x42, 0)));
    }
}
