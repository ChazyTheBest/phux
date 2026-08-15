//! The two read-only diagnostic MCP tools: `phux_status` and `phux_doctor`.
//!
//! An agent driving phux over MCP could already *act* on the server — spawn,
//! kill, send keys, signal — and could not ask whether that server was
//! healthy. Every health signal phux produced was human-only, so the thing
//! best placed to notice a crash-looping supervised server was the one thing
//! that could not see it. The alternative was shelling out to the `phux`
//! binary, which defeats the point of having an MCP surface at all.
//!
//! ## Two tools, not one
//!
//! `phux status` and `phux doctor` answer different questions and are two
//! tools rather than one `action`-multiplexed diagnostic, for the reason
//! [ADR-0071](../../../ADR/0071-what-phux-1-0-commits-to.md) point 7(b) gives
//! for the `phux_agent_*` split: a multiplexer's frozen schema is the union
//! of every action it will ever carry, and after 1.0 nothing can leave that
//! union. The two shapes happen to coincide today (`socket` and nothing
//! else); freezing them separately is what keeps a later argument on one of
//! them from appearing on the other.
//!
//! ## Reuse, not a second opinion
//!
//! Both tools execute the canonical `phux` CLI with argv (never a shell)
//! through [`crate::cli_adapter`] and return its versioned document
//! verbatim. Neither re-implements a check. A diagnostic that disagrees with
//! `phux doctor` about whether the server is healthy is worse than no
//! diagnostic, so there is exactly one implementation of every check and this
//! module is a transport for it.
//!
//! ## A non-zero exit is an answer here, not a failure
//!
//! Both verbs spend exit `1` on their *interesting* result and still print
//! the whole document on stdout: `phux status --json` answers a stopped
//! server with `{"running": false, ...}`, and `phux doctor --json` exits `1`
//! whenever any check failed — which is precisely the call an agent made the
//! tool call to learn about. Collapsing that into a tool error would throw
//! the document away and report "the diagnostic broke" for the case the
//! diagnostic exists to describe, so `1` is allowed through
//! [`crate::cli_adapter::CliAdapter::run_allowing`] and the document is
//! returned.
//!
//! Exit `1` is overloaded on both verbs — the same code also carries a
//! genuine failure (the server hung up mid-probe; the document would not
//! serialize), and there the CLI's contract puts one JSON error object on
//! stderr and leaves stdout empty. The two are told apart by stdout, not by
//! the code: an empty stdout is a real failure and reports the CLI's stderr
//! contract line rather than a JSON parse error.
//!
//! ## What this family deliberately does NOT contain
//!
//! - **No repair tool.** `phux doctor` is read-only on purpose — a
//!   diagnostic that repairs things is a diagnostic nobody can trust to
//!   describe the system — and the remedies it names (`phux service
//!   reconcile`, `phux upgrade`) restart or rewrite supervisor state on the
//!   human's machine. They stay a `hint` string an agent relays, not a tool
//!   it can call.
//! - **No log-reading tool.** Both documents report the log *paths*; turning
//!   an MCP tool into a file reader is the surface
//!   [ADR-0077](../../../ADR/0077-agent-read-surface.md) point 1 already
//!   declined for captures.

#![allow(
    clippy::similar_names,
    reason = "argv and parsed args are deliberately adjacent in thin CLI wrappers"
)]

use serde_json::{Value, json};

use crate::cli_adapter::{CliAdapter, DEFAULT_CALL_TIMEOUT, push_socket};
use crate::cli_tools::schema;
use crate::tools::{ToolError, strict_object};

/// The exit code both verbs spend on a *result* rather than a failure: a
/// stopped server for `status`, a failed check for `doctor`. See the module
/// docs for why stdout, not this code, is what separates the two meanings.
const EXIT_ANSWERED: i32 = 1;

/// Shared `socket` description. Diagnosing the wrong server is the one way
/// these tools mislead, so the override is spelled out on both.
const SOCKET_DESC: &str = "Override the UDS path of the server to diagnose. \
    Defaults to PHUX_SOCKET or the daemon default.";

/// Every schema in this family, in catalog order.
#[must_use]
pub(crate) fn schemas() -> Vec<Value> {
    vec![status_schema(), doctor_schema()]
}

/// Whether `name` belongs to this family (used by `tools/call` dispatch).
#[must_use]
pub(crate) fn owns(name: &str) -> bool {
    matches!(name, "phux_status" | "phux_doctor")
}

/// Dispatch one diagnostic call.
///
/// # Errors
///
/// Returns [`ToolError`] for an unknown name, a malformed argument, or a
/// canonical-CLI failure that produced no document.
pub(crate) async fn call(name: &str, args: &Value) -> Result<Value, ToolError> {
    call_with_adapter(name, args, &CliAdapter::discover()).await
}

async fn call_with_adapter(
    name: &str,
    args: &Value,
    adapter: &CliAdapter,
) -> Result<Value, ToolError> {
    match name {
        "phux_status" => run_diagnostic("status", args, adapter).await,
        "phux_doctor" => run_diagnostic("doctor", args, adapter).await,
        other => Err(ToolError::new(format!("unknown diagnostic tool: {other}"))),
    }
}

fn status_schema() -> Value {
    schema(
        "phux_status",
        "Report the server behind one socket: whether it is running, its pid, when it bound the \
         socket, the negotiated protocol version, attached-client and session counts, satellite \
         panes, unreachable satellites, and the log paths to read next. READ-ONLY, and it never \
         auto-starts a server — asking whether a server is running may not create one. \
         A STOPPED SERVER IS AN ANSWER, NOT AN ERROR: the CLI exits non-zero and this tool still \
         returns the document, so branch on `running`, never on whether the call succeeded. When \
         `running` is false the document also carries `error` {code, message} and `remedy` from \
         the same closed vocabulary every other phux JSON verb uses. \
         `pid` MAY BE NULL ON A RUNNING SERVER — it comes from the socket's peer credentials \
         rather than from the server itself — so never read a null pid as \"no server\"; \
         `running` is the only field that answers that. `unreachable` is always present and \
         empty means the fleet view is complete. \
         This describes one server at one socket. Use phux_doctor for the wider question of \
         whether the install is healthy: crash-looping, version-skewed, or supervised by a \
         legacy unit.",
        json!({ "socket": { "type": "string", "minLength": 1, "maxLength": 4096, "description": SOCKET_DESC } }),
        &[],
    )
}

fn doctor_schema() -> Value {
    schema(
        "phux_doctor",
        "Run every phux health check and return the whole verdict: config parse, instance/profile \
         isolation, socket path length, server reachability, server-health, plugin manifests, the \
         agent shim, and log paths. This is the same code path as `phux doctor`, so the two can \
         never disagree about whether the install is healthy. READ-ONLY — a diagnostic that \
         repairs things is a diagnostic nobody can trust — and it starts nothing. \
         A FAILING CHECK IS AN ANSWER, NOT AN ERROR: the CLI exits non-zero when any check failed \
         and this tool still returns the document, so branch on `ok` and on each check's \
         `status`, never on whether the call succeeded. \
         Result: `{schema_version, ok, failed, checks: [{name, status, detail, hint}]}` with \
         `status` one of `pass`, `warn`, `fail`. \
         READ EVERY ROW, NOT THE FIRST: check names are NOT unique. `server-health` reports one \
         row per condition that holds — a crash-loop (the server restarted repeatedly inside the \
         start-history window), a legacy supervisor unit that restarts on every exit unthrottled, \
         and version skew between the running server and this binary — and those co-occur more \
         than they don't, because a legacy unit is exactly what turns a dying server into a \
         crash-loop. \
         `warn` means a check could not be verified or does not apply right now, and is \
         deliberately NOT a pass: a stopped server warns rather than failing, because that is a \
         normal state and not a broken install. Every `warn` and `fail` carries a `hint` naming \
         the remedy; relay it, do not run it — the remedies restart supervised services and \
         rewrite units on the human's machine. \
         Only the socket and server checks follow `socket`; everything else describes the machine \
         this adapter is running on.",
        json!({ "socket": { "type": "string", "minLength": 1, "maxLength": 4096, "description": SOCKET_DESC } }),
        &[],
    )
}

/// Execute `phux <verb> --json` and return its document.
async fn run_diagnostic(
    verb: &str,
    args: &Value,
    adapter: &CliAdapter,
) -> Result<Value, ToolError> {
    strict_object(args, &["socket"], &[])?;
    let mut argv = vec![verb.to_owned(), "--json".to_owned()];
    push_socket(&mut argv, args)?;
    let output = adapter
        .run_allowing(argv, DEFAULT_CALL_TIMEOUT, &[EXIT_ANSWERED])
        .await?;

    // Exit 1 carries two meanings on these verbs and stdout is what tells
    // them apart: the answer document, or nothing at all beside one JSON
    // error object on stderr. Parsing an empty stdout would report a
    // malformed-JSON bug for what is really "the server hung up mid-probe",
    // which is the wrong diagnosis to hand a caller who asked for a
    // diagnosis.
    if output.stdout.trim().is_empty() {
        let message = output.stderr.trim();
        return Err(ToolError::new(if message.is_empty() {
            format!("phux {verb} --json exited without printing a document")
        } else {
            message.to_owned()
        }));
    }
    serde_json::from_str(&output.stdout).map_err(|err| {
        ToolError::new(format!(
            "phux {verb} --json returned malformed JSON: {err}; stdout={:?}",
            output.stdout
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::*;

    /// A fake `phux` that logs its argv one entry per line and answers each
    /// diagnostic verb the way the real CLI does on its interesting path:
    /// the whole document on stdout under exit 1.
    fn fake_cli() -> (TempDir, CliAdapter, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("argv");
        let executable = temp.path().join("phux");
        let script = format!(
            r#"#!/bin/sh
: > '{log}'
for arg in "$@"; do
  printf '%s\n' "$arg" >> '{log}'
done
case "$1" in
  status) printf '{{"schema_version":1,"running":false,"error":{{"code":"no_server"}}}}\n'
          exit 1 ;;
  doctor) printf '{{"schema_version":1,"ok":false,"failed":1,"checks":[{{"name":"server-health","status":"fail"}}]}}\n'
          exit 1 ;;
esac
"#,
            log = log.display(),
        );
        fs::write(&executable, script).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        (temp, CliAdapter::new(executable), log)
    }

    /// A fake `phux` reproducing the OTHER exit-1: the shared JSON error
    /// contract on stderr with stdout left empty.
    fn failing_cli() -> (TempDir, CliAdapter) {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("phux");
        fs::write(
            &executable,
            "#!/bin/sh\n\
             printf '{\"schema_version\":1,\"error\":{\"code\":\"server_disconnected\"}}\\n' >&2\n\
             exit 1\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        (temp, CliAdapter::new(executable))
    }

    async fn assert_argv(
        adapter: &CliAdapter,
        log: &Path,
        name: &str,
        args: Value,
        expected: &[&str],
    ) -> Value {
        let result = call_with_adapter(name, &args, adapter)
            .await
            .unwrap_or_else(|err| panic!("{name} failed: {err:?}"));
        let actual = fs::read_to_string(log).unwrap();
        assert_eq!(actual.lines().collect::<Vec<_>>(), expected, "{name}");
        result
    }

    /// Two distinct tools with their own frozen shapes, and no `action`
    /// discriminant anywhere — the union ADR-0071 point 7(b) refuses.
    #[test]
    fn the_diagnostics_are_distinct_tools_with_no_action_multiplexer() {
        let schemas = schemas();
        let names: Vec<&str> = schemas
            .iter()
            .filter_map(|schema| schema["name"].as_str())
            .collect();
        assert_eq!(names, vec!["phux_status", "phux_doctor"]);
        for schema in &schemas {
            assert_eq!(schema["inputSchema"]["type"], "object");
            assert_eq!(schema["inputSchema"]["additionalProperties"], false);
            assert!(
                schema["inputSchema"]["properties"].get("action").is_none(),
                "{} multiplexes on `action`",
                schema["name"],
            );
            assert_eq!(schema["inputSchema"]["required"], json!([]));
            let properties = schema["inputSchema"]["properties"].as_object().unwrap();
            assert_eq!(
                properties.keys().collect::<Vec<_>>(),
                vec!["socket"],
                "{} widened past `socket`",
                schema["name"],
            );
            assert!(owns(schema["name"].as_str().unwrap()));
        }
    }

    /// Both descriptions have to carry the rule that makes their result
    /// readable: a non-zero exit is the answer, so a caller branches on the
    /// document's own field. `phux_doctor` additionally has to say that
    /// `server-health` repeats, or a consumer reads the first row and
    /// reports a misleadingly clean bill of health.
    #[test]
    fn the_descriptions_state_the_answer_not_error_rule() {
        let status = status_schema();
        let status = status["description"].as_str().unwrap();
        assert!(
            status.contains("A STOPPED SERVER IS AN ANSWER, NOT AN ERROR"),
            "{status}",
        );
        assert!(status.contains("branch on `running`"), "{status}");
        assert!(
            status.contains("MAY BE NULL ON A RUNNING SERVER"),
            "a null pid must not read as \"no server\": {status}",
        );
        assert!(status.contains("never auto-starts"), "{status}");

        let doctor = doctor_schema();
        let doctor = doctor["description"].as_str().unwrap();
        assert!(
            doctor.contains("A FAILING CHECK IS AN ANSWER, NOT AN ERROR"),
            "{doctor}",
        );
        assert!(
            doctor.contains("READ EVERY ROW, NOT THE FIRST"),
            "the co-occurring server-health rows must be stated: {doctor}",
        );
        assert!(doctor.contains("READ-ONLY"), "{doctor}");
        assert!(
            doctor.contains("relay it, do not run it"),
            "the hints name remedies that restart services: {doctor}",
        );
    }

    /// Each tool executes the exact canonical argv, and the interesting
    /// non-zero exit returns the document instead of an error.
    #[tokio::test]
    async fn each_tool_executes_canonical_argv_and_keeps_the_document() {
        let (_temp, adapter, log) = fake_cli();

        let status = assert_argv(
            &adapter,
            &log,
            "phux_status",
            json!({ "socket": "/sock" }),
            &["status", "--json", "--socket", "/sock"],
        )
        .await;
        assert_eq!(
            status["running"],
            json!(false),
            "exit 1 must arrive as `running: false`, not as a tool error",
        );
        assert_eq!(status["error"]["code"], json!("no_server"));

        let doctor = assert_argv(
            &adapter,
            &log,
            "phux_doctor",
            json!({}),
            &["doctor", "--json"],
        )
        .await;
        assert_eq!(
            doctor["ok"],
            json!(false),
            "exit 1 must arrive as `ok: false`, not as a tool error",
        );
        assert_eq!(doctor["checks"][0]["name"], json!("server-health"));
    }

    /// The other exit 1: no document, one JSON error object on stderr. The
    /// caller gets that contract line, not a malformed-JSON complaint about
    /// an empty string.
    #[tokio::test]
    async fn a_document_less_failure_reports_the_cli_error_contract() {
        let (_temp, adapter) = failing_cli();
        for name in ["phux_status", "phux_doctor"] {
            let err = call_with_adapter(name, &json!({}), &adapter)
                .await
                .expect_err("an empty stdout is a failure, not a document");
            assert!(
                err.0.contains("server_disconnected"),
                "{name} lost the CLI's error contract: {err:?}",
            );
            assert!(
                !err.0.contains("malformed JSON"),
                "{name} misdiagnosed an empty stdout: {err:?}",
            );
        }
    }

    /// Validation happens before any subprocess: an adapter pointed at a
    /// program that cannot exist proves nothing was executed.
    #[tokio::test]
    async fn malformed_arguments_are_rejected_before_execution() {
        let adapter = CliAdapter::new("must-not-execute");
        for (name, args) in [
            ("phux_status", json!({ "target": "@1" })),
            ("phux_doctor", json!({ "json": true })),
            // No multiplexer to address.
            ("phux_status", json!({ "action": "status" })),
            ("phux_doctor", json!({ "socket": 7 })),
            ("phux_diagnose", json!({})),
        ] {
            assert!(
                call_with_adapter(name, &args, &adapter).await.is_err(),
                "{name} accepted {args}",
            );
        }
    }
}
