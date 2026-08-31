//! M4 latency diagnosis: measure a single `terminal.draw()` (render logic + diff) via
//! `TestBackend`, and how many draws the current loop does per keystroke. This isolates whether
//! the ~200 ms is per-draw cost multiplied by draws-per-burst.
//!
//! ```text
//! cargo run --release --example bench_render
//! ```

use std::time::Instant;

use everyfind::ipc::{Response, SearchHit};
use everyfind::tui::app::AppState;
use everyfind::tui::render::render;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

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

fn bench_draw(width: u16, height: u16, n_results: usize) {
    let mut app = AppState::new("kernel");
    app.dispatch_current();
    app.on_response(1, results(n_results), 12);

    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    // Warm.
    for _ in 0..5 {
        term.draw(|f| render(f, &app, &mut ratatui::widgets::ListState::default()))
            .unwrap();
    }
    // Force a real diff each iteration by toggling the query (so the whole screen changes).
    let mut times = Vec::new();
    for i in 0..200 {
        // Alternate the selected row so ratatui re-diffs the list every draw.
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
        let t0 = Instant::now();
        term.draw(|f| render(f, &app, &mut ratatui::widgets::ListState::default()))
            .unwrap();
        times.push(t0.elapsed().as_micros());
    }
    times.sort_unstable();
    println!(
        "draw {width}x{height}, {n_results:>3} results:  p50={:>5}us  p95={:>5}us  max={:>5}us",
        times[times.len() / 2],
        times[times.len() * 95 / 100],
        times[times.len() - 1],
    );
}

fn main() {
    println!("== single terminal.draw() to TestBackend (render logic + diff, no real terminal) ==");
    bench_draw(120, 40, 200);
    bench_draw(120, 40, 50);
    bench_draw(200, 60, 200);
    bench_draw(80, 24, 200);
}
