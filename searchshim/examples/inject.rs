//! Install the agent into explorer.exe via SetWindowsHookEx, confirm it landed,
//! then pull the hook back out.
//!
//! WH_GETMESSAGE with a specific thread id maps the agent DLL into that thread's
//! process (explorer's) the next time the thread pumps a message. We nudge it
//! with a WM_NULL, wait, read the agent's log, and unhook. Safe: a passthrough
//! hook changes nothing, and unhooking unloads the DLL.
//!
//!   cargo run --example inject -- <path-to-efagent.dll>

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetShellWindow, GetWindowThreadProcessId, PostMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    WH_GETMESSAGE, WM_NULL,
};

type HookFn = unsafe extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn main() {
    let dll = std::env::args()
        .nth(1)
        .expect("usage: inject <path-to-efagent.dll>");
    let log = std::env::temp_dir().join("efagent.log");
    let _ = std::fs::write(&log, b"");

    unsafe {
        let hmod = LoadLibraryW(wide(&dll).as_ptr());
        if hmod.is_null() {
            println!("could not load {dll}");
            return;
        }
        let proc = GetProcAddress(hmod, c"HookProc".as_ptr() as *const u8);
        let Some(proc) = proc else {
            println!("efagent.dll has no HookProc export");
            return;
        };
        let hookfn: HookFn = std::mem::transmute(proc);

        // Explorer's shell (desktop) thread, a known thread that lives in
        // explorer.exe. Good enough to prove we can get in.
        let shell = GetShellWindow();
        if shell.is_null() {
            println!("no shell window (explorer not running?)");
            return;
        }
        let mut pid = 0u32;
        let shell_tid = GetWindowThreadProcessId(shell, &mut pid);
        // A specific thread id can be passed as arg 3 to target the process that
        // hosts a search window (folder windows can live in their own process).
        let tid: u32 = std::env::args()
            .nth(3)
            .and_then(|s| s.parse().ok())
            .unwrap_or(shell_tid);
        println!("target thread {tid} (shell tid was {shell_tid}, pid {pid})");

        let hook = SetWindowsHookExW(WH_GETMESSAGE, Some(hookfn), hmod, tid);
        if hook.is_null() {
            println!("SetWindowsHookExW failed (err {})", last_error());
            return;
        }
        let hold_secs: u64 = std::env::args()
            .nth(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or(3);
        println!("hook installed; nudging explorer to load the agent (holding {hold_secs}s)...");

        // Make the hooked thread pump a message so the DLL maps in.
        for _ in 0..5 {
            PostMessageW(shell, WM_NULL, 0, 0);
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        // Keep the hook installed so a search can be driven while the agent's
        // vtable patch is live.
        std::thread::sleep(std::time::Duration::from_secs(hold_secs));

        UnhookWindowsHookEx(hook);
        println!("hook removed.");

        println!("--- efagent.log ---");
        match std::fs::read_to_string(&log) {
            Ok(s) if !s.trim().is_empty() => print!("{s}"),
            _ => println!("(empty) - agent did NOT load into explorer"),
        }
    }
}

fn last_error() -> u32 {
    unsafe { windows_sys::Win32::Foundation::GetLastError() }
}

// Keep the type import used even though it is only named in a transmute target.
#[allow(dead_code)]
fn _keep(_: *const c_void) {}
