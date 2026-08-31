//! Everyfind M4 probe: what does the machine actually deliver for astral emoji / CJK / IME?
//!
//! The TUI's `AppState` is proven correct for a **single** `KeyCode::Char` per glyph
//! (`tui::app` tests, incl. 🔍 / 𝕏). The reported "emoji input breaks" is therefore in the INPUT
//! layer. First finding (default mode below): a `🔍` (astral, U+1F50D) commit produces **ZERO**
//! crossterm events, while Japanese (BMP, via IME) arrives normally, so an astral surrogate pair
//! is being dropped somewhere at/under crossterm's event layer.
//!
//! Two modes:
//! - **default**: dump every `crossterm::event::read()` Event (Key/Paste/all variants).
//! - **`--raw`**: bypass crossterm and dump every raw `ReadConsoleInputW` INPUT_RECORD (KEY_EVENT
//!   down/up, virtual-key, and the u16 `UnicodeChar` **including lone surrogates 0xD800-0xDFFF**),
//!   to see exactly what the Windows console hands us for `🔍` (both halves? one? none? another
//!   event type?).
//!
//! Run each in a real terminal (Windows Terminal, then conhost); type/panel-insert 🔍, 日本語, 𝕏,
//! use an IME, and paste. Esc quits.
//!
//! ```text
//! cargo run --release --example probe_key_input
//! cargo run --release --example probe_key_input -- --raw
//! ```

use std::io::{stdout, Write};

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

fn main() {
    if std::env::args().any(|a| a == "--raw") {
        raw_console_dump();
    } else {
        crossterm_dump();
    }
}

/// Dump every crossterm event (the app's actual input path).
fn crossterm_dump() {
    enable_raw_mode().expect("enable raw mode");
    let _ = execute!(stdout(), EnableBracketedPaste);
    let mut out = stdout();
    let _ = write!(
        out,
        "[crossterm] type 🔍 日本語 𝕏, use an IME, and paste. Esc / Ctrl+C to quit.\r\n\r\n"
    );
    let _ = out.flush();

    loop {
        let ev = match event::read() {
            Ok(e) => e,
            Err(e) => {
                let _ = write!(out, "read error: {e}\r\n");
                break;
            }
        };
        match ev {
            Event::Key(k) => {
                if k.code == KeyCode::Esc
                    || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL))
                {
                    break;
                }
                let detail = match k.code {
                    KeyCode::Char(c) => format!("Char '{c}' U+{:04X}", c as u32),
                    other => format!("{other:?}"),
                };
                let _ = write!(
                    out,
                    "Key   {detail:<22}  kind={:?}  mods={:?}\r\n",
                    k.kind, k.modifiers
                );
            }
            Event::Paste(s) => {
                let cps: Vec<String> = s.chars().map(|c| format!("U+{:04X}", c as u32)).collect();
                let _ = write!(out, "Paste {s:?}  [{}]\r\n", cps.join(" "));
            }
            other => {
                let _ = write!(out, "{other:?}\r\n");
            }
        }
        let _ = out.flush();
    }

    let _ = execute!(stdout(), DisableBracketedPaste);
    disable_raw_mode().expect("disable raw mode");
    println!("\r\ndone (crossterm).");
}

/// Dump raw console INPUT_RECORDs, bypassing crossterm's parse/surrogate handling entirely.
fn raw_console_dump() {
    use windows_sys::Win32::System::Console::{
        GetStdHandle, ReadConsoleInputW, INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE,
    };

    enable_raw_mode().expect("enable raw mode");
    // SAFETY: STD_INPUT_HANDLE is a valid standard-handle id.
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut out = stdout();
    let _ = write!(
        out,
        "[raw ReadConsoleInputW] type/panel 🔍, 日本語, 𝕏; a surrogate half is 0xD800-0xDFFF. Esc to quit.\r\n\r\n"
    );
    let _ = out.flush();

    // SAFETY: a zeroed INPUT_RECORD array is a valid (empty) buffer for ReadConsoleInputW to fill.
    let mut buf: [INPUT_RECORD; 32] = unsafe { std::mem::zeroed() };
    'outer: loop {
        let mut read = 0u32;
        // SAFETY: valid handle, buffer, and count; `read` receives the number of records filled.
        let ok =
            unsafe { ReadConsoleInputW(handle, buf.as_mut_ptr(), buf.len() as u32, &mut read) };
        if ok == 0 {
            let _ = write!(out, "ReadConsoleInputW failed\r\n");
            break;
        }
        for rec in &buf[..read as usize] {
            if rec.EventType == KEY_EVENT as u16 {
                // SAFETY: EventType == KEY_EVENT, so the KeyEvent union arm is the active one, and
                // `uChar.UnicodeChar` is the u16 code unit (possibly a lone surrogate).
                let k = unsafe { rec.Event.KeyEvent };
                let uc = unsafe { k.uChar.UnicodeChar }; // u16, may be a lone surrogate
                let kind = if k.bKeyDown != 0 { "down" } else { "up  " };
                let _ = write!(
                    out,
                    "KEY {kind}  vk={:#06x}  uChar=U+{uc:04X}  repeat={}  ctrl={:#06x}\r\n",
                    k.wVirtualKeyCode, k.wRepeatCount, k.dwControlKeyState,
                );
                // Esc (VK_ESCAPE = 0x1B) on key-down quits.
                if k.wVirtualKeyCode == 0x1B && k.bKeyDown != 0 {
                    break 'outer;
                }
            } else {
                let _ = write!(out, "EVENT type={}\r\n", rec.EventType);
            }
        }
        let _ = out.flush();
    }

    disable_raw_mode().expect("disable raw mode");
    println!("\r\ndone (raw). For 🔍 (U+1F50D): do you see BOTH a U+D83D and a U+DD0D record,");
    println!("only one, or none? down and up? That determines whether a fix is feasible.");
}
