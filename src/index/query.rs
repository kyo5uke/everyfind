//! The query language: Everything's syntax, over the MFT index.
//!
//! # Grammar
//!
//! ```text
//! or     := and ( '|' and )*        OR, lowest precedence
//! and    := unary+                  juxtaposition; whitespace means AND
//! unary  := '!' unary | primary     NOT
//! primary:= '<' or '>' | term       angle brackets group
//! ```
//!
//! A bare term matches a **substring of the file name**. A term containing `\`
//! or `/` matches the **full path** instead (as Everything does), and one
//! containing `*` or `?` is a wildcard match against the *whole* name.
//! Double quotes keep whitespace inside a term and suppress nothing else.
//!
//! # Modifiers: reshape the term that follows
//!
//! | Modifier | Meaning |
//! |---|---|
//! | `case:`, `nocase:` | force / suppress case sensitivity for this term |
//! | `path:`, `nopath:` | match the full path / the name only |
//! | `wholeword:`, `ww:` | the term must be a whole word |
//! | `wholefilename:`, `wfn:` | the whole name must equal the term |
//! | `startwith:`, `endwith:` | anchored at either end |
//! | `regex:` | the term is a regular expression over the name |
//! | `pathregex:` | ...over the full path |
//! | `wildcards:` | force `*`/`?` interpretation |
//! | `nowildcards:` | take `*`/`?` literally |
//!
//! # Functions: constraints in their own right
//!
//! | Function | Meaning |
//! |---|---|
//! | `ext:rs;toml` | extension is any of the alternatives |
//! | `file:`, `files:` | files only |
//! | `folder:`, `folders:`, `dir:` | directories only |
//! | `size:<n>` | allocated size in bytes (see below) |
//! | `len:<n>` | name length in characters |
//! | `parents:<n>` | how many directories deep |
//! | `root:` | directly under the drive root |
//! | `empty:` | an empty folder, or a zero-byte file |
//! | `dupe:`, `namepartdupe:` | the name occurs more than once on the volume |
//! | `child:<term>` | a directory containing a matching child |
//! | `childcount:<n>`, `childfilecount:<n>`, `childfoldercount:<n>` | |
//! | `attrib:<letters>` | `d` directory, `l` reparse point (see limits) |
//! | `count:<n>` | cap the number of results |
//! | `audio: video: pic: doc: exe: zip: font:` | extension groups |
//!
//! Numbers accept `123`, `>123`, `>=123`, `<123`, `<=123`, `=123` and
//! `100..200`. Sizes accept `kb mb gb tb` (1024-based) and Everything's named
//! buckets (`empty tiny small medium large huge gigantic`).
//!
//! # What this index cannot answer
//!
//! - **Dates** (`dm:` `dc:` `da:` `dr:` and their dupe forms). The MFT
//!   enumeration this index is built from returns names, parents and attributes
//!   but not timestamps. Reading those means parsing raw `$STANDARD_INFORMATION`
//!   records, which is a different piece of work, not a query-language one.
//! - **`size:` is *allocated* size**, in whole clusters, because that is what
//!   the size pass collects for `ef du`. A 1-byte file reports one cluster.
//! - **`attrib:`** knows only what the index stores: directory and reparse
//!   point. Hidden/system/read-only are not carried.
//! - **`content:`** belongs to the grix engine (`ef content`), which searches
//!   inside files; it is deliberately not part of the name index.
//!
//! Malformed shapes never error: an incremental search box must keep matching
//! while the user is mid-keystroke, so `ext:` with no value, a lone `!`, an
//! unterminated quote or a bad regex all degrade to a literal word.

use std::collections::BinaryHeap;

use memchr::memmem;
use rayon::prelude::*;

use super::{flags, fold, Index};

/// The rank class for a hit whose name matches the term somewhere other than at its start,
/// the weakest of the three, and the one [`Index::top_ranked`]'s reservation protects.
const CLASS_ELSEWHERE: u8 = 2;

// ---------------------------------------------------------------- the query

/// A parsed query: an expression tree plus the bits the caller reads directly.
#[derive(Debug, Default, Clone)]
pub struct Query {
    /// The whole condition.
    pub expr: Expr,
    /// Positive literal name substrings, in the order they appeared. Kept
    /// alongside the tree because ranking and the TUI's highlighter both want
    /// "what did the user actually type" without walking the expression.
    pub words: Vec<String>,
    /// Positive path fragments, same rationale (the Explorer scope seeding in
    /// `ef --in` asserts on these).
    pub paths: Vec<String>,
    /// `count:`, a cap the caller may apply.
    pub limit: Option<u32>,
    /// Patterns for `child:` terms, referenced by index from the tree so the
    /// per-entry "does this directory contain such a child" pass can be built
    /// once instead of per candidate.
    pub child_pats: Vec<Pat>,
}

impl Query {
    /// No constraints at all (matches everything, root excepted).
    pub fn is_empty(&self) -> bool {
        matches!(self.expr, Expr::All)
    }

    /// One plain substring and nothing else: the shape the M1 fast path in
    /// [`Index::search`] handles, and the shape the search box is in most of the
    /// time.
    fn plain_word(&self) -> Option<&str> {
        match &self.expr {
            Expr::All => Some(""),
            // No `limit` guard: `count:` only ever reaches `top_ranked`, never `search_query`,
            // so it cannot change what this fast path returns. The `Expr::All` arm above proves
            // it: `count:50` alone parses to exactly that and already took the fast path. With
            // the guard, `kernel32 count:50` left the single-pass contiguous scan for the full
            // parallel tree walk to reach the same rows, which is the shape a UI sends on every
            // keystroke.
            Expr::Term(Term::Name(p)) => match &p.kind {
                PatKind::Contains(s) if p.case.is_none() => Some(s),
                _ => None,
            },
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub enum Expr {
    /// Matches everything.
    #[default]
    All,
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Term(Term),
}

#[derive(Debug, Clone)]
pub enum Term {
    /// The name matches the pattern.
    Name(Pat),
    /// The full reconstructed path matches.
    Path(Pat),
    /// Extension is any of these (lowercase, no dot).
    Ext(Vec<String>),
    /// `true` = directories only, `false` = files only.
    Kind(bool),
    /// Directly under the drive root.
    Root,
    /// An empty directory, or a zero-byte file.
    Empty,
    /// The name occurs more than once on the volume.
    Dupe,
    /// Name length in characters.
    Len(Num),
    /// Directory depth below the root.
    Parents(Num),
    /// Allocated size in bytes.
    Size(Num),
    /// Child counts.
    Children(ChildKind, Num),
    /// This directory contains a child matching `child_pats[i]`.
    HasChild(usize),
    /// One of the [`flags`] bits, and whether it must be set.
    Attrib(u16, bool),
    /// A regular expression over the name (`false`) or the full path (`true`).
    Regex(std::sync::Arc<regex::bytes::Regex>, bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildKind {
    Any,
    Files,
    Folders,
}

/// A name/path pattern, before it is compiled against a case setting.
#[derive(Debug, Clone)]
pub struct Pat {
    pub kind: PatKind,
    /// `Some(true)` forced case-sensitive, `Some(false)` forced insensitive,
    /// `None` follows the search's own flag.
    pub case: Option<bool>,
}

#[derive(Debug, Clone)]
pub enum PatKind {
    Contains(String),
    Whole(String),
    Prefix(String),
    Suffix(String),
    /// Must appear delimited by non-word characters.
    Word(String),
    /// `*` and `?` against the whole string.
    Glob(String),
}

/// A numeric predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Num {
    Eq(u64),
    Lt(u64),
    Le(u64),
    Gt(u64),
    Ge(u64),
    Range(u64, u64),
}

impl Num {
    fn test(&self, v: u64) -> bool {
        match *self {
            Num::Eq(n) => v == n,
            Num::Lt(n) => v < n,
            Num::Le(n) => v <= n,
            Num::Gt(n) => v > n,
            Num::Ge(n) => v >= n,
            Num::Range(a, b) => v >= a && v <= b,
        }
    }
}

// ------------------------------------------------------------- the tokenizer

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// A term. `quoted` suppresses operator interpretation of its contents.
    Word(String, bool),
    Or,
    Not,
    Open,
    Close,
}

/// Split into tokens. Quotes toggle and are stripped; an unbalanced quote runs
/// to the end of the input, which is what a half-typed query looks like.
fn tokenize(input: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut in_quotes = false;
    let flush = |cur: &mut String, quoted: &mut bool, out: &mut Vec<Tok>| {
        if !cur.is_empty() || *quoted {
            out.push(Tok::Word(std::mem::take(cur), *quoted));
            *quoted = false;
        }
    };
    for c in input.chars() {
        if in_quotes {
            if c == '"' {
                in_quotes = false;
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            '"' => {
                in_quotes = true;
                // Only a term that *opens* with a quote is wholly literal.
                // `path:"C:\Program Files"` keeps its modifier: the quotes are
                // there to protect the value's spaces, not to disarm the prefix.
                quoted |= cur.is_empty();
            }
            c if c.is_whitespace() => flush(&mut cur, &mut quoted, &mut out),
            '|' => {
                flush(&mut cur, &mut quoted, &mut out);
                out.push(Tok::Or);
            }
            // `<` opens a group, except right after a function's colon, where it
            // is the comparison in `len:<8`.
            '<' if cur.ends_with(':') => cur.push('<'),
            '<' => {
                flush(&mut cur, &mut quoted, &mut out);
                out.push(Tok::Open);
            }
            // `>` closes a group everywhere except in a comparison, where it can
            // only ever sit immediately after the `:` of a function (`size:>1mb`,
            // `len:>=8`). Windows filenames cannot contain `<` or `>`, so there
            // is no third reading to worry about.
            '>' if cur.ends_with([':', '>', '<', '=']) => cur.push('>'),
            '>' => {
                flush(&mut cur, &mut quoted, &mut out);
                out.push(Tok::Close);
            }
            '!' if cur.is_empty() => out.push(Tok::Not),
            c => cur.push(c),
        }
    }
    flush(&mut cur, &mut quoted, &mut out);
    out
}

// ---------------------------------------------------------------- the parser

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
    q: Query,
    /// How many `!` operators this token sits under. Words recorded while non-zero are
    /// exclusions, and must not become the ranking needle; see `parse_unary`.
    negated: u32,
    /// How deep the recursive descent currently is.
    ///
    /// Every `!` and every `<` costs a stack frame, and the request frame cap is 64 KiB, so a
    /// query of sixty thousand `!` is a legal request. Measured in release with the default
    /// 2 MiB worker stack: twenty thousand of them overflow the stack, and a stack overflow
    /// cannot be caught, so the whole daemon dies. From an unprivileged local process, under
    /// the default pipe ACL. `simplify`, `compile`, `eval` and even `Drop` walk the same tree,
    /// so the tree itself has to be bounded, not just this walk.
    nesting: u32,
    /// How many `<` groups are open.
    ///
    /// Without it `parse_and` broke on every `>`, and `parse` calls the descent exactly once
    /// and drops whatever is left, so `a > b` parsed as `a` (the rest of the query silently
    /// discarded), and `>foo` parsed to `Expr::All`, which `search_query` answers with **every
    /// entry on the volume**. Measured before this: `>foo` returned 6,163,356 rows.
    depth: u32,
}

/// Parse a query. Never fails; see the module note on mid-keystroke input.
pub fn parse(input: &str) -> Query {
    let toks = tokenize(input);
    let mut p = Parser {
        toks: &toks,
        pos: 0,
        q: Query::default(),
        negated: 0,
        nesting: 0,
        depth: 0,
    };
    let expr = p.parse_or();
    let mut q = std::mem::take(&mut p.q);
    q.expr = simplify(expr);
    q
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn parse_or(&mut self) -> Expr {
        let mut parts = Vec::new();
        self.push_branch(&mut parts);
        while matches!(self.peek(), Some(Tok::Or)) {
            self.pos += 1;
            self.push_branch(&mut parts);
        }
        match parts.len() {
            0 => Expr::All,
            1 => parts.pop().unwrap(),
            _ => Expr::Or(parts),
        }
    }

    /// Parse one alternative and keep it, unless there was nothing there to parse.
    ///
    /// An empty branch is `Expr::All`, and `simplify` lets a single `All` swallow the whole
    /// disjunction, so `readme |`, which is what a search box sees between typing the bar and
    /// typing the second alternative, matched **every entry on the volume**. Measured on the
    /// fixture: 8 of 8. The same shape as the stray `>` this parser already guards, one
    /// function away.
    ///
    /// "Nothing there" is told from a genuine `All` by whether any token was consumed:
    /// `count:50 | b` really does mean "everything, or b".
    fn push_branch(&mut self, parts: &mut Vec<Expr>) {
        let before = self.pos;
        let e = self.parse_and();
        if self.pos > before || !matches!(e, Expr::All) {
            parts.push(e);
        }
    }

    fn parse_and(&mut self) -> Expr {
        let mut parts = Vec::new();
        while let Some(t) = self.peek() {
            match t {
                Tok::Or => break,
                // Only when it closes a group we opened. An unmatched one is a literal `>`,
                // handled in `parse_primary`, which was unreachable until this distinction
                // existed.
                Tok::Close if self.depth > 0 => break,
                _ => {
                    let e = self.parse_unary();
                    if !matches!(e, Expr::All) {
                        parts.push(e);
                    }
                }
            }
        }
        match parts.len() {
            0 => Expr::All,
            1 => parts.pop().unwrap(),
            _ => Expr::And(parts),
        }
    }

    /// The deepest nesting the parser will build. Past this, a token is taken literally: the
    /// module's stated policy for input it cannot make sense of. Nothing anyone types by hand
    /// comes near it; 64 KiB of `!` does.
    const MAX_NESTING: u32 = 64;

    fn parse_unary(&mut self) -> Expr {
        if matches!(self.peek(), Some(Tok::Not)) {
            self.pos += 1;
            // A trailing `!` is a literal, not a syntax error. So is one too deep to build:
            // taking it as a character is what stops sixty thousand of them from becoming
            // sixty thousand stack frames.
            let too_deep = self.nesting >= Self::MAX_NESTING;
            return match self.peek() {
                None | Some(Tok::Or) | Some(Tok::Close) => {
                    Expr::Term(Term::Name(Pat::contains("!")))
                }
                _ if too_deep => Expr::Term(Term::Name(Pat::contains("!"))),
                _ => {
                    self.nesting += 1;
                    // Everything under a `!` is excluded, so its words are not the query's
                    // words. `Query::words` is documented as the *positive* substrings and
                    // `top_ranked` takes the needle from it, so `report !documentation`
                    // ranked against `documentation`, a word every surviving hit is by
                    // construction free of. Every hit then fell into the bottom class and the
                    // exact/prefix/substring order collapsed to path length.
                    self.negated += 1;
                    let inner = self.parse_unary();
                    self.negated -= 1;
                    self.nesting -= 1;
                    Expr::Not(Box::new(inner))
                }
            };
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Expr {
        let too_deep = self.nesting >= Self::MAX_NESTING;
        match self.peek().cloned() {
            // Too deep to build: the bracket is a character, the same way a stray `>` is.
            Some(Tok::Open) if too_deep => {
                self.pos += 1;
                Expr::Term(Term::Name(Pat::contains("<")))
            }
            Some(Tok::Open) => {
                self.pos += 1;
                self.depth += 1;
                self.nesting += 1;
                let inner = self.parse_or();
                self.depth -= 1;
                self.nesting -= 1;
                if matches!(self.peek(), Some(Tok::Close)) {
                    self.pos += 1;
                }
                inner
            }
            Some(Tok::Close) => {
                // An unmatched `>`, which is nobody's intention; it is what a half-typed
                // `<a|b>` looks like on the way in, and what a typo looks like on the way out.
                //
                // Reading it as the literal character was the original plan (and this arm was
                // unreachable, so it never ran). It cannot be right: `>` is not legal in a
                // Windows file name, so a literal `>` term matches nothing, and `a > b` would
                // silently return zero rows for a query whose two real terms both match. This
                // module's policy for half-typed input is to keep matching, so the stray token
                // contributes nothing and the terms either side still apply.
                self.pos += 1;
                Expr::All
            }
            Some(Tok::Word(w, quoted)) => {
                self.pos += 1;
                self.term(&w, quoted)
            }
            _ => {
                self.pos += 1;
                Expr::All
            }
        }
    }

    /// Turn one word token into a term, applying any modifier prefixes.
    fn term(&mut self, raw: &str, quoted: bool) -> Expr {
        if raw.is_empty() {
            return Expr::All;
        }
        if quoted {
            // A quoted term is taken literally, operators and all.
            self.remember_word(raw.to_string());
            return Expr::Term(Term::Name(Pat::contains(raw)));
        }
        let mut rest = raw;
        let mut case: Option<bool> = None;
        let mut force_path = false;
        let mut force_name = false;
        let mut wildcards: Option<bool> = None;
        let mut shape: Option<&'static str> = None;

        // Modifier prefixes stack: `case:wfn:readme.md` is legal.
        while let Some((head, tail)) = split_prefix(rest) {
            match head.as_str() {
                "case" => case = Some(true),
                "nocase" => case = Some(false),
                "path" => force_path = true,
                "nopath" => force_name = true,
                "wildcards" => wildcards = Some(true),
                "nowildcards" => wildcards = Some(false),
                "wholeword" | "ww" => shape = Some("word"),
                "wholefilename" | "wfn" => shape = Some("whole"),
                "startwith" | "startswith" => shape = Some("prefix"),
                "endwith" | "endswith" => shape = Some("suffix"),
                "regex" => {
                    return self.regex_term(tail, false, case);
                }
                "pathregex" => {
                    return self.regex_term(tail, true, case);
                }
                _ => {
                    // Not a modifier: try the standalone functions.
                    if let Some(e) = self.function(&head, tail) {
                        return e;
                    }
                    break;
                }
            }
            if tail.is_empty() {
                // `case:` alone: nothing to apply it to.
                return Expr::All;
            }
            rest = tail;
        }

        let use_glob = wildcards.unwrap_or(rest.contains('*') || rest.contains('?'));
        let is_path = force_path || (!force_name && (rest.contains('\\') || rest.contains('/')));

        // A path is spelled with `\` in the haystack, so a typed `/` is normalised, *before*
        // the shape is chosen, not after. Patching only the `Contains` variant afterwards left
        // every other shape with its forward slashes: `path:endwith:docs/readme.md` compiled to
        // a suffix test for `docs/readme.md` against a haystack that only ever holds `\`, so it
        // could never match; `src/*.rs` (a glob, and a path) the same. Only the one shape that
        // had a test worked.
        let rest = if is_path {
            std::borrow::Cow::Owned(rest.replace('/', "\\"))
        } else {
            std::borrow::Cow::Borrowed(rest)
        };
        let rest = rest.as_ref();
        let kind = match shape {
            Some("word") => PatKind::Word(rest.to_string()),
            Some("whole") => PatKind::Whole(rest.to_string()),
            Some("prefix") => PatKind::Prefix(rest.to_string()),
            Some("suffix") => PatKind::Suffix(rest.to_string()),
            _ if use_glob => PatKind::Glob(rest.to_string()),
            _ => PatKind::Contains(rest.to_string()),
        };
        let pat = Pat { kind, case };
        if is_path {
            self.remember_path(rest.to_string());
            Expr::Term(Term::Path(pat))
        } else {
            if matches!(pat.kind, PatKind::Contains(_)) {
                self.remember_word(rest.to_string());
            }
            Expr::Term(Term::Name(pat))
        }
    }

    fn regex_term(&mut self, body: &str, over_path: bool, case: Option<bool>) -> Expr {
        let insensitive = !case.unwrap_or(false);
        match regex::bytes::RegexBuilder::new(body)
            .case_insensitive(insensitive)
            .build()
        {
            Ok(r) => Expr::Term(Term::Regex(std::sync::Arc::new(r), over_path)),
            // A regex the user is halfway through typing is not an error.
            Err(_) => {
                self.remember_word(body.to_string());
                Expr::Term(Term::Name(Pat::contains(body)))
            }
        }
    }

    /// Record a positive name substring, for ranking. A word under `!` is dropped.
    fn remember_word(&mut self, w: String) {
        if self.negated == 0 {
            self.q.words.push(w);
        }
    }

    /// Record a positive path fragment, on the same rule.
    fn remember_path(&mut self, p: String) {
        if self.negated == 0 {
            self.q.paths.push(p);
        }
    }

    /// The `name:value` forms that are constraints rather than modifiers.
    fn function(&mut self, head: &str, val: &str) -> Option<Expr> {
        // Filters that take no value of their own. `folder:temp` means "a directory, named
        // temp": the same reading `audio:foo` already had below. They were dropping `val`
        // entirely: measured, `folder:temp` returned 343,561 rows (every directory on the
        // volume) where `temp folder:` returned 1,119, and `file:readme` returned 2,821,039
        // where `readme file:` returned 2,321. Worse than a wrong count, the empty `q.words`
        // left every hit ranked `CLASS_ELSEWHERE`, so the order was path length.
        let term = match head {
            "ext" => Term::Ext(ext_group(val)?),
            "file" | "files" => return Some(self.filter_and_word(Term::Kind(false), val)),
            "folder" | "folders" | "dir" | "dirs" => {
                return Some(self.filter_and_word(Term::Kind(true), val))
            }
            "root" => return Some(self.filter_and_word(Term::Root, val)),
            "empty" => return Some(self.filter_and_word(Term::Empty, val)),
            "dupe" | "namepartdupe" => return Some(self.filter_and_word(Term::Dupe, val)),
            "len" => Term::Len(num(val)?),
            "parents" => Term::Parents(num(val)?),
            "size" => Term::Size(size(val)?),
            "childcount" => Term::Children(ChildKind::Any, num(val)?),
            "childfilecount" => Term::Children(ChildKind::Files, num(val)?),
            "childfoldercount" => Term::Children(ChildKind::Folders, num(val)?),
            "child" => {
                if val.is_empty() {
                    return None;
                }
                let i = self.q.child_pats.len();
                self.q.child_pats.push(Pat::contains(val));
                Term::HasChild(i)
            }
            "count" => {
                // No value yet is not malformed input, it is a half-typed one: every keystroke
                // of `count:50` passes through `count:`. Falling through there made it a
                // literal word no name contains, so the list blanked and refilled as the user
                // typed. A value that is present and not a number does fall through, which is
                // this module's policy for malformed input, and either way a limit set earlier
                // in the same query is left alone, where assigning `None` used to clear it.
                if val.is_empty() {
                    return Some(Expr::All);
                }
                let n = val.parse::<u32>().ok()?;
                self.q.limit = Some(n);
                return Some(Expr::All);
            }
            "attrib" | "attributes" => return attrib(val),
            _ => {
                let group = type_group(head)?;
                // `audio:` and friends take no value; `audio:foo` means the
                // group AND the word, which is how Everything reads it.
                let ext = Expr::Term(Term::Ext(group));
                if val.is_empty() {
                    return Some(ext);
                }
                self.remember_word(val.to_string());
                return Some(Expr::And(vec![
                    ext,
                    Expr::Term(Term::Name(Pat::contains(val))),
                ]));
            }
        };
        Some(Expr::Term(term))
    }

    /// A filter that takes no value of its own, plus the value as a name word if one was
    /// given. `folder:` alone is every directory; `folder:temp` is the directories named
    /// `temp`. Recording the word is what keeps ranking working: the needle comes from
    /// `q.words`, and without it every hit ties at the bottom class.
    fn filter_and_word(&mut self, filter: Term, val: &str) -> Expr {
        let filter = Expr::Term(filter);
        if val.is_empty() {
            return filter;
        }
        // Through `term`, not straight to `Pat::contains`. The value is an ordinary term and
        // has to be read like one: `file:*.dll` is a glob and `folder:src\util` is a path.
        // Forcing a literal made both match nothing, which is worse than the bug it replaced
        // (they used to match *everything*), because a silent zero reads as "no such file".
        //
        // `term` can come back here (`folder:file:...`), so it counts against the same nesting
        // budget as `!` and `<`.
        if self.nesting >= Self::MAX_NESTING {
            return filter;
        }
        self.nesting += 1;
        let word = self.term(val, false);
        self.nesting -= 1;
        Expr::And(vec![filter, word])
    }
}

impl Pat {
    fn contains(s: &str) -> Self {
        Pat {
            kind: PatKind::Contains(s.to_string()),
            case: None,
        }
    }
}

/// `head:tail` where `head` is ASCII letters. Returns `None` when the token has
/// no such prefix (so `C:\x` is a path, not a `c:` function).
fn split_prefix(s: &str) -> Option<(String, &str)> {
    let colon = s.find(':')?;
    let head = &s[..colon];
    if head.is_empty() || !head.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    Some((head.to_ascii_lowercase(), &s[colon + 1..]))
}

/// `rs;.toml` -> `["rs", "toml"]`; `None` when no usable member remains.
fn ext_group(v: &str) -> Option<Vec<String>> {
    let group: Vec<String> = v
        .split(';')
        .map(|e| e.trim_start_matches('.').to_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    (!group.is_empty()).then_some(group)
}

/// `>123`, `1..9`, `=7`, `7`.
fn num(v: &str) -> Option<Num> {
    parse_pred(v, |s| s.parse::<u64>().ok())
}

/// The same shapes, with size units and Everything's named buckets.
///
/// The buckets partition: each runs from its lower bound **inclusive** to the next one's lower
/// bound **exclusive**. [`Num::Range`] is closed at both ends, so each upper bound is written
/// one byte short of the boundary. They used to share their endpoints, which made a file of
/// exactly 10 KB both `tiny` and `small`, five sizes on this volume that answered two
/// mutually exclusive questions with "yes".
///
/// Inclusive at the bottom rather than the top, to agree with `gigantic`: a file of exactly
/// 128 MB is gigantic, not huge.
fn size(v: &str) -> Option<Num> {
    const KB: u64 = 1024;
    const MB: u64 = KB * KB;
    match v.to_ascii_lowercase().as_str() {
        "empty" => return Some(Num::Eq(0)),
        "tiny" => return Some(Num::Range(0, 10 * KB - 1)),
        "small" => return Some(Num::Range(10 * KB, 100 * KB - 1)),
        "medium" => return Some(Num::Range(100 * KB, MB - 1)),
        "large" => return Some(Num::Range(MB, 16 * MB - 1)),
        "huge" => return Some(Num::Range(16 * MB, 128 * MB - 1)),
        "gigantic" => return Some(Num::Ge(128 * MB)),
        _ => {}
    }
    parse_pred(v, parse_bytes)
}

fn parse_bytes(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (digits, mult) = if let Some(d) = s.strip_suffix("tb").or(s.strip_suffix("tib")) {
        (d, 1u64 << 40)
    } else if let Some(d) = s.strip_suffix("gb").or(s.strip_suffix("gib")) {
        (d, 1 << 30)
    } else if let Some(d) = s.strip_suffix("mb").or(s.strip_suffix("mib")) {
        (d, 1 << 20)
    } else if let Some(d) = s.strip_suffix("kb").or(s.strip_suffix("kib")) {
        (d, 1 << 10)
    } else if let Some(d) = s.strip_suffix('b') {
        (d, 1)
    } else {
        (s.as_str(), 1)
    };
    // `size:>17179869184gb` is 2^34 * 2^30 = 2^64: in release it wrapped to zero, so a
    // query for files over sixteen exabytes returned every file with an allocated cluster
    // (measured: 2,220,544 rows). In a debug build it panicked the daemon's request thread.
    // A number that cannot be represented is malformed input, and this module's stated
    // policy for malformed input is to degrade to a literal word.
    digits
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
}

fn parse_pred(v: &str, unit: impl Fn(&str) -> Option<u64>) -> Option<Num> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    if let Some((a, b)) = v.split_once("..") {
        return Some(Num::Range(unit(a)?, unit(b)?));
    }
    for (prefix, make) in [
        (">=", Num::Ge as fn(u64) -> Num),
        ("<=", Num::Le as fn(u64) -> Num),
        ("==", Num::Eq as fn(u64) -> Num),
        (">", Num::Gt as fn(u64) -> Num),
        ("<", Num::Lt as fn(u64) -> Num),
        ("=", Num::Eq as fn(u64) -> Num),
    ] {
        if let Some(r) = v.strip_prefix(prefix) {
            return Some(make(unit(r)?));
        }
    }
    Some(Num::Eq(unit(v)?))
}

/// `attrib:dl`, only the bits the index actually carries.
fn attrib(v: &str) -> Option<Expr> {
    let mut parts = Vec::new();
    for c in v.chars() {
        let (bit, want) = match c.to_ascii_lowercase() {
            'd' => (flags::IS_DIR, true),
            'l' => (flags::REPARSE_POINT, true),
            _ => continue, // r/h/s/a are not stored; ignore rather than mislead
        };
        parts.push(Expr::Term(Term::Attrib(bit, want)));
    }
    match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(Expr::And(parts)),
    }
}

/// Everything's file-type shortcuts.
fn type_group(name: &str) -> Option<Vec<String>> {
    let list: &str = match name {
        "audio" | "music" => {
            "aac ac3 aif aifc aiff au cda dts flac it m1a m2a m3u m4a mid midi mka mod mp2 mp3 \
             mpa ogg opus ra rmi spc snd umx voc wav wma xm"
        }
        "video" | "movie" => {
            "3g2 3gp amv asf avi bik divx drc dv f4v flv gvi hdmov m1v m2t m2ts m2v m4p m4v mkv \
             mov mp2v mp4 mpe mpeg mpg mpv2 mts mxf ogm ogv qt rm rmvb swf ts vob webm wm wmv"
        }
        "pic" | "image" => {
            "ani avif bmp dds gif heic heif ico jfif jpe jpeg jpg jxl pcx png psd raw svg tga \
             tif tiff webp wmf"
        }
        "doc" | "document" => {
            "csv doc docm docx dot dotm dotx epub key md mobi odp ods odt pages pdf pot potm \
             potx pps ppsm ppsx ppt pptm pptx rtf tex txt wpd wps xls xlsb xlsm xlsx xlt xltm xltx"
        }
        "exe" | "program" => "bat cmd com exe msi msix msp ps1 scr vbs",
        "zip" | "archive" | "compressed" => {
            "7z ace arj bz2 cab gz gzip iso jar lz lzh lzma rar tar taz tbz tbz2 tgz txz xz z zip zst"
        }
        "font" => "fnt fon otf pfb pfm ttc ttf woff woff2",
        "code" | "source" => {
            "c cc cpp cs css cxx go h hpp hs htm html java js json jsx kt lua m mm php py rb rs \
             scala sh sql swift toml ts tsx vue yaml yml zig"
        }
        _ => return None,
    };
    Some(list.split_whitespace().map(str::to_string).collect())
}

/// Flatten single-child And/Or and drop `All` from conjunctions, so the fast
/// path can recognise a plain one-word query however it was written.
fn simplify(e: Expr) -> Expr {
    match e {
        Expr::And(v) => {
            let mut parts: Vec<Expr> = v
                .into_iter()
                .map(simplify)
                .filter(|p| !matches!(p, Expr::All))
                .collect();
            match parts.len() {
                0 => Expr::All,
                1 => parts.pop().unwrap(),
                _ => Expr::And(parts),
            }
        }
        Expr::Or(v) => {
            let mut parts: Vec<Expr> = v.into_iter().map(simplify).collect();
            if parts.iter().any(|p| matches!(p, Expr::All)) {
                return Expr::All;
            }
            match parts.len() {
                0 => Expr::All,
                1 => parts.pop().unwrap(),
                _ => Expr::Or(parts),
            }
        }
        Expr::Not(b) => Expr::Not(Box::new(simplify(*b))),
        other => other,
    }
}

// ------------------------------------------------------------- the evaluator

/// A pattern compiled for one case setting: the needle is already folded (or
/// not) and the haystack to test it against is decided.
struct CPat {
    /// Test the raw name arena rather than the folded one.
    raw: bool,
    kind: CPatKind,
}

impl CPat {
    /// Which arena this pattern's needle belongs to. The caller has to hand it the matching
    /// haystack; `path_hits` used to hand every path pattern the folded one regardless, so a
    /// raw needle could never match.
    fn is_raw(&self) -> bool {
        self.raw
    }
}

enum CPatKind {
    Contains(memmem::Finder<'static>),
    Whole(Vec<u8>),
    Prefix(Vec<u8>),
    Suffix(Vec<u8>),
    Word(memmem::Finder<'static>, usize),
    Glob(Vec<u8>),
}

impl CPatKind {
    /// How many bytes the needle is. A longer literal rejects more entries per
    /// byte compared, which is what the conjunction ordering below sorts on.
    fn needle_len(&self) -> usize {
        match self {
            CPatKind::Contains(f) => f.needle().len(),
            CPatKind::Whole(w) | CPatKind::Prefix(w) | CPatKind::Suffix(w) => w.len(),
            CPatKind::Word(_, n) => *n,
            CPatKind::Glob(g) => g.len(),
        }
    }
}

impl CPat {
    fn compile(p: &Pat, case_insensitive: bool) -> CPat {
        let insensitive = p.case.map(|c| !c).unwrap_or(case_insensitive);
        let prep = |s: &String| {
            if insensitive {
                fold::fold_query(s).into_bytes()
            } else {
                s.as_bytes().to_vec()
            }
        };
        let kind = match &p.kind {
            PatKind::Contains(s) => CPatKind::Contains(memmem::Finder::new(&prep(s)).into_owned()),
            PatKind::Whole(s) => CPatKind::Whole(prep(s)),
            PatKind::Prefix(s) => CPatKind::Prefix(prep(s)),
            PatKind::Suffix(s) => CPatKind::Suffix(prep(s)),
            PatKind::Word(s) => {
                let b = prep(s);
                let n = b.len();
                CPatKind::Word(memmem::Finder::new(&b).into_owned(), n)
            }
            PatKind::Glob(s) => CPatKind::Glob(prep(s)),
        };
        CPat {
            raw: !insensitive,
            kind,
        }
    }

    #[inline]
    fn test(&self, hay: &[u8]) -> bool {
        match &self.kind {
            CPatKind::Contains(f) => f.find(hay).is_some(),
            CPatKind::Whole(w) => hay == w.as_slice(),
            CPatKind::Prefix(w) => hay.starts_with(w),
            CPatKind::Suffix(w) => hay.ends_with(w),
            CPatKind::Word(f, n) => {
                let mut from = 0;
                while let Some(at) = f.find(&hay[from..]) {
                    let i = from + at;
                    let before_ok = i == 0 || !is_word_byte(hay[i - 1]);
                    let after = i + n;
                    let after_ok = after >= hay.len() || !is_word_byte(hay[after]);
                    if before_ok && after_ok {
                        return true;
                    }
                    from = i + 1;
                    if from >= hay.len() {
                        break;
                    }
                }
                false
            }
            CPatKind::Glob(g) => glob_match(g, hay),
        }
    }
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// `*` (any run) and `?` (one character) against the whole haystack.
///
/// The classic two-pointer walk: linear in practice and, unlike naive
/// recursion, it cannot blow up on a pattern like `a*a*a*a*b`.
///
/// `?` consumes a whole UTF-8 sequence, not one byte. Bytewise, `??.txt` matched `ab.txt` and
/// not `日本.txt` (six bytes for two characters) while `????.txt` matched the second and not the
/// first: off by the encoding, on exactly the corpus this index is aimed at. Everything's `?`
/// is one character, and so is this one. The `*` backtrack advances by a character too, or it
/// could resume in the middle of a sequence and compare a continuation byte to a pattern byte.
fn glob_match(pat: &[u8], hay: &[u8]) -> bool {
    /// Bytes in the UTF-8 sequence starting at `i`, from the leading byte alone. At least one,
    /// so the walk always advances even on malformed input.
    fn seq_len(hay: &[u8], i: usize) -> usize {
        let n = match hay[i] {
            b if b < 0x80 => 1,
            b if b >> 5 == 0b110 => 2,
            b if b >> 4 == 0b1110 => 3,
            b if b >> 3 == 0b11110 => 4,
            _ => 1, // a stray continuation byte: step over it rather than stall
        };
        n.min(hay.len() - i)
    }

    let (mut p, mut h) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while h < hay.len() {
        if p < pat.len() && pat[p] == b'?' {
            p += 1;
            h += seq_len(hay, h);
        } else if p < pat.len() && pat[p] == hay[h] {
            p += 1;
            h += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = p;
            mark = h;
            p += 1;
        } else if star != usize::MAX {
            p = star + 1;
            mark += seq_len(hay, mark);
            h = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

/// The compiled tree.
use std::sync::atomic::{AtomicU32, Ordering};

/// One separator-carrying `Contains` needle, split on `\` for the per-directory
/// suffix automaton of [`PathTable`].
///
/// A needle `go\pkg\mod` matches a joined path exactly when some directory's
/// path ends with `...go\pkg` and the next component starts with `mod`, so the
/// first segment is tested as a component *suffix*, the middles as whole
/// components, and the last as a component *prefix*. Empty segments fall out
/// of the same rules: a leading `\` makes the first suffix test vacuous, a
/// trailing `\` makes the last prefix test vacuous, and a doubled `\\` demands
/// an empty component, which no path has.
struct TableNeedle {
    segs: Vec<Vec<u8>>,
    /// This needle's first chain bit in the per-directory state word.
    bit0: u32,
}

/// The lazily-filled per-directory state that makes a fused group of `!path:`
/// excludes O(needles) per entry instead of O(path length * needles).
///
/// Rebuilding the joined path per candidate (even once, fused) is what the
/// numbers said it was: on the real volume a one-character query spent most of
/// its 1.65 s doing exactly that. But every candidate under one directory asks
/// the same question of the same directory path, so the answer is stored per
/// directory instead: bit 30 is "this directory's own path already contains
/// some needle", bits 0.. are suffix-automaton progress ("the path so far ends
/// with the needle's first j segments"), and a candidate then needs only its
/// parent's word and its own name. States fill lazily along whatever parent
/// chains the scan actually touches, under `Relaxed` atomics: two threads can
/// only ever store the same value.
struct PathTable {
    needles: Vec<TableNeedle>,
    /// Negated component-form excludes, absorbed into the same walk: a
    /// separator-less fragment matches the joined path iff it matches some
    /// single component, so one probe of each directory's name (memoised in
    /// the verdict bit) and one of the candidate's own replaces the
    /// per-candidate ancestor walk the standalone `Component` node does.
    comp: Vec<memmem::Finder<'static>>,
    /// Indices (into the node's `none_of`) this table does **not** cover:
    /// non-`Contains` shapes, separator-less needles, or bit overflow. They
    /// keep the joined-path test, paid only by candidates the table clears.
    fallback: Vec<usize>,
    /// Per-entry state, `COMPUTED`-tagged; only directory slots are written.
    states: Vec<AtomicU32>,
}

const TBL_COMPUTED: u32 = 1 << 31;
const TBL_VERDICT: u32 = 1 << 30;

impl PathTable {
    /// Build a table for a fused negation group, or `None` when the group is
    /// case-sensitive, has positive terms, or holds nothing the automaton can
    /// take (then the fused Phase-1 evaluation stands on its own).
    fn build(
        raw: bool,
        all_of: &[CPat],
        none_of: &[CPat],
        comp: Vec<memmem::Finder<'static>>,
        entries: usize,
    ) -> Option<PathTable> {
        if raw || !all_of.is_empty() {
            return None;
        }
        let mut needles = Vec::new();
        let mut fallback = Vec::new();
        let mut bit0 = 0u32;
        for (i, p) in none_of.iter().enumerate() {
            let eligible = match &p.kind {
                CPatKind::Contains(f) if f.needle().contains(&b'\\') => {
                    let segs: Vec<Vec<u8>> = f
                        .needle()
                        .split(|&b| b == b'\\')
                        .map(<[u8]>::to_vec)
                        .collect();
                    let bits = (segs.len() - 1) as u32;
                    if bit0 + bits <= 30 {
                        needles.push(TableNeedle { segs, bit0 });
                        bit0 += bits;
                        true
                    } else {
                        false
                    }
                }
                _ => false,
            };
            if !eligible {
                fallback.push(i);
            }
        }
        if needles.is_empty() && comp.is_empty() {
            return None;
        }
        Some(PathTable {
            needles,
            comp,
            fallback,
            states: std::iter::repeat_with(|| AtomicU32::new(0))
                .take(entries)
                .collect(),
        })
    }

    /// The state after appending component `nm` to a path in state `ps`.
    fn step(&self, ps: u32, nm: &[u8]) -> u32 {
        let mut s = ps & TBL_VERDICT;
        if s == 0 && self.comp.iter().any(|f| f.find(nm).is_some()) {
            s |= TBL_VERDICT;
        }
        for n in &self.needles {
            let m = n.segs.len();
            if ps & (1 << (n.bit0 + (m as u32 - 2))) != 0 && nm.starts_with(&n.segs[m - 1]) {
                s |= TBL_VERDICT;
            }
            if nm.ends_with(&n.segs[0]) {
                s |= 1 << n.bit0;
            }
            for j in 1..m - 1 {
                if ps & (1 << (n.bit0 + j as u32 - 1)) != 0 && nm == n.segs[j] {
                    s |= 1 << (n.bit0 + j as u32);
                }
            }
        }
        s
    }

    /// Whether an entry named `nm` under a parent in state `ps` is excluded.
    /// [`step`](Self::step) minus the bookkeeping a non-directory never needs.
    fn hits(&self, ps: u32, nm: &[u8]) -> bool {
        if ps & TBL_VERDICT != 0 {
            return true;
        }
        self.comp.iter().any(|f| f.find(nm).is_some())
            || self.needles.iter().any(|n| {
                let m = n.segs.len();
                ps & (1 << (n.bit0 + (m as u32 - 2))) != 0 && nm.starts_with(&n.segs[m - 1])
            })
    }
}

enum CExpr {
    All,
    And(Vec<CExpr>),
    Or(Vec<CExpr>),
    Not(Box<CExpr>),
    Name(CPat),
    /// Every name test in one conjunction, fused.
    ///
    /// Three separate `Name` nodes mean three recursive calls, three enum
    /// matches and three haystack lookups for what is one slice and three
    /// `memmem` probes. Fusing them is worth ~1.5x on `word + operator`
    /// queries; the flat scan this tree replaced got the same effect by
    /// having no tree at all.
    Names {
        /// Which arena the needles were folded against.
        raw: bool,
        all_of: Vec<CPat>,
        none_of: Vec<CPat>,
    },
    Path(PathPred),
    /// Every joined-path test of one conjunction, fused: the path is rebuilt
    /// **once** per entry and probed by every pattern.
    ///
    /// The `!path:` excludes file makes this shape common, and each separate
    /// `Path` node rebuilt the same path for itself: seven separator fragments
    /// meant seven ancestor walks and seven `Vec`s per candidate. Measured on
    /// the real volume (6.2M entries, 18-line excludes file), a one-character
    /// query spent 1.45 of its 1.65 seconds there: the cost the interactive
    /// TUI blocks on while the first keystroke's query is in flight.
    PathsJoined {
        /// Which arena the patterns were compiled against (a `case:` group
        /// keeps its own node, exactly like [`CExpr::Names`]).
        raw: bool,
        all_of: Vec<CPat>,
        none_of: Vec<CPat>,
        /// The per-directory accelerator for a pure-negation group, the
        /// excludes shape. `None` falls back to build-and-test per entry.
        table: Option<PathTable>,
    },
    Ext(Vec<Vec<u8>>),
    Kind(bool),
    Root,
    Empty,
    Dupe,
    Len(Num),
    Parents(Num),
    Size(Num),
    Children(ChildKind, Num),
    HasChild(usize),
    Attrib(u16, bool),
    Regex(std::sync::Arc<regex::bytes::Regex>, bool),
}

/// A folded path-fragment predicate.
///
/// A fragment with no `\` and no `:` and length >= 2 provably cannot span a path
/// separator or sit inside the `c:` drive prefix, so joined-path containment is
/// equivalent to "some component contains it", checked against each ancestor's
/// fold slice with zero reconstruction (`Component`). Everything else takes the
/// exact joined-path check (`Joined`). The `<orphan>` synthetic prefix is handled
/// in the component walk so the two modes stay observably identical.
enum PathPred {
    Component(memmem::Finder<'static>),
    Joined(CPat),
}

impl PathPred {
    fn new(p: &Pat, case_insensitive: bool) -> Self {
        // The single-component shortcut searches the fold arena, so it can only ever answer a
        // case-insensitive term. It used to take `case:path:Foo` too, and silently drop the
        // modifier: the one reading of `case:` that looks like it works.
        if p.case != Some(true) {
            if let PatKind::Contains(s) = &p.kind {
                let folded = fold::fold_query(s);
                if !folded.contains('\\') && !folded.contains(':') && folded.len() >= 2 {
                    return PathPred::Component(
                        memmem::Finder::new(folded.as_bytes()).into_owned(),
                    );
                }
            }
        }
        // Path matching is case-insensitive by Windows convention unless the
        // user says otherwise, so the pattern compiles against the fold arena.
        let mut p = p.clone();
        if p.case.is_none() {
            p.case = Some(false);
        }
        PathPred::Joined(CPat::compile(&p, case_insensitive))
    }
}

/// Collapse the name tests of one conjunction into a single [`CExpr::Names`].
///
/// Positive and negated names fuse together (they read the same haystack) but
/// only when they agree on which arena that is, which is why a `case:` term
/// keeps its own group.
fn fuse_names(parts: Vec<CExpr>) -> Vec<CExpr> {
    let mut groups: Vec<(bool, Vec<CPat>, Vec<CPat>)> = Vec::new();
    let mut rest: Vec<CExpr> = Vec::new();
    for p in parts {
        match p {
            CExpr::Name(pat) => push_name(&mut groups, pat, false),
            CExpr::Not(b) => match *b {
                CExpr::Name(pat) => push_name(&mut groups, pat, true),
                other => rest.push(CExpr::Not(Box::new(other))),
            },
            other => rest.push(other),
        }
    }
    for (raw, all_of, none_of) in groups {
        // A lone positive name is cheaper as itself: no vector walk.
        if none_of.is_empty() && all_of.len() == 1 {
            rest.push(CExpr::Name(all_of.into_iter().next().unwrap()));
        } else {
            rest.push(CExpr::Names {
                raw,
                all_of,
                none_of,
            });
        }
    }
    rest
}

/// Collapse the joined-path tests of one conjunction into one
/// [`CExpr::PathsJoined`] per arena, mirroring [`fuse_names`]: they all probe
/// the same reconstructed path, so reconstruct it once. Component-form path
/// terms are left alone: they walk the ancestors' name slices and never build
/// a path at all.
fn fuse_paths(parts: Vec<CExpr>, entries: usize) -> Vec<CExpr> {
    let mut groups: Vec<(bool, Vec<CPat>, Vec<CPat>)> = Vec::new();
    let mut comp: Vec<memmem::Finder<'static>> = Vec::new();
    let mut rest: Vec<CExpr> = Vec::new();
    for p in parts {
        match p {
            CExpr::Path(PathPred::Joined(pat)) => push_name(&mut groups, pat, false),
            CExpr::Not(b) => match *b {
                CExpr::Path(PathPred::Joined(pat)) => push_name(&mut groups, pat, true),
                CExpr::Path(PathPred::Component(f)) => comp.push(f),
                other => rest.push(CExpr::Not(Box::new(other))),
            },
            other => rest.push(other),
        }
    }
    // Negated component fragments ride the folded pure-negation group's table;
    // synthesise that group when the excludes are all component-form.
    if !comp.is_empty()
        && !groups
            .iter()
            .any(|(raw, all_of, _)| !raw && all_of.is_empty())
    {
        groups.push((false, Vec::new(), Vec::new()));
    }
    for (raw, all_of, none_of) in groups {
        let comp_here = if !raw && all_of.is_empty() {
            std::mem::take(&mut comp)
        } else {
            Vec::new()
        };
        // A lone test amortises nothing; keep the plain node and its cost tier.
        if comp_here.is_empty() && all_of.len() + none_of.len() == 1 {
            let (pat, negated) = match all_of.into_iter().next() {
                Some(p) => (p, false),
                None => (none_of.into_iter().next().unwrap(), true),
            };
            let node = CExpr::Path(PathPred::Joined(pat));
            rest.push(if negated {
                CExpr::Not(Box::new(node))
            } else {
                node
            });
        } else {
            let table = PathTable::build(raw, &all_of, &none_of, comp_here, entries);
            rest.push(CExpr::PathsJoined {
                raw,
                all_of,
                none_of,
                table,
            });
        }
    }
    debug_assert!(comp.is_empty(), "components must have found their group");
    rest
}

fn push_name(groups: &mut Vec<(bool, Vec<CPat>, Vec<CPat>)>, pat: CPat, negated: bool) {
    let raw = pat.raw;
    let slot = match groups.iter_mut().find(|(r, _, _)| *r == raw) {
        Some(s) => s,
        None => {
            groups.push((raw, Vec::new(), Vec::new()));
            groups.last_mut().unwrap()
        }
    };
    if negated {
        slot.2.push(pat);
    } else {
        slot.1.push(pat);
    }
}

/// FNV-1a over a folded name, used only to group equal names together cheaply.
///
/// Not a hash map key and not security-relevant: a collision costs one byte comparison in the
/// tie-break that follows it, never a wrong answer. Chosen for being a few instructions per
/// byte with no setup, on names that are a couple of dozen bytes long.
fn name_hash(name: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in name {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Where a test belongs in a conjunction: lower runs first.
///
/// Two things decide it, and the second one is easy to get wrong.
///
/// **Cost.** A flag test is a bitwise and; a name scan touches the fold arena;
/// a component walk climbs the ancestor chain; the joined-path forms rebuild the
/// whole path into a fresh `Vec`. That ordering is just work per entry.
///
/// **Selectivity.** Cheapness is worth nothing if the test lets everything
/// through, and a *negated* term almost always does: `!ext:log` rejects a tenth
/// of a volume where `report` rejects nine tenths. So negations sort behind the
/// positives even though they cost the same, and within the name tier a longer
/// needle goes first. This is what the flat scan did by hand before the tree;
/// ordering purely by cost measured ~2x worse on `word + two negations`.
fn cost(e: &CExpr) -> (u8, i64) {
    /// How much later a negation runs than the same test would positively.
    const NEGATED: u8 = 3;
    match e {
        CExpr::All => (0, 0),
        // Free, and often very selective (`folder:` is a few percent).
        CExpr::Kind(_) | CExpr::Root | CExpr::Attrib(..) => (0, 0),
        // A typed word is usually the most selective thing in the query; a
        // one- or two-byte one is not, so it drops behind the array lookups.
        CExpr::Name(p) => {
            let n = p.kind.needle_len();
            (if n >= 3 { 1 } else { 2 }, -(n as i64))
        }
        // A fused group is led by its most selective positive name; a group of
        // only negations is as unselective as a single one.
        CExpr::Names {
            all_of, none_of, ..
        } => {
            let best = all_of.iter().map(|p| p.kind.needle_len()).max();
            match best {
                Some(n) => (if n >= 3 { 1 } else { 2 }, -(n as i64)),
                None => {
                    let n = none_of
                        .iter()
                        .map(|p| p.kind.needle_len())
                        .max()
                        .unwrap_or(0);
                    (4, -(n as i64))
                }
            }
        }
        CExpr::Ext(_)
        | CExpr::Len(_)
        | CExpr::Parents(_)
        | CExpr::Size(_)
        | CExpr::Empty
        | CExpr::Dupe
        | CExpr::Children(..)
        | CExpr::HasChild(_) => (2, 0),
        CExpr::Path(PathPred::Component(_)) => (4, 0),
        CExpr::Path(PathPred::Joined(_)) => (5, 0),
        // The fused node runs the build once however many patterns it holds; a
        // group of only negations is as unselective as a single negated path.
        CExpr::PathsJoined { all_of, .. } => {
            if all_of.is_empty() {
                (5 + NEGATED, 0)
            } else {
                (5, 0)
            }
        }
        CExpr::Regex(_, over_path) => (if *over_path { 6 } else { 5 }, 0),
        CExpr::Not(b) => {
            let (tier, tie) = cost(b);
            (tier + NEGATED, tie)
        }
        // A compound is as expensive as its worst branch.
        CExpr::And(v) | CExpr::Or(v) => v.iter().map(cost).max().unwrap_or((0, 0)),
    }
}

/// Per-query precomputation, built once and only for the terms that need it.
#[derive(Default)]
struct Aux {
    /// `(files, dirs)` directly inside each entry.
    children: Option<Vec<(u32, u32)>>,
    /// Entries whose folded name occurs more than once on the volume.
    dupes: Option<Vec<bool>>,
    /// One flag vector per `child:` term: does this directory contain a match.
    has_child: Vec<Vec<bool>>,
}

impl Index {
    fn compile(&self, e: &Expr, ci: bool) -> CExpr {
        match e {
            Expr::All => CExpr::All,
            Expr::And(v) => {
                let parts: Vec<CExpr> = v.iter().map(|x| self.compile(x, ci)).collect();
                let parts = fuse_names(parts);
                let mut parts = fuse_paths(parts, self.entries.len());
                // Order matters: `And` short-circuits, so the cheapest and most
                // selective test has to run first. Written order is whatever the
                // user typed, which is why this sorts; the flat scan this
                // replaced did the same thing by hand (longest word first), and
                // dropping it cost ~40% on `word + operator` queries.
                parts.sort_by_key(cost);
                if parts.len() == 1 {
                    parts.pop().unwrap()
                } else {
                    CExpr::And(parts)
                }
            }
            Expr::Or(v) => CExpr::Or(v.iter().map(|x| self.compile(x, ci)).collect()),
            Expr::Not(b) => CExpr::Not(Box::new(self.compile(b, ci))),
            Expr::Term(t) => match t {
                Term::Name(p) => CExpr::Name(CPat::compile(p, ci)),
                Term::Path(p) => CExpr::Path(PathPred::new(p, ci)),
                Term::Ext(g) => CExpr::Ext(g.iter().map(|s| s.as_bytes().to_vec()).collect()),
                Term::Kind(d) => CExpr::Kind(*d),
                Term::Root => CExpr::Root,
                Term::Empty => CExpr::Empty,
                Term::Dupe => CExpr::Dupe,
                Term::Len(n) => CExpr::Len(*n),
                Term::Parents(n) => CExpr::Parents(*n),
                Term::Size(n) => CExpr::Size(*n),
                Term::Children(k, n) => CExpr::Children(*k, *n),
                Term::HasChild(i) => CExpr::HasChild(*i),
                Term::Attrib(m, w) => CExpr::Attrib(*m, *w),
                Term::Regex(r, p) => CExpr::Regex(r.clone(), *p),
            },
        }
    }

    /// Walk the tree and build only the auxiliary tables it asks for.
    fn build_aux(&self, q: &Query, ce: &CExpr, ci: bool) -> Aux {
        let (mut needs_children, mut needs_dupes) = (false, false);
        fn walk(e: &CExpr, c: &mut bool, d: &mut bool) {
            match e {
                CExpr::And(v) | CExpr::Or(v) => v.iter().for_each(|x| walk(x, c, d)),
                CExpr::Not(b) => walk(b, c, d),
                CExpr::Children(..) | CExpr::Empty => *c = true,
                CExpr::Dupe => *d = true,
                _ => {}
            }
        }
        walk(ce, &mut needs_children, &mut needs_dupes);

        let mut aux = Aux::default();
        if needs_children {
            let mut counts = vec![(0u32, 0u32); self.entries.len()];
            for e in self.entries.iter() {
                if e.flags & flags::DEAD != 0 {
                    continue;
                }
                let p = e.parent;
                if (p as usize) < counts.len() {
                    if e.flags & flags::IS_DIR != 0 {
                        counts[p as usize].1 += 1;
                    } else {
                        counts[p as usize].0 += 1;
                    }
                }
            }
            aux.children = Some(counts);
        }
        if needs_dupes {
            // Sort ids by folded name and mark every member of a run > 1. O(n
            // log n) and unavoidable; `dupe:` is a whole-volume question.
            // Tombstones excluded, like the two aux tables built either side of this one. A
            // deleted entry keeps its name in the arena, so counting it made the *survivor* of
            // a deleted pair report as a duplicate until the next rebuild.
            // Sorted by a hash of the name rather than by the name, because what this costs is
            // not the comparisons but where they read from.
            //
            // Comparing two names is two random loads into a 143 MB arena. At six million ids
            // that is ~23 such pairs per element, every one of them a likely cache miss, and
            // it is the whole of the cost: measured on the real volume, `dupe:` took 5.9 s and
            // `kernel32 dupe:` took 5.6 s: the same table, built in full, to answer about
            // seventy-five rows. Spreading the sort across the cores alone only reached 5.0 s.
            //
            // Hashing once up front turns almost all of that into comparing two `u64`s inside
            // a packed array: equal names always hash alike, so the arena is only consulted
            // when two hashes collide, which is where the tie-break below still compares the
            // bytes. Equal names therefore still land adjacent, and only they do.
            //
            // Worth the trouble because this is the one request a client can send that costs
            // seconds, and it spends them under the index *read* lock, which, being
            // writer-preferring, is seconds in which the watch thread's next apply queues and
            // every search behind it waits.
            let name_of = |id: u32| {
                let e = &self.entries[id as usize];
                self.fold.get(e.fold_off, e.fold_len)
            };
            let mut keyed: Vec<(u64, u32)> = (0..self.entries.len() as u32)
                .into_par_iter()
                .filter(|&id| self.entries[id as usize].flags & flags::DEAD == 0)
                .map(|id| (name_hash(name_of(id)), id))
                .collect();
            // Unstable is fine and always was: only runs of equal names are read, never the
            // order within one.
            keyed.par_sort_unstable_by(|a, b| {
                a.0.cmp(&b.0).then_with(|| name_of(a.1).cmp(name_of(b.1)))
            });
            let mut marks = vec![false; self.entries.len()];
            let mut i = 0;
            while i < keyed.len() {
                let (hash, name) = (keyed[i].0, name_of(keyed[i].1));
                let mut j = i + 1;
                while j < keyed.len() && keyed[j].0 == hash && name_of(keyed[j].1) == name {
                    j += 1;
                }
                if j - i > 1 {
                    for &(_, id) in &keyed[i..j] {
                        marks[id as usize] = true;
                    }
                }
                i = j;
            }
            aux.dupes = Some(marks);
        }
        for pat in &q.child_pats {
            let cp = CPat::compile(pat, ci);
            let mut marks = vec![false; self.entries.len()];
            for e in self.entries.iter() {
                if e.flags & flags::DEAD != 0 {
                    continue;
                }
                let hay = if cp.raw {
                    self.arena.get(e.name_off, e.name_len)
                } else {
                    self.fold.get(e.fold_off, e.fold_len)
                };
                if cp.test(hay) && (e.parent as usize) < marks.len() {
                    marks[e.parent as usize] = true;
                }
            }
            aux.has_child.push(marks);
        }
        aux
    }

    /// `en` is the entry for `id`, passed down so a conjunction of N terms costs
    /// one bounds-checked lookup instead of N + 1.
    fn eval(&self, e: &CExpr, id: u32, en: &super::Entry, aux: &Aux) -> bool {
        match e {
            CExpr::All => true,
            // Explicit loops, not iterator adapters: this runs once per entry on
            // the volume, and the closure form does not inline through the
            // recursion.
            CExpr::And(v) => {
                for x in v {
                    if !self.eval(x, id, en, aux) {
                        return false;
                    }
                }
                true
            }
            CExpr::Or(v) => {
                for x in v {
                    if self.eval(x, id, en, aux) {
                        return true;
                    }
                }
                false
            }
            CExpr::Not(b) => !self.eval(b, id, en, aux),
            CExpr::Name(p) => {
                let hay = if p.raw {
                    self.arena.get(en.name_off, en.name_len)
                } else {
                    self.fold.get(en.fold_off, en.fold_len)
                };
                p.test(hay)
            }
            CExpr::Names {
                raw,
                all_of,
                none_of,
            } => {
                let hay = if *raw {
                    self.arena.get(en.name_off, en.name_len)
                } else {
                    self.fold.get(en.fold_off, en.fold_len)
                };
                for p in all_of {
                    if !p.test(hay) {
                        return false;
                    }
                }
                for p in none_of {
                    if p.test(hay) {
                        return false;
                    }
                }
                true
            }
            CExpr::Path(p) => self.path_hits(id, p),
            CExpr::PathsJoined {
                raw,
                all_of,
                none_of,
                table,
            } => {
                if let Some(t) = table {
                    // The excludes shape: parent's directory word + own name.
                    let ps = self.exclude_parent_state(t, en.parent);
                    if t.hits(ps, self.fold.get(en.fold_off, en.fold_len)) {
                        return false;
                    }
                    if t.fallback.is_empty() {
                        return true;
                    }
                    let path = self.path_bytes(id, *raw);
                    return t.fallback.iter().all(|&i| !none_of[i].test(&path));
                }
                let path = self.path_bytes(id, *raw);
                for p in all_of {
                    if !p.test(&path) {
                        return false;
                    }
                }
                for p in none_of {
                    if p.test(&path) {
                        return false;
                    }
                }
                true
            }
            CExpr::Ext(group) => {
                let folded = self.fold.get(en.fold_off, en.fold_len);
                let ext: &[u8] = match folded.iter().rposition(|&b| b == b'.') {
                    Some(pos) if pos + 1 < folded.len() => &folded[pos + 1..],
                    _ => &[],
                };
                !ext.is_empty() && group.iter().any(|g| g.as_slice() == ext)
            }
            CExpr::Kind(dir) => (en.flags & flags::IS_DIR != 0) == *dir,
            CExpr::Root => en.parent == self.root,
            CExpr::Empty => {
                if en.flags & flags::IS_DIR != 0 {
                    aux.children
                        .as_ref()
                        .map(|c| c[id as usize] == (0, 0))
                        .unwrap_or(false)
                } else {
                    self.alloc_clusters.get(id as usize).copied().unwrap_or(0) == 0
                }
            }
            CExpr::Dupe => aux.dupes.as_ref().map(|d| d[id as usize]).unwrap_or(false),
            CExpr::Len(n) => {
                let name = self.arena.get(en.name_off, en.name_len);
                n.test(String::from_utf8_lossy(name).chars().count() as u64)
            }
            CExpr::Parents(n) => n.test(self.depth(id) as u64),
            CExpr::Size(n) => {
                let clusters = self.alloc_clusters.get(id as usize).copied().unwrap_or(0);
                n.test(clusters as u64 * self.cluster_bytes as u64)
            }
            CExpr::Children(kind, n) => {
                let Some(c) = aux.children.as_ref() else {
                    return false;
                };
                let (files, dirs) = c[id as usize];
                let v = match kind {
                    ChildKind::Any => files as u64 + dirs as u64,
                    ChildKind::Files => files as u64,
                    ChildKind::Folders => dirs as u64,
                };
                n.test(v)
            }
            CExpr::HasChild(i) => aux
                .has_child
                .get(*i)
                .map(|m| m[id as usize])
                .unwrap_or(false),
            CExpr::Attrib(mask, want) => ((en.flags & mask) != 0) == *want,
            CExpr::Regex(r, over_path) => {
                // The raw arena either way, and the regex's own case flag does the folding,
                // which is what the name branch below has always done. The path branch used
                // the pre-lowercased arena instead, so `case:pathregex:Users` built a
                // case-sensitive regex and then ran it against a haystack with no uppercase
                // in it: zero results, always.
                if *over_path {
                    r.is_match(&self.path_bytes(id, true))
                } else {
                    r.is_match(self.arena.get(en.name_off, en.name_len))
                }
            }
        }
    }

    /// How many directories sit between `id` and the drive root.
    ///
    /// The walk yields `id` itself first, so the count is one more than the depth, and
    /// `saturating_sub`, because an empty index yields nothing at all and `0 - 1` would panic
    /// in a debug build for a question nobody can ask of it.
    fn depth(&self, id: u32) -> u32 {
        (self.ancestors(id).count() as u32).saturating_sub(1)
    }

    /// Search with a parsed [`Query`]. A plain one-word query takes the M1 fast
    /// path in [`search`](Index::search) unchanged, so the common case keeps its
    /// latency profile.
    pub fn search_query(
        &self,
        q: &Query,
        case_insensitive: bool,
        include_orphans: bool,
    ) -> Vec<u32> {
        if let Some(w) = q.plain_word() {
            return self.search(w, case_insensitive, include_orphans);
        }
        let ce = self.compile(&q.expr, case_insensitive);
        let aux = self.build_aux(q, &ce, case_insensitive);
        // Unwrap the top-level conjunction here rather than recursing into it
        // once per entry: at a few million entries that one saved call is
        // measurable, and a top-level `And` is what almost every query is.
        let terms: &[CExpr] = match &ce {
            CExpr::And(v) => v,
            single => std::slice::from_ref(single),
        };

        (0..self.entries.len() as u32)
            .into_par_iter()
            .filter(|&id| {
                if id == self.root {
                    return false;
                }
                let e = &self.entries[id as usize];
                if e.flags & flags::DEAD != 0 {
                    return false;
                }
                if !include_orphans && e.flags & flags::IS_ORPHAN != 0 {
                    return false;
                }
                for t in terms {
                    if !self.eval(t, id, e, &aux) {
                        return false;
                    }
                }
                true
            })
            .collect()
    }

    /// Order `ids` for display and return the top `limit`: exact name matches first,
    /// then name-prefix matches, then plain substring hits; ties prefer the shorter
    /// path, then the lower id (stable). The needle is the query's longest positive
    /// word, the same primary the scan used; with no words (operator-only queries)
    /// everything ties on class and the shortest paths surface first.
    /// O(hits * log limit) via a bounded heap; the full hit set is never sorted.
    /// `reserve_elsewhere` keeps that many of the returned rows for hits the term matches
    /// *inside* the name rather than at its start. Zero means pure rank order.
    ///
    /// Without it, a term short enough to have thousands of name-prefix matches fills every
    /// row with them and the substring hits (the ones only Everyfind can find) never appear.
    /// Measured under one home directory: `a` has 93,232 names beginning with it, so a page of
    /// 500 held one substring hit; `tai` has 258, so 242 rows were left for them. The effect is
    /// a search that looks prefix-only exactly when the user has typed least.
    pub fn top_ranked(
        &self,
        ids: &[u32],
        q: &Query,
        case_insensitive: bool,
        limit: usize,
        reserve_elsewhere: usize,
    ) -> Vec<u32> {
        let limit = match q.limit {
            Some(n) => limit.min(n as usize),
            None => limit,
        };
        if limit == 0 || ids.is_empty() {
            return Vec::new();
        }
        let reserve_elsewhere = reserve_elsewhere.min(limit);
        let needle_owned: String = match q.words.iter().max_by_key(|w| w.len()) {
            Some(w) if case_insensitive => fold::fold_query(w),
            Some(w) => w.clone(),
            None => String::new(),
        };
        let needle = needle_owned.as_bytes();

        // Max-heap holding the current top `limit`; the root is the worst of them,
        // evicted whenever a better candidate arrives. The reservation is bounded by the hit
        // count as well as `limit`, because `limit` reaches here straight off the wire: asking
        // for `u32::MAX` otherwise reserves 48 GiB for a heap that can never hold more than
        // `ids.len()` entries. Measured on Windows 11 that reservation *succeeds* (the segment
        // heap commits on touch), so it is waste rather than a crash, but it is waste
        // proportional to a number a client picks, which is not a thing to leave lying around.
        let mut heap: BinaryHeap<(u8, u32, u32)> =
            BinaryHeap::with_capacity(limit.min(ids.len()) + 1);
        // A second, smaller heap over the elsewhere-matches alone, filled in the same pass.
        // One scan either way: the reservation costs a comparison per hit, not a re-scan.
        let mut elsewhere: BinaryHeap<(u8, u32, u32)> =
            BinaryHeap::with_capacity(reserve_elsewhere.min(ids.len()) + 1);
        let keep = |h: &mut BinaryHeap<(u8, u32, u32)>, key, cap: usize| {
            if cap == 0 {
                return;
            }
            if h.len() < cap {
                h.push(key);
            } else if let Some(&worst) = h.peek() {
                if key < worst {
                    h.pop();
                    h.push(key);
                }
            }
        };
        for &id in ids {
            let key = self.rank_key(id, needle, case_insensitive);
            keep(&mut heap, key, limit);
            if key.0 == CLASS_ELSEWHERE {
                keep(&mut elsewhere, key, reserve_elsewhere);
            }
        }

        let mut chosen = heap.into_sorted_vec();
        let have = chosen.iter().filter(|k| k.0 == CLASS_ELSEWHERE).count();
        if have < reserve_elsewhere {
            let already: std::collections::HashSet<u32> = chosen.iter().map(|k| k.2).collect();
            let add: Vec<_> = elsewhere
                .into_sorted_vec()
                .into_iter()
                .filter(|k| !already.contains(&k.2))
                .take(reserve_elsewhere - have)
                .collect();
            // Make room by dropping the weakest rows that are *not* elsewhere-matches, from
            // the back: the reservation trades away the tail of the prefix run, not the head.
            //
            // One pass, marking then retaining. Repeated `Vec::remove` walked the whole
            // elsewhere run to find each victim and then shifted the tail for every one of
            // them: `room * have` element moves, which a request is free to maximise
            // (`limit` and `reserve_elsewhere` are only clamped to `MAX_SEARCH_LIMIT`) into
            // seconds of CPU held under the index read lock, and the read lock is what the
            // watch thread's write lock queues behind.
            let mut room = add.len();
            let mut drop_from_back = vec![false; chosen.len()];
            for i in (0..chosen.len()).rev() {
                if room == 0 {
                    break;
                }
                if chosen[i].0 != CLASS_ELSEWHERE {
                    drop_from_back[i] = true;
                    room -= 1;
                }
            }
            let mut keep = drop_from_back.into_iter();
            chosen.retain(|_| !keep.next().unwrap_or(false));
            chosen.extend(add);
            chosen.sort_unstable();
        }
        chosen.into_iter().map(|(_, _, id)| id).collect()
    }

    /// `(match class, path length, id)`, lexicographically ascending = better.
    fn rank_key(&self, id: u32, needle: &[u8], case_insensitive: bool) -> (u8, u32, u32) {
        let e = &self.entries[id as usize];
        let name: &[u8] = if case_insensitive {
            self.fold.get(e.fold_off, e.fold_len)
        } else {
            self.arena.get(e.name_off, e.name_len)
        };
        let class = if needle.is_empty() {
            CLASS_ELSEWHERE
        } else if name == needle {
            0
        } else if name.starts_with(needle) {
            1
        } else {
            CLASS_ELSEWHERE
        };
        (class, self.path_len(id), id)
    }

    /// Length of the reconstructed path, without building it.
    fn path_len(&self, id: u32) -> u32 {
        let mut walk = self.ancestors(id);
        let mut len: u32 = 0;
        for cur in walk.by_ref() {
            len += self.entries[cur as usize].name_len as u32 + 1;
        }
        // The prefix the path would carry: `c:\` or the longer `<orphan>\`. It used to be a
        // hardcoded 3, so orphan hits measured ~6 short and sorted ahead of equally deep real
        // paths whenever `--include-orphans` was on.
        len + if walk.orphaned() { 9 } else { 3 }
    }

    /// Does `id`'s path match the predicate?
    fn path_hits(&self, id: u32, pred: &PathPred) -> bool {
        match pred {
            PathPred::Component(f) => {
                let mut walk = self.ancestors(id);
                for cur in walk.by_ref() {
                    let e = &self.entries[cur as usize];
                    if f.find(self.fold.get(e.fold_off, e.fold_len)).is_some() {
                        return true;
                    }
                }
                // Parity with the joined form, which includes the synthetic `<orphan>` prefix
                // in its haystack.
                walk.orphaned() && f.find(b"<orphan>").is_some()
            }
            // The haystack has to be the arena the pattern was compiled against, or a
            // case-sensitive path term matches nothing at all.
            PathPred::Joined(p) => p.test(&self.path_bytes(id, p.is_raw())),
        }
    }

    /// The full path of `id` as bytes, `\`-separated, with the same `c:\` / `<orphan>\`
    /// prefixes as [`path`](Index::path).
    ///
    /// `raw` picks the arena: the original names with the drive letter as stored, or the
    /// case-folded ones with the drive lowercased. Both are needed and only the folded one
    /// existed, which is why `case:path:Foo` could never match: [`CPat::compile`] honours
    /// `case: Some(true)` by keeping the needle unfolded, and it was then tested against a
    /// haystack in which an uppercase byte cannot occur.
    /// The [`PathTable`] state of the directory `parent` names, computing and
    /// storing any ancestors still unfilled on the way. Mirrors [`ancestors`]
    /// exactly: the same sentinel handling, the same cycle bound, and the same
    /// `<orphan>` / lowercased-drive prefix, fed to the automaton as a virtual
    /// top component, so a needle can anchor at `c:\`.
    ///
    /// [`ancestors`]: Index::ancestors
    fn exclude_parent_state(&self, t: &PathTable, parent: u32) -> u32 {
        let mut chain: Vec<u32> = Vec::new();
        let mut cur = parent;
        let mut left = super::MAX_PATH_DEPTH;
        let mut base = loop {
            match cur {
                super::ORPHAN_SENTINEL => break t.step(0, b"<orphan>"),
                p if p == self.root || p == super::ROOT_SENTINEL => {
                    break t.step(0, &[self.drive.to_ascii_lowercase() as u8, b':'])
                }
                p if (p as usize) >= self.entries.len() => {
                    break t.step(0, &[self.drive.to_ascii_lowercase() as u8, b':'])
                }
                p => {
                    let s = t.states[p as usize].load(Ordering::Relaxed);
                    if s & TBL_COMPUTED != 0 {
                        break s;
                    }
                    if left == 0 {
                        // A cycle: bounded like the path walk, decided like a top.
                        break t.step(0, &[self.drive.to_ascii_lowercase() as u8, b':']);
                    }
                    left -= 1;
                    chain.push(p);
                    cur = self.entries[p as usize].parent;
                }
            }
        };
        for &d in chain.iter().rev() {
            let e = &self.entries[d as usize];
            base = t.step(base, self.fold.get(e.fold_off, e.fold_len)) | TBL_COMPUTED;
            // Two threads racing here store the same value: the input is the
            // same immutable index, so the memo is write-once in effect.
            t.states[d as usize].store(base, Ordering::Relaxed);
        }
        base
    }

    fn path_bytes(&self, id: u32, raw: bool) -> Vec<u8> {
        let mut walk = self.ancestors(id);
        let mut parts: Vec<(u32, u16)> = Vec::new();
        for cur in walk.by_ref() {
            let e = &self.entries[cur as usize];
            parts.push(if raw {
                (e.name_off, e.name_len)
            } else {
                (e.fold_off, e.fold_len)
            });
        }
        let mut out = Vec::with_capacity(64);
        if walk.orphaned() {
            out.extend_from_slice(b"<orphan>\\");
        } else {
            let drive = if raw {
                self.drive
            } else {
                self.drive.to_ascii_lowercase()
            };
            out.push(drive as u8);
            out.extend_from_slice(b":\\");
        }
        let arena = if raw { &self.arena } else { &self.fold };
        for (i, &(off, len)) in parts.iter().rev().enumerate() {
            if i > 0 {
                out.push(b'\\');
            }
            out.extend_from_slice(arena.get(off, len));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of a parsed expression, for terse assertions.
    fn shape(e: &Expr) -> String {
        match e {
            Expr::All => "all".into(),
            Expr::And(v) => format!("and({})", v.iter().map(shape).collect::<Vec<_>>().join(",")),
            Expr::Or(v) => format!("or({})", v.iter().map(shape).collect::<Vec<_>>().join(",")),
            Expr::Not(b) => format!("not({})", shape(b)),
            Expr::Term(t) => match t {
                Term::Name(p) => format!("name:{}", pat(p)),
                Term::Path(p) => format!("path:{}", pat(p)),
                Term::Ext(g) => format!("ext:{}", g.join(";")),
                Term::Kind(true) => "folder".into(),
                Term::Kind(false) => "file".into(),
                Term::Root => "root".into(),
                Term::Empty => "empty".into(),
                Term::Dupe => "dupe".into(),
                Term::Len(n) => format!("len:{n:?}"),
                Term::Parents(n) => format!("parents:{n:?}"),
                Term::Size(n) => format!("size:{n:?}"),
                Term::Children(k, n) => format!("children:{k:?}:{n:?}"),
                Term::HasChild(i) => format!("child#{i}"),
                Term::Attrib(m, w) => format!("attrib:{m}:{w}"),
                Term::Regex(_, p) => format!("regex(path={p})"),
            },
        }
    }

    fn pat(p: &Pat) -> String {
        match &p.kind {
            PatKind::Contains(s) => format!("~{s}"),
            PatKind::Whole(s) => format!("={s}"),
            PatKind::Prefix(s) => format!("^{s}"),
            PatKind::Suffix(s) => format!("${s}"),
            PatKind::Word(s) => format!("w{s}"),
            PatKind::Glob(s) => format!("g{s}"),
        }
    }

    #[test]
    fn whitespace_is_and() {
        assert_eq!(shape(&parse("a b").expr), "and(name:~a,name:~b)");
    }

    #[test]
    fn pipe_is_or_and_binds_looser_than_and() {
        assert_eq!(
            shape(&parse("a b | c d").expr),
            "or(and(name:~a,name:~b),and(name:~c,name:~d))"
        );
    }

    #[test]
    fn angle_brackets_group() {
        assert_eq!(
            shape(&parse("a <b | c>").expr),
            "and(name:~a,or(name:~b,name:~c))"
        );
    }

    #[test]
    fn bang_negates_and_a_lone_bang_is_literal() {
        assert_eq!(shape(&parse("a !b").expr), "and(name:~a,not(name:~b))");
        assert_eq!(shape(&parse("!").expr), "name:~!");
        assert_eq!(
            shape(&parse("!ext:tmp").expr),
            "not(ext:tmp)",
            "negation applies to functions too"
        );
    }

    #[test]
    fn quotes_keep_spaces_and_suppress_operators() {
        assert_eq!(shape(&parse("\"a b\"").expr), "name:~a b");
        assert_eq!(shape(&parse("\"a|b\"").expr), "name:~a|b");
    }

    #[test]
    fn wildcards_match_the_whole_name() {
        assert_eq!(shape(&parse("*.txt").expr), "name:g*.txt");
        assert_eq!(shape(&parse("nowildcards:a*b").expr), "name:~a*b");
        assert_eq!(shape(&parse("wildcards:ab").expr), "name:gab");
    }

    #[test]
    fn a_term_with_a_separator_is_a_path() {
        assert_eq!(shape(&parse(r"src\util").expr), r"path:~src\util");
        assert_eq!(
            shape(&parse("src/util").expr),
            r"path:~src\util",
            "forward slashes normalise"
        );
        assert_eq!(parse("src/util").paths, vec![r"src\util".to_string()]);
        assert_eq!(shape(&parse(r"nopath:src\util").expr), r"name:~src\util");
    }

    #[test]
    fn modifiers_stack_and_reshape() {
        assert_eq!(shape(&parse("wfn:readme.md").expr), "name:=readme.md");
        assert_eq!(shape(&parse("ww:log").expr), "name:wlog");
        assert_eq!(shape(&parse("startwith:pre").expr), "name:^pre");
        assert_eq!(shape(&parse("endwith:.rs").expr), "name:$.rs");
        let q = parse("case:wfn:README");
        assert_eq!(shape(&q.expr), "name:=README");
        assert!(matches!(&q.expr, Expr::Term(Term::Name(p)) if p.case == Some(true)));
    }

    #[test]
    fn functions_parse() {
        assert_eq!(shape(&parse("ext:rs;.toml").expr), "ext:rs;toml");
        assert_eq!(shape(&parse("folder:").expr), "folder");
        assert_eq!(shape(&parse("file:").expr), "file");
        assert_eq!(shape(&parse("root:").expr), "root");
        assert_eq!(shape(&parse("empty:").expr), "empty");
        assert_eq!(shape(&parse("dupe:").expr), "dupe");
        assert_eq!(shape(&parse("attrib:d").expr), "attrib:1:true");
        assert_eq!(shape(&parse("child:cargo.toml").expr), "child#0");
        assert_eq!(parse("count:50").limit, Some(50));
    }

    #[test]
    fn numbers_and_sizes() {
        assert_eq!(num("7"), Some(Num::Eq(7)));
        assert_eq!(num(">7"), Some(Num::Gt(7)));
        assert_eq!(num(">=7"), Some(Num::Ge(7)));
        assert_eq!(num("1..9"), Some(Num::Range(1, 9)));
        assert_eq!(size("1kb"), Some(Num::Eq(1024)));
        assert_eq!(size(">1mb"), Some(Num::Gt(1024 * 1024)));
        assert_eq!(size("empty"), Some(Num::Eq(0)));
        assert_eq!(size("gigantic"), Some(Num::Ge(128 * 1024 * 1024)));

        // The named buckets partition: every size belongs to exactly one of them. They shared
        // their endpoints, so a file of exactly 10 KB was both `tiny` and `small`.
        let buckets = ["tiny", "small", "medium", "large", "huge", "gigantic"];
        for bytes in [
            0,
            1,
            10 * 1024 - 1,
            10 * 1024,
            100 * 1024 - 1,
            100 * 1024,
            1024 * 1024 - 1,
            1024 * 1024,
            16 * 1024 * 1024 - 1,
            16 * 1024 * 1024,
            128 * 1024 * 1024 - 1,
            128 * 1024 * 1024,
            u64::MAX,
        ] {
            let matched: Vec<&str> = buckets
                .iter()
                .copied()
                .filter(|b| size(b).expect("a named bucket").test(bytes))
                .collect();
            assert_eq!(
                matched.len(),
                1,
                "{bytes} bytes matched {matched:?}, not exactly one bucket"
            );
        }
        // `size:>1mb` must survive tokenizing: the `>` is inside the term.
        assert_eq!(shape(&parse("size:>1mb").expr), "size:Gt(1048576)");
    }

    #[test]
    fn type_groups_expand() {
        let q = parse("audio:");
        assert!(matches!(&q.expr, Expr::Term(Term::Ext(g)) if g.contains(&"mp3".to_string())));
        // `pic:holiday` is the group AND the word.
        assert_eq!(shape(&parse("pic:holiday").expr), {
            let g = type_group("pic").unwrap().join(";");
            format!("and(ext:{g},name:~holiday)")
        });
    }

    #[test]
    fn regex_falls_back_to_a_literal_while_being_typed() {
        assert_eq!(shape(&parse(r"regex:^a.*z$").expr), "regex(path=false)");
        assert_eq!(shape(&parse("regex:[").expr), "name:~[");
    }

    #[test]
    fn drive_letters_are_paths_not_functions() {
        assert_eq!(shape(&parse(r"C:\Users").expr), r"path:~C:\Users");
    }

    #[test]
    fn empty_input_matches_everything() {
        assert!(parse("").is_empty());
        assert!(parse("   ").is_empty());
    }

    #[test]
    fn glob_matching() {
        assert!(glob_match(b"*.txt", b"a.txt"));
        assert!(!glob_match(b"*.txt", b"a.txtx"));
        assert!(glob_match(b"a?c", b"abc"));
        assert!(!glob_match(b"a?c", b"ac"));
        assert!(glob_match(b"*", b""));
        assert!(glob_match(b"a*a*a*a*b", b"aaaaaaaaaab"));
        assert!(!glob_match(b"a*a*a*a*b", b"aaaaaaaaaac"));
    }

    /// `?` is one character. Bytewise it counted UTF-8 bytes, so `??.txt` missed `日本.txt`
    /// (six bytes, two characters) while `????.txt` matched it: wrong in both directions, and
    /// wrong on the corpus this index is for.
    #[test]
    fn a_question_mark_is_one_character_not_one_byte() {
        assert!(glob_match("??.txt".as_bytes(), "日本.txt".as_bytes()));
        assert!(!glob_match("????.txt".as_bytes(), "日本.txt".as_bytes()));
        assert!(glob_match("?.txt".as_bytes(), "あ.txt".as_bytes()));
        assert!(
            glob_match("??.txt".as_bytes(), "ab.txt".as_bytes()),
            "ASCII unchanged"
        );

        // A `*` backtrack must land on a character boundary too, or it compares a continuation
        // byte against the pattern.
        assert!(glob_match("*本.txt".as_bytes(), "日本.txt".as_bytes()));
        assert!(!glob_match("*本.txt".as_bytes(), "日月.txt".as_bytes()));
        assert!(glob_match("日*t".as_bytes(), "日本語.txt".as_bytes()));

        // Emoji are four bytes each.
        assert!(glob_match("?.png".as_bytes(), "🐈.png".as_bytes()));
        assert!(!glob_match("??.png".as_bytes(), "🐈.png".as_bytes()));
    }

    #[test]
    fn whole_word_needs_delimiters() {
        let p = CPat::compile(
            &Pat {
                kind: PatKind::Word("log".into()),
                case: Some(false),
            },
            true,
        );
        assert!(p.test(b"app.log"));
        assert!(p.test(b"log"));
        assert!(!p.test(b"catalog"));
        assert!(!p.test(b"logger"));
    }

    #[test]
    fn the_fast_path_still_recognises_a_plain_word() {
        assert_eq!(parse("kernel32").plain_word(), Some("kernel32"));
        assert_eq!(parse("").plain_word(), Some(""));
        assert_eq!(parse("a b").plain_word(), None);
        assert_eq!(parse("ext:rs").plain_word(), None);
    }
}
