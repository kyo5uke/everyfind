//! Stamp the Windows version resource into `ef_search_engine.dll`, for the same reason the
//! main crate does it, only more so.
//!
//! This DLL shadows a system CLSID through an HKCU registration and patches vtables in
//! explorer.exe. That is, to an ML heuristic, an almost perfect description of malware; an
//! unsigned binary carrying no company, product or version metadata is the shape that trips
//! it (the `Bearfoos.B!ml` false positive the main binaries hit). Metadata does not make it
//! signed, but it removes the cheapest reason to flag it.
//!
//! No-op off Windows so a cross compile still builds.

fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=Cargo.toml");
        if let Err(e) = winresource::WindowsResource::new().compile() {
            // Non-fatal: a missing resource compiler must not break the build. The DLL just
            // ships without the metadata.
            println!("cargo:warning=version resource not stamped: {e}");
        }
    }
}
