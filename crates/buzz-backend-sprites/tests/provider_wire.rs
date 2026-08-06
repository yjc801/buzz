//! Golden wire fixtures (spec §Provider Protocol).
//!
//! These drive the **built binary** over a real pipe rather than calling an
//! in-process function: the contract the desktop depends on is
//! `stdin → one JSON object on stdout → exit code`, and an in-process test
//! would assert the shape of a value while skipping the three things that
//! actually break — the process writing nothing, writing two objects, or
//! signalling the outcome through the exit code.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/provider-wire")
}

/// A HOME with no `~/.sprites` in it, so the ambient credential chain fails
/// deterministically instead of resolving whatever login the developer has.
fn poisoned_home() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sprites-wire-home-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("could not create the poisoned HOME");
    dir
}

/// Feed one request to the binary; return `(stdout, exit code)`.
fn run(request: &str) -> (String, i32) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_buzz-backend-sprites"))
        // No ambient token and an empty HOME, so a fixture that accidentally
        // reaches the Sprites API fails loudly at credential resolution
        // instead of depending on whatever login the developer has.
        .env_remove("SPRITE_TOKEN")
        .env_remove("SPRITES_TOKEN")
        .env("HOME", poisoned_home())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("could not run the provider binary");
    child
        .stdin
        .take()
        .expect("no stdin")
        .write_all(request.as_bytes())
        .expect("could not write the request");
    let out = child.wait_with_output().expect("provider did not exit");
    (
        String::from_utf8(out.stdout).expect("stdout was not UTF-8"),
        out.status.code().unwrap_or(-1),
    )
}

fn read(name: &str) -> String {
    std::fs::read_to_string(fixtures().join(name))
        .unwrap_or_else(|e| panic!("could not read fixture {name}: {e}"))
}

/// Every response fixture, byte-compared after key-sorted re-serialization so
/// a field rename fails here rather than in a desktop reading `undefined`.
#[test]
fn responses_match_their_fixtures() {
    let cases = [
        "deploy-bad-nsec",
        "deploy-inactivity-zero",
        "deploy-no-owner",
        "deploy-relay-mesh",
        "deploy-relay-mesh-padded",
        "deploy-unprovisioned-command",
    ];
    // The list must cover every response fixture on disk. A literal array is
    // never empty, so `!is_empty()` would assert nothing; what can actually go
    // wrong is a fixture added to the directory and never added here, which
    // reads as a passing suite that exercises one case fewer than it appears to.
    let mut on_disk: Vec<String> = std::fs::read_dir(fixtures())
        .expect("could not read the fixture directory")
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter_map(|name| Some(name.strip_suffix(".response.json")?.to_string()))
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = cases.iter().map(|c| c.to_string()).collect();
    listed.sort();
    assert_eq!(on_disk, listed, "response fixtures and cases disagree");

    for case in cases {
        let (stdout, code) = run(&read(&format!("{case}.request.json")));
        assert_eq!(code, 0, "{case}: a produced response must exit 0");

        // Exactly one object, terminated by exactly one newline. Two responses
        // would leave the desktop's reader holding a second one forever.
        assert_eq!(
            stdout.matches('\n').count(),
            1,
            "{case}: expected exactly one line, got {stdout:?}"
        );

        let actual: serde_json::Value =
            serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{case}: {e}: {stdout:?}"));
        let expected: serde_json::Value =
            serde_json::from_str(&read(&format!("{case}.response.json"))).unwrap();
        assert_eq!(actual, expected, "{case}: response drifted from its fixture");
    }
}

/// `info` is checked on the fields the desktop reads. The desktop's
/// `validate_provider_info` additionally rejects unknown top-level fields, so
/// the exact key set is pinned here too.
#[test]
fn info_response_carries_the_contract_fields() {
    let (stdout, code) = run(&read("info.request.json"));
    assert_eq!(code, 0);
    let info: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(info["ok"], true);
    assert_eq!(info["protocol_version"], 1);
    assert_eq!(info["name"], "sprites");

    let mut keys: Vec<&str> = info.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["config_schema", "description", "name", "ok", "protocol_version", "version"],
        "the desktop rejects unknown top-level info fields"
    );

    let schema = &info["config_schema"];
    assert_eq!(schema["required"], serde_json::json!([]));
    assert_eq!(
        schema["properties"]["inactivity_seconds"]["default"],
        serde_json::json!(7200)
    );
    assert!(schema["properties"]["org"].is_object());
}

/// The desktop's richest payload must parse. No response fixture: this one
/// reaches credential resolution, whose outcome depends on the environment.
/// What it guards is that every field the desktop sends is *accepted* — a
/// payload the provider rejects at parse time is a deploy that never starts.
#[test]
fn the_full_desktop_payload_is_accepted() {
    let (stdout, code) = run(&read("deploy-full-launch.request.json"));
    assert_eq!(code, 0);
    let response: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let error = response["error"].as_str().unwrap_or_default();
    // It fails — the harness deliberately supplies no credential — but it
    // must fail at credential resolution, having accepted every field above.
    assert!(
        error.contains("Sprites API credential"),
        "the full payload was rejected before credential resolution: {error}"
    );
}

/// The respond-to matrix, driven through the built binary.
///
/// The env builder runs before credential resolution in `deploy_agent`, so
/// under the harness's poisoned credentials the error string *is* the
/// ordering assertion: "Sprites API credential" means the gate passed and we
/// reached resolution, anything else means we refused first. A test asserting
/// only `ok: false` would pass on the credential error and prove nothing.
///
/// The cases are applied to the real full-launch request so each one differs
/// from a known-good deploy in exactly the field under test.
#[test]
fn the_respond_to_gate_matches_the_harness_acceptance_surface() {
    let key_a = "a".repeat(64);
    let padded_upper = format!("  {}  ", "A".repeat(64));
    // (name, respond_to, allowlist, must reach credential resolution)
    let cases: Vec<(&str, &str, Option<Vec<String>>, bool)> = vec![
        ("allowlist + []", "allowlist", Some(vec![]), false),
        ("allowlist + absent", "allowlist", None, false),
        (
            "allowlist + junk",
            "allowlist",
            Some(vec!["beefcafe".into()]),
            false,
        ),
        ("unparseable mode", "npub1abc", None, false),
        ("padded mode", " allowlist ", None, false),
        (
            "allowlist + two valid",
            "allowlist",
            Some(vec![key_a.clone(), "b".repeat(64)]),
            true,
        ),
        (
            "owner-only + junk list",
            "owner-only",
            Some(vec!["beefcafe".into()]),
            true,
        ),
        (
            "allowlist + padded upper",
            "allowlist",
            Some(vec![padded_upper]),
            true,
        ),
        ("nobody", "nobody", None, true),
        ("anyone", "anyone", None, true),
    ];

    let base: serde_json::Value =
        serde_json::from_str(&read("deploy-full-launch.request.json")).unwrap();

    for (name, mode, allowlist, reaches_credentials) in cases {
        let mut request = base.clone();
        let agent = &mut request["agent"];
        agent["respond_to"] = serde_json::json!(mode);
        agent["respond_to_allowlist"] = match &allowlist {
            Some(list) => serde_json::json!(list),
            None => serde_json::Value::Null,
        };

        let (stdout, code) = run(&request.to_string());
        assert_eq!(code, 0, "{name}: provider did not exit cleanly");
        let response: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        let error = response["error"].as_str().unwrap_or_default();
        let reached = error.contains("Sprites API credential");

        assert_eq!(
            reached, reaches_credentials,
            "{name}: expected reaches_credentials={reaches_credentials}, got error: {error}"
        );
        if !reaches_credentials {
            assert!(
                error.contains("deploy refused"),
                "{name}: refused, but not by the gate: {error}"
            );
        }
    }
}
