//! Current-process memory statistics via `GetProcessMemoryInfo`.
//!
//! M1 acceptance records **both** `WorkingSetSize` and `PrivateUsage`.

use windows_sys::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// A snapshot of the current process's memory usage, in bytes.
#[derive(Debug, Clone, Copy)]
pub struct MemoryUsage {
    /// Physical memory currently mapped (`WorkingSetSize`).
    pub working_set: u64,
    /// Committed private (non-shared) bytes (`PrivateUsage`).
    pub private_usage: u64,
}

/// Read the current process's memory counters, or `None` if the query fails.
pub fn current_memory() -> Option<MemoryUsage> {
    // SAFETY: zeroed POD; `cb` set to the extended struct's size before the call.
    let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;

    // SAFETY: GetCurrentProcess returns a valid pseudo-handle; `counters` is writable and
    // its `cb` matches the buffer we pass (as the base PROCESS_MEMORY_COUNTERS type).
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            std::ptr::from_mut(&mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        )
    };
    if ok == 0 {
        return None;
    }
    Some(MemoryUsage {
        working_set: counters.WorkingSetSize as u64,
        private_usage: counters.PrivateUsage as u64,
    })
}
