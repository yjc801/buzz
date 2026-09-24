use super::*;
use crate::managed_agents::discovery::command_search;
#[test]
fn command_discovery_dirs_include_all_spawn_search_sources_without_duplicates() {
    let workspace = PathBuf::from("workspace");
    let path = PathBuf::from("path");
    let managed = PathBuf::from("managed");
    let login = PathBuf::from("login");
    let nvm = PathBuf::from("nvm");

    assert_eq!(
        command_search::merge_command_discovery_dirs([
            vec![workspace.clone(), path.clone()],
            vec![path, managed.clone()],
            vec![login.clone()],
            vec![managed, nvm.clone()],
        ]),
        vec![
            workspace,
            PathBuf::from("path"),
            PathBuf::from("managed"),
            login,
            nvm
        ]
    );
}

#[test]
fn acp_command_filename_supports_windows_shims_and_rejects_extensionless_windows_files() {
    assert_eq!(
        acp_command_from_filename("buzz-janet-acp.EXE", true),
        Some("buzz-janet-acp")
    );
    assert_eq!(
        acp_command_from_filename("buzz-janet-acp.cmd", true),
        Some("buzz-janet-acp")
    );
    assert_eq!(
        acp_command_from_filename("buzz-janet-acp.BAT", true),
        Some("buzz-janet-acp")
    );
    assert_eq!(acp_command_from_filename("buzz-janet-acp", true), None);
    assert_eq!(
        acp_command_from_filename("buzz-janet-acp", false),
        Some("buzz-janet-acp")
    );
    assert_eq!(acp_command_from_filename("buzz-acp.exe", true), None);
}

#[test]
fn discovers_only_namespaced_acp_commands() {
    let dir = tempfile::tempdir().expect("temp dir");
    for command in [
        "buzz-janet-acp",
        "buzz-acp",
        "buzz--acp",
        "janet-acp",
        "buzz-janet-helper",
    ] {
        let filename = if cfg!(windows) {
            format!("{command}.cmd")
        } else {
            command.to_string()
        };
        let path = dir.path().join(filename);
        std::fs::write(&path, "#!/bin/sh\n").expect("write fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).expect("chmod fixture");
        }
    }

    let candidates = discover_acp_command_candidates_in([dir.path().to_path_buf()], |command| {
        Some(dir.path().join(command))
    });
    assert_eq!(
        candidates
            .into_iter()
            .map(|(command, _)| command)
            .collect::<Vec<_>>(),
        vec!["buzz-janet-acp"]
    );
}

#[test]
fn acp_command_discovery_deduplicates_path_entries() {
    let first = tempfile::tempdir().expect("first temp dir");
    let second = tempfile::tempdir().expect("second temp dir");
    let command = "buzz-janet-acp";
    let filename = if cfg!(windows) {
        format!("{command}.cmd")
    } else {
        command.to_string()
    };
    for dir in [first.path(), second.path()] {
        let path = dir.join(&filename);
        std::fs::write(&path, "#!/bin/sh\n").expect("write fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).expect("chmod fixture");
        }
    }

    let resolved = second.path().join(&filename);
    let candidates = discover_acp_command_candidates_in(
        [first.path().to_path_buf(), second.path().to_path_buf()],
        |_| Some(resolved.clone()),
    );
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].0, "buzz-janet-acp");
    assert_eq!(candidates[0].1, resolved);
}

#[test]
fn portable_acp_aliases_exclude_paths_arguments_and_platform_metacharacters() {
    for command in ["buzz-acp", "buzz-janet-acp", "buzz-Team_2-acp"] {
        assert!(is_portable_acp_command(command), "{command}");
    }
    for command in [
        "",
        "buzz--acp",
        "/tmp/buzz-janet-acp",
        r"C:\buzz-janet-acp.cmd",
        "buzz-../evil-acp",
        "buzz-a b-acp",
        "buzz-a&b-acp",
        "buzz-a%PATH%-acp",
        "buzz-a\nb-acp",
        "buzz-a\u{202e}b-acp",
        "other-command",
        "buzz-janet-acp.exe",
    ] {
        assert!(!is_portable_acp_command(command), "{command:?}");
    }
}
