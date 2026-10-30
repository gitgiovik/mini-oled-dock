//! Embeds the Windows manifest in the exe: elevated execution (`requireAdministrator`, needed to act
//! on windows of elevated apps), per-monitor v2 DPI and the segment heap (less fragmentation).
//! Also delay-loads rarely used system DLLs so the watchdog and CLI modes never map them.
//! API checked against the embed-manifest 1.5.1 source (`embed_manifest`, `new_manifest`,
//! `ManifestBuilder::requested_execution_level` / `dpi_awareness` / `heap_type`).

use embed_manifest::manifest::{DpiAwareness, ExecutionLevel, HeapType};
use embed_manifest::{embed_manifest, new_manifest};

/// DLLs loaded lazily (only the dock proper uses them; the watchdog and CLI modes do not).
const DELAY_LOAD: &[&str] = &[
    "shell32.dll",
    "ole32.dll",
    "oleaut32.dll",
    "propsys.dll",
    "shlwapi.dll",
    "dwmapi.dll",
    "powrprof.dll",
    "psapi.dll",
    "api-ms-win-shcore-scaling-l1-1-1.dll",
    "api-ms-win-core-winrt-l1-1-0.dll",
];

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // `DOCK_NO_MANIFEST`: preview build (`--preview`) that can run without elevation.
        let level = if std::env::var_os("DOCK_NO_MANIFEST").is_some() {
            ExecutionLevel::AsInvoker
        } else {
            ExecutionLevel::RequireAdministrator
        };
        let manifest = new_manifest("Dock.Shell")
            .requested_execution_level(level)
            .dpi_awareness(DpiAwareness::PerMonitorV2)
            .heap_type(HeapType::SegmentHeap);
        if let Err(e) = embed_manifest(manifest) {
            // A missing manifest must not fail the build, but it must be reported.
            println!("cargo:warning=cannot embed the manifest: {e}");
        }
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            for dll in DELAY_LOAD {
                println!("cargo:rustc-link-arg-bins=/DELAYLOAD:{dll}");
            }
            println!("cargo:rustc-link-arg-bins=delayimp.lib");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=DOCK_NO_MANIFEST");
}
