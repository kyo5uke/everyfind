//! `efd`: the Everyfind daemon (M3).
//!
//! Holds the in-memory filename index, tails the USN journal to keep it live (M2), and serves
//! searches / status to the `ef` client over the `\\.\pipe\everyfind` named pipe.
//!
//! Modes:
//! - `efd --foreground`: run in the console (Ctrl-C to stop). Requires an **elevated**
//!   terminal (opening `\\.\C:` needs admin).
//! - service mode (`efd service-run`, added M3 step 6): run under the SCM as LocalSystem.
//!
//! The snapshot/config live under `%ProgramData%\everyfind\` (absolute paths: a service's CWD
//! is `System32`).

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;

use everyfind::daemon::{self, DaemonConfig};
use everyfind::ipc::acl::AclMode;
use everyfind::service;

/// Everyfind daemon: resident index + USN tail + named-pipe server.
#[derive(Debug, Parser)]
#[command(name = "efd", version, about)]
struct Args {
    /// Run in the foreground (console output; Ctrl-C to stop).
    #[arg(long)]
    foreground: bool,

    /// Run under the Windows Service Control Manager (invoked by the SCM, not by hand).
    #[arg(long, conflicts_with = "foreground")]
    service_run: bool,

    /// Target NTFS volume, e.g. `C:`.
    #[arg(long, default_value = "C:")]
    volume: String,

    /// Pipe access mode: `interactive` (default; non-elevated `ef` works) or `admins`.
    #[arg(long, default_value = "interactive")]
    acl: String,

    /// Snapshot path (default: `%ProgramData%\everyfind\index.snapshot`).
    #[arg(long, value_name = "PATH")]
    snapshot: Option<PathBuf>,

    /// Journal poll interval, milliseconds.
    #[arg(long, default_value_t = 1000)]
    poll_ms: u64,

    /// Rebuild the index when arena garbage exceeds this fraction (0..1).
    #[arg(long, default_value_t = 0.3)]
    garbage_threshold: f64,

    /// Minimum seconds between periodic snapshot writes.
    #[arg(long, default_value_t = 300)]
    save_interval_secs: u64,
}

fn build_config(args: &Args) -> Result<DaemonConfig> {
    let acl = AclMode::parse(&args.acl)
        .ok_or_else(|| anyhow!("invalid --acl {:?} (use interactive|admins)", args.acl))?;
    let snapshot_path = args
        .snapshot
        .clone()
        .unwrap_or_else(service::default_snapshot_path);
    // Create the data directory with an explicit SY+BA-only DACL (snapshot/log must not be a
    // side channel around the pipe ACL; see service::ensure_secure_dir).
    if let Some(parent) = snapshot_path.parent() {
        service::ensure_secure_dir(parent)
            .with_context(|| format!("creating the data directory {}", parent.display()))?;
    }
    Ok(DaemonConfig {
        volume: args.volume.clone(),
        acl,
        snapshot_path,
        poll_interval: Duration::from_millis(args.poll_ms),
        garbage_threshold: args.garbage_threshold,
        save_interval: Duration::from_secs(args.save_interval_secs),
    })
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.service_run {
        // The SCM invokes this. Logging is initialized to the rotating file inside run_service;
        // do NOT init a console subscriber here.
        let cfg = build_config(&args)?;
        return service::run_service(cfg);
    }

    if !args.foreground {
        return Err(anyhow!(
            "efd must be run with --foreground (console) or as an installed service \
             (use `ef service install`)"
        ));
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = build_config(&args)?;
    daemon::run(cfg, true, Arc::new(AtomicBool::new(false)))
}
