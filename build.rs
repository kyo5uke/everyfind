//! Stamp the Windows version resource (CompanyName / ProductName / version /
//! description) into every binary. A described, versioned executable is much
//! less likely to trip an ML antivirus heuristic than a metadata-less freshly
//! built one: the exact Bearfoos.B!ml false-positive shape we hit on a dev
//! machine. Fields come from `[package.metadata.winresource]` in Cargo.toml;
//! the version is taken from CARGO_PKG_VERSION automatically.
//!
//! No-op off Windows so a GNU/cross compile still builds.

fn main() {
    #[cfg(windows)]
    {
        // Rerun only when the manifest changes (the metadata source).
        println!("cargo:rerun-if-changed=Cargo.toml");
        if let Err(e) = winresource::WindowsResource::new().compile() {
            // Non-fatal: a missing resource compiler must not break `cargo
            // build`. The binary just ships without the metadata.
            println!("cargo:warning=version resource not stamped: {e}");
        }
    }
}
