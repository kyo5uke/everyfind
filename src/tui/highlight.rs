//! Client-side highlight span computation for the TUI.
//!
//! The daemon returns paths without match spans: the fold/original offset divergence noted in
//! Server-side spans are unreliable. Instead the client re-derives spans on the few
//! displayed rows. To stay byte-correct under a case fold that **changes length** (`İ`->`i̇`,
//! `ẞ`->`ß`), we fold the filename **per char through the shared [`crate::index::fold`]**: the
//! exact same fold the index's fold arena is built from, while recording, for each folded byte,
//! the **original** byte offset it came from. The needle is [`fold::fold_query`], the same needle
//! `Index::search` uses, so a highlighted span is always a real engine match
//! (`tests/search.rs::query_is_a_single_literal_substring_not_and_of_tokens` pins that the engine
//! is a single literal substring, no AND/tokenization).
//!
//! **Reusing `index::fold` is the single-source invariant**: highlight and the engine
//! cannot drift because they call the same fold. If M6 changes the fold, this follows for free.
//!
//! Only the **filename component** is highlighted (decision #2): the engine matched the name, not
//! directory segments, so a query that only appears in a parent directory is not highlighted.

use crate::index::fold;

/// A byte range `[start, end)` within a display string to highlight. Offsets are into the whole
/// `path` string and lie on its char boundaries.
pub type Span = (usize, usize);

/// Compute highlight spans (in `path`'s own byte offsets) for case-insensitive occurrences of
/// `query` within the **filename component** of `path` (the text after the last `\` or `/`).
///
/// Returns non-overlapping, ascending spans. An empty query, or no match, yields an empty vec.
/// The fold mirrors the index exactly (per-char `char::to_lowercase`), so highlighting can never
/// diverge from what the engine matched.
pub fn filename_spans(path: &str, query: &str) -> Vec<Span> {
    let needle = fold::fold_query(query);
    if needle.is_empty() {
        return Vec::new();
    }

    // Filename component = the text after the last path separator (`\` or `/`, both ASCII).
    let fn_start = path.rfind(['\\', '/']).map(|i| i + 1).unwrap_or(0);
    let filename = &path[fn_start..];

    // Per-char fold into `folded` (through the SHARED `index::fold`, so it is byte-identical to
    // the fold arena), plus two `folded_byte -> filename_byte` maps: one to the START of the
    // source char, one to its END.
    //
    // Both are needed because a fold can change length. With only the start map, a match whose
    // end landed inside one char's expansion mapped back to that char's start: the same offset
    // as its own beginning, so the span was empty and nothing highlighted. `İ` folds to two
    // chars, so searching `i` in `İstanbul.txt` matched (the engine agrees) and drew no
    // highlight at all; `ai` in `aİ.txt` highlighted the `a` alone.
    let mut folded = String::with_capacity(filename.len());
    let mut starts: Vec<usize> = Vec::with_capacity(filename.len() + 1);
    let mut ends: Vec<usize> = Vec::with_capacity(filename.len() + 1);
    for (byte_off, ch) in filename.char_indices() {
        let before = folded.len();
        fold::fold_char_into(ch, &mut folded);
        for _ in 0..(folded.len() - before) {
            starts.push(byte_off);
            ends.push(byte_off + ch.len_utf8());
        }
    }
    starts.push(filename.len());
    ends.push(filename.len());

    // Enumerate non-overlapping occurrences of the needle; map each back to original filename
    // offsets, then shift into `path` offsets. The span starts where the first matched char
    // starts and ends where the last matched char ends, so a match that covers part of an
    // expansion still highlights the whole source character.
    let mut spans = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = folded[from..].find(&needle) {
        let fs = from + rel;
        let fe = fs + needle.len();
        let end = if fe > fs { ends[fe - 1] } else { starts[fs] };
        spans.push((fn_start + starts[fs], fn_start + end));
        from = fe; // non-overlapping
    }
    spans
}

/// Spans for a full M6 query string: the union of each positive word's occurrences in the
/// filename ([`filename_spans`] per word, overlaps merged). Operator terms (`ext:`, `path:`,
/// `!not`) never highlight: they matched metadata or ancestors, not name bytes on screen.
/// The word split is [`crate::index::query::parse`], the same parser the daemon uses, so the
/// highlighted words are exactly the words the engine AND-ed.
pub fn query_spans(path: &str, q: &str) -> Vec<Span> {
    let parsed = crate::index::query::parse(q);
    let mut all: Vec<Span> = Vec::new();
    for w in &parsed.words {
        all.extend(filename_spans(path, w));
    }
    if parsed.words.len() > 1 {
        all.sort_unstable();
        all = merge(all);
    }
    all
}

/// Merge sorted, possibly overlapping spans into sorted non-overlapping ones (the render
/// contract of [`filename_spans`]).
fn merge(spans: Vec<Span>) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for (s, e) in spans {
        if let Some(last) = out.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        out.push((s, e));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The substrings the spans point at, for offset-independent assertions.
    fn hits<'a>(path: &'a str, query: &str) -> Vec<&'a str> {
        filename_spans(path, query)
            .into_iter()
            .map(|(s, e)| &path[s..e])
            .collect()
    }

    #[test]
    fn ascii_match_in_filename() {
        let path = r"C:\Windows\System32\kernel32.dll";
        let spans = filename_spans(path, "kernel32");
        let fn_start = path.rfind('\\').unwrap() + 1;
        assert_eq!(spans, vec![(fn_start, fn_start + "kernel32".len())]);
        assert_eq!(&path[spans[0].0..spans[0].1], "kernel32");
    }

    #[test]
    fn case_insensitive() {
        let path = r"C:\x\kernel32.dll";
        assert_eq!(hits(path, "KERNEL32"), vec!["kernel32"]);
        assert_eq!(hits(path, "Kernel32"), vec!["kernel32"]);
    }

    #[test]
    fn directory_only_match_is_not_highlighted() {
        // "system32" appears only in the directory part, never the filename -> no span.
        let path = r"C:\Windows\System32\readme.txt";
        assert!(filename_spans(path, "system32").is_empty());
        assert!(filename_spans(path, "windows").is_empty());
    }

    #[test]
    fn length_changing_fold_maps_to_original_bytes() {
        // The engine (and this module) fold per-char via `char::to_lowercase`, which for a few
        // chars CHANGES byte length; the map must still point at the original bytes.
        // 'İ' (U+0130, 2 bytes) lowercases to "i\u{0307}" (3 bytes): a GROWING fold.
        let grow = "C:\\x\\İ.txt";
        assert_eq!("İ".to_lowercase(), "i\u{0307}"); // premise
        assert_eq!(hits(grow, "İ"), vec!["İ"]);
        // 'ẞ' (U+1E9E, 3 bytes) lowercases to "ß" (2 bytes): a SHRINKING fold. "STRAẞE" folds
        // to "straße"; a query "ß" must highlight the original ẞ, and "straße" the whole run.
        let shrink = "C:\\x\\STRAẞE.txt";
        assert_eq!("ẞ".to_lowercase(), "ß"); // premise
        assert_eq!(hits(shrink, "ß"), vec!["ẞ"]);
        assert_eq!(hits(shrink, "straße"), vec!["STRAẞE"]);
    }

    /// A match that covers only *part* of one character's expansion still has to highlight that
    /// whole character. `İ` folds to `i` + a combining dot, so a search for `i` matched (the
    /// engine agrees, the row is in the results) and drew a zero-width span: nothing
    /// highlighted at all, on a row that is there because it matched.
    #[test]
    fn a_match_inside_an_expansion_still_highlights_its_character() {
        let path = "C:\\x\\İstanbul.txt";
        assert_eq!(
            hits(path, "i"),
            vec!["İ"],
            "the source character, not an empty span"
        );

        // Partial overlap at the other end: `ai` covers `a` plus the first half of `İ`'s
        // expansion, and must cover both source characters.
        assert_eq!(hits("C:\\x\\aİ.txt", "ai"), vec!["aİ"]);

        // The whole expansion still behaves.
        assert_eq!(hits(path, "i\u{0307}stanbul"), vec!["İstanbul"]);
    }

    #[test]
    fn emoji_and_fullwidth_map_by_byte_not_column() {
        let path = "C:\\x\\🔍検索find.txt";
        assert_eq!(hits(path, "find"), vec!["find"]);
        assert_eq!(hits(path, "検索"), vec!["検索"]);
        // The emoji itself (folds to itself) is matchable.
        assert_eq!(hits(path, "🔍"), vec!["🔍"]);
    }

    #[test]
    fn multiple_non_overlapping_occurrences() {
        let path = r"C:\x\aba_aba.txt";
        let spans = filename_spans(path, "aba");
        assert_eq!(spans.len(), 2);
        assert_eq!(hits(path, "aba"), vec!["aba", "aba"]);
    }

    #[test]
    fn empty_query_and_no_match_yield_nothing() {
        let path = r"C:\x\kernel32.dll";
        assert!(filename_spans(path, "").is_empty());
        assert!(filename_spans(path, "no-such").is_empty());
    }

    #[test]
    fn query_with_space_is_literal_matching_engine_semantics() {
        // The PRIMITIVE stays a single literal substring (it mirrors `Index::search`): a space
        // makes "kernel dll" a literal that a spaceless name cannot contain -> no highlight.
        // The M6 word-AND behavior lives in `query_spans` below.
        let path = r"C:\x\kernel32.dll";
        assert!(filename_spans(path, "kernel dll").is_empty());
    }

    #[test]
    fn no_separator_path_is_all_filename() {
        assert_eq!(hits("kernel32.dll", "kernel"), vec!["kernel"]);
    }

    fn qhits<'a>(path: &'a str, query: &str) -> Vec<&'a str> {
        query_spans(path, query)
            .into_iter()
            .map(|(s, e)| &path[s..e])
            .collect()
    }

    #[test]
    fn query_spans_highlights_each_word() {
        let path = r"C:\x\kernel32.dll";
        assert_eq!(qhits(path, "kernel dll"), vec!["kernel", "dll"]);
        // Single word == the primitive.
        assert_eq!(qhits(path, "kernel32"), vec!["kernel32"]);
    }

    #[test]
    fn query_spans_merges_overlapping_words() {
        let path = r"C:\x\kernel32.dll";
        // "kern" (0..4) and "ernel" (1..6) overlap -> one merged "kernel" span.
        assert_eq!(qhits(path, "kern ernel"), vec!["kernel"]);
    }

    #[test]
    fn query_spans_ignores_operator_terms() {
        let path = r"C:\x\kernel32.dll";
        assert_eq!(qhits(path, "dll ext:dll"), vec!["dll"]);
        assert_eq!(qhits(path, "dll !readme path:x"), vec!["dll"]);
        // Operator-only query: nothing to highlight.
        assert!(query_spans(path, "ext:dll").is_empty());
    }

    #[test]
    fn query_spans_quoted_word_keeps_spaces() {
        let path = r"C:\x\annual report.pdf";
        assert_eq!(qhits(path, "\"annual rep\""), vec!["annual rep"]);
    }
}
