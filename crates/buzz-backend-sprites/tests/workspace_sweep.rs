//! `buzz-workspace sweep` contract (spec: the `sweep` section of
//! `src/assets/workspace.sh`).
//!
//! The scenario lives in `tests/workspace-sweep.test.sh`: a real git origin
//! with pull-request head refs, the production script under a private
//! `HOME`, and a stub `curl` standing in for api.github.com. This wrapper
//! exists so the contract runs under `cargo test` / nextest with the rest of
//! the crate, on every host that has bash and git — which is every CI lane
//! that builds this provider.

use std::path::Path;
use std::process::Command;

#[test]
fn the_sweep_contract_holds() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/workspace-sweep.test.sh");
    let out = Command::new("bash")
        .arg(&script)
        .output()
        .expect("bash is required to run the sweep contract");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "workspace-sweep.test.sh failed\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stdout.contains(" passed, 0 failed"),
        "the contract test did not report a clean run:\n{stdout}"
    );
}
