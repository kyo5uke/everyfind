//! Regression guard for the M4 key->render latency root cause: one `terminal.draw()` must be a
//! handful of `write()` calls, not hundreds. The TUI wraps stderr in a `BufWriter` so a frame
//! becomes ~2 writes; an **unbuffered** writer makes ratatui emit ~300 tiny writes per frame,
//! each an ANSI console syscall on a real terminal (~190 ms/draw, measured, `examples/
//! bench_draw_writes`). If someone drops the `BufWriter`, this test fails.

use std::cell::RefCell;
use std::io::{self, BufWriter, Write};
use std::rc::Rc;

use everyfind::ipc::{Response, SearchHit};
use everyfind::tui::app::AppState;
use everyfind::tui::render::render;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// Counts `write()` calls, forwarding to a sink.
struct Counting {
    writes: Rc<RefCell<usize>>,
    inner: io::Sink,
}
impl Write for Counting {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        *self.writes.borrow_mut() += 1;
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn app_with_results(n: usize) -> AppState {
    let mut app = AppState::new("kernel");
    app.dispatch_current();
    let results = (0..n)
        .map(|i| SearchHit {
            path: format!(r"C:\Windows\System32\drivers\module_{i:04}_kernel32.dll"),
            is_dir: i % 7 == 0,
        })
        .collect();
    app.on_response(
        1,
        Response::Search {
            total_hits: 123_456,
            results,
            building: false,
        },
        12,
    );
    app
}

/// Draw one full frame (200 results) and return the number of `write()` calls, with the writer
/// wrapped in a `BufWriter` or not.
fn writes_for_one_frame(buffered: bool) -> usize {
    let writes = Rc::new(RefCell::new(0usize));
    let counting = Counting {
        writes: Rc::clone(&writes),
        inner: io::sink(),
    };
    let app = app_with_results(200);
    if buffered {
        let mut term = Terminal::new(CrosstermBackend::new(BufWriter::new(counting))).unwrap();
        term.draw(|f| render(f, &app, &mut ratatui::widgets::ListState::default()))
            .unwrap();
        // Drop flushes the BufWriter; count settles after.
        drop(term);
    } else {
        let mut term = Terminal::new(CrosstermBackend::new(counting)).unwrap();
        term.draw(|f| render(f, &app, &mut ratatui::widgets::ListState::default()))
            .unwrap();
        drop(term);
    }
    let n = *writes.borrow();
    n
}

#[test]
fn bufwriter_collapses_a_frame_to_a_couple_of_writes() {
    let unbuffered = writes_for_one_frame(false);
    let buffered = writes_for_one_frame(true);

    // Unbuffered: ratatui emits one write per styled run / cursor move, hundreds for a full
    // frame (measured ~318). Assert it is clearly "many".
    assert!(
        unbuffered > 50,
        "expected an unbuffered frame to be many small writes, got {unbuffered}"
    );
    // Buffered: the whole frame coalesces to ~1 write + the flush. Assert it is tiny.
    assert!(
        buffered <= 4,
        "BufWriter must collapse a frame to a few writes, got {buffered}"
    );
    // And the collapse must be dramatic (the actual M4 fix: 318 -> ~2).
    assert!(
        unbuffered >= buffered * 10,
        "BufWriter must cut writes by an order of magnitude ({unbuffered} vs {buffered})"
    );
}
