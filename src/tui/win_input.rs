//! **[M6a]** Windows-native TUI input: the fix for astral (emoji) input (crossterm #561).
//!
//! In M4 the TUI read input via `crossterm::event::read`, which drops astral (non-BMP) characters:
//! [crossterm #561](https://github.com/crossterm-rs/crossterm/issues/561): on Windows an astral
//! char arrives as a UTF-16 surrogate **pair** in two separate KEY records, and crossterm buffers
//! the high surrogate but clears that buffer when the intervening **key-up** surrogate record
//! arrives before the low key-down, so the character produces zero events (`examples/probe_key_input
//! --raw`: `🔍` = U+1F50D comes as `U+D83D` then `U+DD0D`; the console delivers it, crossterm loses it).
//!
//! This module is a Windows-only reader ([`read_loop`]) that calls `ReadConsoleInputW` directly,
//! **ignores key-up records** (so the high surrogate survives), and **combines the surrogate pair
//! itself** ([`translate`], the pure testable core) before mapping the TUI's bounded key set to a
//! `crossterm::event::Event` fed into the existing input channel. Bracketed paste is **disabled**
//! in this mode (a raw reader would mis-parse its ESC sequences; pasted text arrives as ordinary
//! key records, astral included). Selected via `EF_INPUT` (`win` default; `crossterm` = the M4
//! path) so a regression is one env var from the known-good reader; see `input_backend_is_win` in
//! `tui::mod`.

use std::sync::mpsc::Sender;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use super::Msg;

// Virtual-key codes for the bounded key set the TUI needs.
const VK_BACK: u16 = 0x08;
const VK_TAB: u16 = 0x09;
const VK_RETURN: u16 = 0x0D;
const VK_ESCAPE: u16 = 0x1B;
const VK_PRIOR: u16 = 0x21; // PageUp
const VK_NEXT: u16 = 0x22; // PageDown
const VK_END: u16 = 0x23;
const VK_HOME: u16 = 0x24;
const VK_LEFT: u16 = 0x25;
const VK_UP: u16 = 0x26;
const VK_RIGHT: u16 = 0x27;
const VK_DOWN: u16 = 0x28;
const VK_DELETE: u16 = 0x2E;

// `dwControlKeyState` bits the TUI acts on. Alt is not a TUI chord, but it has to be *read*:
// see `ctrl` below.
const RIGHT_ALT_PRESSED: u32 = 0x0001;
const LEFT_ALT_PRESSED: u32 = 0x0002;
const RIGHT_CTRL_PRESSED: u32 = 0x0004;
const LEFT_CTRL_PRESSED: u32 = 0x0008;
const SHIFT_PRESSED: u32 = 0x0010;

/// Translate one raw Windows console KEY record into a [`KeyEvent`], combining a UTF-16 surrogate
/// pair carried across two **key-down** records via `surrogate` (the crossterm #561 fix).
///
/// - `down == false` (key-up) -> `None`, **leaving the surrogate buffer intact** (crossterm instead
///   lets an intervening key-up surrogate record clear it, which is the bug).
/// - high surrogate (`0xD800..=0xDBFF`) key-down -> buffer it, `None`.
/// - low surrogate (`0xDC00..=0xDFFF`) key-down -> combine with the buffered high into one astral
///   `KeyCode::Char`; a lone low (no buffered high) -> `None`.
/// - otherwise, clear any dangling high surrogate, then map `vk` (Enter/Esc/Backspace/Delete/Tab/
///   arrows/Home/End/PgUp/PgDn), or Ctrl+letter (`Char`+`CONTROL`), or a printable BMP `u_char`
///   (includes IME-composed CJK) -> `Char`; unmapped -> `None`.
pub fn translate(
    down: bool,
    vk: u16,
    u_char: u16,
    ctrl_state: u32,
    surrogate: &mut Option<u16>,
) -> Option<KeyEvent> {
    if !down {
        // Ignore key-up. Crucially, do NOT touch the surrogate buffer: the key-up surrogate
        // record between the two key-downs is exactly what crossterm mishandles.
        return None;
    }

    // --- surrogate combining (astral chars arrive as two separate key-down records) ---
    if (0xD800..=0xDBFF).contains(&u_char) {
        *surrogate = Some(u_char);
        return None;
    }
    if (0xDC00..=0xDFFF).contains(&u_char) {
        let high = surrogate.take()?; // lone low surrogate -> dropped
        let ch = char::decode_utf16([high, u_char]).next()?.ok()?;
        return Some(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    // A non-surrogate record: any buffered high surrogate is stale.
    *surrogate = None;

    // Ctrl, but not AltGr. Windows synthesises AltGr as Ctrl+Alt, so a plain Ctrl test is true
    // for every AltGr chord, and the Ctrl+letter branch below then throws away the character
    // the layout had already composed. That made every AltGr *letter* untypeable: `ą ć ę ł ń ó
    // ś ź ż` on Polish (Programmers), `@` and `µ` on German, `€` on several. Non-letter AltGr
    // keys survived because their VK falls outside the letter range, which is what made the
    // failure look arbitrary. There is no Ctrl+Alt chord in this TUI to lose by excluding it.
    let alt = ctrl_state & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0;
    let ctrl = ctrl_state & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0 && !alt;
    let shift = ctrl_state & SHIFT_PRESSED != 0;

    // Special keys by virtual-key code (independent of the accompanying uChar).
    let special = match vk {
        VK_RETURN => Some(KeyCode::Enter),
        VK_ESCAPE => Some(KeyCode::Esc),
        VK_BACK => Some(KeyCode::Backspace),
        VK_DELETE => Some(KeyCode::Delete),
        VK_TAB => Some(KeyCode::Tab),
        VK_LEFT => Some(KeyCode::Left),
        VK_RIGHT => Some(KeyCode::Right),
        VK_UP => Some(KeyCode::Up),
        VK_DOWN => Some(KeyCode::Down),
        VK_HOME => Some(KeyCode::Home),
        VK_END => Some(KeyCode::End),
        VK_PRIOR => Some(KeyCode::PageUp),
        VK_NEXT => Some(KeyCode::PageDown),
        _ => None,
    };
    if let Some(code) = special {
        let mut m = KeyModifiers::NONE;
        if ctrl {
            m |= KeyModifiers::CONTROL;
        }
        if shift {
            m |= KeyModifiers::SHIFT;
        }
        return Some(KeyEvent::new(code, m));
    }

    // Ctrl + letter (Ctrl+Y / Ctrl+E / Ctrl+O / Ctrl+C ...): the console delivers uChar as a control
    // code, so map via the letter vk. crossterm reports the lowercase letter + CONTROL; match it.
    if ctrl && (0x41..=0x5A).contains(&vk) {
        let ch = (b'a' + (vk as u8 - 0x41)) as char;
        return Some(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL));
    }

    // A printable BMP character (uChar already reflects shift/caps; includes IME-composed CJK).
    // Control codes (< 0x20) without a mapped vk are ignored.
    if u_char >= 0x20 {
        if let Some(ch) = char::from_u32(u_char as u32) {
            return Some(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
    }
    None
}

/// Send `event` into the loop's channel; returns `false` when the receiver is gone (stop).
fn forward(tx: &Sender<Msg>, event: Event) -> bool {
    tx.send(Msg::Input(event)).is_ok()
}

// `read_loop` (the raw `ReadConsoleInputW` reader that drives `translate`) is defined in the
// platform section below and wired via `EF_INPUT` in `tui::mod`.
pub use imp::read_loop;

#[cfg(windows)]
mod imp {
    use std::sync::mpsc::Sender;

    use crossterm::event::Event;
    use windows_sys::Win32::System::Console::{
        GetStdHandle, ReadConsoleInputW, INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE,
        WINDOW_BUFFER_SIZE_EVENT,
    };

    use super::{forward, translate};
    use crate::tui::Msg;

    /// Read console input forever, translating KEY records (key-up ignored, surrogate pairs
    /// combined) and buffer-resize records into `crossterm::event::Event`s on `tx`. Returns when
    /// the channel closes or the read fails. This is the `EF_INPUT=win` reader (the fix); the
    /// `crossterm` path (`event::read`) remains the rollback (`tui::mod`).
    pub fn read_loop(tx: Sender<Msg>) {
        // SAFETY: STD_INPUT_HANDLE is a valid predefined handle id.
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if handle.is_null() || handle as isize == -1 {
            return;
        }
        let mut surrogate: Option<u16> = None;
        // A modest batch; ReadConsoleInputW blocks until at least one record is available.
        let mut records: [INPUT_RECORD; 32] = unsafe { std::mem::zeroed() };
        loop {
            let mut read: u32 = 0;
            // SAFETY: valid console input handle; `records` is a valid, sized INPUT_RECORD buffer.
            let ok = unsafe {
                ReadConsoleInputW(
                    handle,
                    records.as_mut_ptr(),
                    records.len() as u32,
                    &mut read,
                )
            };
            if ok == 0 || read == 0 {
                return; // console closed / error -> stop the reader (loop falls back to timeout)
            }
            for rec in &records[..read as usize] {
                match rec.EventType {
                    x if x == KEY_EVENT as u16 => {
                        // SAFETY: EventType == KEY_EVENT selects the KeyEvent union arm.
                        let k = unsafe { rec.Event.KeyEvent };
                        // SAFETY: uChar union is UnicodeChar (u16) for the wide console input.
                        let u_char = unsafe { k.uChar.UnicodeChar };
                        if let Some(ev) = translate(
                            k.bKeyDown != 0,
                            k.wVirtualKeyCode,
                            u_char,
                            k.dwControlKeyState,
                            &mut surrogate,
                        ) {
                            if !forward(&tx, Event::Key(ev)) {
                                return;
                            }
                        }
                    }
                    x if x == WINDOW_BUFFER_SIZE_EVENT as u16 => {
                        // SAFETY: EventType selects the WindowBufferSize union arm.
                        let sz = unsafe { rec.Event.WindowBufferSizeEvent.dwSize };
                        if !forward(&tx, Event::Resize(sz.X.max(0) as u16, sz.Y.max(0) as u16)) {
                            return;
                        }
                    }
                    _ => {} // mouse / focus / menu: ignored, like the crossterm TUI path
                }
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use crate::tui::Msg;
    use std::sync::mpsc::Sender;
    /// Non-Windows stub (the crate is Windows-only; this keeps `cargo check` honest elsewhere).
    pub fn read_loop(_tx: Sender<Msg>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn ch(ev: Option<KeyEvent>) -> KeyEvent {
        ev.expect("expected a key event")
    }

    /// The real `probe_key_input --raw` data: a high surrogate (`U+D83E`) then a low surrogate
    /// (`U+DDCE`) arriving as two separate **key-down** records must combine into **one** astral
    /// char (`U+1F9CE`), and a key-up record in between must NOT drop it (the crossterm #561 bug).
    #[test]
    fn surrogate_pair_across_two_key_downs_combines_into_one_astral_char() {
        let mut sur = None;
        assert_eq!(translate(true, 0, 0xD83E, 0, &mut sur), None); // high -> buffered
                                                                   // A key-up in between must be ignored WITHOUT clearing the buffer (the fix vs crossterm).
        assert_eq!(translate(false, 0, 0xD83E, 0, &mut sur), None);
        assert_eq!(
            sur,
            Some(0xD83E),
            "key-up must not clear the surrogate buffer"
        );
        let ev = ch(translate(true, 0, 0xDDCE, 0, &mut sur)); // low -> combine
        assert_eq!(ev.code, KeyCode::Char('\u{1F9CE}'));
        assert_eq!(ev.modifiers, KeyModifiers::NONE);
        assert_eq!(sur, None, "buffer consumed after combining");
    }

    /// The `🔍` (U+1F50D) case from the module docs: `U+D83D` then `U+DD0D` -> one char.
    #[test]
    fn magnifier_emoji_surrogate_pair_combines() {
        let mut sur = None;
        assert_eq!(translate(true, 0, 0xD83D, 0, &mut sur), None);
        let ev = ch(translate(true, 0, 0xDD0D, 0, &mut sur));
        assert_eq!(ev.code, KeyCode::Char('🔍'));
    }

    #[test]
    fn lone_low_surrogate_is_dropped() {
        let mut sur = None;
        assert_eq!(translate(true, 0, 0xDD0D, 0, &mut sur), None);
    }

    #[test]
    fn special_keys_map_by_vk() {
        let c = |vk| ch(translate(true, vk, 0, 0, &mut None)).code;
        assert_eq!(c(VK_RETURN), KeyCode::Enter);
        assert_eq!(c(VK_ESCAPE), KeyCode::Esc);
        assert_eq!(c(VK_BACK), KeyCode::Backspace);
        assert_eq!(c(VK_DELETE), KeyCode::Delete);
        assert_eq!(c(VK_LEFT), KeyCode::Left);
        assert_eq!(c(VK_RIGHT), KeyCode::Right);
        assert_eq!(c(VK_UP), KeyCode::Up);
        assert_eq!(c(VK_DOWN), KeyCode::Down);
        assert_eq!(c(VK_HOME), KeyCode::Home);
        assert_eq!(c(VK_END), KeyCode::End);
        assert_eq!(c(VK_PRIOR), KeyCode::PageUp);
        assert_eq!(c(VK_NEXT), KeyCode::PageDown);
    }

    #[test]
    fn ctrl_letter_maps_to_lowercase_char_with_control() {
        // Ctrl+Y (vk 0x59) arrives with uChar = 0x19 (control code) + LEFT_CTRL_PRESSED.
        let ev = ch(translate(true, 0x59, 0x19, LEFT_CTRL_PRESSED, &mut None));
        assert_eq!(ev.code, KeyCode::Char('y'));
        assert_eq!(ev.modifiers, KeyModifiers::CONTROL);
        // Ctrl+C (quit) similarly.
        let ev = ch(translate(true, 0x43, 0x03, LEFT_CTRL_PRESSED, &mut None));
        assert_eq!(ev.code, KeyCode::Char('c'));
        assert_eq!(ev.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn printable_ascii_and_cjk_pass_through() {
        // 'a' (vk 0x41, uChar 0x61).
        let ev = ch(translate(true, 0x41, 0x61, 0, &mut None));
        assert_eq!(ev.code, KeyCode::Char('a'));
        assert_eq!(ev.modifiers, KeyModifiers::NONE);
        // Shift+A -> uChar 'A'.
        assert_eq!(
            ch(translate(true, 0x41, 0x41, SHIFT_PRESSED, &mut None)).code,
            KeyCode::Char('A')
        );
        // IME-composed CJK 'あ' (U+3042): single BMP unit, passes straight through.
        assert_eq!(
            ch(translate(true, 0, 0x3042, 0, &mut None)).code,
            KeyCode::Char('あ')
        );
    }

    #[test]
    fn key_up_produces_nothing() {
        assert_eq!(translate(false, 0x41, 0x61, 0, &mut None), None);
    }

    /// AltGr is Ctrl+Alt on Windows, so the Ctrl+letter branch used to claim it and drop the
    /// character the layout had already composed. Polish (Programmers) AltGr+A produces
    /// U+0105 with `LEFT_CTRL | RIGHT_ALT`; it has to arrive as that character, unmodified.
    #[test]
    fn altgr_produces_its_character_rather_than_a_ctrl_chord() {
        let ev = ch(translate(
            true,
            0x41,
            0x0105,
            LEFT_CTRL_PRESSED | RIGHT_ALT_PRESSED,
            &mut None,
        ));
        assert_eq!(ev.code, KeyCode::Char('ą'));
        assert_eq!(ev.modifiers, KeyModifiers::NONE);
    }

    /// And a real Ctrl chord still is one: Ctrl+Y delivers uChar 0x19, which only the vk
    /// branch can name.
    #[test]
    fn plain_ctrl_letter_is_still_a_chord() {
        let ev = ch(translate(true, 0x59, 0x0019, LEFT_CTRL_PRESSED, &mut None));
        assert_eq!(ev.code, KeyCode::Char('y'));
        assert_eq!(ev.modifiers, KeyModifiers::CONTROL);
    }
}
