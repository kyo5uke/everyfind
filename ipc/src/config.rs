//! Client-side persistent excludes (M6 step 2).
//!
//! `%APPDATA%\everyfind\excludes.txt`: one path fragment per line, `#` comments,
//! blank lines ignored. **No default excludes**: Everyfind shows everything unless
//! the user opts in (hiding results by default would betray the "the index is the
//! whole volume" promise).
//!
//! The excludes are applied by the CLIENT: they are rendered as `!path:"..."` terms
//! of the M6 query language and appended to the query string, so the daemon needs
//! no per-user configuration and the protocol stays v3. `--no-excludes` skips the
//! file for one invocation; `ef config` shows what is loaded.

use std::path::PathBuf;

/// `%APPDATA%\everyfind\excludes.txt`, or `None` when `APPDATA` is unset.
pub fn excludes_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("everyfind").join("excludes.txt"))
}

/// Parse the excludes file: one fragment per line, trimmed; blank lines and
/// `#` comments ignored. A leading UTF-8 BOM is stripped: Windows editors and
/// PowerShell 5.1 routinely write one, and it must not hide a first-line comment.
pub fn parse_excludes(content: &str) -> Vec<String> {
    content
        .strip_prefix('\u{feff}')
        .unwrap_or(content)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// Load the user's excludes; empty when the file is absent or unreadable (an
/// unreadable config must degrade to "show everything", never to an error).
pub fn load_excludes() -> Vec<String> {
    let Some(p) = excludes_path() else {
        return Vec::new();
    };
    match std::fs::read_to_string(&p) {
        Ok(s) => parse_excludes(&s),
        Err(_) => Vec::new(),
    }
}

/// Render excludes as query terms: `!path:"frag"` each. `"` is not legal in
/// Windows file names, so quote-wrapping is always safe (keeps spaces intact).
pub fn as_query_suffix(excludes: &[String]) -> String {
    excludes
        .iter()
        .map(|e| format!("!path:\"{e}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Append the exclude terms to a user query (no-op for an empty suffix).
///
/// The user's query is wrapped in a group, because appending bare text lets a trailing operator
/// reach across the join and take the excludes as its own operand. Measured on the real volume:
/// `readme |`, which is what a search box holds between typing the bar and typing the second
/// alternative, became
///
/// ```text
/// readme | !path:"go\pkg\mod" !path:"pip\cache" ...
/// ```
///
/// i.e. "named readme, **or** not in any of those directories": 3,195,581 rows against the
/// 11,509 the same query returns with `--no-excludes`. The excludes are meant to narrow the
/// answer unconditionally, and only a group makes that true whatever the user has typed so far.
pub fn compose(query: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        query.to_string()
    } else if query.trim().is_empty() {
        suffix.to_string()
    } else {
        format!("<{query}> {suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_comments_and_blanks() {
        let got = parse_excludes("# junk dirs\nnode_modules\n\n  target  \n#x\n$Recycle.Bin\n");
        assert_eq!(got, vec!["node_modules", "target", "$Recycle.Bin"]);
    }

    #[test]
    fn parse_strips_a_leading_bom_before_the_first_comment() {
        // PowerShell 5.1's `-Encoding utf8` writes a BOM; the first-line comment
        // must still be recognized (found live, 2026-07-26).
        let got = parse_excludes("\u{feff}# comment\nWinSxS\n");
        assert_eq!(got, vec!["WinSxS"]);
    }

    #[test]
    fn suffix_quotes_every_fragment() {
        let ex = vec![
            "node_modules".to_string(),
            "Program Files\\Temp".to_string(),
        ];
        assert_eq!(
            as_query_suffix(&ex),
            "!path:\"node_modules\" !path:\"Program Files\\Temp\""
        );
    }

    #[test]
    fn compose_handles_empty_sides() {
        assert_eq!(compose("kernel32", ""), "kernel32");
        assert_eq!(compose("", "!path:\"x\""), "!path:\"x\"");
        // Grouped, so a trailing operator in the user's half cannot take the excludes as its
        // operand: `a b |` meant "a and b, **or** not excluded", which is nearly everything.
        assert_eq!(compose("a b", "!path:\"x\""), "<a b> !path:\"x\"");
        assert_eq!(compose("a |", "!path:\"x\""), "<a |> !path:\"x\"");
    }
}
