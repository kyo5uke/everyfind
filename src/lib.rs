//! # Everyfind
//!
//! Instant filename search for Windows: a terminal-native "Everything".
//!
//! Same principle as voidtools' Everything: enumerate the NTFS MFT to build an
//! in-memory filename index, then tail the USN journal to keep it live. Search is
//! a parallel in-memory scan with millisecond latency.
//!
//! ## Milestone status
//! **M2 complete: USN journal tailing + snapshot persistence** (on the M1 index engine).
//! The architecture is documented in the module docs below.
//!
//! ## Modules
//! - [`volume`]: the `UsnVolume` trait, `RawRecord`/`UsnEvent`, `FakeVolume`, and the real
//!   `Win32Volume` (read-only MFT enumeration + USN journal read).
//! - [`index`]: string/fold arenas, two-pass build, path reconstruction, search, and live
//!   USN event apply (`Index::apply_event`).
//! - [`snapshot`]: bincode persistence of the index + journal cursor (M2).
//! - [`watch`]: live tailing. Enumerate/resume, apply journal changes, recover by
//!   re-enumeration (M2).
//! - [`sysinfo`]: current-process memory stats for the `--stats` report.
//! - [`ipc`] holds the `efd` <-> `ef` named-pipe protocol: length-prefixed framing, versioned
//!   request/response, frame-size caps, ACL, and the pipe server/client transport (M3).
//! - [`daemon`] holds the `efd` core: shared `RwLock<Watched>`, watch thread, pipe accept loop,
//!   and the search/status request dispatch (M3).
//! - [`tui`] holds the `ef` interactive TUI (M4): incremental search rendered to stderr, with
//!   client-side match highlighting; stdout reserved for the selected path.
//! - [`content`]: `ef content`, search inside files through the embedded grix engine (trigram
//!   index + ripgrep-compatible confirming scan); client-side only, the daemon never
//!   pays for it.
//! - [`search_engine`]: Explorer's own search box, backed by Everyfind, by shadowing the
//!   search data source's CLSID per user so the rows come from here while the breadcrumb,
//!   the view and the box stay native. The DLL that does it lives in `searchshim/`.
//!
//! Three earlier routes to the same place were removed once this one worked: a Federated
//! Search connector over a loopback OpenSearch endpoint, a shell namespace extension, and a
//! watcher that followed Explorer from outside and redirected it. Each one handed Explorer a
//! path or a URL and asked it to show something; none of them could leave the window native.
//! `git log` has them if they are ever wanted back.

pub mod content;

/// The daemon protocol, and the excludes every part of Everyfind honours. Its own crate
/// (`ipc/`) so that a protocol version is something a build can disagree about out loud,
/// re-exported here under the names callers already used.
pub use everyfind_ipc as ipc;
pub use everyfind_ipc::config;
pub mod daemon;
pub mod index;
pub mod search_engine;
pub mod service;
pub mod shell;
pub mod snapshot;
pub mod sysinfo;
pub mod tui;
pub mod volume;
pub mod watch;

/// Encode a `&str` as the NUL-terminated wide string every Win32 `*W` entry point wants.
///
/// One copy for this crate. There were three (in the service registration, the search-engine
/// installer and the TUI's shell-out) and a fourth was being borrowed out of the protocol
/// crate's private innards, which is what made the crate split fail to compile and was the
/// only honest signal that any of this was duplicated.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Group an integer with thousands separators (ASCII, locale-free).
///
/// One copy, because there were two: the CLI's status output and the TUI's status line had
/// byte-identical implementations, and `ef.rs` is a separate crate target so a `pub(crate)`
/// would not have reached it.
pub fn commas(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}
