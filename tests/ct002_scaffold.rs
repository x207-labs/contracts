//! CT-002 scaffold checks. Needs `cargo near build non-reproducible-wasm` first.

use std::path::Path;

const ABI: &str = "target/near/x207_circle_abi.json";

// TC-CT-002-01
#[test]
fn build_produces_wasm_and_abi_with_functions() {
    assert!(
        Path::new("target/near/x207_circle.wasm").exists(),
        "run cargo near build first"
    );
    let abi: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(ABI).expect("run cargo near build first"))
            .unwrap();
    let functions = abi["body"]["functions"]
        .as_array()
        .expect("functions array");
    assert!(!functions.is_empty());
}

// TC-CT-002-02
#[test]
fn near_sdk_is_pinned_exact() {
    let manifest = std::fs::read_to_string("Cargo.toml").unwrap();
    assert!(
        manifest.contains("near-sdk = \"=5.29.1\"")
            || manifest.contains("near-sdk = { version = \"=5.29.1\", features = [\"unstable\"] }")
    );
    let lock = std::fs::read_to_string("Cargo.lock").unwrap();
    assert!(lock.contains("name = \"near-sdk\"\nversion = \"5.29.1\""));
}

// TC-CT-002-03
#[test]
fn module_tree_matches_structure() {
    for module in [
        "lib",
        "member",
        "voucher",
        "clearing",
        "settlement",
        "bundle",
        "risk",
        "params",
        "events",
    ] {
        assert!(
            Path::new(&format!("x207-circle/src/{module}.rs")).exists(),
            "{module}.rs"
        );
    }
    assert!(Path::new("tests").is_dir());
    assert!(Path::new("dao").is_dir());
}
