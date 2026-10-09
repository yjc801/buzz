//! Keep-awake loop contract (`src/assets/keepawake.sh`).
//!
//! The scenario lives in `tests/keepawake.test.sh`: the production script
//! under a private `HOME`, a real process named `buzz-acp` standing in for
//! the harness, and a stub `curl` standing in for the sprite's Tasks API.
//! Linux only — the script reads `/proc` and uses `setsid`, `flock`, and
//! `/dev/shm`, which is also the only place it ever runs.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;

#[test]
fn the_keep_awake_contract_holds() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/keepawake.test.sh");
    let out = Command::new("bash")
        .arg(&script)
        .output()
        .expect("bash is required to run the keep-awake contract");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "keepawake.test.sh failed\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stdout.contains(" passed, 0 failed"),
        "the contract test did not report a clean run:\n{stdout}"
    );
}
