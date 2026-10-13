//! Collector contract (`src/assets/collect.sh`).
//!
//! The scenario lives in `tests/collect.test.sh`: the production script under
//! a private `HOME`, with a stub `curl` standing in for the Sprites gateway
//! and the object store behind it. Linux only — the script uses GNU `find`,
//! `flock`, and `/dev/shm`, and a sprite is the only place it ever runs.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;

#[test]
fn the_collector_contract_holds() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/collect.test.sh");
    let out = Command::new("bash")
        .arg(&script)
        .output()
        .expect("bash is required to run the collector contract");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "collect.test.sh failed\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stdout.contains(" passed, 0 failed"),
        "the contract test did not report a clean run:\n{stdout}"
    );
}
