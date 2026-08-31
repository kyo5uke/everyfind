//! `ef`: the Everyfind client (M3).
//!
//! A thin, fast client over the `\\.\pipe\everyfind` named pipe. `ef <query>` searches;
//! `ef status` reports daemon health. **The search path does no logging setup, no config
//! read, and one pipe round trip**: client-side waste is what the < 15 ms budget cannot
//! afford. Works from a **non-elevated** terminal (the default
//! `interactive` pipe DACL grants INTERACTIVE).

use everyfind::commas;
use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use everyfind::config;
use everyfind::ipc::acl::AclMode;
use everyfind::ipc::client::{self, ClientError};
use everyfind::ipc::{DuRowWire, Request, Response, SearchHit, StatusReport, PIPE_NAME};
use everyfind::{service, tui};

/// Instant filename search (client for the Everyfind daemon).
#[derive(Debug, Parser)]
#[command(name = "ef", version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    search: SearchArgs,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show daemon status (entries, journal lag, memory, snapshot, uptime).
    Status,
    /// Disk usage: the largest folders/files under a path (default: volume root).
    Du(DuArgs),
    /// Manage the Everyfind Windows service (install/uninstall/start/stop; needs Administrator).
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Show the client config: the persistent excludes file and what it loads.
    Config,
    /// Content search (grix engine): search *inside* files under the current
    /// directory's tree. Runs entirely in this client; the daemon and the
    /// name index are not involved.
    #[command(visible_alias = "c")]
    Content(ContentArgs),
    /// Add/remove "Search here with Everyfind" in the folder right-click menu
    /// (per-user, no Administrator needed). Opens this CLI's TUI scoped to the
    /// folder; nothing is loaded into Explorer, and it is not part of the
    /// search-box integration below.
    #[command(subcommand)]
    Shell(ShellCmd),
    /// Put Everyfind behind **Explorer's own search box** by replacing the engine
    /// underneath it. The breadcrumb, the view and the box stay native; only the
    /// rows come from here.
    #[command(subcommand)]
    Explorer(ExplorerCmd),
    /// (plumbing) Build/refresh the content index for a root, spawned
    /// detached by `ef content`; not meant for interactive use.
    #[command(hide = true, name = "content-index")]
    ContentIndex { root: std::path::PathBuf },
}

/// `ef content`: the grix-powered content search (kept as its own argument
/// set: nothing here is shared with the name-search flags).
#[derive(Debug, Args)]
struct ContentArgs {
    /// Regex to search for inside files (ripgrep-compatible semantics).
    pattern: String,

    /// Restrict the search to these files/directories (default: the whole
    /// indexed tree containing the current directory).
    #[arg(value_name = "PATH")]
    paths: Vec<std::path::PathBuf>,

    /// Match case-sensitively (default: case-insensitive, like `ef`).
    #[arg(short = 's', long)]
    case_sensitive: bool,
}

#[derive(Debug, Args)]
struct DuArgs {
    /// Path to report on, e.g. `C:\Users` (default: the indexed volume's root).
    path: Option<String>,

    /// Depth: 1 = immediate children, N = descendants within N levels.
    #[arg(short = 'd', long, default_value_t = 1)]
    depth: u32,

    /// Maximum number of rows to print.
    #[arg(short = 'n', long, default_value_t = 20)]
    top: u32,

    /// Print raw byte counts instead of human-readable sizes.
    #[arg(long)]
    bytes: bool,

    /// Connect/response timeout, milliseconds.
    ///
    /// It bounds the whole round trip now, which it did not before: only the connect was
    /// bounded, and the connect never blocks. Ten seconds because the deadline is real: a
    /// whole-volume term on a six-million-entry index takes about three seconds of honest
    /// work, and two would have failed it. This is how long to wait for an answer, not how
    /// long an answer should take.
    #[arg(long, default_value_t = 10_000)]
    timeout_ms: u64,
}

#[derive(Debug, Subcommand)]
enum ExplorerCmd {
    /// Replace the **engine** behind Explorer's own search box: results become
    /// Everyfind's while the breadcrumb, view, and box stay completely native.
    /// Shadows the search data-source CLSID under HKCU (per-user, reversible, no
    /// injection; it applies to every Explorer process automatically).
    #[command(subcommand)]
    Engine(EngineCmd),
}

#[derive(Debug, Subcommand)]
enum EngineCmd {
    /// Stage the provider DLL and register the override. Takes effect on the next
    /// search (already-open windows included).
    Install {
        /// Path to the built provider DLL. Defaults to `ef_search_engine.dll`
        /// next to `ef.exe`.
        #[arg(long)]
        dll: Option<std::path::PathBuf>,
        /// Turn on the provider's diagnostic log at %TEMP%\searchshim.log.
        #[arg(long)]
        debug: bool,
        /// Leave Windows Search's crawl scope alone. Without this, the volume the
        /// daemon serves is marked as indexed so *every* folder on it answers with
        /// Everyfind, which does not make Windows index anything, it only decides
        /// which engine Explorer asks. Skip it and folders outside the existing
        /// scope (the drive root, C:\Windows) keep answering with Windows' own
        /// results. Other drives are never marked: Everyfind indexes one volume,
        /// and Explorer must go on walking the ones it cannot answer for.
        #[arg(long)]
        no_scope: bool,
    },
    /// Remove the override; native Windows Search resumes on the next search.
    Uninstall,
    /// Show whether the override is installed and what it points at.
    Status,
}

#[derive(Debug, Subcommand)]
enum ShellCmd {
    /// Add the context-menu entry (folders, folder backgrounds, drives).
    /// On Windows 11 it appears under "Show more options" (Shift+F10).
    Install,
    /// Remove the context-menu entry.
    Uninstall,
}

#[derive(Debug, Subcommand)]
enum ServiceCmd {
    /// Install and register the service (auto-start, LocalSystem).
    Install {
        /// Target NTFS volume to index.
        #[arg(long, default_value = "C:")]
        volume: String,
        /// Pipe access mode: `interactive` (default) or `admins`.
        #[arg(long, default_value = "interactive")]
        acl: String,
    },
    /// Stop, delete, and remove all data (leaves no trace).
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the installed service.
    Stop,
}

#[derive(Debug, Args)]
struct SearchArgs {
    /// Substring to search for in file names.
    query: Option<String>,

    /// Scope the search to a folder: seeds the query with a `path:` term.
    /// This is what the Explorer context menu passes; handy standalone too
    /// (`ef --in C:\proj` opens the TUI already narrowed to that tree).
    /// A volume root (`C:\`) seeds nothing; it would match everything.
    #[arg(long = "in", value_name = "DIR")]
    in_dir: Option<std::path::PathBuf>,

    /// Match case-sensitively (default: case-insensitive).
    #[arg(short = 's', long)]
    case_sensitive: bool,

    /// Include orphan entries (parent directory unknown).
    #[arg(short = 'a', long = "all")]
    include_orphans: bool,

    /// Maximum number of paths to print (one-shot mode).
    #[arg(short = 'n', long, default_value_t = 20)]
    limit: u32,

    /// Launch the interactive TUI, seeded with the query if given. A bare `ef` on a terminal
    /// also opens the TUI; `ef <query>` (without `-i`) stays a one-shot print.
    #[arg(short = 'i', long)]
    interactive: bool,

    /// (TUI) print key->render latency percentiles to stderr on exit.
    #[arg(long)]
    profile: bool,

    /// Ignore the persistent excludes (%APPDATA%\everyfind\excludes.txt) this run.
    #[arg(long)]
    no_excludes: bool,

    /// Connect/response timeout, milliseconds.
    ///
    /// It bounds the whole round trip now, which it did not before: only the connect was
    /// bounded, and the connect never blocks. Ten seconds because the deadline is real: a
    /// whole-volume term on a six-million-entry index takes about three seconds of honest
    /// work, and two would have failed it. This is how long to wait for an answer, not how
    /// long an answer should take.
    #[arg(long, default_value_t = 10_000)]
    timeout_ms: u64,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let timeout = Duration::from_millis(cli.search.timeout_ms);

    match &cli.command {
        Some(Command::Service(cmd)) => run_service_cmd(cmd),
        Some(Command::Status) => finish(run_status(timeout)),
        Some(Command::Du(a)) => finish(run_du(a)),
        Some(Command::Config) => {
            run_config();
            ExitCode::SUCCESS
        }
        Some(Command::Content(a)) => run_content(a),
        Some(Command::ContentIndex { root }) => match everyfind::content::build_index(root) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("ef: {e:#}");
                ExitCode::FAILURE
            }
        },
        Some(Command::Shell(cmd)) => {
            let (verb, result) = match cmd {
                ShellCmd::Install => ("installed", everyfind::shell::install()),
                ShellCmd::Uninstall => ("removed", everyfind::shell::uninstall()),
            };
            match result {
                Ok(()) => {
                    eprintln!(
                        "ef: context-menu entry {verb} (Windows 11 shows it under \"Show more options\")"
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("ef: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Some(Command::Explorer(cmd)) => run_explorer_cmd(cmd),
        None => run_default(&cli.search, timeout),
    }
}

/// `ef explorer ...`: the search-box integration. One route now; the connector, the
/// namespace extension and the outside-in watcher that used to sit beside it are gone.
fn run_explorer_cmd(cmd: &ExplorerCmd) -> ExitCode {
    match cmd {
        ExplorerCmd::Engine(cmd) => run_engine_cmd(cmd),
    }
}

fn run_engine_cmd(cmd: &EngineCmd) -> ExitCode {
    use everyfind::search_engine;
    match cmd {
        EngineCmd::Install {
            dll,
            debug,
            no_scope,
        } => match search_engine::install(dll.as_deref(), *debug, !*no_scope) {
            Ok((path, scoped)) => {
                use everyfind::search_engine::Scoped;
                println!("ef: search engine installed -> {}", path.display());
                if *debug {
                    println!("    diagnostics on (%TEMP%\\searchshim.log)");
                }
                // Which folders now come to Everyfind, said out loud. Declaring a drive
                // indexed only changes which engine Explorer asks (it indexes nothing) so
                // a drive Everyfind does not serve must not be declared, and the user is the
                // one who needs to know which that is.
                match scoped {
                    Scoped::Added(d) => {
                        println!("    {d}: marked indexed, so every folder on it asks everyfind.")
                    }
                    Scoped::AlreadyIncluded(d) => {
                        println!(
                            "    {d}: already answered with the indexed engine; scope unchanged."
                        )
                    }
                    Scoped::Skipped => {
                        println!("    crawl scope left alone: folders Windows does not index");
                        println!("    (drive roots, C:\\Windows) still answer natively.");
                    }
                    Scoped::UnknownVolume => {
                        println!("    the daemon did not say which volume it serves, so the crawl");
                        println!(
                            "    scope was left alone. Start it and re-run this to have drive"
                        );
                        println!("    roots and C:\\Windows answer with Everyfind too:");
                        println!("      ef service start");
                    }
                }
                println!("    Other drives are left exactly as Windows had them - Everyfind");
                println!("    indexes one volume, and a drive it cannot answer for must keep");
                println!("    answering the way it did.");
                println!("    Explorer's search now answers with Everyfind (next search).");
                println!("    Undo any time: ef explorer engine uninstall");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("ef: install failed: {e:#}");
                ExitCode::FAILURE
            }
        },
        EngineCmd::Uninstall => match search_engine::uninstall() {
            Ok(()) => {
                println!("ef: search engine override removed; native Windows Search restored.");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("ef: uninstall failed: {e:#}");
                ExitCode::FAILURE
            }
        },
        EngineCmd::Status => {
            let s = search_engine::status();
            if !s.installed {
                println!("search engine: not installed (native Windows Search)");
            } else {
                let who = if s.points_at_ours {
                    "everyfind"
                } else {
                    "unknown DLL"
                };
                println!("search engine: installed ({who})");
                if let Some(d) = &s.dll {
                    println!("  dll     : {d}");
                }
                println!(
                    "  engines : {}/{} replaced{}",
                    s.engines_shadowed,
                    s.engines_total,
                    if s.engines_shadowed < s.engines_total {
                        "  (partial: some folders still answer natively)"
                    } else {
                        ""
                    }
                );
                println!("  debug   : {}", if s.debug { "on" } else { "off" });
            }
            ExitCode::SUCCESS
        }
    }
}

/// `ef content <regex>`: search inside files, printed for whoever is reading.
///
/// On a terminal the hits come out the way ripgrep presents them: a heading per
/// file, line numbers right-aligned within it, matches highlighted, and a
/// summary with the wall time. Into a pipe the format stays the flat
/// `path:line:text` it has always been, so `| head`, diffing and scripts keep
/// working on it.
fn run_content(args: &ContentArgs) -> ExitCode {
    use std::io::{IsTerminal as _, Write as _};

    let opts = everyfind::content::ContentOpts {
        case_insensitive: !args.case_sensitive,
        scopes: args.paths.clone(),
    };
    let started = std::time::Instant::now();
    match everyfind::content::search(std::path::Path::new("."), &args.pattern, &opts) {
        Ok(out) => {
            let elapsed = started.elapsed();
            let stdout = std::io::stdout();
            if stdout.is_terminal() && !out.hits.is_empty() {
                let color = enable_vt_stdout();
                // Highlighting recompiles the pattern here: grix owns the real
                // matcher and does not expose match spans. A pattern the regex
                // crate cannot take simply goes unhighlighted.
                let re = regex::RegexBuilder::new(&args.pattern)
                    .case_insensitive(!args.case_sensitive)
                    .build()
                    .ok();
                let files = {
                    let mut n = 0usize;
                    let mut last: Option<&str> = None;
                    for h in &out.hits {
                        if last != Some(h.rel_path.as_str()) {
                            n += 1;
                            last = Some(h.rel_path.as_str());
                        }
                    }
                    n
                };
                let mut w = std::io::BufWriter::new(stdout.lock());
                let _ = w.write_all(render_content_hits(&out.hits, re.as_ref(), color).as_bytes());
                let (dim, reset) = if color {
                    (SGR_DIM, SGR_RESET)
                } else {
                    ("", "")
                };
                let _ = writeln!(
                    w,
                    "\n{dim}{} lines, {} files, {:.2} s{reset}",
                    everyfind::commas(out.hits.len() as u64),
                    everyfind::commas(files as u64),
                    elapsed.as_secs_f64()
                );
                let _ = w.flush();
            } else {
                let mut w = std::io::BufWriter::new(stdout.lock());
                for h in &out.hits {
                    if writeln!(w, "{}:{}:{}", h.rel_path, h.line_number, h.text).is_err() {
                        break; // downstream closed the pipe (e.g. `| head`)
                    }
                }
                let _ = w.flush();
            }
            if out.needs_index {
                eprintln!(
                    "ef: no content index for {} yet. This search ran as a full scan; building one in the background for next time...",
                    out.root.display()
                );
                everyfind::content::spawn_background_build(&out.root);
            }
            if out.hits.is_empty() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("ef: {e:#}");
            ExitCode::from(2)
        }
    }
}

// ANSI styling for the terminal content output. Plain constants rather than a
// styling crate: four styles, one reset, and the pipe format must stay
// byte-identical to what it always was.
const SGR_PATH: &str = "\x1b[1;36m";
const SGR_LINENO: &str = "\x1b[32m";
const SGR_MATCH: &str = "\x1b[1;33m";
const SGR_DIM: &str = "\x1b[2m";
const SGR_RESET: &str = "\x1b[0m";

/// Group content hits by file: a path heading, `  line: text` rows with the
/// numbers right-aligned within the file, matched spans highlighted, a blank
/// line between files. Pure so the shape is testable without a terminal.
fn render_content_hits(
    hits: &[everyfind::content::ContentHit],
    re: Option<&regex::Regex>,
    color: bool,
) -> String {
    use std::fmt::Write as _;

    let (path_c, num_c, hit_c, reset) = if color {
        (SGR_PATH, SGR_LINENO, SGR_MATCH, SGR_RESET)
    } else {
        ("", "", "", "")
    };
    let mut out = String::new();
    let mut i = 0;
    while i < hits.len() {
        let file = &hits[i].rel_path;
        let end = i + hits[i..].iter().take_while(|h| h.rel_path == *file).count();
        if !out.is_empty() {
            out.push('\n');
        }
        let _ = writeln!(out, "{path_c}{file}{reset}");
        let width = hits[i..end]
            .iter()
            .map(|h| h.line_number)
            .max()
            .unwrap_or(0)
            .to_string()
            .len();
        for h in &hits[i..end] {
            let _ = write!(out, "  {num_c}{:>width$}{reset}: ", h.line_number);
            match re {
                // Regex match offsets sit on char boundaries of the (lossy,
                // hence valid UTF-8) line, so the slicing cannot split a char.
                Some(re) if color => {
                    let mut pos = 0;
                    for m in re.find_iter(&h.text) {
                        out.push_str(&h.text[pos..m.start()]);
                        let _ = write!(out, "{hit_c}{}{reset}", &h.text[m.start()..m.end()]);
                        pos = m.end();
                    }
                    out.push_str(&h.text[pos..]);
                }
                _ => out.push_str(&h.text),
            }
            out.push('\n');
        }
        i = end;
    }
    out
}

/// Turn on ANSI processing for stdout. Windows Terminal has it on already;
/// classic conhost needs the mode bit set. Failing means "no color", never
/// "no output": the caller still prints the grouped plain form.
fn enable_vt_stdout() -> bool {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        STD_OUTPUT_HANDLE,
    };
    // SAFETY: STD_OUTPUT_HANDLE is a valid predefined handle id; the mode
    // pointer is a live local.
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h.is_null() || h as isize == -1 {
            return false;
        }
        let mut mode = 0u32;
        if GetConsoleMode(h, &mut mode) == 0 {
            return false;
        }
        if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0 {
            return true;
        }
        SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

/// The `!path:` suffix carrying the persistent excludes (empty when disabled).
fn excludes_suffix(no_excludes: bool) -> String {
    if no_excludes {
        String::new()
    } else {
        config::as_query_suffix(&config::load_excludes())
    }
}

fn run_config() {
    match config::excludes_path() {
        Some(p) => {
            println!("excludes file : {}", p.display());
            let ex = config::load_excludes();
            if ex.is_empty() {
                println!("excludes      : (none - everything is shown)");
                println!("format        : one path fragment per line; `#` comments; applied as !path:\"...\"");
            } else {
                println!("excludes      : {} fragment(s), applied as:", ex.len());
                for e in &ex {
                    println!("  !path:\"{e}\"");
                }
                println!("(disable for one run with --no-excludes)");
            }
        }
        None => println!("APPDATA is not set; no excludes file is loaded."),
    }
}

/// No subcommand: pick the TUI or the one-shot print. `ef <query>` (no `-i`) prints; `ef -i
/// [query]` or a bare `ef` on a TTY opens the interactive TUI.
/// Build the query `--in DIR` starts from.
///
/// Two shapes matter, both learned the hard way:
/// - **A drive root (`C:\`) gets no scope at all.** `path:C:` matches every
///   indexed file, so the filter buys nothing and only costs a full-path
///   rebuild per candidate. "Search here" at the volume root *is* a plain
///   search.
/// - **Quotes only when the path needs them.** The query tokenizer treats `"`
///   as a toggle, so a seed ending in a quote swallows whatever the user types
///   next: `path:"C:"` + `kernel32` parsed as `path:"C:kernel32"`, a path that
///   cannot exist, hence the "type anything, see nothing" report. A bare
///   fragment has no closing quote to fuse with, and the trailing space keeps
///   the next keystroke a separate token either way.
fn seed_query(dir: &str, query: Option<&str>) -> String {
    let typed = query.unwrap_or("");
    // Explorer expands the context-menu command `--in "%V"` verbatim, so a
    // drive root arrives as the literal `"C:\"`, and Windows' command-line
    // parser reads the `\"` as an *escaped quote*, handing us `C:"`. Strip that
    // artifact before anything else, or a drive root fails the root test below
    // and turns into the impossible scope `path:C:"` (0 hits, forever).
    let trimmed = dir.trim_end_matches('"').trim_end_matches('\\');
    // `C:` (a drive root, backslash trimmed) -> nothing to scope by.
    let is_drive_root = trimmed.len() == 2
        && trimmed.ends_with(':')
        && trimmed.starts_with(|c: char| c.is_ascii_alphabetic());
    if trimmed.is_empty() || is_drive_root {
        return typed.to_string();
    }
    let scope = if trimmed.contains(' ') {
        format!("path:\"{trimmed}\"")
    } else {
        format!("path:{trimmed}")
    };
    format!("{scope} {typed}")
}

/// The drive letter of a path (`C:\x` -> `'C'`), or `None` for a UNC / relative
/// path where the "not indexed" check does not apply.
fn drive_letter(p: &std::path::Path) -> Option<char> {
    // Tolerates the `C:"` shape Explorer's `--in "%V"` yields for a drive root
    // (see `seed_query`); the letter is the first char either way.
    let s = p.to_string_lossy();
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), Some(':')) if c.is_ascii_alphabetic() => Some(c),
        _ => None,
    }
}

/// Ask the daemon which drive it indexes. Best-effort: on any error (daemon not
/// running, timeout) return `None` so the caller skips the check rather than
/// blocking a search behind a status round trip.
fn indexed_drive(timeout: Duration) -> Option<char> {
    match client::request(PIPE_NAME, &Request::Status, timeout).ok()? {
        Response::Status(s) => Some(s.drive),
        _ => None,
    }
}

fn run_default(args: &SearchArgs, timeout: Duration) -> ExitCode {
    // `--in DIR` (the Explorer right-click entry) scopes to a folder. The index
    // covers exactly one volume; a folder on a *different* drive (a second
    // disk, a network mapping, a VeraCrypt/mounted volume) is simply not in it,
    // so a scoped search would silently return nothing. Detect that up front and
    // say why, instead of showing an empty result that reads as broken.
    if let Some(dir) = &args.in_dir {
        if let Some(indexed) = indexed_drive(timeout) {
            if let Some(d) = drive_letter(dir) {
                if !d.eq_ignore_ascii_case(&indexed) {
                    eprintln!(
                        "ef: {d}:\\ is not indexed; Everyfind indexes only {indexed}:\\ \
(one volume per daemon). Nothing to search here."
                    );
                    // Launched from Explorer's right-click a fresh console opens
                    // and would vanish the instant we exit; keep it up so the
                    // message is readable. Skipped when output is redirected.
                    if std::io::stderr().is_terminal() && std::io::stdin().is_terminal() {
                        eprint!("Press Enter to close...");
                        let mut s = String::new();
                        let _ = std::io::stdin().read_line(&mut s);
                    }
                    return ExitCode::from(2);
                }
            }
        }
    }

    let seeded: Option<String> = match (&args.in_dir, &args.query) {
        (Some(dir), q) => Some(seed_query(&dir.to_string_lossy(), q.as_deref())),
        (None, q) => q.clone(),
    };
    let want_tui = args.interactive || args.query.is_none();
    if !want_tui {
        return finish(run_search(seeded.as_deref().unwrap_or(""), args, timeout));
    }
    // The TUI needs a real terminal for both input (stdin) and its stderr rendering; stdout may
    // be redirected (that is what makes `vim $(ef)` work).
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        if args.interactive {
            eprintln!("ef: -i/--interactive requires an interactive terminal (stdin and stderr)");
        } else {
            eprintln!(
                "usage: ef <query>  |  ef -i [query]  |  ef status  |  ef service <cmd>   (see --help)"
            );
        }
        return ExitCode::from(2);
    }
    run_tui(
        seeded.as_deref().unwrap_or(""),
        timeout,
        args.profile,
        excludes_suffix(args.no_excludes),
        everyfind::tui::worker::SearchFlags {
            case_sensitive: args.case_sensitive,
            include_orphans: args.include_orphans,
        },
    )
}

/// Drive the TUI. On Enter it returns the selected path, which we print to **stdout** (the only
/// stdout output; the TUI itself draws to stderr, so command substitution captures a clean path).
fn run_tui(
    initial: &str,
    timeout: Duration,
    profile: bool,
    excludes: String,
    how: everyfind::tui::worker::SearchFlags,
) -> ExitCode {
    match tui::run(initial, timeout, profile, excludes, how) {
        Ok(Some(path)) => {
            println!("{path}");
            ExitCode::SUCCESS
        }
        // Aborted (Esc / Ctrl+C): nothing on stdout, exit 1.
        Ok(None) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("ef: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Marks an error whose message the caller has already printed in full.
///
/// The daemon-error arms printed the message and then handed back a `ClientError::Io` wrapping
/// the same text, which `finish` printed again. A still-building index came out as the helpful
/// sentence followed by a bare `ef: building`, and `ef status` against a version-skewed daemon
/// trailed a lone `ef: protocol`.
const ALREADY_REPORTED: &str = "\u{0}reported";

fn reported() -> ClientError {
    ClientError::Io(std::io::Error::other(ALREADY_REPORTED))
}

fn finish(result: Result<(), ClientError>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(ClientError::NotRunning(_)) => {
            eprintln!("Everyfind daemon (efd) is not running.");
            ExitCode::FAILURE
        }
        Err(ClientError::Io(e)) if e.to_string() == ALREADY_REPORTED => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("ef: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Service management (admin). These bypass the pipe entirely (SCM calls).
fn run_service_cmd(cmd: &ServiceCmd) -> ExitCode {
    let result = match cmd {
        ServiceCmd::Install { volume, acl } => match AclMode::parse(acl) {
            Some(mode) => service::install(volume, mode).map(|()| {
                println!(
                    "service '{}' installed. start with: ef service start",
                    service::SERVICE_NAME
                );
            }),
            None => Err(anyhow::anyhow!(
                "invalid --acl {acl:?} (use interactive|admins)"
            )),
        },
        ServiceCmd::Uninstall => {
            service::uninstall().map(|()| println!("service uninstalled; data removed."))
        }
        ServiceCmd::Start => service::start().map(|()| println!("service started.")),
        ServiceCmd::Stop => service::stop().map(|()| println!("service stopped.")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ef service: {e:#}");
            eprintln!("(service management requires an elevated / Administrator terminal)");
            ExitCode::FAILURE
        }
    }
}

fn run_search(query: &str, args: &SearchArgs, timeout: Duration) -> Result<(), ClientError> {
    // Persistent excludes ride along as `!path:` terms; the display below keeps
    // showing only what the user typed (`--in` scoping included, so a scoped
    // zero-hit report names its scope).
    let req = Request::Search {
        query: config::compose(query, &excludes_suffix(args.no_excludes)),
        case_sensitive: args.case_sensitive,
        include_orphans: args.include_orphans,
        limit: args.limit,
        // Pure rank order on a terminal: raising -n shows the rest.
        reserve_elsewhere: 0,
    };
    match client::request(PIPE_NAME, &req, timeout)? {
        Response::Search {
            total_hits,
            results,
            building,
        } => {
            if building {
                eprintln!(
                    "ef: the index is still building (first start after boot). Try again in a moment. `ef status` shows progress."
                );
                return Err(ClientError::Io(std::io::Error::other("building")));
            }
            print_search(query, total_hits, &results, args.limit);
            Ok(())
        }
        Response::Error { code, message } => {
            eprintln!("ef: daemon error ({code:?}): {message}");
            Err(reported())
        }
        Response::Status(_) | Response::Du { .. } => {
            eprintln!("ef: unexpected response to a search");
            Err(ClientError::Io(std::io::Error::other("protocol")))
        }
    }
}

fn print_search(query: &str, total_hits: u64, results: &[SearchHit], limit: u32) {
    if total_hits == 0 {
        println!("no matches for {query:?}");
        return;
    }
    println!("{} match(es) for {query:?}:", commas(total_hits));
    // One-shot output stays path-only (human-facing, unchanged from M3); `is_dir` is a TUI
    // affordance.
    for hit in results {
        println!("  {}", hit.path);
    }
    let shown = results.len() as u64;
    if total_hits > shown {
        println!(
            "  ... and {} more (raise -n, currently {limit})",
            commas(total_hits - shown)
        );
    }
}

fn run_du(a: &DuArgs) -> Result<(), ClientError> {
    let timeout = Duration::from_millis(a.timeout_ms);
    let path = a.path.clone().unwrap_or_default(); // "" -> the daemon resolves to the volume root
    let req = Request::Du {
        path: path.clone(),
        depth: a.depth,
        top_n: a.top,
        // Reserved protocol field: v0.1 is allocated-on-disk only (no logical-size flag).
        real: false,
    };
    match client::request(PIPE_NAME, &req, timeout)? {
        Response::Du {
            total_bytes,
            truncated,
            real: _,
            rows,
            sizes_resolved,
            entries,
        } => {
            print_du(
                &path,
                total_bytes,
                truncated,
                &rows,
                a.bytes,
                sizes_resolved,
                entries,
            );
            Ok(())
        }
        Response::Error { code, message } => {
            eprintln!("ef du: daemon error ({code:?}): {message}");
            Err(reported())
        }
        _ => {
            eprintln!("ef du: unexpected response");
            Err(ClientError::Io(std::io::Error::other("protocol")))
        }
    }
}

fn print_du(
    path: &str,
    total_bytes: u64,
    truncated: bool,
    rows: &[DuRowWire],
    bytes: bool,
    sizes_resolved: u64,
    entries: u64,
) {
    let label = if path.is_empty() {
        "(volume root)"
    } else {
        path
    };
    println!("du {label}  (sizes: allocated on-disk; --bytes for raw)");
    if truncated {
        println!("  warning: some files exceed the 16 TiB size cap; totals are a lower bound");
    }
    // A total is only worth printing if the sizes behind it were actually read. The size pass
    // is fail-soft, so when it yields little or nothing `du` would otherwise add up whatever
    // the journal has since filled in and present it with the same confidence as a complete
    // answer. Measured on a live volume in exactly that state: 47.0 GiB printed against
    // 827.6 GB actually used. Saying so is the difference between a partial answer and a
    // wrong one.
    if entries > 0 && sizes_resolved * 10 < entries * 9 {
        let pct = 100.0 * sizes_resolved as f64 / entries as f64;
        println!(
            "  warning: only {} of {} entries have a size ({:.0}%); this total is far short of the real one.",
            commas(sizes_resolved),
            commas(entries),
            pct
        );
        println!(
            "     The size pass did not complete; `efd`'s log says why. A restart re-runs it."
        );
    }
    println!("  total: {}", fmt_size(total_bytes, bytes));
    for r in rows {
        let pct = if total_bytes > 0 {
            100.0 * r.size_bytes as f64 / total_bytes as f64
        } else {
            0.0
        };
        let slash = if r.is_dir { "\\" } else { "" };
        let mark = if r.truncated { "  (+)" } else { "" };
        println!(
            "  {:>11}  {:5.1}%  {}{slash}{mark}",
            fmt_size(r.size_bytes, bytes),
            pct,
            r.path,
        );
    }
}

/// Human-readable (or raw, when `raw`) byte size.
fn fmt_size(bytes: u64, raw: bool) -> String {
    if raw {
        return bytes.to_string();
    }
    const U: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", U[i])
}

fn run_status(timeout: Duration) -> Result<(), ClientError> {
    match client::request(PIPE_NAME, &Request::Status, timeout)? {
        Response::Status(s) => {
            print_status(&s);
            print_explorer_status();
            Ok(())
        }
        Response::Error { code, message } => {
            eprintln!("ef: daemon error ({code:?}): {message}");
            Err(reported())
        }
        Response::Search { .. } | Response::Du { .. } => {
            eprintln!("ef: unexpected response to status");
            Err(ClientError::Io(std::io::Error::other("protocol")))
        }
    }
}

/// The Explorer integration's moving parts, if any of them are installed.
///
/// This exists because of a specific failure: both background pieces were
/// stopped, and the only symptom the user saw was Explorer's search box being
/// slow again: Windows Search had quietly taken back over. Nothing said so.
/// `ef status` is the command people already run when something feels wrong, so
/// the answer belongs here, not behind three separate subcommands.
///
/// Silent when nothing is installed: someone who never touched the Explorer
/// integration should not have to read about it.
fn print_explorer_status() {
    let s = everyfind::search_engine::status();
    if !s.installed {
        return;
    }
    println!();
    println!("explorer integration:");
    println!(
        "  search engine     : {}{}",
        if s.points_at_ours {
            "everyfind"
        } else {
            "an unknown DLL"
        },
        if s.engines_shadowed < s.engines_total {
            "  (partial: some folders still answer natively)"
        } else {
            ""
        }
    );
}

fn print_status(s: &StatusReport) {
    println!("Everyfind daemon:");
    println!("  volume            : {}:", s.drive);
    if s.building {
        println!(
            "  state             : building initial index ({} entries so far)",
            commas(s.build_progress)
        );
    }
    println!(
        "  entries           : {} ({} live)",
        commas(s.entries),
        commas(s.live_entries)
    );
    println!(
        "  sizes (du)        : {} / {} resolved",
        commas(s.sizes_resolved),
        commas(s.entries)
    );
    println!(
        "  journal lag       : {} USN (synced {}s ago)",
        commas(s.usn_lag),
        s.last_sync_secs
    );
    println!(
        "  memory            : WorkingSet {:.1} MiB / PrivateUsage {:.1} MiB",
        mib(s.working_set),
        mib(s.private_usage)
    );
    match s.last_snapshot_secs {
        Some(secs) => println!(
            "  snapshot          : gen {}, {}s ago (cursor {})",
            s.snapshot_generation, secs, s.snapshot_cursor
        ),
        None => println!("  snapshot          : none yet"),
    }
    println!("  uptime            : {}", human_secs(s.uptime_secs));
    println!("  poll interval     : {} ms", s.poll_interval_ms);
    println!("  proto / pid       : v{} / {}", s.proto_version, s.pid);
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn human_secs(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The terminal form groups by file, right-aligns the numbers within one,
    /// and separates files with a blank line; color wraps only the heading,
    /// the numbers and the matched spans.
    #[test]
    fn content_hits_render_grouped_and_aligned() {
        use everyfind::content::ContentHit;
        let hit = |p: &str, n: u64, t: &str| ContentHit {
            rel_path: p.into(),
            line_number: n,
            text: t.into(),
        };
        let hits = vec![
            hit("a/b.c", 9, "foo bar"),
            hit("a/b.c", 1234, "bar again"),
            hit("z.txt", 7, "no match"),
        ];
        let re = regex::Regex::new("bar").unwrap();

        let plain = render_content_hits(&hits, Some(&re), false);
        assert_eq!(
            plain,
            "a/b.c\n     9: foo bar\n  1234: bar again\n\nz.txt\n  7: no match\n"
        );

        let colored = render_content_hits(&hits, Some(&re), true);
        assert!(colored.starts_with("\u{1b}[1;36ma/b.c\u{1b}[0m"));
        assert!(colored.contains("\u{1b}[1;33mbar\u{1b}[0m"));
        assert!(
            !colored.contains("\u{1b}[1;33mno match"),
            "a non-matching line must not be highlighted"
        );
    }

    /// The seed must never fuse with the next keystroke. This is the exact bug
    /// the quoted form had: `path:"C:"` + `kernel32` tokenized as one fragment
    /// (`path:"C:kernel32"`), so a right-click search showed nothing no matter
    /// what was typed.
    #[test]
    fn seed_does_not_swallow_the_next_keystroke() {
        let seed = seed_query(r"C:\Users\me\proj", None);
        assert_eq!(seed, r"path:C:\Users\me\proj ");
        // Typing continues the string; the term stays its own token.
        let parsed = everyfind::index::query::parse(&format!("{seed}kernel32"));
        assert_eq!(parsed.words, vec!["kernel32".to_string()]);
        assert_eq!(parsed.paths, vec![r"C:\Users\me\proj".to_string()]);
    }

    #[test]
    fn drive_root_gets_no_scope() {
        // `path:C:` matches everything; scoping to a volume root is a plain search.
        assert_eq!(seed_query(r"C:\", None), "");
        assert_eq!(seed_query(r"C:\", Some("kernel32")), "kernel32");
        assert_eq!(seed_query("D:", None), "");
    }

    /// Explorer's `--in "%V"` on a drive root becomes `--in "C:\"`, and the
    /// Windows command-line parser turns `\"` into an escaped quote, so the
    /// value we actually receive is `C:"`. Left alone it produced the scope
    /// `path:C:"`, a path that cannot exist, hence "0 hits and nothing shows"
    /// when right-clicking C:\.
    #[test]
    fn trailing_quote_from_explorer_is_stripped() {
        assert_eq!(seed_query("C:\"", None), "");
        assert_eq!(seed_query("C:\"", Some("kernel32")), "kernel32");
        // The same artifact on a normal folder must not leak into the scope.
        assert_eq!(
            seed_query("C:\\Users\\me\"", Some("todo")),
            r"path:C:\Users\me todo"
        );
    }

    #[test]
    fn spaces_in_a_path_still_get_quotes() {
        let seed = seed_query(r"C:\Program Files\App", Some("readme"));
        assert_eq!(seed, r#"path:"C:\Program Files\App" readme"#);
        let parsed = everyfind::index::query::parse(&seed);
        assert_eq!(parsed.paths, vec![r"C:\Program Files\App".to_string()]);
        assert_eq!(parsed.words, vec!["readme".to_string()]);
    }

    #[test]
    fn drive_letter_extraction() {
        assert_eq!(drive_letter(std::path::Path::new(r"Z:\secret")), Some('Z'));
        assert_eq!(drive_letter(std::path::Path::new(r"\server\share")), None);
    }
}
