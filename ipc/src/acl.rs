//! Named-pipe access control (probe P14).
//!
//! The index holds every user's filenames, so the pipe DACL is a deliberate
//! information-disclosure decision. Two modes, built from SDDL:
//! - **Interactive** (default): SYSTEM + Administrators full; INTERACTIVE gets the explicit
//!   no-append mask `0x12018b` (read + write-data + write-attributes, **not**
//!   `FILE_APPEND_DATA` = `FILE_CREATE_PIPE_INSTANCE`, so an interactive user cannot squat a
//!   second instance). Lets a non-elevated `ef` connect.
//! - **Admins**: SYSTEM + Administrators only (requires an elevated client; see the UAC
//!   deny-only analysis in the plan).
//!
//! A NULL security descriptor is **never** used: P14(i) showed the OS default grants
//! `FILE_READ` to Everyone and Anonymous.

use std::ptr;

use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;

use super::pipe::{wide, Sd};

const SDDL_REVISION_1: u32 = 1;

/// Who may connect to the daemon's pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclMode {
    /// SYSTEM + Administrators + INTERACTIVE (non-elevated `ef` works). Default.
    Interactive,
    /// SYSTEM + Administrators only.
    Admins,
}

impl AclMode {
    /// The SDDL string for this mode. `GA` (generic all) canonicalizes to `FA` when applied to
    /// a kernel object (P14(ii)); the `0x12018b` INTERACTIVE mask is preserved verbatim.
    pub fn sddl(self) -> &'static str {
        match self {
            AclMode::Interactive => "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12018b;;;IU)",
            AclMode::Admins => "D:(A;;GA;;;SY)(A;;GA;;;BA)",
        }
    }

    /// Parse a `--acl` value (`interactive` / `admins`), case-insensitive.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "interactive" => Some(AclMode::Interactive),
            "admins" => Some(AclMode::Admins),
            _ => None,
        }
    }
}

/// Build a self-relative `SECURITY_DESCRIPTOR` for `mode`, returning the pointer to place in a
/// `SECURITY_ATTRIBUTES`. The allocation is **intentionally leaked**: it is a process-lifetime
/// singleton referenced whenever a new pipe instance is created (the SD is copied into each
/// instance at creation, so it only needs to outlive the daemon, which it does).
pub fn build_sd(mode: AclMode) -> anyhow::Result<Sd> {
    let w = wide(mode.sddl());
    let mut psd: Sd = ptr::null_mut();
    // SAFETY: `w` is a valid NUL-terminated wide SDDL string; `psd` receives the allocated SD.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w.as_ptr(),
            SDDL_REVISION_1,
            &mut psd,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        anyhow::bail!(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW failed for {:?}: {}",
            mode.sddl(),
            std::io::Error::last_os_error(),
        );
    }
    Ok(psd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sddl_strings_are_as_designed() {
        // Interactive grants IU the no-append mask; admins omits IU entirely.
        assert_eq!(
            AclMode::Interactive.sddl(),
            "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12018b;;;IU)"
        );
        assert_eq!(AclMode::Admins.sddl(), "D:(A;;GA;;;SY)(A;;GA;;;BA)");
        // No mode grants Everyone (WD) or Anonymous (AN), the reason we never use a NULL SD.
        for m in [AclMode::Interactive, AclMode::Admins] {
            assert!(!m.sddl().contains(";WD)"));
            assert!(!m.sddl().contains(";AN)"));
        }
    }

    #[test]
    fn parse_is_case_insensitive() {
        assert_eq!(AclMode::parse("interactive"), Some(AclMode::Interactive));
        assert_eq!(AclMode::parse("INTERACTIVE"), Some(AclMode::Interactive));
        assert_eq!(AclMode::parse("Admins"), Some(AclMode::Admins));
        assert_eq!(AclMode::parse("everyone"), None);
    }

    #[test]
    fn build_sd_succeeds_for_both_modes() {
        // The SDDL parses into a real SECURITY_DESCRIPTOR (non-null) for both modes.
        for m in [AclMode::Interactive, AclMode::Admins] {
            let sd = build_sd(m).expect("SDDL should convert");
            assert!(!sd.is_null());
        }
    }
}
