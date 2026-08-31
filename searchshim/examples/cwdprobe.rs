//! Appends its own working directory to %TEMP%\cwdprobe.txt and exits.
//!
//! Run once from a normal folder and once from a search result: if the two answers differ,
//! programs started from the results are being given the wrong place to run in, which is why
//! anything that loads a file from beside itself fails there and works from its folder.
use std::io::Write;

fn main() {
    let line = format!(
        "cwd = {}
",
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|e| format!("<{e}>")),
    );
    let out = std::env::temp_dir().join("cwdprobe.txt");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out)
    {
        let _ = f.write_all(line.as_bytes());
    }
}
