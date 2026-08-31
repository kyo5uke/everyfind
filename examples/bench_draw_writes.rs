//! M4 latency diagnosis (round 2): count the **write() syscalls per `terminal.draw()`**.
//!
//! Hypothesis #3 (reviewer): `CrosstermBackend` writes are **unbuffered**, so one frame becomes
//! hundreds of tiny writes; on a real Windows console each write is an expensive syscall (ANSI
//! processing) -> a single draw can cost ~100-200 ms even though the render *logic* is 0.23 ms
//! (`bench_render`) and TestBackend (no writes) is fast. This harness counts the writes directly,
//! unbuffered vs wrapped in a `BufWriter`, against an `io::sink()` (so it runs anywhere; the
//! COUNT is the mechanism; the per-write cost only materializes on a real console).
//!
//! ```text
//! cargo run --release --example bench_draw_writes
//! ```

use std::cell::RefCell;
use std::io::{self, BufWriter, Write};
use std::rc::Rc;

use everyfind::ipc::{Response, SearchHit};
use everyfind::tui::app::AppState;
use everyfind::tui::render::render;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

#[derive(Default, Clone, Copy)]
struct Counts {
    writes: usize,
    bytes: usize,
    flushes: usize,
}

/// A writer that counts write()/flush() calls (the "syscalls" a real console would pay for),
/// forwarding to an inner sink.
struct Counting<W> {
    inner: W,
    counts: Rc<RefCell<Counts>>,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut c = self.counts.borrow_mut();
        c.writes += 1;
        c.bytes += buf.len();
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.counts.borrow_mut().flushes += 1;
        self.inner.flush()
    }
}

fn results(n: usize) -> Response {
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        v.push(SearchHit {
            path: format!(r"C:\Windows\System32\drivers\module_{i:04}_kernel32.dll"),
            is_dir: i % 7 == 0,
        });
    }
    Response::Search {
        total_hits: 123_456,
        results: v,
        building: false,
    }
}

fn app_with(query: &str, n: usize) -> AppState {
    let mut app = AppState::new(query);
    app.dispatch_current();
    app.on_response(1, results(n), 12);
    app
}

/// Draw `frames` frames (alternating the selection so every frame produces a real diff) and
/// return the average write/byte/flush counts per frame.
fn measure<W: Write, F: FnMut() -> Terminal<CrosstermBackend<W>>>(
    label: &str,
    counts: Rc<RefCell<Counts>>,
    mut make_terminal: F,
) {
    let mut app = app_with("kernel", 200);
    let mut term = make_terminal();
    // Warm (first draw is always full); then reset the counter.
    term.draw(|f| render(f, &app, &mut ratatui::widgets::ListState::default()))
        .unwrap();
    *counts.borrow_mut() = Counts::default();

    let frames = 30u32;
    for i in 0..frames {
        if i % 2 == 0 {
            app.on_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Down,
                crossterm::event::KeyModifiers::NONE,
            ));
        } else {
            app.on_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Up,
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        term.draw(|f| render(f, &app, &mut ratatui::widgets::ListState::default()))
            .unwrap();
    }
    let c = *counts.borrow();
    println!(
        "{label:<34} writes/frame={:>5.0}  bytes/frame={:>6.0}  flushes/frame={:>4.1}",
        c.writes as f64 / frames as f64,
        c.bytes as f64 / frames as f64,
        c.flushes as f64 / frames as f64,
    );
}

fn main() {
    println!("== write() calls per terminal.draw() (120x40, 200 results, selection-move diff) ==");

    // Unbuffered (current production: CrosstermBackend::new(io::stderr()); stderr is unbuffered).
    let counts = Rc::new(RefCell::new(Counts::default()));
    {
        let counts2 = Rc::clone(&counts);
        measure("UNBUFFERED (current)", Rc::clone(&counts), move || {
            let w = Counting {
                inner: io::sink(),
                counts: Rc::clone(&counts2),
            };
            Terminal::new(CrosstermBackend::new(w)).unwrap()
        });
    }

    // BufWriter-wrapped (the proposed fix): ratatui's many small writes coalesce; the Counting
    // sink sees only the BufWriter's flush writes.
    let counts = Rc::new(RefCell::new(Counts::default()));
    {
        let counts2 = Rc::clone(&counts);
        measure("BUFWRITER (proposed fix)", Rc::clone(&counts), move || {
            let w = BufWriter::new(Counting {
                inner: io::sink(),
                counts: Rc::clone(&counts2),
            });
            Terminal::new(CrosstermBackend::new(w)).unwrap()
        });
    }

    println!(
        "\nEach UNBUFFERED write is a console syscall on a real terminal (~0.1-0.5 ms with ANSI\n\
         processing); BUFWRITER collapses a frame to ~1 write + 1 flush. Confirm the real per-frame\n\
         draw time with `ef -i --profile` (now reports draw count + draw-time percentiles)."
    );
}
