//! Socketless, machine-readable installed-binary discovery.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Command;
use serde_json::{Value, json};

const CAPABILITIES_SCHEMA_VERSION: u8 = 1;

fn collect_visible(prefix: &str, command: &Command, paths: &mut Vec<String>) {
    for child in command.get_subcommands() {
        if child.is_hide_set() || child.get_name() == "help" {
            continue;
        }
        let path = format!("{prefix} {}", child.get_name());
        paths.push(path.clone());
        collect_visible(&path, child, paths);
    }
}

fn command_paths(command: &Command) -> Vec<String> {
    let mut paths = Vec::new();
    collect_visible("phux", command, &mut paths);
    paths.sort_unstable();
    paths
}

const fn executable_name() -> &'static str {
    if cfg!(windows) {
        "phux-mcp.exe"
    } else {
        "phux-mcp"
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn find_mcp(current_exe: Option<&Path>, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if let Some(sibling) = current_exe
        .and_then(Path::parent)
        .map(|parent| parent.join(executable_name()))
        && is_executable(&sibling)
    {
        return Some(sibling);
    }

    path.and_then(|value| {
        std::env::split_paths(value)
            .map(|dir| dir.join(executable_name()))
            .find(|candidate| is_executable(candidate))
    })
}

fn schema_contracts() -> Value {
    json!([
        { "invocation": "phux --capabilities --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux ls --json", "schema_version": 3, "kind": "document" },
        { "invocation": "phux snapshot --json", "schema_version": 3, "kind": "document" },
        { "invocation": "phux snapshot --rendered --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux status --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux new --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux spawn --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux launch --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux resize --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux ask --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux agent list|show|explain --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux agent wait|prompt|send-keys|answer|start --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux config agents --json", "schema_version": 2, "kind": "document" },
        { "invocation": "phux config check|plugins --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux plugin --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux workspace inspect --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux workspace save|restore --json", "schema_version": 2, "kind": "document" },
        { "invocation": "phux worktree --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux host --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux tag --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux pair --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux rec|play --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux logs|doctor|update --json", "schema_version": 1, "kind": "document" },
        { "invocation": "phux run --json", "schema_version": null, "kind": "document", "note": "unversioned result" },
        { "invocation": "phux watch --json", "schema_version": null, "kind": "ndjson", "note": "event vocabulary is the compatibility contract" },
        { "invocation": "phux --json failures", "schema_version": 1, "kind": "error" }
    ])
}

fn document(command: &Command, mcp: Option<&Path>) -> Value {
    let protocol = phux_protocol::PROTOCOL_VERSION;
    let available = mcp.is_some();
    json!({
        "schema_version": CAPABILITIES_SCHEMA_VERSION,
        "binary": {
            "name": "phux",
            "version": env!("CARGO_PKG_VERSION"),
            "wire_protocol": format!("{}.{}.{}", protocol.major, protocol.minor, protocol.patch),
        },
        "commands": command_paths(command),
        "skill": {
            "command": "phux --skill[=SCOPE]",
            "scopes": ["quick", "agent", "terminal", "full"],
            "default_scope": "full"
        },
        "json_contracts": schema_contracts(),
        "mcp": {
            "available": available,
            "command": mcp,
            "skill_args": ["--skill"],
            "schema_args": ["--schema"],
            "schema_note": "phux-mcp --schema is the authoritative tools/list input-schema catalog"
        }
    })
}

pub(crate) fn run(command: &Command) -> ExitCode {
    let current_exe = std::env::current_exe().ok();
    let mcp = find_mcp(current_exe.as_deref(), std::env::var_os("PATH").as_deref());
    match serde_json::to_vec_pretty(&document(command, mcp.as_deref())) {
        Ok(mut rendered) => {
            rendered.push(b'\n');
            crate::output::bytes(&rendered);
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("phux: could not render capabilities: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test fixture assertions")]
mod tests {
    use std::fs;

    use clap::CommandFactory;

    use super::*;

    #[test]
    fn document_is_versioned_sorted_and_hides_plumbing() {
        let doc = document(&crate::Cli::command(), None);
        assert_eq!(doc["schema_version"], 1);
        assert_eq!(doc["binary"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(doc["binary"]["wire_protocol"], "0.7.0");
        assert_eq!(doc["mcp"]["available"], false);
        let commands = doc["commands"].as_array().unwrap();
        assert!(
            commands
                .windows(2)
                .all(|pair| { pair[0].as_str().unwrap() < pair[1].as_str().unwrap() })
        );
        assert!(commands.iter().any(|path| path == "phux agent prompt"));
        assert!(!commands.iter().any(|path| path == "phux stdio-bridge"));
        assert!(
            !commands
                .iter()
                .any(|path| path == "phux gen-reference-docs")
        );
        assert!(doc["json_contracts"].as_array().unwrap().iter().any(|row| {
            row["invocation"] == "phux watch --json" && row["schema_version"].is_null()
        }));
    }

    #[test]
    fn sibling_mcp_wins_then_path_is_the_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        let path_bin = temp.path().join("path");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&path_bin).unwrap();
        let current = bin.join("phux");
        let sibling = bin.join(executable_name());
        let fallback = path_bin.join(executable_name());
        fs::write(&sibling, "").unwrap();
        fs::write(&fallback, "").unwrap();
        #[cfg(unix)]
        for path in [&sibling, &fallback] {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        assert_eq!(
            find_mcp(Some(&current), Some(path_bin.as_os_str())),
            Some(sibling.clone())
        );
        fs::remove_file(sibling).unwrap();
        assert_eq!(
            find_mcp(Some(&current), Some(path_bin.as_os_str())),
            Some(fallback)
        );
    }
}
