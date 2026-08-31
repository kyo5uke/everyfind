//! ratatui rendering for the TUI: input line + result list + status
//! line, with client-side match highlighting. Pure draw over [`AppState`] (no IO) so it is
//! tested headlessly with ratatui's `TestBackend`.
//!
//! Widths are left entirely to ratatui/unicode-width: match spans are sliced by **byte offset**
//! (via [`filename_spans`]) into `Span`s, never by column (P17: width != bytes for CJK/emoji).

use crate::commas;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::ipc::SearchHit;
use crate::tui::app::AppState;
use crate::tui::highlight::query_spans;

const PROMPT: &str = "> ";

/// Draw the whole UI for one frame.
pub fn render(frame: &mut Frame, app: &AppState, list: &mut ListState) {
    let areas = Layout::vertical([
        Constraint::Length(1), // input line
        Constraint::Min(1),    // results / hint
        Constraint::Length(1), // status line
    ])
    .split(frame.area());
    let (input_area, body_area, status_area) = (areas[0], areas[1], areas[2]);

    render_input(frame, app, input_area);
    if app.is_query_empty() {
        render_hint(frame, body_area);
    } else {
        render_results(frame, app, list, body_area);
    }
    render_status(frame, app, status_area);
}

fn render_input(frame: &mut Frame, app: &AppState, area: Rect) {
    let query = app.query();
    let line = Line::from(vec![
        Span::styled(PROMPT, Style::default().fg(Color::Cyan)),
        Span::raw(query.clone()),
    ]);
    // Where the cursor sits within the line, prompt included.
    let before: String = query.chars().take(app.cursor()).collect();
    let want = PROMPT.len() as u16 + Line::from(before).width() as u16;

    // Scroll the line so that column stays on screen. Without this the paragraph was simply
    // truncated at the right edge and the cursor clamped to it: past the width of the terminal
    // nothing new appeared, backspace gave no feedback, and the box read as hung, while the
    // query really was changing and the results below really were updating. The seeded case
    // hits it immediately, since `ef --in <dir>` starts with a `path:` term already typed.
    let last = area.width.saturating_sub(1);
    let scroll = want.saturating_sub(last);
    frame.render_widget(Paragraph::new(line).scroll((0, scroll)), area);
    frame.set_cursor_position(Position {
        x: area.x + want - scroll,
        y: area.y,
    });
}

fn render_results(frame: &mut Frame, app: &AppState, state: &mut ListState, area: Rect) {
    let query = app.query();
    let items: Vec<ListItem> = app
        .results()
        .iter()
        .map(|hit| ListItem::new(result_line(hit, &query)))
        .collect();
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    // `state` outlives the frame on purpose; see `Ui::list`. ratatui reads the offset, scrolls
    // it just enough to include the selection, and writes it back; discarding it every frame is
    // what pinned the highlight to the bottom row.
    state.select(if app.results().is_empty() {
        None
    } else {
        Some(app.selected())
    });
    frame.render_stateful_widget(list, area, state);
}

/// Compose one result row: the path with the query occurrences highlighted in the filename, plus
/// a trailing `\` marker for directories.
fn result_line(hit: &SearchHit, query: &str) -> Line<'static> {
    let path = &hit.path;
    let hl = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    let mut spans: Vec<Span> = Vec::new();
    let mut pos = 0usize;
    for (s, e) in query_spans(path, query) {
        if s > pos {
            spans.push(Span::raw(path[pos..s].to_string()));
        }
        spans.push(Span::styled(path[s..e].to_string(), hl));
        pos = e;
    }
    if pos < path.len() {
        spans.push(Span::raw(path[pos..].to_string()));
    }
    if hit.is_dir {
        spans.push(Span::styled(
            "\\".to_string(),
            Style::default().fg(Color::Blue),
        ));
    }
    Line::from(spans)
}

fn render_hint(frame: &mut Frame, area: Rect) {
    let dim = Style::default().fg(Color::DarkGray);
    let lines = vec![
        Line::from(""),
        Line::styled("  Type to search filenames.", dim),
        Line::styled(
            // ASCII only: `↑↓` and `·` are East-Asian *ambiguous* width: one
            // cell in Windows Terminal, two in classic conhost with a Japanese
            // font. ratatui counts one, so the line rendered wider than its
            // buffer said and the tail of "quit" survived every redraw.
            "  Up/Down move | Enter open | ^E reveal | ^Y copy | ^Enter print | Esc quit",
            dim,
        ),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_status(frame: &mut Frame, app: &AppState, area: Rect) {
    let (text, style) = match app.error() {
        Some(err) => (
            format!(" error: {err} "),
            Style::default().fg(Color::White).bg(Color::Red),
        ),
        None => (
            status_text(app),
            Style::default().fg(Color::Black).bg(Color::Gray),
        ),
    };
    frame.render_widget(Paragraph::new(Line::from(text)).style(style), area);
}

fn status_text(app: &AppState) -> String {
    let total = app.total_hits();
    let shown = app.results().len() as u64;
    let hits = if total > shown {
        format!(
            "{} hits (showing {shown} - narrow to refine)",
            commas(total)
        )
    } else {
        format!("{} hits", commas(total))
    };
    let mut parts = vec![hits];
    if let Some(s) = app.status() {
        parts.push(format!("{} indexed", commas(s.entries)));
        parts.push(format!("lag {}", commas(s.usn_lag)));
    }
    if let Some(ms) = app.last_rt_ms() {
        parts.push(format!("{ms}ms"));
    }
    format!(" {} ", parts.join(" | "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::StatusReport;
    use crate::ipc::{Response, PROTO_VERSION};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

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

    fn status(entries: u64, lag: u64) -> StatusReport {
        StatusReport {
            drive: 'C',
            entries,
            live_entries: entries,
            sizes_resolved: entries,
            usn_lag: lag,
            last_sync_secs: 0,
            working_set: 0,
            private_usage: 0,
            building: false,
            build_progress: 0,
            snapshot_generation: 0,
            last_snapshot_secs: None,
            snapshot_cursor: 0,
            uptime_secs: 0,
            poll_interval_ms: 1000,
            proto_version: PROTO_VERSION,
            pid: 1,
        }
    }

    fn draw(app: &AppState, w: u16, h: u16) -> Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render(f, app, &mut ListState::default()))
            .unwrap();
        term.backend().buffer().clone()
    }

    /// The visible text of buffer row `y`.
    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area().width)
            .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
            .collect()
    }

    /// The (x, symbol) of every cell in row `y` whose foreground is `fg`.
    fn cells_with_fg(buf: &Buffer, y: u16, fg: Color) -> Vec<(u16, String)> {
        (0..buf.area().width)
            .filter_map(|x| {
                let c = buf.cell((x, y))?;
                (c.fg == fg).then(|| (x, c.symbol().to_string()))
            })
            .collect()
    }

    #[test]
    fn renders_input_results_and_status() {
        let mut app = AppState::new("ker");
        app.dispatch_current(); // req_gen -> 1 (the shell kicks off the seeded search)
        app.on_status(status(6_000_000, 0));
        app.on_response(1, search(&[("C:\\Windows\\kernel32.dll", false)], 1), 8);
        let buf = draw(&app, 60, 6);
        assert!(row(&buf, 0).starts_with("> ker"), "input line");
        // The result path appears somewhere in the body.
        let body: String = (1..5).map(|y| row(&buf, y)).collect();
        assert!(
            body.contains("kernel32.dll"),
            "result row missing: {body:?}"
        );
        // Status line shows hit count, index size, and rt.
        let status = row(&buf, 5);
        assert!(status.contains("1 hits"), "status: {status:?}");
        assert!(status.contains("6,000,000 indexed"), "status: {status:?}");
        assert!(status.contains("8ms"), "status: {status:?}");
    }

    #[test]
    fn highlights_the_query_in_the_filename() {
        // Two results so we can inspect a NON-selected row (the selected row is REVERSED, which
        // would recolor). Row 0 is selected; check the highlight on row 1.
        let mut app = AppState::new("aa");
        app.dispatch_current(); // req_gen -> 1
        app.on_response(
            1,
            search(
                &[("C:\\x\\first.txt", false), ("C:\\y\\aa_here.txt", false)],
                2,
            ),
            1,
        );
        let buf = draw(&app, 40, 6);
        // Row 1 is the second result; "aa" in the filename must be Yellow.
        let yellow: String = cells_with_fg(&buf, 2, Color::Yellow)
            .into_iter()
            .map(|(_, s)| s)
            .collect();
        assert_eq!(
            yellow, "aa",
            "expected 'aa' highlighted yellow, got {yellow:?}"
        );
    }

    #[test]
    fn directory_hit_gets_a_trailing_marker() {
        let mut app = AppState::new("sys");
        app.dispatch_current(); // req_gen -> 1
        app.on_response(1, search(&[("C:\\Windows\\System32", true)], 1), 1);
        let buf = draw(&app, 40, 4);
        let body: String = (1..3).map(|y| row(&buf, y)).collect();
        assert!(body.contains("System32\\"), "dir marker missing: {body:?}");
    }

    #[test]
    fn empty_query_shows_the_hint_not_a_list() {
        let app = AppState::new("");
        let buf = draw(&app, 80, 6);
        let body: String = (1..5).map(|y| row(&buf, y)).collect();
        assert!(body.contains("Enter open"), "hint missing: {body:?}");
        assert!(body.contains("Esc quit"), "hint missing: {body:?}");
    }

    /// The chrome (hint, separators, messages) must stay ASCII: `↑`, `·` and the em dash are
    /// East-Asian *ambiguous* width: one cell in Windows Terminal, two in
    /// classic conhost with a Japanese font. ratatui budgets one, the line then
    /// renders wider than its buffer says, and the overhang survives every
    /// redraw (seen live: the `it` of `Esc quit` sitting inside the results).
    #[test]
    fn the_chrome_is_ascii_so_every_terminal_agrees_on_its_width() {
        let hint = AppState::new("");
        let buf = draw(&hint, 90, 6);
        for y in 0..6 {
            let r = row(&buf, y);
            assert!(r.is_ascii(), "non-ASCII chrome in row {y}: {r:?}");
        }

        let mut app = AppState::new("dll");
        app.dispatch_current();
        app.on_response(1, search(&[("C:\\a.dll", false)], 104_000), 12);
        let status = row(&draw(&app, 90, 6), 5);
        assert!(status.is_ascii(), "non-ASCII status: {status:?}");
    }

    #[test]
    fn status_signals_truncation_when_more_hits_than_shown() {
        let mut app = AppState::new("dll");
        app.dispatch_current(); // req_gen -> 1
        app.on_response(
            1,
            search(&[("C:\\a.dll", false), ("C:\\b.dll", false)], 104_000),
            12,
        );
        let buf = draw(&app, 70, 6);
        let status = row(&buf, 5);
        assert!(status.contains("104,000 hits"), "status: {status:?}");
        assert!(status.contains("narrow to refine"), "status: {status:?}");
    }

    /// A query longer than the terminal is wide has to scroll, not vanish. `ef --in <dir>`
    /// seeds a `path:` term that is already half a line, so this is the first thing a
    /// context-menu search does. Before, the paragraph was truncated at the right edge and the
    /// cursor clamped there: every further keystroke was invisible.
    #[test]
    fn a_long_query_scrolls_so_the_cursor_stays_visible() {
        let long = r"path:C:\Users\me\Documents\Projects\everyfind\src kernel";
        let mut app = AppState::new(long);
        app.set_viewport_rows(4);
        let width = 40u16;
        let buf = draw(&app, width, 6);
        let input = row(&buf, 0);

        // The end of what was typed is on screen; the start has scrolled off.
        assert!(
            input.contains("kernel"),
            "the tail of the query must be visible: {input:?}"
        );
        assert!(
            !input.contains("path:C:"),
            "the head must have scrolled off a 40-column line: {input:?}"
        );
    }
}
