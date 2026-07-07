// SPDX-License-Identifier: GPL-3.0-or-later
//
//! Human-readable QMK keycode table for `clackctl`.
//!
//! Maps the 16-bit keycodes that travel over the D-Bus interface to the
//! canonical QMK `KC_*` names, plus the short aliases QMK users already
//! know (`ESC`, `BSPC`, `LSFT`, …) and single-character punctuation
//! (`-`, `[`, `;`). Codes are the QMK *basic* keycode range — HID usage
//! ids re-exported by `quantum/keycodes.h` — which is what VIA's
//! `id_dynamic_keymap_*` commands speak. Quantum keycodes (layer taps,
//! mod taps) are 16-bit values outside this table; they parse and print
//! as raw hex.
//!
//! **Context:** `clackctl` is a raw-u16 tool underneath; this table only
//! changes how those values are read and written on the command line.
//! Nothing here is sent to the device — an unknown name is a CLI error
//! long before any D-Bus call is made.

/// One nameable keycode.
pub struct Key {
    /// The 16-bit QMK keycode as used by VIA's dynamic-keymap commands.
    pub code: u16,
    /// Canonical QMK name, always `KC_`-prefixed (e.g. `KC_BACKSPACE`).
    pub name: &'static str,
    /// Grid label for `clackctl keymap` — at most 4 characters.
    pub short: &'static str,
    /// Extra accepted spellings (QMK short aliases, punctuation, common
    /// synonyms). Matching is case-insensitive.
    pub aliases: &'static [&'static str],
}

macro_rules! keys {
    ($($code:literal $name:literal $short:literal [$($alias:literal),*];)+) => {
        &[$(Key { code: $code, name: $name, short: $short, aliases: &[$($alias),*] }),+]
    };
}

/// The full name table, ordered by keycode.
///
/// Names and values follow QMK `quantum/keycodes.h`. Do not guess new
/// entries — a wrong value here writes the wrong key to hardware.
pub const ALL: &[Key] = keys![
    0x0000 "KC_NO" "--" ["none", "no", "xxx"];
    0x0001 "KC_TRANSPARENT" "TRNS" ["trans", "trns"];
    0x0004 "KC_A" "A" [];
    0x0005 "KC_B" "B" [];
    0x0006 "KC_C" "C" [];
    0x0007 "KC_D" "D" [];
    0x0008 "KC_E" "E" [];
    0x0009 "KC_F" "F" [];
    0x000A "KC_G" "G" [];
    0x000B "KC_H" "H" [];
    0x000C "KC_I" "I" [];
    0x000D "KC_J" "J" [];
    0x000E "KC_K" "K" [];
    0x000F "KC_L" "L" [];
    0x0010 "KC_M" "M" [];
    0x0011 "KC_N" "N" [];
    0x0012 "KC_O" "O" [];
    0x0013 "KC_P" "P" [];
    0x0014 "KC_Q" "Q" [];
    0x0015 "KC_R" "R" [];
    0x0016 "KC_S" "S" [];
    0x0017 "KC_T" "T" [];
    0x0018 "KC_U" "U" [];
    0x0019 "KC_V" "V" [];
    0x001A "KC_W" "W" [];
    0x001B "KC_X" "X" [];
    0x001C "KC_Y" "Y" [];
    0x001D "KC_Z" "Z" [];
    0x001E "KC_1" "1" [];
    0x001F "KC_2" "2" [];
    0x0020 "KC_3" "3" [];
    0x0021 "KC_4" "4" [];
    0x0022 "KC_5" "5" [];
    0x0023 "KC_6" "6" [];
    0x0024 "KC_7" "7" [];
    0x0025 "KC_8" "8" [];
    0x0026 "KC_9" "9" [];
    0x0027 "KC_0" "0" [];
    0x0028 "KC_ENTER" "ENT" ["ent", "return"];
    0x0029 "KC_ESCAPE" "ESC" ["esc"];
    0x002A "KC_BACKSPACE" "BSPC" ["bspc"];
    0x002B "KC_TAB" "TAB" [];
    0x002C "KC_SPACE" "SPC" ["spc"];
    0x002D "KC_MINUS" "-" ["mins", "-"];
    0x002E "KC_EQUAL" "=" ["eql", "="];
    0x002F "KC_LEFT_BRACKET" "[" ["lbrc", "["];
    0x0030 "KC_RIGHT_BRACKET" "]" ["rbrc", "]"];
    0x0031 "KC_BACKSLASH" "\\" ["bsls", "\\"];
    0x0032 "KC_NONUS_HASH" "#" ["nuhs"];
    0x0033 "KC_SEMICOLON" ";" ["scln", ";"];
    0x0034 "KC_QUOTE" "'" ["quot", "'"];
    0x0035 "KC_GRAVE" "`" ["grv", "`"];
    0x0036 "KC_COMMA" "," ["comm", ","];
    0x0037 "KC_DOT" "." ["."];
    0x0038 "KC_SLASH" "/" ["slsh", "/"];
    0x0039 "KC_CAPS_LOCK" "CAPS" ["caps"];
    0x003A "KC_F1" "F1" [];
    0x003B "KC_F2" "F2" [];
    0x003C "KC_F3" "F3" [];
    0x003D "KC_F4" "F4" [];
    0x003E "KC_F5" "F5" [];
    0x003F "KC_F6" "F6" [];
    0x0040 "KC_F7" "F7" [];
    0x0041 "KC_F8" "F8" [];
    0x0042 "KC_F9" "F9" [];
    0x0043 "KC_F10" "F10" [];
    0x0044 "KC_F11" "F11" [];
    0x0045 "KC_F12" "F12" [];
    0x0046 "KC_PRINT_SCREEN" "PSCR" ["pscr", "prtsc"];
    0x0047 "KC_SCROLL_LOCK" "SCRL" ["scrl"];
    0x0048 "KC_PAUSE" "PAUS" ["paus", "brk"];
    0x0049 "KC_INSERT" "INS" ["ins"];
    0x004A "KC_HOME" "HOME" [];
    0x004B "KC_PAGE_UP" "PGUP" ["pgup"];
    0x004C "KC_DELETE" "DEL" ["del"];
    0x004D "KC_END" "END" [];
    0x004E "KC_PAGE_DOWN" "PGDN" ["pgdn"];
    0x004F "KC_RIGHT" "RGHT" ["rght"];
    0x0050 "KC_LEFT" "LEFT" [];
    0x0051 "KC_DOWN" "DOWN" [];
    0x0052 "KC_UP" "UP" [];
    0x0053 "KC_NUM_LOCK" "NUM" ["num"];
    0x0054 "KC_KP_SLASH" "P/" ["psls"];
    0x0055 "KC_KP_ASTERISK" "P*" ["past"];
    0x0056 "KC_KP_MINUS" "P-" ["pmns"];
    0x0057 "KC_KP_PLUS" "P+" ["ppls"];
    0x0058 "KC_KP_ENTER" "PENT" ["pent"];
    0x0059 "KC_KP_1" "P1" [];
    0x005A "KC_KP_2" "P2" [];
    0x005B "KC_KP_3" "P3" [];
    0x005C "KC_KP_4" "P4" [];
    0x005D "KC_KP_5" "P5" [];
    0x005E "KC_KP_6" "P6" [];
    0x005F "KC_KP_7" "P7" [];
    0x0060 "KC_KP_8" "P8" [];
    0x0061 "KC_KP_9" "P9" [];
    0x0062 "KC_KP_0" "P0" [];
    0x0063 "KC_KP_DOT" "P." ["pdot"];
    0x0064 "KC_NONUS_BACKSLASH" "NUBS" ["nubs"];
    0x0065 "KC_APPLICATION" "APP" ["app", "menu"];
    0x0067 "KC_KP_EQUAL" "P=" ["peql"];
    0x0068 "KC_F13" "F13" [];
    0x0069 "KC_F14" "F14" [];
    0x006A "KC_F15" "F15" [];
    0x006B "KC_F16" "F16" [];
    0x006C "KC_F17" "F17" [];
    0x006D "KC_F18" "F18" [];
    0x006E "KC_F19" "F19" [];
    0x006F "KC_F20" "F20" [];
    0x0070 "KC_F21" "F21" [];
    0x0071 "KC_F22" "F22" [];
    0x0072 "KC_F23" "F23" [];
    0x0073 "KC_F24" "F24" [];
    0x00A5 "KC_SYSTEM_POWER" "PWR" ["pwr"];
    0x00A6 "KC_SYSTEM_SLEEP" "SLEP" ["slep", "sleep"];
    0x00A7 "KC_SYSTEM_WAKE" "WAKE" ["wake"];
    0x00A8 "KC_AUDIO_MUTE" "MUTE" ["mute"];
    0x00A9 "KC_AUDIO_VOL_UP" "VOLU" ["volu"];
    0x00AA "KC_AUDIO_VOL_DOWN" "VOLD" ["vold"];
    0x00AB "KC_MEDIA_NEXT_TRACK" "MNXT" ["mnxt", "next"];
    0x00AC "KC_MEDIA_PREV_TRACK" "MPRV" ["mprv", "prev"];
    0x00AD "KC_MEDIA_STOP" "MSTP" ["mstp"];
    0x00AE "KC_MEDIA_PLAY_PAUSE" "MPLY" ["mply", "play"];
    0x00AF "KC_MEDIA_SELECT" "MSEL" ["msel"];
    0x00B0 "KC_MEDIA_EJECT" "EJCT" ["ejct"];
    0x00B1 "KC_MAIL" "MAIL" [];
    0x00B2 "KC_CALCULATOR" "CALC" ["calc"];
    0x00B3 "KC_MY_COMPUTER" "MYCM" ["mycm"];
    0x00B4 "KC_WWW_SEARCH" "WSCH" ["wsch"];
    0x00B5 "KC_WWW_HOME" "WHOM" ["whom"];
    0x00B6 "KC_WWW_BACK" "WBAK" ["wbak"];
    0x00B7 "KC_WWW_FORWARD" "WFWD" ["wfwd"];
    0x00B8 "KC_WWW_STOP" "WSTP" ["wstp"];
    0x00B9 "KC_WWW_REFRESH" "WREF" ["wref"];
    0x00BA "KC_WWW_FAVORITES" "WFAV" ["wfav"];
    0x00BB "KC_MEDIA_FAST_FORWARD" "MFFD" ["mffd"];
    0x00BC "KC_MEDIA_REWIND" "MRWD" ["mrwd"];
    0x00BD "KC_BRIGHTNESS_UP" "BRIU" ["briu"];
    0x00BE "KC_BRIGHTNESS_DOWN" "BRID" ["brid"];
    0x00E0 "KC_LEFT_CTRL" "LCTL" ["lctl", "lctrl"];
    0x00E1 "KC_LEFT_SHIFT" "LSFT" ["lsft", "lshift"];
    0x00E2 "KC_LEFT_ALT" "LALT" ["lalt", "lopt"];
    0x00E3 "KC_LEFT_GUI" "LGUI" ["lgui", "lcmd", "lwin"];
    0x00E4 "KC_RIGHT_CTRL" "RCTL" ["rctl", "rctrl"];
    0x00E5 "KC_RIGHT_SHIFT" "RSFT" ["rsft", "rshift"];
    0x00E6 "KC_RIGHT_ALT" "RALT" ["ralt", "ropt", "algr"];
    0x00E7 "KC_RIGHT_GUI" "RGUI" ["rgui", "rcmd", "rwin"];
];

/// Looks a keycode up by value.
pub fn by_code(code: u16) -> Option<&'static Key> {
    ALL.iter().find(|k| k.code == code)
}

/// Parses a key *name* (not a number) to its keycode.
///
/// Accepts, case-insensitively: the canonical `KC_*` name, the name
/// without the `KC_` prefix (with `-` accepted for `_`, so `page-up`
/// works), and any alias — including single-character punctuation like
/// `-` or `[`.
pub fn by_name(s: &str) -> Option<u16> {
    let raw = s.trim();
    // Punctuation aliases first — the `-`→`_` normalisation below would
    // destroy them.
    if let Some(k) = ALL
        .iter()
        .find(|k| k.aliases.iter().any(|a| a.eq_ignore_ascii_case(raw)))
    {
        return Some(k.code);
    }
    let norm = raw.to_ascii_uppercase().replace('-', "_");
    let norm = norm.strip_prefix("KC_").unwrap_or(&norm);
    ALL.iter()
        .find(|k| {
            k.name["KC_".len()..] == *norm
                || k.aliases.iter().any(|a| a.eq_ignore_ascii_case(norm))
        })
        .map(|k| k.code)
}

/// Formats a keycode for humans: `KC_A (0x0004)`, or `0x1234 (no name —
/// quantum keycode?)` when the value is outside the basic table.
pub fn describe(code: u16) -> String {
    match by_code(code) {
        Some(k) => format!("{} ({:#06x})", k.name, code),
        None => format!("{code:#06x} (no name — quantum keycode?)"),
    }
}

/// Formats a keycode for the `keymap` grid: the short label, or raw hex
/// for values outside the table.
pub fn short_label(code: u16) -> String {
    match by_code(code) {
        Some(k) => k.short.to_owned(),
        None => format!("{code:#06x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_names_resolve() {
        assert_eq!(by_name("KC_A"), Some(0x0004));
        assert_eq!(by_name("KC_ENTER"), Some(0x0028));
        assert_eq!(by_name("KC_RIGHT_GUI"), Some(0x00E7));
    }

    #[test]
    fn prefixless_and_case_insensitive_names_resolve() {
        assert_eq!(by_name("a"), Some(0x0004));
        assert_eq!(by_name("enter"), Some(0x0028));
        assert_eq!(by_name("Page-Up"), Some(0x004B));
        assert_eq!(by_name("left_shift"), Some(0x00E1));
    }

    #[test]
    fn aliases_resolve() {
        assert_eq!(by_name("esc"), Some(0x0029));
        assert_eq!(by_name("bspc"), Some(0x002A));
        assert_eq!(by_name("lwin"), Some(0x00E3));
        assert_eq!(by_name("-"), Some(0x002D));
        assert_eq!(by_name("["), Some(0x002F));
    }

    #[test]
    fn digits_resolve_to_number_row() {
        assert_eq!(by_name("1"), Some(0x001E));
        assert_eq!(by_name("0"), Some(0x0027));
    }

    #[test]
    fn unknown_names_do_not_resolve() {
        assert_eq!(by_name("notakey"), None);
        assert_eq!(by_name("0x004C"), None);
    }

    #[test]
    fn describe_names_known_and_unknown() {
        assert_eq!(describe(0x0004), "KC_A (0x0004)");
        assert!(describe(0x5f00).contains("no name"));
    }

    #[test]
    fn table_is_sorted_and_unique_by_code() {
        // INVARIANT: keeps `by_code` unambiguous and the `keys` listing
        // readable; a duplicated code would silently shadow a name.
        for pair in ALL.windows(2) {
            assert!(
                pair[0].code < pair[1].code,
                "table out of order near {} / {}",
                pair[0].name,
                pair[1].name
            );
        }
    }

    #[test]
    fn every_name_round_trips() {
        for k in ALL {
            assert_eq!(by_name(k.name), Some(k.code), "name {} does not round-trip", k.name);
        }
    }
}
