//! The TUI state machine: pure and transport-agnostic, so it is driven
//! entirely by unit tests (no pipe, no terminal).
//!
//! The shell ([`super`]'s crossterm loop) feeds it key events and worker replies and executes
//! the [`Action`]s it returns; it never does IO itself. The two invariants that matter:
//!
//! - **Sequential + coalesce + generation** (decision #4): every query change bumps [`req_gen`]
//!   and returns an [`Action::Dispatch`] tagged with it. [`on_response`] applies a reply **only
//!   if its `gen` is still the latest**: a stale (superseded) reply is dropped, so a burst of
//!   fast keystrokes always settles on the final query's results.
//! - **Enter acts on the currently displayed list** (decision #3, fzf-like): it never waits
//!   on an in-flight reply; because stale replies are dropped, "displayed" is always coherent.
//!   Enter *opens* the selection (the obvious meaning); **Ctrl+Enter** prints its path to
//!   stdout instead, which is what makes `vim $(ef)` compose.
//!
//! [`req_gen`]: AppState::req_gen
//! [`on_response`]: AppState::on_response

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::ipc::{Response, SearchHit, StatusReport};

/// What the shell should do after feeding a key. Returned by [`AppState::on_key`]; the shell
/// performs the side effect (dispatch a search, touch the clipboard, launch Explorer, exit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Issue a search for `query`, tagged with `gen`. The reply must be handed back via
    /// [`AppState::on_response`] with the same `gen`.
    Dispatch { gen: u64, query: String },
    /// Ctrl+Enter: emit this absolute path on **stdout** and exit 0 (composability).
    Select(String),
    /// Ctrl+Y: copy this path to the clipboard.
    Copy(String),
    /// Ctrl+E: reveal this path in Explorer (`explorer /select,`).
    Reveal(String),
    /// Ctrl+O: open this path with its associated program.
    Open(String),
    /// Esc / Ctrl+C: exit 1, writing nothing to stdout.
    Quit,
}

/// The interactive search state.
#[derive(Debug)]
pub struct AppState {
    /// The query, as chars (edits and cursor motion are char-based, so full-width / emoji work).
    input: Vec<char>,
    /// Text cursor position, a char index in `input` (`0..=input.len()`).
    cursor: usize,
    /// The currently displayed results (up to the fetch limit).
    results: Vec<SearchHit>,
    /// The daemon's reported total hit count (may exceed `results.len()`; "narrow it").
    total_hits: u64,
    /// Selected row index into `results`.
    selected: usize,
    /// Latest dispatched generation. A response with a smaller gen is stale.
    req_gen: u64,
    /// Round-trip time of the last applied search, milliseconds (status line).
    last_rt_ms: Option<u64>,
    /// The most recent daemon status snapshot (status line: entries / lag / drive).
    status: Option<StatusReport>,
    /// A daemon error message to surface (cleared on the next successful search).
    error: Option<String>,
    /// Rows available in the result viewport, for PgUp/PgDn. Updated by the shell on render.
    viewport_rows: usize,
}

impl AppState {
    /// Create a state seeded with `initial_query` (empty for a bare `ef`). The cursor sits at the
    /// end. The shell calls [`dispatch_current`](Self::dispatch_current) once to kick off the
    /// first search (if the seed is non-empty).
    pub fn new(initial_query: &str) -> Self {
        let input: Vec<char> = initial_query.chars().collect();
        let cursor = input.len();
        Self {
            input,
            cursor,
            results: Vec::new(),
            total_hits: 0,
            selected: 0,
            req_gen: 0,
            last_rt_ms: None,
            status: None,
            error: None,
            viewport_rows: 10,
        }
    }

    // --- accessors for the renderer ---

    /// The current query as a string.
    pub fn query(&self) -> String {
        self.input.iter().collect()
    }

    /// Cursor position as a char index.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The displayed results.
    pub fn results(&self) -> &[SearchHit] {
        &self.results
    }

    /// The total hit count reported by the daemon.
    pub fn total_hits(&self) -> u64 {
        self.total_hits
    }

    /// The selected row index (into [`results`](Self::results)).
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Last applied search round-trip time, milliseconds.
    pub fn last_rt_ms(&self) -> Option<u64> {
        self.last_rt_ms
    }

    /// The latest daemon status, if fetched.
    pub fn status(&self) -> Option<&StatusReport> {
        self.status.as_ref()
    }

    /// A pending daemon error message, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether the query is effectively empty (no dispatch -> show the hint).
    pub fn is_query_empty(&self) -> bool {
        self.query().trim().is_empty()
    }

    /// The selected path, if a row is selected.
    pub fn selected_path(&self) -> Option<String> {
        self.results.get(self.selected).map(|h| h.path.clone())
    }

    /// Set the result viewport height (rows), used for PgUp/PgDn. Called by the shell on render
    /// and resize.
    pub fn set_viewport_rows(&mut self, rows: usize) {
        self.viewport_rows = rows.max(1);
    }

    // --- inputs from the shell ---

    /// Dispatch the current query. Bumps [`req_gen`](Self::req_gen) so any in-flight reply for a
    /// prior query becomes stale. An **empty** query dispatches nothing (clears the list and
    /// shows the hint); a non-empty query returns [`Action::Dispatch`].
    pub fn dispatch_current(&mut self) -> Option<Action> {
        self.req_gen += 1;
        if self.is_query_empty() {
            self.results.clear();
            self.total_hits = 0;
            self.selected = 0;
            self.error = None;
            None
        } else {
            Some(Action::Dispatch {
                gen: self.req_gen,
                query: self.query(),
            })
        }
    }

    /// Insert pasted text at the cursor (bracketed paste). Some terminals / IMEs / the Windows
    /// emoji panel deliver text as a paste rather than per-char key events. Control characters
    /// (newlines, tabs) are dropped: the query is single-line. Char-based like [`on_key`], so
    /// multi-byte glyphs are handled correctly.
    pub fn on_paste(&mut self, text: &str) -> Option<Action> {
        let mut changed = false;
        for c in text.chars().filter(|c| !c.is_control()) {
            self.input.insert(self.cursor, c);
            self.cursor += 1;
            changed = true;
        }
        if changed {
            self.dispatch_current()
        } else {
            None
        }
    }

    /// Feed a key event. Returns the [`Action`] the shell should perform, if any.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Action> {
        // Windows delivers key-release events too; act only on press/repeat.
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => Some(Action::Quit),
            KeyCode::Esc => Some(Action::Quit),
            KeyCode::Char('y') if ctrl => self.selected_path().map(Action::Copy),
            KeyCode::Char('e') if ctrl => self.selected_path().map(Action::Reveal),
            KeyCode::Char('o') if ctrl => self.selected_path().map(Action::Open),
            // Enter does the obvious thing: open the file/folder with its
            // associated program (a folder lands in Explorer). Printing the
            // path to stdout (what makes `vim $(ef)` work) moves to
            // Ctrl+Enter, since "I picked it, open it" is what a person
            // expects and shell composition is the specialist's path.
            KeyCode::Enter if ctrl => self.selected_path().map(Action::Select),
            KeyCode::Enter => self.selected_path().map(Action::Open),
            KeyCode::Up => {
                self.move_selection_up(1);
                None
            }
            KeyCode::Down => {
                self.move_selection_down(1);
                None
            }
            KeyCode::PageUp => {
                self.move_selection_up(self.viewport_rows);
                None
            }
            KeyCode::PageDown => {
                self.move_selection_down(self.viewport_rows);
                None
            }
            KeyCode::Home if !ctrl => {
                self.cursor = 0;
                None
            }
            KeyCode::End if !ctrl => {
                self.cursor = self.input.len();
                None
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            KeyCode::Right => {
                if self.cursor < self.input.len() {
                    self.cursor += 1;
                }
                None
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.input.remove(self.cursor - 1);
                    self.cursor -= 1;
                    return self.dispatch_current();
                }
                None
            }
            KeyCode::Delete => {
                if self.cursor < self.input.len() {
                    self.input.remove(self.cursor);
                    return self.dispatch_current();
                }
                None
            }
            // Printable input (ignore control-modified chars; those are commands above).
            KeyCode::Char(c) if !ctrl => {
                self.input.insert(self.cursor, c);
                self.cursor += 1;
                self.dispatch_current()
            }
            _ => None,
        }
    }

    /// Apply a worker reply tagged with `gen`. **Discarded if `gen` is not the latest** (stale).
    /// On a successful search the selection resets to the top row (fzf-like).
    pub fn on_response(&mut self, gen: u64, resp: Response, rt_ms: u64) {
        if gen != self.req_gen {
            return; // stale: a newer query has been dispatched
        }
        match resp {
            Response::Search {
                total_hits,
                results,
                building,
            } => {
                self.total_hits = total_hits;
                self.results = results;
                self.selected = 0;
                self.last_rt_ms = Some(rt_ms);
                // During the initial enumeration the result is empty-but-not-final; show why
                // instead of a bare "0" that reads as broken.
                self.error = building
                    .then(|| "building initial index - results will appear shortly".to_string());
            }
            Response::Error { message, .. } => {
                self.error = Some(message);
            }
            // A search dispatch never yields Status / Du; ignore defensively.
            Response::Status(_) | Response::Du { .. } => {}
        }
    }

    /// Update the status-line snapshot (from an idle-tick `Status` request, not gen-tagged).
    pub fn on_status(&mut self, report: StatusReport) {
        self.status = Some(report);
    }

    fn move_selection_up(&mut self, n: usize) {
        self.selected = self.selected.saturating_sub(n);
    }

    fn move_selection_down(&mut self, n: usize) {
        if self.results.is_empty() {
            self.selected = 0;
            return;
        }
        let last = self.results.len() - 1;
        self.selected = (self.selected + n).min(last);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }
    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    fn search(paths: &[(&str, bool)], total: u64) -> Response {
        Response::Search {
            building: false,
            total_hits: total,
            results: paths
                .iter()
                .map(|&(p, d)| SearchHit {
                    path: p.to_string(),
                    is_dir: d,
                })
                .collect(),
        }
    }

    fn result_paths(app: &AppState) -> Vec<String> {
        app.results().iter().map(|h| h.path.clone()).collect()
    }

    #[test]
    fn typing_dispatches_incrementing_generations() {
        let mut app = AppState::new("");
        assert_eq!(
            app.on_key(ch('a')),
            Some(Action::Dispatch {
                gen: 1,
                query: "a".into()
            })
        );
        assert_eq!(
            app.on_key(ch('b')),
            Some(Action::Dispatch {
                gen: 2,
                query: "ab".into()
            })
        );
    }

    #[test]
    fn stale_responses_are_discarded_and_final_query_wins() {
        // The landmine: a fast-typing burst whose replies arrive OUT OF ORDER must settle on the
        // final query's results.
        let mut app = AppState::new("");
        app.on_key(ch('a')); // gen 1
        app.on_key(ch('b')); // gen 2
        app.on_key(ch('c')); // gen 3 -> query "abc"

        app.on_response(1, search(&[("a-hit", false)], 1), 5); // stale
        assert!(app.results().is_empty(), "gen1 must be discarded");

        app.on_response(3, search(&[("abc-hit", false)], 1), 7); // latest -> applied
        assert_eq!(result_paths(&app), vec!["abc-hit"]);

        app.on_response(2, search(&[("ab-hit", false)], 1), 6); // stale, arrives late
        assert_eq!(
            result_paths(&app),
            vec!["abc-hit"],
            "a late stale reply must not overwrite the latest"
        );
        assert_eq!(app.last_rt_ms(), Some(7));
    }

    #[test]
    fn empty_query_clears_and_does_not_dispatch() {
        let mut app = AppState::new("");
        app.on_key(ch('a')); // gen 1, dispatch "a"
        app.on_response(1, search(&[("a-hit", false)], 1), 5);
        assert_eq!(result_paths(&app), vec!["a-hit"]);

        // Backspace to empty: no dispatch, list cleared, gen bumped so gen1's late reply is stale.
        assert_eq!(app.on_key(key(KeyCode::Backspace)), None);
        assert!(app.is_query_empty());
        assert!(app.results().is_empty());
        app.on_response(1, search(&[("a-hit", false)], 1), 5);
        assert!(
            app.results().is_empty(),
            "stale reply after clearing ignored"
        );
    }

    #[test]
    fn selection_moves_and_clamps() {
        let mut app = AppState::new("");
        app.set_viewport_rows(2);
        app.on_key(ch('x')); // gen 1
        app.on_response(
            1,
            search(
                &[
                    ("r0", false),
                    ("r1", false),
                    ("r2", false),
                    ("r3", false),
                    ("r4", false),
                ],
                5,
            ),
            3,
        );
        assert_eq!(app.selected(), 0);
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.selected(), 1);
        app.on_key(key(KeyCode::PageDown)); // +2 -> 3
        assert_eq!(app.selected(), 3);
        for _ in 0..10 {
            app.on_key(key(KeyCode::Down));
        }
        assert_eq!(app.selected(), 4, "clamped at last row");
        app.on_key(key(KeyCode::PageUp)); // -2 -> 2
        assert_eq!(app.selected(), 2);
        for _ in 0..10 {
            app.on_key(key(KeyCode::Up));
        }
        assert_eq!(app.selected(), 0, "clamped at first row");
    }

    #[test]
    fn a_new_query_resets_selection_to_top() {
        let mut app = AppState::new("");
        app.on_key(ch('x')); // gen 1
        app.on_response(1, search(&[("a", false), ("b", false), ("c", false)], 3), 1);
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.selected(), 2);
        app.on_key(ch('y')); // gen 2 -> new query "xy"
        app.on_response(2, search(&[("p", false), ("q", false)], 2), 1);
        assert_eq!(app.selected(), 0, "selection resets on a new result set");
    }

    #[test]
    fn enter_opens_current_row_without_waiting() {
        let mut app = AppState::new("");
        app.on_key(ch('x')); // gen 1
        app.on_response(1, search(&[("first", false), ("second", true)], 2), 1);
        app.on_key(key(KeyCode::Down));
        // Enter = open (what a person expects after picking a row)...
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Some(Action::Open("second".into()))
        );
        // ...Ctrl+Enter = print the path, which keeps `vim $(ef)` working.
        assert_eq!(
            app.on_key(ctrl(KeyCode::Enter)),
            Some(Action::Select("second".into()))
        );
    }

    #[test]
    fn enter_on_empty_results_does_nothing() {
        let mut app = AppState::new("");
        assert_eq!(app.on_key(key(KeyCode::Enter)), None);
    }

    #[test]
    fn ctrl_shortcuts_carry_the_selected_path() {
        let mut app = AppState::new("");
        app.on_key(ch('x')); // gen 1
        app.on_response(1, search(&[("C:\\a b\\file.txt", false)], 1), 1);
        assert_eq!(
            app.on_key(ctrl(KeyCode::Char('y'))),
            Some(Action::Copy("C:\\a b\\file.txt".into()))
        );
        assert_eq!(
            app.on_key(ctrl(KeyCode::Char('e'))),
            Some(Action::Reveal("C:\\a b\\file.txt".into()))
        );
        assert_eq!(
            app.on_key(ctrl(KeyCode::Char('o'))),
            Some(Action::Open("C:\\a b\\file.txt".into()))
        );
    }

    #[test]
    fn esc_and_ctrl_c_quit() {
        let mut app = AppState::new("");
        assert_eq!(app.on_key(key(KeyCode::Esc)), Some(Action::Quit));
        assert_eq!(app.on_key(ctrl(KeyCode::Char('c'))), Some(Action::Quit));
    }

    #[test]
    fn ctrl_c_is_quit_but_plain_c_is_input() {
        let mut app = AppState::new("");
        assert_eq!(
            app.on_key(ch('c')),
            Some(Action::Dispatch {
                gen: 1,
                query: "c".into()
            })
        );
    }

    #[test]
    fn cursor_edits_are_char_based_for_unicode() {
        let mut app = AppState::new("");
        for c in "検索".chars() {
            app.on_key(ch(c));
        }
        assert_eq!(app.query(), "検索");
        assert_eq!(app.cursor(), 2);
        app.on_key(key(KeyCode::Left));
        assert_eq!(app.cursor(), 1);
        app.on_key(ch('x')); // insert between the two CJK chars
        assert_eq!(app.query(), "検x索");
        app.on_key(key(KeyCode::Home));
        assert_eq!(app.cursor(), 0);
        app.on_key(key(KeyCode::Backspace)); // nothing before cursor
        assert_eq!(app.query(), "検x索");
    }

    #[test]
    fn astral_and_cjk_continuous_input_builds_the_query() {
        // crossterm combines a surrogate pair into ONE KeyCode::Char, so app.rs sees a single char
        // per astral glyph (🔍 = U+1F50D, 𝕏 = U+1D54F). The Vec<char> model builds the query
        // byte-length-agnostically. This PROVES app.rs is not the emoji bug (no byte/char mixing);
        // any real breakage is in the input layer (crossterm delivery); see
        // examples/probe_key_input.
        let mut app = AppState::new("");
        for c in "🔍検索𝕏a".chars() {
            app.on_key(ch(c));
        }
        assert_eq!(app.query(), "🔍検索𝕏a");
        assert_eq!(app.cursor(), 5); // 5 chars regardless of 4/3/3/4/1-byte encodings
        assert_eq!(app.query().chars().count(), app.cursor());
    }

    #[test]
    fn multibyte_insert_delete_and_nav_stay_consistent() {
        // Insert into the middle of a multi-byte query, then Delete/Backspace from both ends,
        // must never panic and must stay on char boundaries.
        let mut app = AppState::new("");
        for c in "🔍索".chars() {
            app.on_key(ch(c)); // "🔍索", cursor 2
        }
        app.on_key(key(KeyCode::Left)); // between 🔍 and 索
        app.on_key(ch('検')); // "🔍検索", cursor 2
        assert_eq!(app.query(), "🔍検索");
        app.on_key(key(KeyCode::Home));
        app.on_key(key(KeyCode::Delete)); // remove 🔍 (a 4-byte char)
        assert_eq!(app.query(), "検索");
        assert_eq!(app.cursor(), 0);
        app.on_key(key(KeyCode::End));
        app.on_key(key(KeyCode::Backspace)); // remove 索
        assert_eq!(app.query(), "検");
        assert!(app.cursor() <= app.query().chars().count());
    }

    #[test]
    fn pasted_text_with_emoji_is_inserted_char_wise() {
        let mut app = AppState::new("ab");
        app.on_key(key(KeyCode::Home)); // cursor 0
        let action = app.on_paste("🔍x");
        assert_eq!(app.query(), "🔍xab");
        assert!(matches!(action, Some(Action::Dispatch { .. })));
        // Control chars in a paste are dropped (single-line query); an all-control paste is a no-op.
        assert_eq!(app.on_paste("\n\t"), None);
        assert_eq!(app.query(), "🔍xab");
    }

    #[test]
    fn seeded_query_dispatches_from_the_shell() {
        let mut app = AppState::new("kernel");
        assert_eq!(app.query(), "kernel");
        assert_eq!(
            app.dispatch_current(),
            Some(Action::Dispatch {
                gen: 1,
                query: "kernel".into()
            })
        );
    }

    #[test]
    fn daemon_error_is_surfaced_then_cleared_by_next_search() {
        let mut app = AppState::new("");
        app.on_key(ch('x')); // gen 1
        app.on_response(
            1,
            Response::Error {
                code: crate::ipc::ErrCode::Internal,
                message: "boom".into(),
            },
            0,
        );
        assert_eq!(app.error(), Some("boom"));
        app.on_key(ch('y')); // gen 2
        app.on_response(2, search(&[("ok", false)], 1), 1);
        assert_eq!(app.error(), None);
        assert_eq!(result_paths(&app), vec!["ok"]);
    }
}
