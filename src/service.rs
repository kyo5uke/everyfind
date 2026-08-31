//! Windows service integration (M3 step 6): install / uninstall / start / stop, and the
//! service host (`efd service-run`) that runs the daemon under the SCM as LocalSystem.
//!
//! Design:
//! - Identity: name `everyfind`, display `Everyfind (instant file search)`, with a description.
//! - **Normal auto-start** (not delayed): snapshot restore ~ 1 s, so booting straight into a
//!   usable index is part of the product promise.
//! - **Failure actions**: restart after 5 s, twice, resetting the failure count after 1 day.
//! - Data under `%ProgramData%\everyfind\` with an **explicit SY+BA-only protected DACL**: the
//!   default would grant Users read, turning `index.snapshot` (every filename) and `efd.log`
//!   into a side channel around the pipe ACL (the file-level analogue of the P14 NULL-SD
//!   lesson). Absolute paths only (a service's CWD is `System32`).
//! - **Start responsiveness**: the first start with no snapshot runs a ~30 s enumeration; the
//!   host advances the `StartPending` checkpoint each second and honors a STOP mid-enumeration
//!   (via [`Win32Volume::set_cancel`](crate::volume::Win32Volume::set_cancel)).
//! - Uninstall removes the service registration **and** the data directory: no trace.

use std::ffi::{c_void, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use crate::daemon::{self, DaemonConfig};
use crate::ipc::acl::AclMode;
use crate::wide;

/// Service key name (SCM registry).
pub const SERVICE_NAME: &str = "everyfind";
/// Human-friendly display name.
pub const DISPLAY_NAME: &str = "Everyfind (instant file search)";
/// Service description.
pub const DESCRIPTION: &str =
    "Everyfind daemon: keeps an in-memory NTFS filename index live (MFT + USN journal) and \
     serves instant searches to the ef client over a local named pipe.";

/// Log rotation cap (`efd.log` -> `efd.log.1` at 10 MiB, one generation).
const LOG_CAP_BYTES: u64 = 10 * 1024 * 1024;

/// `%ProgramData%\everyfind`: absolute data directory. The `ProgramData` system environment
/// variable is present for LocalSystem and tracks any redirection, so it is authoritative here;
/// the fallback is the canonical default.
pub fn data_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("everyfind")
}

/// Default snapshot path under the data directory.
pub fn default_snapshot_path() -> PathBuf {
    data_dir().join("index.snapshot")
}

/// `efd.exe` next to the current executable (the client installs the daemon binary).
fn efd_exe_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the current executable")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("executable has no parent directory"))?;
    Ok(dir.join("efd.exe"))
}

// --- ProgramData directory with an explicit SY+BA-only protected DACL (refinement #1) ---

/// Create `path` (a directory) with an explicit DACL granting **only** SYSTEM and
/// Administrators full control, protected from inheritance and inheritable by its children, so
/// files created inside (`index.snapshot`, `efd.log`) do not expose all filenames to Users.
/// If the directory already exists, its DACL is re-applied (protected).
pub fn ensure_secure_dir(path: &Path) -> Result<()> {
    // P = protected (drop inherited ACEs); OICI = object+container inherit (children inherit);
    // FA = FILE_ALL_ACCESS. SY = LocalSystem, BA = Administrators. No Users, no Everyone.
    // `O:BA` sets the owner too, and that is not decoration. `%ProgramData%` lets any standard
    // user create a subdirectory, and the creator *owns* it, and an owner keeps implicit
    // READ_CONTROL and WRITE_DAC however tight the DACL is, so they can hand access back to
    // themselves whenever they like. Pre-create `%ProgramData%\everyfind` before the service is
    // ever installed and the protected DACL applied on top changes nothing: `index.snapshot` is
    // every filename on the volume, which is exactly what the pipe ACL exists to keep from
    // them, and it is also a file a LocalSystem process reads back at startup.
    const DIR_SDDL: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
    let psd = string_to_sd(DIR_SDDL)?;
    let wpath = wide(&path.to_string_lossy());

    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: psd,
        bInheritHandle: 0,
    };
    // SAFETY: valid wide path and SECURITY_ATTRIBUTES with a valid SD.
    let created = unsafe { CreateDirectoryW(wpath.as_ptr(), &sa) };
    if created == 0 {
        let e = unsafe { GetLastError() };
        if e == ERROR_ALREADY_EXISTS {
            apply_protected_dacl(&wpath, psd)
                .with_context(|| format!("re-applying the DACL on {}", path.display()))?;
        } else {
            bail!(
                "CreateDirectoryW({}) failed: {}",
                path.display(),
                io::Error::from_raw_os_error(e as i32)
            );
        }
    }
    Ok(())
}

/// Convert an SDDL string to a self-relative SECURITY_DESCRIPTOR (leaked; short-lived callers).
fn string_to_sd(sddl: &str) -> Result<*mut c_void> {
    let w = wide(sddl);
    let mut psd: *mut c_void = ptr::null_mut();
    // SAFETY: valid wide SDDL; `psd` receives the allocated SD.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w.as_ptr(),
            1,
            &mut psd,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        bail!(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW failed: {}",
            io::Error::last_os_error()
        );
    }
    Ok(psd)
}

/// Apply the DACL from `psd` to an existing directory, protected from inheritance.
fn apply_protected_dacl(wpath: &[u16], psd: *mut c_void) -> Result<()> {
    const SE_FILE_OBJECT: i32 = 1;
    const OWNER_SECURITY_INFORMATION: u32 = 0x0000_0001;
    const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
    const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;

    let mut present: i32 = 0;
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut defaulted: i32 = 0;
    // SAFETY: `psd` is a valid SD; out-params are valid.
    let ok = unsafe { GetSecurityDescriptorDacl(psd, &mut present, &mut dacl, &mut defaulted) };
    if ok == 0 || present == 0 {
        bail!(
            "GetSecurityDescriptorDacl failed: {}",
            io::Error::last_os_error()
        );
    }
    // The owner as well as the DACL. Re-applying only the DACL left whoever pre-created the
    // directory owning it, and an owner can undo any DACL; see `DIR_SDDL`.
    let mut owner: *mut c_void = ptr::null_mut();
    let mut owner_defaulted: i32 = 0;
    // SAFETY: `psd` is a valid SD; out-params are valid.
    let got_owner =
        unsafe { GetSecurityDescriptorOwner(psd, &mut owner, &mut owner_defaulted) } != 0;
    let owner = if got_owner { owner } else { ptr::null_mut() };
    let info = DACL_SECURITY_INFORMATION
        | PROTECTED_DACL_SECURITY_INFORMATION
        | if owner.is_null() {
            0
        } else {
            OWNER_SECURITY_INFORMATION
        };
    // SAFETY: valid object name, owner and DACL; group/sacl null (not changed).
    let rc = unsafe {
        SetNamedSecurityInfoW(
            wpath.as_ptr() as *mut u16,
            SE_FILE_OBJECT,
            info,
            owner,
            ptr::null_mut(),
            dacl,
            ptr::null_mut(),
        )
    };
    if rc != 0 {
        bail!(
            "SetNamedSecurityInfoW failed: {}",
            io::Error::from_raw_os_error(rc as i32)
        );
    }
    Ok(())
}

// --- install / uninstall / start / stop (client-side, admin required) ---

/// Install the service (auto-start, LocalSystem) pointing at `efd.exe service-run --volume ...
/// --acl ...`, set its description + failure actions, and create the secured data directory.
pub fn install(volume: &str, acl: AclMode) -> Result<()> {
    install_with_exe(&efd_exe_path()?, volume, acl)
}

/// Like [`install`], but with an explicit daemon executable path (used by the round-trip test,
/// whose `efd.exe` is not next to the test binary).
pub fn install_with_exe(efd_exe: &Path, volume: &str, acl: AclMode) -> Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("opening the service control manager (run as Administrator)")?;

    let acl_arg = match acl {
        AclMode::Interactive => "interactive",
        AclMode::Admins => "admins",
    };
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart, // normal auto-start (not delayed)
        error_control: ServiceErrorControl::Normal,
        executable_path: efd_exe.to_path_buf(),
        launch_arguments: vec![
            OsString::from("--service-run"),
            OsString::from("--volume"),
            OsString::from(volume),
            OsString::from("--acl"),
            OsString::from(acl_arg),
        ],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };

    let service = manager
        .create_service(
            &info,
            ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS,
        )
        .context("creating the service (already installed?)")?;
    service
        .set_description(DESCRIPTION)
        .context("setting the service description")?;

    // Restart after 5 s, twice, then give up; reset the failure count after a day.
    let failure = ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86_400)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5),
            },
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5),
            },
            ServiceAction {
                action_type: ServiceActionType::None,
                delay: Duration::ZERO,
            },
        ]),
    };
    service
        .update_failure_actions(failure)
        .context("configuring failure actions")?;

    ensure_secure_dir(&data_dir()).context("creating the secured data directory")?;
    Ok(())
}

/// Stop (if running) and delete the service, then remove the data directory: no trace.
pub fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("opening the service control manager (run as Administrator)")?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    match manager.open_service(SERVICE_NAME, access) {
        Ok(service) => {
            // Best-effort stop; ignore "not running".
            if let Ok(status) = service.query_status() {
                if status.current_state != ServiceState::Stopped {
                    let pid = status.process_id;
                    let _ = service.stop();
                    // The same 90 s `stop()` allows, for the same reason: the final snapshot
                    // takes as long as it takes, and it does not take less because this is an
                    // uninstall. At 15 s a six-million-entry index on a cold disk could still
                    // be saving, and then `delete()` would mark a running service for removal
                    // and `remove_dir_all` would race the daemon still writing into that very
                    // directory, failing with "directory not empty" *after* the registration
                    // was gone. The stated promise is "no trace"; the failure left the data.
                    if !wait_for_state(&service, ServiceState::Stopped, Duration::from_secs(90)) {
                        anyhow::bail!(
                            "the service did not stop within 90 s; refusing to delete it \
                             while it may still be writing to its data directory"
                        );
                    }
                    if !wait_for_process_exit(pid, Duration::from_secs(30)) {
                        anyhow::bail!(
                            "the service reported STOPPED but its process is still running; \
                             refusing to remove its data directory underneath it"
                        );
                    }
                }
            }
            service.delete().context("deleting the service")?;
        }
        Err(e) => {
            // Not installed is fine for an idempotent uninstall; other errors surface.
            tracing::warn!("open_service during uninstall: {e} (already removed?)");
        }
    }
    // Remove the data directory (snapshot + log). Leaves no trace.
    let dir = data_dir();
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("removing the data directory {}", dir.display()))?;
    }
    Ok(())
}

/// Start the installed service.
pub fn start() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::START | ServiceAccess::QUERY_STATUS,
        )
        .context("opening the service (installed?)")?;
    service.start::<&str>(&[]).context("starting the service")?;
    Ok(())
}

/// Stop the installed service.
pub fn stop() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
        )
        .context("opening the service (installed?)")?;
    // The process id, read before the stop; afterwards there is nothing left to ask.
    let pid = service.query_status().ok().and_then(|s| s.process_id);
    service.stop().context("stopping the service")?;
    // Generous wait: the daemon takes a final snapshot on stop (~0.67 s in release, slower in a
    // debug build / on a cold disk); the service advances its StopPending checkpoint meanwhile.
    if !wait_for_state(&service, ServiceState::Stopped, Duration::from_secs(90)) {
        anyhow::bail!("the service did not reach STOPPED within 90 s");
    }
    // And then for the process itself, so that "stopped" means the binary can be replaced.
    if !wait_for_process_exit(pid, Duration::from_secs(30)) {
        anyhow::bail!("the service reported STOPPED but its process is still running");
    }
    Ok(())
}

/// Whether the service is currently installed.
pub fn is_installed() -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS))
        .is_ok()
}

/// Whether the service is currently in the `Running` state.
pub fn is_running() -> bool {
    query_state() == Some(ServiceState::Running)
}

/// Whether the service is stopped (or not installed).
pub fn is_stopped() -> bool {
    matches!(query_state(), Some(ServiceState::Stopped) | None)
}

/// Poll until the service reaches `Running`, or `timeout` elapses. Returns whether it did.
pub fn wait_until_running(timeout: Duration) -> bool {
    wait_until(is_running, timeout)
}

/// Poll until the service is stopped, or `timeout` elapses. Returns whether it did.
pub fn wait_until_stopped(timeout: Duration) -> bool {
    wait_until(is_stopped, timeout)
}

fn wait_until(cond: fn() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(300));
    }
}

fn query_state() -> Option<ServiceState> {
    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)
        .ok()?;
    service.query_status().ok().map(|s| s.current_state)
}

/// Wait for the service to reach `target`. Returns whether it did.
///
/// The result used to be discarded, so both callers reported success unconditionally.
fn wait_for_state(
    service: &windows_service::service::Service,
    target: ServiceState,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match service.query_status() {
            Ok(s) if s.current_state == target => return true,
            _ => thread::sleep(Duration::from_millis(200)),
        }
    }
    false
}

/// Wait for *this service's* process to be gone, which is not the same as the SCM reporting
/// STOPPED.
///
/// The SCM flips to STOPPED when the service *tells* it to; the process then runs its remaining
/// teardown and finally closes its handles. In between, `efd.exe` on disk is still open, which
/// is exactly when somebody replaces the binary and gets "the process cannot access the file
/// because it is being used by another process". Measured: that is what happens if you copy
/// immediately after `ef service stop` returns.
///
/// By process id, not by name. Matching on `efd.exe` catches the documented foreground mode
/// (`efd --foreground`), another user's session and a second install, none of which have
/// anything to do with this service, and a stop that refuses because of one of those is a
/// stop that cannot be completed at all. `pid` is read from the service's own status before
/// the stop; `None` (already stopped, or the SCM would not say) means there is nothing to wait
/// for.
fn wait_for_process_exit(pid: Option<u32>, timeout: Duration) -> bool {
    let Some(pid) = pid.filter(|p| *p != 0) else {
        return true;
    };
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_alive(pid) {
            // The handle can outlive the process by a moment; give the kernel that moment.
            thread::sleep(Duration::from_millis(200));
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Whether the process with this id still exists.
///
/// `OpenProcess` for `SYNCHRONIZE` succeeds while the process is alive, including while it is
/// a zombie whose handles are still open, which is the state being waited out. `ERROR_ACCESS_DENIED`
/// means it exists and is not ours to open, which still counts as alive; anything else means it
/// is gone.
fn process_alive(pid: u32) -> bool {
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const ERROR_ACCESS_DENIED: u32 = 5;
    // SAFETY: a plain id-to-handle call; the handle is closed below if one came back.
    let h = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if h.is_null() {
        return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
    }
    // SAFETY: a handle we just opened.
    unsafe { CloseHandle(h) };
    true
}

// --- service host (`efd service-run`) ---

/// Config handed from `efd`'s CLI to the SCM-invoked [`service_main`] via a set-once global
/// (the dispatcher calls `service_main` with the SCM's start args, not our binPath args).
static SERVICE_CFG: OnceLock<DaemonConfig> = OnceLock::new();

/// Entry for `efd service-run`: stash the config, init file logging, and hand control to the
/// SCM dispatcher (which blocks until the service stops).
pub fn run_service(cfg: DaemonConfig) -> Result<()> {
    let _ = SERVICE_CFG.set(cfg);
    init_service_logging();
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("starting the service dispatcher")?;
    Ok(())
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service_inner() {
        tracing::error!("service exited with error: {e:#}");
    }
}

fn run_service_inner() -> Result<()> {
    let cfg = SERVICE_CFG
        .get()
        .ok_or_else(|| anyhow!("service config not set"))?
        .clone();

    // The control handler only signals; the daemon does the graceful save on shutdown.
    let event_handler = move |control| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                daemon::request_shutdown();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
        .context("registering the service control handler")?;

    let set_status = |state: ServiceState, checkpoint: u32, wait_hint: Duration, accept: bool| {
        let controls_accepted = if accept {
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        } else {
            ServiceControlAccept::empty()
        };
        let _ = status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint,
            wait_hint,
            process_id: None,
        });
    };

    // StartPending: accept STOP even during the initial enumeration.
    let mut checkpoint = 1u32;
    set_status(
        ServiceState::StartPending,
        checkpoint,
        Duration::from_secs(30),
        true,
    );

    // Run the daemon on a worker; it flips `ready` once the pipe is listening.
    let ready = Arc::new(AtomicBool::new(false));
    let worker = {
        let ready = Arc::clone(&ready);
        thread::spawn(move || daemon::run(cfg, false, ready))
    };

    // Advance the checkpoint each second until ready, or until the worker exits (a STOP during
    // enumeration cancels it, or a real error).
    while !ready.load(Ordering::SeqCst) {
        if worker.is_finished() {
            // Exited before listening: a STOP cancelled the enumeration (clean) or start failed.
            let result = worker
                .join()
                .unwrap_or_else(|_| Err(anyhow!("worker panicked")));
            report_stopped(&status_handle, &result);
            return result;
        }
        checkpoint += 1;
        set_status(
            ServiceState::StartPending,
            checkpoint,
            Duration::from_secs(30),
            true,
        );
        thread::sleep(Duration::from_secs(1));
    }

    // Running.
    set_status(ServiceState::Running, 0, Duration::ZERO, true);
    tracing::info!("service running");

    // Wait for the worker to finish (a STOP/SHUTDOWN triggers the daemon's final save). While
    // stopping, keep advancing the StopPending checkpoint each second so the SCM sees progress
    // even if the save runs long (wait hint sized from probe P15(a): ~0.67 s in release).
    loop {
        if worker.is_finished() {
            break;
        }
        if daemon::is_shutdown_requested() {
            checkpoint += 1;
            set_status(
                ServiceState::StopPending,
                checkpoint,
                Duration::from_secs(10),
                false,
            );
        }
        thread::sleep(Duration::from_secs(1));
    }

    let result = worker
        .join()
        .unwrap_or_else(|_| Err(anyhow!("worker panicked")));
    report_stopped(&status_handle, &result);
    result
}

/// Report the terminal `Stopped` state, mapping a start/run failure to a service-specific
/// non-zero exit code so the SCM's failure actions can trigger.
fn report_stopped(handle: &ServiceStatusHandle, result: &Result<()>) {
    let exit_code = match result {
        Ok(()) => ServiceExitCode::Win32(0),
        Err(_) => ServiceExitCode::ServiceSpecific(1),
    };
    let _ = handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code,
        checkpoint: 0,
        wait_hint: Duration::ZERO,
        process_id: None,
    });
}

/// Initialize file logging to `%ProgramData%\everyfind\efd.log` (rotated). Best-effort: a
/// service has no console.
fn init_service_logging() {
    let dir = data_dir();
    if let Err(e) = ensure_secure_dir(&dir) {
        // Fail closed. The log records searched paths and index state, and this is the call
        // that makes the directory unreadable to anyone but SYSTEM and Administrators, so
        // "could not secure it" must not be followed by "wrote to it anyway". Discarding the
        // error is what made that possible; the daemon simply runs without a log file.
        eprintln!(
            "efd: not logging; {} could not be secured: {e:#}",
            dir.display()
        );
        return;
    }
    let log_path = dir.join("efd.log");
    if let Ok(file) = RotatingFile::open(log_path, LOG_CAP_BYTES) {
        let writer = RotatingLog {
            inner: Arc::new(Mutex::new(file)),
        };
        let _ = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(writer)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .try_init();
    }
}

// --- size-capped log rotation (no external crate; one generation) ---

struct RotatingFile {
    path: PathBuf,
    cap: u64,
    file: File,
    written: u64,
}

impl RotatingFile {
    fn open(path: PathBuf, cap: u64) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path,
            cap,
            file,
            written,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        let _ = self.file.flush();
        let backup = PathBuf::from(format!("{}.1", self.path.display()));
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(&self.path, &backup)?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

#[derive(Clone)]
struct RotatingLog {
    inner: Arc<Mutex<RotatingFile>>,
}

impl Write for RotatingLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.written + buf.len() as u64 > g.cap {
            let _ = g.rotate();
        }
        let n = g.file.write(buf)?;
        g.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .file
            .flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RotatingLog {
    type Writer = RotatingLog;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// --- windows-sys FFI for the ProgramData directory DACL ---

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, ACL, SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
use windows_sys::Win32::System::Threading::OpenProcess;

const ERROR_ALREADY_EXISTS: u32 = 183;
