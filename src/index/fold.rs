//! The canonical case-fold: the **single source of truth** for how Everyfind lowercases text
//! for case-insensitive search. Centralized so the index and the client-side highlight can never
//! drift apart.
//!
//! Callers:
//! - **corpus fold** ([`fold_char_into`] / [`fold_corpus`]): the file names. Used by the fold
//!   arena build (`index::build`) and by the highlight haystack (`tui::highlight`).
//! - **query fold** ([`fold_query`]): the search needle. Used by `Index::search` /
//!   `Index::search_contig` and by the highlight needle.
//!
//! **INVARIANT:** highlight correctness depends on
//! `highlight's fold == the index's fold`. If the fold ever changes, e.g. the deferred M6
//! "ASCII on-the-fly lowercasing" optimization that would eliminate the fold arena, every caller
//! here changes in lockstep, which is the whole point of centralizing it. **The M6 fold-optimization
//! gate is bound to "update `tui::highlight` to match".**
//!
//! The corpus rule is **per-char `char::to_lowercase`** and the query rule is **whole-string
//! `str::to_lowercase`**: the two historical rules, preserved verbatim (they differ only for the
//! Greek final sigma, a pre-existing edge; M4 does not change search semantics).

/// Append the case-fold of a single `char` to `out`: the corpus rule, applied per file-name
/// char. The index folds names char-by-char (during UTF-16 decode) through this function, and the
/// highlight builds its byte-offset map by folding through it too, so the two are byte-identical.
#[inline]
pub fn fold_char_into(c: char, out: &mut String) {
    for lc in c.to_lowercase() {
        out.push(lc);
    }
}

/// Fold a whole `&str` with the corpus rule (per-char). For callers that hold a string rather
/// than a UTF-16 stream (the highlight haystack).
pub fn fold_corpus(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        fold_char_into(c, &mut out);
    }
    out
}

/// Fold a query into the search needle (whole-string `str::to_lowercase`). Single-sourced here so
/// the search needle and the highlight needle are provably the same rule.
pub fn fold_query(query: &str) -> String {
    query.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_fold_is_per_char_lowercase() {
        assert_eq!(fold_corpus("KERNEL32.DLL"), "kernel32.dll");
        assert_eq!(fold_corpus("ReAdMe.Md"), "readme.md");
        // Length-changing folds (documented in tui::highlight): İ grows, ẞ shrinks.
        assert_eq!(fold_corpus("İ"), "i\u{0307}");
        assert_eq!(fold_corpus("STRAẞE"), "straße");
    }

    #[test]
    fn fold_char_into_matches_fold_corpus() {
        // The two entry points must agree char-for-char (the index uses fold_char_into; highlight
        // uses fold_char_into to build its map; this pins they equal the whole-string helper).
        for s in ["Aa", "検索🔍", "İ.txt", "STRAẞE.txt", ""] {
            let mut acc = String::new();
            for c in s.chars() {
                fold_char_into(c, &mut acc);
            }
            assert_eq!(acc, fold_corpus(s), "disagreement folding {s:?}");
        }
    }

    #[test]
    fn query_fold_is_whole_string_lowercase() {
        assert_eq!(fold_query("KERNEL32"), "kernel32");
        assert_eq!(fold_query("ReAdMe"), "readme");
    }
}
