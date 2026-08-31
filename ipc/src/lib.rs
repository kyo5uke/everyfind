//! What every Everyfind process shares: the daemon protocol, and the excludes the user asked
//! for.
//!
//! Its own crate so that "which version of the protocol is this speaking?" is a question with
//! an answer. The daemon, the `ef` CLI and the DLL behind Explorer's search box all link this
//! and nothing else of each other's; a DLL that wants [`Request`] no longer compiles the MFT
//! reader to get it, and a protocol bump is a dependency change rather than an edit inside the
//! crate that also holds the index.
//!
//! - the wire format and its types (this module),
//! - [`client`] / [`server`] / [`pipe`] / [`acl`]: the named-pipe transport under it,
//! - [`config`]: `%APPDATA%\everyfind\excludes.txt`, honoured identically by the CLI and by
//!   the Explorer integration, which is only true because they read it from one place.
//!
//! # The protocol
//!
//! **Transport**: a byte-mode pipe (`PIPE_TYPE_BYTE | PIPE_READMODE_BYTE`), self-framed.
//! **Frame** (both directions), little-endian:
//!
//! ```text
//! [ len: u32 ][ proto_version: u16 ][ bincode(payload) ]
//!         `-- byte length of (version + payload)
//! ```
//!
//! `proto_version` is written **outside** bincode as a raw `u16` so it is always readable even
//! if the bincode encoding of [`Request`] / [`Response`] changes across versions. The reader
//! checks the version **before** attempting to decode the body (bincode enums are not
//! forward-compatible), so a version skew is reported as [`WireError::ProtocolMismatch`]
//! rather than a misleading decode error.
//!
//! **Frame-size caps**: the `len` field is validated against a cap
//! *before* any buffer is allocated, so a bogus length can never make a peer allocate
//! gigabytes. Requests are capped at [`REQUEST_MAX_FRAME`], responses at [`RESPONSE_MAX_FRAME`];
//! **both peers enforce** (a client validates the server's frames too).
//!
//! This module is pure logic over [`std::io::Read`]/[`std::io::Write`]; the pipe transport
//! (`ipc::client` / `ipc::server`) supplies the reader/writer, and these functions are unit
//! tested without a pipe.

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};

pub mod acl;
pub mod client;
pub mod config;
pub mod pipe;
pub mod server;

/// The daemon's named-pipe path. Both `efd` (server) and `ef` (client) use this.
pub const PIPE_NAME: &str = r"\\.\pipe\everyfind";

/// Wire protocol version. Bump on any incompatible change to [`Request`] / [`Response`].
///
/// - **v1** (M3): `Response::Search.results` was `Vec<String>` (paths only).
/// - **v2** (M4): `results` is `Vec<SearchHit>` (path + `is_dir`), for the TUI directory marker.
/// - **v3** (M5): adds [`Request::Du`] / [`Response::Du`] (`ef du`) + `StatusReport.sizes_resolved`.
/// - **v5**: `Request::Search` gains `reserve_elsewhere`, how many of the returned rows to
///   keep for hits matched inside the name rather than at its start. `0` is the old behaviour.
/// - **v6**: `Response::Du` gains `sizes_resolved` / `entries`, so `ef du` can say when the
///   total it is printing rests on sizes that were never read.
pub const PROTO_VERSION: u16 = 6;

/// Maximum framed size (version + payload) of a **request** (client -> server). A search query
/// is tiny; 64 KiB is generous head-room and a hard ceiling against a rogue length.
pub const REQUEST_MAX_FRAME: u32 = 64 * 1024;

/// Maximum framed size (version + payload) of a **response** (server -> client). Bounds the
/// top-N path list; 16 MiB is far above any sane `limit` * path length.
pub const RESPONSE_MAX_FRAME: u32 = 16 * 1024 * 1024;

/// Ceiling the daemon puts on a request's `limit`.
///
/// `limit` is how many paths the daemon *reconstructs and allocates*, and the pipe is reachable
/// by any interactive user under the default ACL, so a client's number is clamped, not trusted.
/// Unclamped, one small request can make the daemon build (and then bincode) hundreds of MB of
/// strings for a response that [`RESPONSE_MAX_FRAME`] will refuse anyway: work amplification with
/// nothing to show for it. The cap sits below what that frame can carry (~160k typical paths), so
/// "asked for more than fits" becomes a truncated answer rather than a failed one; `total_hits`
/// still reports the real count, so the client can say "... and N more". Every shipped client asks
/// for far less (`ef -n` defaults to 20; the TUI and `ef serve` page at 200).
pub const MAX_SEARCH_LIMIT: u32 = 100_000;

/// The same ceiling, for the same reason, on a `Du` request's row count.
///
/// `Du` sizes work from a client's number exactly as `Search` does: one reconstructed path per
/// row, but had no clamp at all. `top_n: u32::MAX` made `rows.truncate(top_n)` a no-op, so a
/// thirty-byte request asked the daemon to rebuild a path for every live entry on the volume
/// (six million here), sort them, bincode the lot, and then have [`RESPONSE_MAX_FRAME`] throw
/// the whole thing away. All of it under the read lock, so the journal stops being applied
/// while it runs.
pub const MAX_DU_ROWS: u32 = 10_000;

/// And on its depth. `depth: u32::MAX` accepts every descendant however deep, which is the
/// other half of the same request. Nothing legitimate goes past a few dozen: the deepest path
/// Windows will create is bounded by `MAX_PATH` extended, and `ef du` is a "what is big here"
/// tool, not a full-tree dump.
pub const MAX_DU_DEPTH: u32 = 64;

/// A request from the `ef` client to the `efd` daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Substring search. The daemon returns the total hit count plus the first `limit`
    /// reconstructed paths (it does **not** reconstruct every match).
    Search {
        query: String,
        case_sensitive: bool,
        include_orphans: bool,
        /// Number of paths to reconstruct and return (the total hit count is always returned).
        limit: u32,
        /// How many of those rows to hold for hits the term matches *inside* the name rather
        /// than at its start. Ranking puts exact and prefix matches first, so a short term with
        /// thousands of prefix matches fills every row with them and the substring hits (the
        /// ones only Everyfind can find) never surface. `0` keeps pure rank order, which is
        /// what `ef` sends: on a terminal you can raise `-n` and see them.
        reserve_elsewhere: u32,
    },
    /// Daemon status snapshot.
    Status,
    /// **M5 `ef du`.** Disk-usage report for `path`'s subtree: the recursive total plus the
    /// largest descendants within `depth` levels (1 = immediate children), capped to `top_n`.
    /// Sizes are on-disk **allocated**. `real` is **reserved for a future logical-size
    /// (`EndOfFile`) mode**: v0.1 serves allocated only, the client always sends `real: false`
    /// (there is no CLI flag), and the daemon rejects `real: true` rather than mislabel sizes.
    Du {
        path: String,
        depth: u32,
        top_n: u32,
        real: bool,
    },
}

/// One search result (protocol v2): the reconstructed absolute path plus whether it is a
/// directory. `is_dir` lets the TUI mark directories (trailing `\` / distinct style); the
/// one-shot `ef <query>` client prints only `path`. `is_dir` comes from the entry's
/// `flags & IS_DIR`, no change to search semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The reconstructed absolute path (e.g. `C:\Windows\System32\kernel32.dll`).
    pub path: String,
    /// Whether the entry is a directory.
    pub is_dir: bool,
}

/// One `ef du` row (protocol v3): a child/descendant path, its recursive subtree size in bytes,
/// whether it is a directory, and whether a file in it exceeded the 16 TiB cap (lower-bound total).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuRowWire {
    pub path: String,
    pub is_dir: bool,
    pub size_bytes: u64,
    pub truncated: bool,
}

/// A response from the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// Search result: the **total** number of matches, plus up to `limit` reconstructed hits.
    /// `total_hits` may exceed `results.len()` (truncation: the client shows "... and N more").
    Search {
        total_hits: u64,
        results: Vec<SearchHit>,
        /// True while the daemon is still building its initial index; an empty result then
        /// means "not ready yet", not "no matches". The client says so rather than "0 matches".
        building: bool,
    },
    /// Status snapshot (see [`StatusReport`]).
    Status(StatusReport),
    /// **M5** disk-usage report: the queried path's recursive total plus its largest children.
    /// `real` echoes the request's size mode; in v0.1 it is always `false` (allocated on-disk,
    /// the logical-size mode is reserved, see [`Request::Du`]).
    Du {
        total_bytes: u64,
        truncated: bool,
        real: bool,
        rows: Vec<DuRowWire>,
        /// How many entries carry a size, out of how many exist.
        ///
        /// Sent so the client can refuse to present a total it has no business presenting. The
        /// size pass is fail-soft by design: a fragmented `$MFT`, an unreadable geometry, a
        /// short read. When it yields nothing, `du` still adds up what the journal has
        /// since filled in and prints a confident figure. Measured on a live volume: 47.0 GiB
        /// against 827.6 GB actually used, with no indication anything was missing. A number
        /// that wrong is worse than no number, and only the daemon knows the difference.
        sizes_resolved: u64,
        entries: u64,
    },
    /// The request could not be served.
    Error { code: ErrCode, message: String },
}

/// Error categories carried in [`Response::Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrCode {
    /// The client's protocol version does not match the daemon's.
    ProtocolMismatch,
    /// A frame exceeded the size cap.
    OversizedFrame,
    /// The request was malformed / could not be decoded.
    BadRequest,
    /// An internal daemon failure.
    Internal,
}

/// The `ef status` payload. Memory fields come from the existing
/// `everyfind::sysinfo::current_memory` (no external crate).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    /// Drive letter the daemon indexes (e.g. `C`).
    pub drive: char,
    /// Total entries (including tombstones).
    pub entries: u64,
    /// Live (non-`DEAD`) entries.
    pub live_entries: u64,
    /// Entries that received an `ef du` size from the size pass (M5). `< entries` reveals a
    /// fail-soft resolution shortfall (surfaced as `sizes: <resolved> / <entries>`).
    pub sizes_resolved: u64,
    /// Volume `next_usn` minus our applied cursor, as of the last poll (0 = caught up).
    pub usn_lag: u64,
    /// Seconds since the last successful journal sync.
    pub last_sync_secs: u64,
    /// `WorkingSetSize` in bytes.
    pub working_set: u64,
    /// `PrivateUsage` in bytes.
    pub private_usage: u64,
    /// True while the daemon is still building its initial index (searches are not ready yet).
    pub building: bool,
    /// Records enumerated so far during the initial build (progress; 0 once ready).
    pub build_progress: u64,
    /// Count of snapshots written this run (0 = none yet).
    pub snapshot_generation: u64,
    /// Seconds since the last snapshot write (`None` = never).
    pub last_snapshot_secs: Option<u64>,
    /// USN cursor of the last snapshot (0 = none yet).
    pub snapshot_cursor: u64,
    /// Daemon uptime in seconds.
    pub uptime_secs: u64,
    /// Journal poll interval in milliseconds.
    pub poll_interval_ms: u64,
    /// The daemon's protocol version.
    pub proto_version: u16,
    /// Daemon process id.
    pub pid: u32,
}

/// A framing / protocol error. `Io` and `Bincode` wrap non-comparable inner errors, so this
/// type is not `PartialEq`; tests match on the variant.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// Underlying reader/writer failure (includes an unexpected EOF mid-frame).
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The frame's `len` exceeded the cap (checked before allocation).
    #[error("frame length {len} exceeds cap {cap}")]
    Oversized { len: u32, cap: u32 },
    /// The frame's `len` was too small to contain the version header.
    #[error("malformed frame: {0}")]
    Malformed(&'static str),
    /// The frame's protocol version did not match [`PROTO_VERSION`] (checked before decode).
    #[error("protocol version mismatch: peer {found}, expected {expected}")]
    ProtocolMismatch { found: u16, expected: u16 },
    /// bincode failed to encode or decode the payload.
    #[error("bincode: {0}")]
    Bincode(#[from] bincode::Error),
}

/// Write one framed message: `[len][version][payload]`. Fails with [`WireError::Oversized`]
/// (writing nothing) if `version + payload` would exceed `cap`.
fn write_message<W: Write>(
    w: &mut W,
    version: u16,
    payload: &[u8],
    cap: u32,
) -> Result<(), WireError> {
    let len = 2u64 + payload.len() as u64; // version + payload
    if len > cap as u64 {
        return Err(WireError::Oversized {
            len: len.min(u32::MAX as u64) as u32,
            cap,
        });
    }
    w.write_all(&(len as u32).to_le_bytes())?;
    w.write_all(&version.to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

/// Read one framed message, returning `(version, payload)`. The `len` is validated against
/// `cap` **before** the payload buffer is allocated, so an oversized `len` cannot drive an
/// allocation. `read_exact` transparently handles a frame split across multiple reads (or
/// several frames coalesced into one read).
fn read_message<R: Read>(r: &mut R, cap: u32) -> Result<(u16, Vec<u8>), WireError> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    if len < 2 {
        return Err(WireError::Malformed(
            "frame shorter than the version header",
        ));
    }
    if len > cap {
        return Err(WireError::Oversized { len, cap });
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    let version = u16::from_le_bytes([body[0], body[1]]);
    Ok((version, body[2..].to_vec()))
}

/// Serialize and frame a [`Request`] (client side).
pub fn write_request<W: Write>(w: &mut W, req: &Request) -> Result<(), WireError> {
    let payload = bincode::serialize(req)?;
    write_message(w, PROTO_VERSION, &payload, REQUEST_MAX_FRAME)
}

/// Read and decode a [`Request`] (server side). The version is checked **before** decoding.
pub fn read_request<R: Read>(r: &mut R) -> Result<Request, WireError> {
    let (version, body) = read_message(r, REQUEST_MAX_FRAME)?;
    if version != PROTO_VERSION {
        return Err(WireError::ProtocolMismatch {
            found: version,
            expected: PROTO_VERSION,
        });
    }
    Ok(bincode::deserialize(&body)?)
}

/// Serialize and frame a [`Response`] (server side).
/// Write a [`Response`], substituting an error for one too large to send.
///
/// An answer over [`RESPONSE_MAX_FRAME`] used to fail here and be dropped: the caller logged
/// at `debug` and closed the connection, so the client saw a socket close mid-frame and
/// reported "failed to fill whole buffer", a transport error for what is a perfectly ordinary
/// situation (a query matching more rows than a frame holds). Saying so costs one more
/// serialisation on a path that is already failing, and turns an unexplained disconnect into a
/// sentence naming the cap and the size that exceeded it.
pub fn write_response<W: Write>(w: &mut W, resp: &Response) -> Result<(), WireError> {
    let payload = bincode::serialize(resp)?;
    if payload.len() > RESPONSE_MAX_FRAME as usize {
        let too_big = Response::Error {
            code: ErrCode::OversizedFrame,
            message: format!(
                "the answer is {} bytes, over the {RESPONSE_MAX_FRAME}-byte response cap - \
                 ask for fewer rows (--limit)",
                payload.len()
            ),
        };
        let payload = bincode::serialize(&too_big)?;
        return write_message(w, PROTO_VERSION, &payload, RESPONSE_MAX_FRAME);
    }
    write_message(w, PROTO_VERSION, &payload, RESPONSE_MAX_FRAME)
}

/// Read and decode a [`Response`] (client side). The version is checked **before** decoding.
pub fn read_response<R: Read>(r: &mut R) -> Result<Response, WireError> {
    let (version, body) = read_message(r, RESPONSE_MAX_FRAME)?;
    if version != PROTO_VERSION {
        return Err(WireError::ProtocolMismatch {
            found: version,
            expected: PROTO_VERSION,
        });
    }
    Ok(bincode::deserialize(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// An answer too large for a frame comes back as an error, not as a closed connection.
    ///
    /// It used to fail the write and be dropped: the daemon logged at `debug`, the client saw
    /// the socket close mid-frame and reported "failed to fill whole buffer". That is a
    /// transport error for what is an ordinary situation: a query matched more rows than a
    /// frame holds.
    #[test]
    fn an_oversized_answer_is_explained_rather_than_dropped() {
        // Rows of ~1 KiB each, enough of them to pass the cap.
        let row = "x".repeat(1_000);
        let results: Vec<SearchHit> = (0..(RESPONSE_MAX_FRAME as usize / 1_000) + 16)
            .map(|i| SearchHit {
                path: format!("{row}{i}"),
                is_dir: false,
            })
            .collect();
        let huge = Response::Search {
            total_hits: results.len() as u64,
            results,
            building: false,
        };
        assert!(
            bincode::serialize(&huge).unwrap().len() > RESPONSE_MAX_FRAME as usize,
            "the fixture is not actually oversized"
        );

        let mut buf = Vec::new();
        write_response(&mut buf, &huge).expect("an oversized answer must still send something");
        let got = read_response(&mut Cursor::new(buf)).expect("and it must be readable");
        match got {
            Response::Error { code, message } => {
                assert_eq!(code, ErrCode::OversizedFrame);
                assert!(
                    message.contains(&RESPONSE_MAX_FRAME.to_string()),
                    "the message should name the cap: {message}"
                );
            }
            other => panic!("expected an oversized-frame error, got {other:?}"),
        }
    }

    /// A reader that yields at most `chunk` bytes per `read`, to exercise partial reads /
    /// `read_exact` looping (a frame split across several `ReadFile`s).
    struct ChunkReader {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
    }
    impl Read for ChunkReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let remaining = &self.data[self.pos..];
            let n = remaining.len().min(buf.len()).min(self.chunk);
            buf[..n].copy_from_slice(&remaining[..n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn sample_request() -> Request {
        Request::Search {
            query: "kernel32".to_string(),
            case_sensitive: false,
            include_orphans: false,
            limit: 20,
            reserve_elsewhere: 0,
        }
    }

    #[test]
    fn request_round_trip() {
        let req = sample_request();
        let mut buf = Vec::new();
        write_request(&mut buf, &req).unwrap();
        let got = read_request(&mut Cursor::new(buf)).unwrap();
        assert_eq!(got, req);
    }

    #[test]
    fn response_round_trip() {
        // A file and a directory hit, to exercise both `is_dir` values (protocol v2).
        let resp = Response::Search {
            total_hits: 42,
            results: vec![
                SearchHit {
                    path: r"C:\Windows\System32\kernel32.dll".to_string(),
                    is_dir: false,
                },
                SearchHit {
                    path: r"C:\Windows\System32".to_string(),
                    is_dir: true,
                },
            ],
            building: false,
        };
        let mut buf = Vec::new();
        write_response(&mut buf, &resp).unwrap();
        let got = read_response(&mut Cursor::new(buf)).unwrap();
        assert_eq!(got, resp);
    }

    #[test]
    fn status_report_round_trip() {
        let resp = Response::Status(StatusReport {
            drive: 'C',
            entries: 6_005_951,
            live_entries: 6_005_883,
            sizes_resolved: 6_005_900,
            usn_lag: 0,
            last_sync_secs: 2,
            working_set: 502 * 1024 * 1024,
            private_usage: 611 * 1024 * 1024,
            building: false,
            build_progress: 0,
            snapshot_generation: 3,
            last_snapshot_secs: Some(120),
            snapshot_cursor: 18_053_124_752,
            uptime_secs: 3600,
            poll_interval_ms: 1000,
            proto_version: PROTO_VERSION,
            pid: 4242,
        });
        let mut buf = Vec::new();
        write_response(&mut buf, &resp).unwrap();
        assert_eq!(read_response(&mut Cursor::new(buf)).unwrap(), resp);
    }

    #[test]
    fn partial_reads_reassemble_frame() {
        // A frame delivered one byte per read must still decode (read_exact loops).
        let req = sample_request();
        let mut buf = Vec::new();
        write_request(&mut buf, &req).unwrap();
        let mut r = ChunkReader {
            data: buf,
            pos: 0,
            chunk: 1,
        };
        assert_eq!(read_request(&mut r).unwrap(), req);
    }

    #[test]
    fn multiple_frames_in_one_buffer() {
        // Two frames coalesced into one stream must read back as two distinct messages.
        let a = Request::Search {
            query: "a".into(),
            case_sensitive: true,
            include_orphans: false,
            limit: 1,
            reserve_elsewhere: 0,
        };
        let b = Request::Status;
        let mut buf = Vec::new();
        write_request(&mut buf, &a).unwrap();
        write_request(&mut buf, &b).unwrap();
        let mut cur = Cursor::new(buf);
        assert_eq!(read_request(&mut cur).unwrap(), a);
        assert_eq!(read_request(&mut cur).unwrap(), b);
    }

    #[test]
    fn write_cap_boundary_is_inclusive() {
        // len == cap is allowed; len == cap + 1 is rejected. cap counts version + payload.
        let mut ok = Vec::new();
        assert!(write_message(&mut ok, PROTO_VERSION, &[0u8; 8], 10).is_ok());
        let mut over = Vec::new();
        let err = write_message(&mut over, PROTO_VERSION, &[0u8; 9], 10).unwrap_err();
        assert!(matches!(err, WireError::Oversized { len: 11, cap: 10 }));
        // On rejection nothing is written.
        assert!(over.is_empty());
    }

    #[test]
    fn write_request_rejects_oversized_payload() {
        // Request direction: a > 64 KiB query is refused at write time.
        let req = Request::Search {
            query: "x".repeat(70_000),
            case_sensitive: false,
            include_orphans: false,
            limit: 20,
            reserve_elsewhere: 0,
        };
        let mut buf = Vec::new();
        let err = write_request(&mut buf, &req).unwrap_err();
        assert!(matches!(err, WireError::Oversized { cap, .. } if cap == REQUEST_MAX_FRAME));
        assert!(buf.is_empty());
    }

    #[test]
    fn read_rejects_oversized_len_without_allocating() {
        // Both directions: a header whose len exceeds the cap is rejected before the body is
        // read, so a truncated stream (header only) still yields Oversized, not UnexpectedEof.
        for (cap, over) in [
            (REQUEST_MAX_FRAME, REQUEST_MAX_FRAME + 1),
            (RESPONSE_MAX_FRAME, RESPONSE_MAX_FRAME + 1),
        ] {
            let header = over.to_le_bytes(); // 4-byte len, no body
            let err = read_message(&mut Cursor::new(header.to_vec()), cap).unwrap_err();
            assert!(matches!(err, WireError::Oversized { len, cap: c } if len == over && c == cap));
        }
    }

    #[test]
    fn read_request_rejects_oversized_len() {
        let header = (REQUEST_MAX_FRAME + 1).to_le_bytes();
        let err = read_request(&mut Cursor::new(header.to_vec())).unwrap_err();
        assert!(matches!(err, WireError::Oversized { cap, .. } if cap == REQUEST_MAX_FRAME));
    }

    #[test]
    fn read_response_rejects_oversized_len() {
        let header = (RESPONSE_MAX_FRAME + 1).to_le_bytes();
        let err = read_response(&mut Cursor::new(header.to_vec())).unwrap_err();
        assert!(matches!(err, WireError::Oversized { cap, .. } if cap == RESPONSE_MAX_FRAME));
    }

    #[test]
    fn version_mismatch_detected_before_bincode_decode() {
        // A frame carrying a future version and a body that is NOT valid bincode for Request
        // must surface as ProtocolMismatch, proving the version gate precedes decoding.
        let garbage = [0xffu8, 0xff, 0xff, 0xff];
        let mut buf = Vec::new();
        write_message(&mut buf, PROTO_VERSION + 1, &garbage, REQUEST_MAX_FRAME).unwrap();
        let err = read_request(&mut Cursor::new(buf)).unwrap_err();
        assert!(matches!(
            err,
            WireError::ProtocolMismatch {
                found,
                expected
            } if found == PROTO_VERSION + 1 && expected == PROTO_VERSION
        ));
    }

    #[test]
    fn stale_client_frame_rejected_as_mismatch() {
        // A stale older request frame reaching the current daemon must surface as
        // ProtocolMismatch *before* any bincode decode: the version gate stands ahead of the
        // body. Pins the current PROTO_VERSION and the older->current skew behavior.
        assert_eq!(PROTO_VERSION, 6);
        let stale = PROTO_VERSION - 1;
        let mut buf = Vec::new();
        write_message(&mut buf, stale, &[0u8; 4], REQUEST_MAX_FRAME).unwrap();
        let err = read_request(&mut Cursor::new(buf)).unwrap_err();
        assert!(matches!(
            err,
            WireError::ProtocolMismatch { found, expected }
                if found == stale && expected == PROTO_VERSION
        ));
    }

    #[test]
    fn du_request_and_response_round_trip() {
        let req = Request::Du {
            path: r"C:\Users".to_string(),
            depth: 1,
            top_n: 20,
            real: false,
        };
        let mut buf = Vec::new();
        write_request(&mut buf, &req).unwrap();
        assert_eq!(read_request(&mut Cursor::new(buf)).unwrap(), req);

        let resp = Response::Du {
            total_bytes: 49_152,
            truncated: false,
            real: false,
            sizes_resolved: 2,
            entries: 2,
            rows: vec![
                DuRowWire {
                    path: r"C:\Users\alice".to_string(),
                    is_dir: true,
                    size_bytes: 32_768,
                    truncated: false,
                },
                DuRowWire {
                    path: r"C:\Users\big.bin".to_string(),
                    is_dir: false,
                    size_bytes: 16_384,
                    truncated: false,
                },
            ],
        };
        let mut rbuf = Vec::new();
        write_response(&mut rbuf, &resp).unwrap();
        assert_eq!(read_response(&mut Cursor::new(rbuf)).unwrap(), resp);
    }

    #[test]
    fn malformed_short_frame_rejected() {
        // len < 2 cannot hold the version header.
        let header = 1u32.to_le_bytes();
        let err = read_message(&mut Cursor::new(header.to_vec()), REQUEST_MAX_FRAME).unwrap_err();
        assert!(matches!(err, WireError::Malformed(_)));
    }

    #[test]
    fn utf8_queries_round_trip() {
        // Japanese, an emoji, a surrogate-pair astral char, a ZWJ sequence, empty, and limit 0.
        for query in ["", "日本語のファイル名", "検索🔍", "𝕏マーク", "👨‍👩‍👧‍👦"]
        {
            let req = Request::Search {
                query: query.to_string(),
                case_sensitive: false,
                include_orphans: true,
                limit: 0,
                reserve_elsewhere: 0,
            };
            let mut buf = Vec::new();
            write_request(&mut buf, &req).unwrap();
            assert_eq!(read_request(&mut Cursor::new(buf)).unwrap(), req);

            let resp = Response::Search {
                total_hits: 1,
                results: vec![SearchHit {
                    path: format!(r"C:\{query}"),
                    is_dir: false,
                }],
                building: false,
            };
            let mut rbuf = Vec::new();
            write_response(&mut rbuf, &resp).unwrap();
            assert_eq!(read_response(&mut Cursor::new(rbuf)).unwrap(), resp);
        }
    }

    #[test]
    fn total_hits_may_exceed_returned_results() {
        // The truncation contract: total_hits is carried independently of results.len().
        let resp = Response::Search {
            total_hits: 100_000,
            results: vec![
                SearchHit {
                    path: r"C:\a".to_string(),
                    is_dir: false,
                },
                SearchHit {
                    path: r"C:\b".to_string(),
                    is_dir: true,
                },
            ],
            building: false,
        };
        let mut buf = Vec::new();
        write_response(&mut buf, &resp).unwrap();
        match read_response(&mut Cursor::new(buf)).unwrap() {
            Response::Search {
                total_hits,
                results,
                building: _,
            } => {
                assert_eq!(total_hits, 100_000);
                assert_eq!(results.len(), 2);
            }
            other => panic!("expected Search, got {other:?}"),
        }
    }
}
