//! Helpers shared by the sandbox tests.

use std::process::Command;
use std::sync::Once;

/// Path of the wasm of a probe crate under `probes/<dir>`, building it on first use. CI builds only
/// the contract itself, so a probe test builds its own wasm (cargo-near is installed there).
pub fn probe_wasm(dir: &str) -> String {
    static BUILD: Once = Once::new();
    let wasm = format!("target/near/{dir}/{dir}.wasm");
    BUILD.call_once(|| {
        if std::path::Path::new(&wasm).exists() {
            return;
        }
        let status = Command::new("cargo")
            .args(["near", "build", "non-reproducible-wasm", "--manifest-path"])
            .arg(format!("probes/{dir}/Cargo.toml"))
            .status()
            .expect("run cargo near build");
        assert!(status.success(), "cargo near build failed for probes/{dir}");
    });
    wasm
}
