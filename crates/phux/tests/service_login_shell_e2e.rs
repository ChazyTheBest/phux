//! Binary-level end-to-end regression for phux-87rr: "service-managed
//! panes inherit a login-shell PATH; ordinary panes are unchanged."
//!
//! The bug: a server installed with `phux service install` runs under
//! launchd/systemd's minimal environment. Its spawned panes ran the
//! configured shell as a plain (non-login) shell, so platform profile
//! initialization (`~/.profile`, `~/.zprofile`, …) never ran — Homebrew
//! and Nix PATH entries were invisible even though environment markers
//! like `NIX_PROFILES` could still be inherited and fool a naive guard
//! into thinking initialization already happened.
//!
//! The fix threads a `login: bool` through
//! `phux_server::terminal_actor::{default_shell_command, shell_command}`
//! (see `crates/phux-server/src/terminal_actor/spawn.rs`), driven by
//! whether `phux server` finds `PHUX_SERVICE_MANAGED` in its own
//! environment — the marker `phux service install` stamps into the
//! generated launchd plist / systemd unit
//! (`crates/phux/src/commands/service.rs::SERVICE_MANAGED_ENV`), never a
//! heuristic sniffed from environment shape.
//!
//! This file proves the whole pipeline against the REAL compiled `phux`
//! binary — real env-var detection in `run_server`, real
//! `shell_command`/`-l` argv construction, a real PTY, a real `/bin/sh`
//! sourcing a real `~/.profile` fixture, and a real command-resolution
//! outcome written to a file by the spawned shell itself. The only thing
//! NOT real is launchd/systemd itself: the "minimal service environment"
//! is simulated by starting the child process with `env_clear()` plus an
//! explicit launchd-shaped minimal `PATH`, and the marker is set directly
//! rather than by an installed unit — a real launchd/systemd test is not
//! runnable in this CI. Everything downstream of "the child process's
//! environment looks like this" is exercised for real.
//!
//! Two scenarios:
//!   * [`service_managed_pane_resolves_a_profile_provided_command`] —
//!     acceptance criterion 5. The marker is present; the profile-added
//!     command must resolve.
//!   * [`ordinary_pane_does_not_source_the_profile_twice`] — acceptance
//!     criterion 6. The marker is absent (an ordinary hand-started
//!     server); the same profile fixture must NOT be sourced, so the
//!     same command must NOT resolve. This is the regression guard: it
//!     would fail immediately if login-shell treatment ever stopped
//!     being conditional.

#![allow(clippy::expect_used, reason = "tests")]
#![allow(clippy::unwrap_used, reason = "tests")]
#![allow(clippy::panic, reason = "tests")]

mod common;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Path to the freshly-built `phux` binary, injected by cargo.
const PHUX: &str = env!("CARGO_BIN_EXE_phux");

/// The marker `phux service install` writes into the generated unit's
/// environment (`crates/phux/src/commands/service.rs::SERVICE_MANAGED_ENV`).
/// Duplicated here as a literal rather than imported: this file spawns the
/// compiled binary as a black-box subprocess (the established pattern in
/// this directory, e.g. `idle_exit_e2e.rs`), so it has no dependency on
/// `phux`'s internal `pub(crate)` items — matching how every other
/// service-unit env var name (`PHUX_WS_ADDR`, `PHUX_SOCKET`, …) is already
/// duplicated as a literal across this codebase rather than shared via a
/// cross-module constant.
const SERVICE_MANAGED_ENV: &str = "PHUX_SERVICE_MANAGED";

/// launchd's own default `PATH` for an agent with no `EnvironmentVariables`
/// override (`man launchd.plist`), used here to build a realistically
/// minimal — not artificially empty — service environment.
const LAUNCHD_DEFAULT_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// Wait for the server to bind (cold-start bound, matching `idle_exit_e2e.rs`).
const SOCKET_DEADLINE: Duration = Duration::from_secs(30);

/// Wait for the seed pane to run its command and write the result file.
///
/// The command itself resolves in milliseconds; the margin is generous
/// (matching `idle_exit_e2e.rs`'s `EXIT_HANG_CEILING` philosophy) because
/// under `just e2e`'s full parallel run this competes with every other
/// PTY-spawning, real-subprocess integration test on the box for CPU and
/// fork/exec bandwidth. This is a HANG detector, not a timing gate.
const RESULT_DEADLINE: Duration = Duration::from_secs(20);

/// Poll cadence for every wait loop in this file.
const POLL: Duration = Duration::from_millis(50);

/// Monotonic counter so concurrent tests never collide on a socket path.
static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A running `phux server` child plus its private socket.
///
/// Sockets live at the root of `/tmp` (matching `idle_exit_e2e.rs` /
/// `rec_e2e.rs`): macOS caps `sun_path` at 104 bytes and this crate runs
/// from deep worktree paths that can already exceed it before adding a
/// filename.
struct ServerGuard {
    _process: common::ServerProcess,
    socket: PathBuf,
}

impl ServerGuard {
    /// Start `phux server` with a from-scratch environment: `env_clear()`
    /// then exactly `HOME` (the caller's fixture directory) and `PATH`
    /// (launchd's own default) — no inherited `$SHELL`, no inherited
    /// Homebrew/Nix PATH entries from whatever shell is running this test
    /// suite (relevant since `just ci` and this repo's dev shell both run
    /// under `nix develop`, which is exactly the transient-PATH shape
    /// phux-87rr's acceptance criterion 4 is about — see
    /// `crates/phux/src/commands/service.rs`'s
    /// `install_never_captures_the_process_path` test for that half).
    ///
    /// `service_managed` stamps [`SERVICE_MANAGED_ENV`] when `true`,
    /// mirroring exactly what `phux service install` writes into the
    /// generated unit; absent, this is indistinguishable from a server a
    /// human started directly from their own terminal.
    fn start(home: &Path, seed_command: &str, service_managed: bool) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let socket = PathBuf::from(format!(
            "/tmp/phux-login-e2e-{}-{n}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket);

        let mut cmd = Command::new(PHUX);
        cmd.env_clear();
        cmd.env("HOME", home);
        cmd.env("PATH", LAUNCHD_DEFAULT_PATH);
        if service_managed {
            cmd.env(SERVICE_MANAGED_ENV, "1");
        }
        cmd.args(["server", "--session", "svc", "--socket"])
            .arg(&socket)
            .arg("--seed-command")
            .arg(seed_command)
            .args(["--exit-after-idle", common::SERVER_IDLE_LIMIT_SECS]);
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn phux server");

        let guard = Self {
            _process: common::ServerProcess::from_child(child, socket.clone()),
            socket,
        };
        let deadline = Instant::now() + SOCKET_DEADLINE;
        while Instant::now() < deadline {
            if guard.socket.exists() {
                return guard;
            }
            std::thread::sleep(POLL);
        }
        panic!(
            "phux server did not bind {} within {SOCKET_DEADLINE:?}",
            guard.socket.display()
        );
    }
}

/// Build a fixture `$HOME` whose `~/.profile` prepends `$HOME/bin` to
/// `PATH` and drops an executable marker script there — the minimal
/// stand-in for what a Homebrew or Nix installer's profile snippet does.
/// Read only by a shell invoked in *login* mode; a plain `sh -c` never
/// sources it, which is exactly the behavior under test.
fn write_profile_fixture() -> TempDir {
    let home = TempDir::new().expect("tempdir");
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir bin");

    let marker = bin.join("phux-profile-marker");
    let mut f = std::fs::File::create(&marker).expect("create marker");
    writeln!(f, "#!/bin/sh\necho PHUX_PROFILE_MARKER_FOUND").expect("write marker");
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o755))
            .expect("chmod marker");
    }

    let profile = home.path().join(".profile");
    std::fs::write(&profile, "PATH=\"$HOME/bin:$PATH\"\nexport PATH\n").expect("write .profile");

    home
}

/// Poll `path` until it exists and is non-empty, or panic at `deadline`.
/// The seed shell writes its result in one shot (`>file 2>&1`), so a
/// non-empty read is never a torn write.
fn read_result_file(path: &Path, deadline: Duration) -> String {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if let Ok(contents) = std::fs::read_to_string(path)
            && !contents.is_empty()
        {
            return contents;
        }
        std::thread::sleep(POLL);
    }
    panic!(
        "seed pane never wrote a result to {} within {deadline:?}",
        path.display()
    );
}

/// phux-87rr acceptance criterion 5: an end-to-end regression that starts
/// the service under a minimal environment and proves a profile-provided
/// command resolves.
#[test]
fn service_managed_pane_resolves_a_profile_provided_command() {
    let home = write_profile_fixture();
    let result_path = home.path().join("result");
    let seed = format!("phux-profile-marker >'{}' 2>&1", result_path.display());

    let _server = ServerGuard::start(home.path(), &seed, true);

    let contents = read_result_file(&result_path, RESULT_DEADLINE);
    assert!(
        contents.contains("PHUX_PROFILE_MARKER_FOUND"),
        "a service-managed server's seed pane must resolve a command \
         `~/.profile` adds to PATH via login-shell treatment; got: {contents:?}\n\
         \n\
         Diagnostics — this assertion depends on the host's /bin/sh, so a \
         failure here is usually the image, not the code:\n\
         \x20 fixture HOME      : {home_dir}\n\
         \x20 ~/.profile exists : {profile_exists}\n\
         \x20 marker executable : {marker_exec}\n\
         \x20 /bin/sh resolves  : {sh_target}\n\
         \x20 sh -l -c          : {probe_login}\n\
         \x20 sh -c             : {probe_plain}\n\
         \x20 /etc/profile.d    : {profile_d}\n\
         \n\
         If `sh -l -c PATH` already lacks the fixture's bin directory, the \
         host's /bin/sh does not source ~/.profile in login mode and the \
         runner image is at fault. If it contains it, the pane was not given \
         the login flag and the defect is in phux.",
        home_dir = home.path().display(),
        profile_exists = home.path().join(".profile").exists(),
        marker_exec = home.path().join("bin/phux-profile-marker").exists(),
        sh_target = std::fs::read_link("/bin/sh").map_or_else(
            |_| "(not a symlink)".to_owned(),
            |p| p.display().to_string()
        ),
        probe_login = probe_shell_path(home.path(), true),
        probe_plain = probe_shell_path(home.path(), false),
        profile_d = probe_system_profile(),
    );
}

/// Run `/bin/sh [-l] -c 'echo $PATH'` against the fixture `$HOME` and return
/// what the shell resolved `PATH` to.
///
/// This exists because the assertion above cannot otherwise distinguish "phux
/// failed to apply login treatment" from "this host's /bin/sh does not source
/// ~/.profile in login mode" — two failures with identical symptoms and
/// opposite owners. Diagnosing that split once cost a CI bisect.
fn probe_shell_path(home: &Path, login: bool) -> String {
    let mut cmd = Command::new("/bin/sh");
    cmd.env_clear();
    cmd.env("HOME", home);
    cmd.env("PATH", LAUNCHD_DEFAULT_PATH);
    if login {
        cmd.arg("-l");
    }
    cmd.arg("-c")
        .arg("printf 'HOME=%s PATH=%s' \"$HOME\" \"$PATH\"");
    cmd.output().map_or_else(
        |e| format!("(probe failed: {e})"),
        |out| String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// What the host's system profile does to a login shell, listed so a failure
/// names the file responsible instead of leaving it to be guessed.
fn probe_system_profile() -> String {
    let mut entries: Vec<String> = std::fs::read_dir("/etc/profile.d").map_or_else(
        |_| Vec::new(),
        |dir| {
            dir.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        },
    );
    entries.sort();
    if entries.is_empty() {
        "(none)".to_owned()
    } else {
        entries.join(" ")
    }
}

/// phux-87rr acceptance criterion 6: existing direct/server-in-terminal
/// pane startup remains regression-free. Same profile fixture, same
/// command, only the service marker is missing — the command must NOT
/// resolve, proving login-shell treatment stayed conditional rather than
/// becoming the new default for every server.
#[test]
fn ordinary_pane_does_not_source_the_profile_twice() {
    let home = write_profile_fixture();
    let result_path = home.path().join("result");
    let seed = format!("phux-profile-marker >'{}' 2>&1", result_path.display());

    let _server = ServerGuard::start(home.path(), &seed, false);

    let contents = read_result_file(&result_path, RESULT_DEADLINE);
    assert!(
        !contents.contains("PHUX_PROFILE_MARKER_FOUND"),
        "an ordinary (non-service) server's seed pane must NOT get \
         login-shell treatment — `~/.profile` must stay unsourced; \
         got: {contents:?}"
    );
}
