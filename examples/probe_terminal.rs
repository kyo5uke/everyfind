//! Everyfind M4 probe P17 (headless part): terminal identity + ratatui display-width model.
//!
//! P17 has two halves. The **visual** half (does an emoji actually render two cells wide in
//! Windows Terminal vs legacy conhost, alternate-screen restore, `explorer /select` on awkward
//! paths) is inherently a *manual* check on a real interactive terminal, deferred to the T:
//! integration checklist, with conhost documented "best effort".
//!
//! This probe pins the half that **is** verifiable without a live terminal: the display-width
//! model `ratatui` (via `unicode-width`) assigns to full-width / emoji / ZWJ strings. The TUI's
//! highlight slices spans by **byte offset** and lets ratatui compute
//! width, so knowing that model (and confirming ASCII byte-len == width but CJK/emoji !=) is
//! what prevents highlight/column drift. It also reports the detected terminal.
//!
//! ```text
//! cargo run --example probe_terminal
//! ```

use ratatui::text::Line;

fn detect_terminal() -> String {
    // Windows Terminal sets WT_SESSION; VS Code sets TERM_PROGRAM=vscode; otherwise assume the
    // classic console host (conhost).
    if std::env::var_os("WT_SESSION").is_some() {
        "Windows Terminal (WT_SESSION set)".into()
    } else if let Some(tp) = std::env::var_os("TERM_PROGRAM") {
        format!("TERM_PROGRAM={}", tp.to_string_lossy())
    } else {
        "legacy conhost or unknown (no WT_SESSION / TERM_PROGRAM)".into()
    }
}

fn main() {
    println!("== P17 (headless): terminal identity + ratatui width model ==");
    println!("terminal: {}", detect_terminal());
    println!();
    println!(
        "{:<28} {:>8} {:>6} {:>6}",
        "sample", "byte_len", "chars", "width"
    );
    println!("{}", "-".repeat(52));

    // (label, string). Mix ASCII, full-width CJK, an emoji (surrogate pair), a ZWJ family
    // cluster, a length-changing case-fold source, and a combining sequence.
    let samples: &[(&str, &str)] = &[
        ("ascii", "kernel32.dll"),
        ("cjk full-width", "全角テスト"),
        ("emoji", "🔍"),
        ("emoji + cjk", "🔍検索.txt"),
        ("zwj family", "👨‍👩‍👧‍👦"),
        ("fold-changing (ß)", "straße.txt"),
        ("combining", "cafe\u{0301}.txt"),
    ];

    for (label, s) in samples {
        let width = Line::from(*s).width();
        println!(
            "{:<28} {:>8} {:>6} {:>6}",
            label,
            s.len(),
            s.chars().count(),
            width
        );
    }

    println!();
    println!("Notes carried into tui/render.rs + tui/highlight.rs:");
    println!("  * ASCII: byte_len == width (safe to reason about columns from bytes).");
    println!("  * CJK/emoji: width != byte_len and != chars -> NEVER slice spans by column;");
    println!("    slice by BYTE offset and let ratatui/unicode-width lay out the cells.");
    println!("  * ZWJ clusters: unicode-width may over-count vs. a terminal that ligates them;");
    println!("    this is the 'best effort' conhost caveat: highlight stays byte-correct.");
    println!();
    println!("Manual (real terminal, T: checklist): emoji cell width in WT vs conhost,");
    println!("alt-screen enter/exit + restore after Esc/Ctrl+C/panic, explorer /select on a");
    println!("space+comma path and a >260-char long path (lpParameters quoting).");
}
