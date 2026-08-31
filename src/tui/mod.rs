//! The `ef` interactive TUI (M4): incremental filename search over the daemon pipe.
//!
//! Key contracts:
//! - **Composability**: rendered to **stderr** + alternate screen; **stdout** carries only the
//!   single selected absolute path (so `vim $(ef)` works). [`run`] returns the selected path; the
//!   caller prints it to stdout *after* the terminal is restored.
//! - **Incremental search**: one IPC [`worker`], coalesce-to-latest, generation-tagged responses;
//!   stale responses are discarded in [`app::AppState`].
//! - **Highlight**: client-side byte-offset spans over the filename component ([`highlight`]).
//! - **Teardown**: a [`TerminalGuard`] restores raw mode / the alternate screen on every exit
//!   path, and a panic hook restores it *before* the backtrace prints (never a "broken terminal").

pub mod app;
pub mod clipboard;
pub mod highlight;
pub mod render;
pub mod shell;
pub mod win_input;
pub mod worker;

use std::io::{self, BufWriter, Stderr};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::cursor::Show;
use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::Terminal;

use app::{Action, AppState};
use worker::{Job, Reply};

/// Number of paths the TUI fetches per search. Scrolling
/// is clamped to the fetched rows; the status line's `total_hits` signals "narrow the query"
/// when there are more. No re-request on scroll-to-end in v0.1.
pub const FETCH_LIMIT: u32 = 200;

/// How often the idle loop refreshes the daemon status (status line).
const STATUS_REFRESH: Duration = Duration::from_secs(2);

/// A message into the shell's single event channel: either a terminal event (from the input
/// reader thread) or a worker reply. Merging them lets the loop block on one `recv`, with no polling.
pub enum Msg {
    Input(Event),
    Reply(Reply),
}

/// The production terminal: the crossterm backend over a **`BufWriter`**-wrapped stderr. The
/// buffer is essential: an unbuffered stderr makes ratatui emit ~318 tiny writes per frame, each
/// an ANSI console syscall on a real terminal (~190 ms/draw, the M4 latency root cause,
/// `examples/bench_draw_writes`). `BufWriter` collapses a frame to ~2 writes; `Terminal::draw`
/// flushes the `BufWriter` at the end of every frame.
type TuiTerminal = Terminal<CrosstermBackend<BufWriter<Stderr>>>;

/// Run the interactive TUI seeded with `initial_query` (empty for a bare `ef`). Returns
/// `Ok(Some(path))` when the user selects a row (the caller prints it to **stdout**), `Ok(None)`
/// when the user aborts (Esc / Ctrl+C, nothing to stdout), or `Err` if the daemon is unreachable.
///
/// The terminal is set up on **stderr** and restored before returning, so the caller's stdout
/// print lands on a clean terminal.
pub fn run(
    initial_query: &str,
    timeout: Duration,
    profile: bool,
    excludes_suffix: String,
    how: worker::SearchFlags,
) -> Result<Option<String>> {
    install_panic_hook();
    // Declare the guard BEFORE the terminal so that on an **unwind** the terminal (its `BufWriter`
    // flush) drops FIRST and the guard (screen restore) drops LAST, so any buffered draw bytes flush
    // while still in the alternate screen, never onto the restored normal screen. If
    // `setup_terminal` fails partway, the guard still cleans up raw mode / the alt screen.
    let guard = TerminalGuard;
    let mut terminal = setup_terminal().context("entering the alternate screen")?;

    let mut prof = Profiler::new(profile);
    let result = event_loop(
        &mut terminal,
        initial_query,
        timeout,
        &mut prof,
        excludes_suffix,
        how,
    );

    // Normal path: the same order explicitly. Flush the terminal, then restore the screen (before
    // the caller prints the selected path to stdout). `Terminal::draw` already flushes per frame,
    // so the buffer is empty here; this is the belt-and-braces ordering the flush audit requires.
    drop(terminal);
    drop(guard);
    prof.report();
    result
}

fn event_loop(
    terminal: &mut TuiTerminal,
    initial_query: &str,
    timeout: Duration,
    prof: &mut Profiler,
    excludes_suffix: String,
    how: worker::SearchFlags,
) -> Result<Option<String>> {
    let (tx, rx) = mpsc::channel::<Msg>();
    let (job_tx, job_rx) = mpsc::channel::<Job>();

    // Worker thread: blocking pipe round trips off the UI thread.
    {
        let tx = tx.clone();
        thread::Builder::new()
            .name("ef-tui-worker".into())
            .spawn(move || worker::worker_loop(job_rx, tx, timeout, excludes_suffix, how))
            .context("spawning the IPC worker")?;
    }
    // Input reader thread: forwards terminal events into the merged channel. `EF_INPUT=win`
    // (default) uses the native `ReadConsoleInputW` reader that fixes astral (emoji) input
    // (crossterm #561); `EF_INPUT=crossterm` is the M4 rollback path.
    {
        let tx = tx.clone();
        let use_win = input_backend_is_win();
        thread::Builder::new()
            .name("ef-tui-input".into())
            .spawn(move || {
                if use_win {
                    win_input::read_loop(tx);
                } else {
                    while let Ok(ev) = event::read() {
                        if tx.send(Msg::Input(ev)).is_err() {
                            break;
                        }
                    }
                }
            })
            .context("spawning the input reader")?;
    }

    let mut ui = Ui::new(initial_query);
    // Kick off the seeded search (if any) and an initial status fetch.
    if let Some(Action::Dispatch { gen, query }) = ui.app.dispatch_current() {
        ui.pending = Some((gen, Instant::now()));
        let _ = job_tx.send(Job::Search { gen, query });
    }
    let _ = job_tx.send(Job::Status);

    let mut draws = 0u64;
    run_ui_loop(terminal, &mut ui, &rx, &job_tx, prof, &mut draws)
}

/// The UI state carried across loop iterations (separated from the terminal/threads so the loop
/// is testable with a `TestBackend` and a scripted channel).
struct Ui {
    app: AppState,
    /// The latest dispatched (gen, dispatch-time), for the key->render measurement.
    pending: Option<(u64, Instant)>,
    /// Set when a matching search reply is applied; the next draw records the elapsed time.
    measure_after_draw: Option<Instant>,
    /// Whether the visible state changed since the last draw (so a batch of no-op events,
    /// such as key releases or focus, does not force a redraw).
    dirty: bool,
    /// The result list's scroll position, kept across frames.
    ///
    /// ratatui computes the window from `ListState::offset` and writes the result back, so a
    /// fresh `ListState` per frame starts every draw at offset 0. It then scrolls forward just
    /// far enough to include the selection, which pins the highlight to the *bottom* row and
    /// makes Up scroll the list down instead of moving the cursor. Past the first page nothing
    /// above the selection could be reached.
    list: ratatui::widgets::ListState,
}

/// What the loop should do after handling one message.
enum Control {
    Continue,
    Exit(Option<String>),
}

impl Ui {
    fn new(initial_query: &str) -> Self {
        Self {
            list: ratatui::widgets::ListState::default(),
            app: AppState::new(initial_query),
            pending: None,
            measure_after_draw: None,
            dirty: false,
        }
    }

    /// Apply one message to the state, performing side effects (dispatch a search, clipboard,
    /// ShellExecute) and marking [`dirty`](Ui::dirty) when the display must be redrawn. Returns
    /// [`Control::Exit`] on select/abort. **Never draws**: the loop draws once per batch.
    fn handle(&mut self, msg: Msg, job_tx: &Sender<Job>) -> Result<Control> {
        match msg {
            // Key releases (Windows delivers them) and other no-op events must NOT force a redraw;
            // that is the per-event-draw pile-up that inflated key->render latency.
            Msg::Input(Event::Key(key)) if key.kind == KeyEventKind::Release => {}
            Msg::Input(Event::Key(key)) => {
                self.dirty = true;
                match self.app.on_key(key) {
                    Some(Action::Dispatch { gen, query }) => {
                        self.pending = Some((gen, Instant::now()));
                        let _ = job_tx.send(Job::Search { gen, query });
                    }
                    Some(Action::Select(path)) => return Ok(Control::Exit(Some(path))),
                    Some(Action::Quit) => return Ok(Control::Exit(None)),
                    Some(Action::Copy(path)) => {
                        let _ = clipboard::set_unicode_text(&path);
                    }
                    Some(Action::Reveal(path)) => {
                        let _ = shell::reveal_in_explorer(&path);
                    }
                    Some(Action::Open(path)) => {
                        let _ = shell::open_with_associated(&path);
                    }
                    None => {}
                }
            }
            // Bracketed paste (real paste, and some IMEs / the emoji panel): insert the text.
            Msg::Input(Event::Paste(text)) => {
                self.dirty = true;
                if let Some(Action::Dispatch { gen, query }) = self.app.on_paste(&text) {
                    self.pending = Some((gen, Instant::now()));
                    let _ = job_tx.send(Job::Search { gen, query });
                }
            }
            Msg::Input(Event::Resize(_, _)) => self.dirty = true,
            Msg::Input(_) => {} // focus / mouse: no redraw needed
            Msg::Reply(Reply::Search { gen, resp, rt_ms }) => {
                self.app.on_response(gen, resp, rt_ms);
                self.dirty = true;
                if let Some((g, t0)) = self.pending {
                    if g == gen {
                        self.measure_after_draw = Some(t0);
                        self.pending = None;
                    }
                }
            }
            Msg::Reply(Reply::Status(report)) => {
                self.app.on_status(report);
                self.dirty = true;
            }
            Msg::Reply(Reply::Disconnected(message)) => anyhow::bail!(message),
        }
        Ok(Control::Continue)
    }
}

/// The event loop, generic over the backend so it runs under a real terminal in production and a
/// `TestBackend` in tests. **Batches**: block for one message, drain the rest without drawing,
/// then draw **once** if anything changed. `draws` counts real draw calls (a test asserts the
/// batch collapses a burst of events into ~1 draw, the key->render regression guard).
fn run_ui_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    ui: &mut Ui,
    rx: &Receiver<Msg>,
    job_tx: &Sender<Job>,
    prof: &mut Profiler,
    draws: &mut u64,
) -> Result<Option<String>> {
    draw_once(terminal, ui, prof)?;
    *draws += 1;

    loop {
        let first = match rx.recv_timeout(STATUS_REFRESH) {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) => {
                let _ = job_tx.send(Job::Status); // idle status refresh; its reply marks dirty
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        };
        if let Control::Exit(outcome) = ui.handle(first, job_tx)? {
            return Ok(outcome);
        }
        // Drain everything else already queued (all pending keys + replies) WITHOUT drawing
        // between them, so a whole burst collapses to a single redraw.
        while let Ok(msg) = rx.try_recv() {
            if let Control::Exit(outcome) = ui.handle(msg, job_tx)? {
                return Ok(outcome);
            }
        }
        if ui.dirty {
            draw_once(terminal, ui, prof)?;
            *draws += 1;
            ui.dirty = false;
        }
    }
}

/// Draw one frame and, if a search reply is awaiting its render, record the key->render time.
fn draw_once<B: Backend>(
    terminal: &mut Terminal<B>,
    ui: &mut Ui,
    prof: &mut Profiler,
) -> io::Result<()> {
    let height = terminal.size().map(|s| s.height).unwrap_or(0);
    ui.app
        .set_viewport_rows((height as usize).saturating_sub(2).max(1));
    let draw_start = Instant::now();
    let Ui { app, list, .. } = &mut *ui;
    terminal.draw(|f| render::render(f, app, list))?;
    prof.record_draw(draw_start.elapsed()); // real-terminal flush cost (the diagnostic stage)
    if let Some(t0) = ui.measure_after_draw.take() {
        prof.record(t0.elapsed());
    }
    Ok(())
}

// --- terminal lifecycle ---

/// Restores raw mode + the alternate screen on drop, covering normal return, `?`-propagation, and
/// unwind. A missed restore is a "broken terminal": the top-priority landmine.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = restore_terminal();
    }
}

/// The input backend: `win` (default) = the native `ReadConsoleInputW` reader (astral-input fix);
/// `crossterm` = the M4 `event::read` path (rollback). Selected by `EF_INPUT`.
fn input_backend_is_win() -> bool {
    !matches!(std::env::var("EF_INPUT").ok().as_deref(), Some("crossterm"))
}

fn setup_terminal() -> io::Result<TuiTerminal> {
    enable_raw_mode()?;
    execute!(io::stderr(), EnterAlternateScreen)?;
    // Bracketed paste is only usable by the crossterm reader (it parses the ESC markers into one
    // `Event::Paste`). The native win reader would mis-parse those markers, so it stays OFF there,
    // a paste then arrives as ordinary key records (astral included), which is exactly the fix.
    if !input_backend_is_win() {
        execute!(io::stderr(), EnableBracketedPaste)?;
    }
    // BufWriter is the fix: one frame = buffered writes + a single flush (~2 syscalls) instead of
    // ~318 unbuffered console writes. `Terminal::draw` flushes it per frame.
    Terminal::new(CrosstermBackend::new(BufWriter::new(io::stderr())))
}

fn restore_terminal() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        io::stderr(),
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    )?;
    Ok(())
}

/// Restore the terminal *before* the default panic hook prints the backtrace; otherwise it would
/// be swallowed by the alternate screen and the terminal left in raw mode.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        prev(info);
    }));
}

// --- key -> render latency profiler (`ef -i --profile`) ---

/// Collects, when enabled, key->render times (ms) and per-draw `terminal.draw()` times (µs) plus
/// the total draw count, to split the budget on real hardware between "draw flush is slow" (many
/// unbuffered console writes per frame -> `bench_draw_writes`) and "too many draws per batch".
struct Profiler {
    on: bool,
    samples: Vec<u128>,
    draw_us: Vec<u128>,
}

impl Profiler {
    fn new(on: bool) -> Self {
        Self {
            on,
            samples: Vec::new(),
            draw_us: Vec::new(),
        }
    }

    /// Record one key->render time (dispatch -> the draw showing its reply).
    fn record(&mut self, d: Duration) {
        if self.on {
            self.samples.push(d.as_millis());
        }
    }

    /// Record one `terminal.draw()` call time (render logic + real-terminal flush).
    fn record_draw(&mut self, d: Duration) {
        if self.on {
            self.draw_us.push(d.as_micros());
        }
    }

    #[cfg(test)]
    fn sample_count(&self) -> usize {
        self.samples.len()
    }

    fn report(&self) {
        if !self.on {
            return;
        }
        if !self.samples.is_empty() {
            let mut s = self.samples.clone();
            s.sort_unstable();
            eprintln!(
                "ef --profile: key->render  n={}  p50={}ms  p95={}ms  max={}ms",
                s.len(),
                percentile(&s, 0.50),
                percentile(&s, 0.95),
                s.last().copied().unwrap_or(0),
            );
        }
        if !self.draw_us.is_empty() {
            let mut d = self.draw_us.clone();
            d.sort_unstable();
            // Draw time in ms (2 dp): this is the stage that dominates on a real terminal.
            let ms = |x: u128| x as f64 / 1000.0;
            eprintln!(
                "ef --profile: terminal.draw  n={}  p50={:.2}ms  p95={:.2}ms  max={:.2}ms  (n draws ~= n key->render batches if 1 draw/batch)",
                d.len(),
                ms(percentile(&d, 0.50)),
                ms(percentile(&d, 0.95)),
                ms(*d.last().unwrap()),
            );
        }
    }
}

fn percentile(sorted: &[u128], p: f64) -> u128 {
    let i = ((sorted.len() as f64 * p) as usize).min(sorted.len() - 1);
    sorted[i]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{Response, SearchHit};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventState, KeyModifiers};
    use ratatui::backend::TestBackend;

    fn press(c: char) -> Msg {
        Msg::Input(Event::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::NONE,
        )))
    }
    fn release(c: char) -> Msg {
        Msg::Input(Event::Key(KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        }))
    }
    fn key(code: KeyCode) -> Msg {
        Msg::Input(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }
    fn ctrl_key(code: KeyCode) -> Msg {
        Msg::Input(Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL)))
    }
    fn search_reply(gen: u64, path: &str) -> Msg {
        Msg::Reply(Reply::Search {
            gen,
            resp: Response::Search {
                total_hits: 1,
                results: vec![SearchHit {
                    path: path.into(),
                    is_dir: false,
                }],
                building: false,
            },
            rt_ms: 12,
        })
    }

    /// Preload a channel with `msgs`, then run the (batched) loop over a `TestBackend` until the
    /// channel disconnects or an exit action fires. Returns (outcome, draw count, final ui,
    /// key->render sample count). A non-empty `seed` mirrors the real seeded-query kickoff.
    fn run_preloaded(msgs: Vec<Msg>, seed: &str) -> (Option<String>, u64, Ui, usize) {
        let (tx, rx) = mpsc::channel::<Msg>();
        let (job_tx, _job_rx) = mpsc::channel::<Job>();
        for m in msgs {
            tx.send(m).unwrap();
        }
        drop(tx); // after the batch, the next recv returns Disconnected -> Ok(None)

        let mut ui = Ui::new(seed);
        if !seed.is_empty() {
            if let Some(Action::Dispatch { gen, .. }) = ui.app.dispatch_current() {
                ui.pending = Some((gen, Instant::now()));
            }
        }
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let mut prof = Profiler::new(true);
        let mut draws = 0u64;
        let outcome = run_ui_loop(&mut term, &mut ui, &rx, &job_tx, &mut prof, &mut draws).unwrap();
        (outcome, draws, ui, prof.sample_count())
    }

    #[test]
    fn a_burst_of_events_collapses_to_one_draw_and_applies_the_reply() {
        // **Regression guard for the ~200 ms key->render.** A burst of many events (6 keys + their
        // Windows key-release events + the search reply) must draw ONCE for the batch, not once
        // per event: per-event draws serialized real-terminal writes ahead of the reply.
        let mut msgs = Vec::new();
        for c in "kernel".chars() {
            msgs.push(press(c));
            msgs.push(release(c)); // no-op events must add zero draws
        }
        // Seed "" (no kickoff) -> the 6 chars dispatch gen 1..6; the worker coalesces to gen 6.
        msgs.push(search_reply(6, r"C:\Windows\System32\kernel32.dll"));

        let (outcome, draws, ui, samples) = run_preloaded(msgs, "");
        assert_eq!(outcome, None); // channel closed -> abort
                                   // Initial draw + exactly ONE batch draw. The 13 events did NOT cause ~14 draws.
        assert_eq!(
            draws, 2,
            "expected batched draws (initial + one), got {draws}"
        );
        // The reply was applied even though it was batched behind the keys (no deferral).
        assert_eq!(ui.app.results().len(), 1);
        assert_eq!(
            ui.app.results()[0].path,
            r"C:\Windows\System32\kernel32.dll"
        );
        // The key->render sample was recorded (reply gen matched the latest pending gen).
        assert_eq!(
            samples, 1,
            "the settled reply must be measured exactly once"
        );
    }

    #[test]
    fn release_only_batch_does_not_redraw() {
        // A batch of only key-release / no-op events must not draw beyond the initial frame.
        let (_outcome, draws, _ui, _s) = run_preloaded(vec![release('a'), release('b')], "");
        assert_eq!(draws, 1, "release-only batch must not redraw");
    }

    #[test]
    fn pasted_text_reaches_the_query_through_the_loop() {
        // Input-layer integration (NOT the AppState unit): a bracketed-paste event must flow
        // through Ui::handle into the query. This is the astral workaround path, when the OS/
        // terminal drops astral KEY events (the measured `probe_key_input` finding), a paste of the
        // same text still reaches the query. AppState char-correctness is covered separately.
        let (_outcome, _draws, ui, _s) =
            run_preloaded(vec![Msg::Input(Event::Paste("🔍検索".into()))], "");
        assert_eq!(ui.app.query(), "🔍検索");
    }

    #[test]
    fn a_seeded_reply_is_drawn_then_ctrl_enter_prints_it() {
        // Seed "kernel" -> kickoff dispatches gen 1; its reply (gen 1) is applied and drawn,
        // then Ctrl+Enter emits the row's path on stdout (plain Enter opens it instead).
        let msgs = vec![search_reply(1, r"C:\hit.txt"), ctrl_key(KeyCode::Enter)];
        let (outcome, _draws, _ui, _samples) = run_preloaded(msgs, "kernel");
        // Enter exits during the same batch (before the post-batch draw), so no sample is
        // recorded here; the burst test above covers measurement.
        assert_eq!(outcome, Some(r"C:\hit.txt".to_string()));
    }

    #[test]
    fn plain_enter_opens_and_does_not_emit_a_path() {
        // Plain Enter now means "open the selection" (ShellExecuteW), so the TUI must NOT
        // exit with a stdout path; that is Ctrl+Enter's job. Guards the composability
        // contract from regressing back onto the key people press to open things.
        let msgs = vec![search_reply(1, r"C:\hit.txt"), key(KeyCode::Enter)];
        let (outcome, _draws, _ui, _samples) = run_preloaded(msgs, "kernel");
        assert_eq!(outcome, None, "plain Enter must not print a path");
    }
}
