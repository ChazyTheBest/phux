//! Startup must not wait on overlay detection (phux-90j5, ADR-0081).
//!
//! The auto-bound remote listener binds a *detected* overlay address, and
//! detecting it means shelling out to the `tailscale` CLI. That subprocess
//! used to run on the server's startup path — after the pre-seeded pane had
//! started its shell, before the accept loop had run even once — so on any
//! developer machine with Tailscale installed the server was unreachable for
//! roughly 286ms while a live pane's clock was already advancing. CI has no
//! `tailscale` binary, degrades to a fast UDP route probe, and never saw it.
//! That asymmetry is the phux-5wxp flake family in miniature: a test whose
//! outcome turns on whether a VPN client happens to be installed.
//!
//! This test pins the property that replaced it: **the server serves clients
//! without waiting for overlay detection to finish.**
//!
//! ## Why there is no timing assertion here
//!
//! A latency claim tempts a wall-clock bound, and a wall-clock bound is
//! exactly what a loaded machine takes away (see the `.config/nextest.toml`
//! measurements). So the proof is an ordering one instead, and it needs no
//! deadline to be meaningful:
//!
//! * the injected detector *blocks* — it returns only when this test releases
//!   it, which it does only after the server has completed a HELLO round trip;
//! * a HELLO reply comes from a per-client task the accept loop spawned, so
//!   receiving it proves the accept loop was live;
//! * therefore the server was serving while a detection call was in flight
//!   and provably had not returned.
//!
//! Against the old code this fails as a hang: detection ran inline on the
//! current-thread runtime, so a detector that never returns is a server that
//! never accepts, and the helper's `WIRE_RECV_TIMEOUT` turns that into a
//! failure rather than a wait-for-Godot. The detector's own ceiling
//! ([`DETECT_CEILING`]) exists so a failing run cannot wedge the process on
//! runtime shutdown, which joins blocking tasks.
//!
//! The gate is asserted separately, because a test that silently stopped
//! exercising detection would keep passing forever.
//!
//! ## Why this test pins `PHUX_PROFILE=default`
//!
//! It has to, and the reason is worth recording because it also bounds the
//! blast radius of the defect. Only the default profile auto-binds (ADR-0081:
//! a TCP port is global to the host, so a dev server must not contend for the
//! installed one's 8787), and `phux_config::instance::is_dev_build` calls any
//! `debug_assertions` binary a dev build — which every `cargo test` binary is.
//!
//! So since phux-c6g6 closed the eager-argument hole, the test suite has not
//! run overlay detection at all: the gate is shut for test binaries, and the
//! ~286ms stall reproduces only in the *installed*, default-profile server.
//! A test that let the ambient profile decide would therefore assert nothing
//! forever, silently. Pinning the profile is what puts this test in the
//! configuration where the stall actually lived.
//!
//! This binary holds exactly one test, and nextest runs each test in its own
//! process, so the environment mutation cannot race a sibling. Nothing else
//! in the process reads the profile before it is set: the socket path is an
//! explicit tempdir, and the injected detector reports no address, so no
//! profile-scoped path is ever resolved.

#![allow(clippy::expect_used, reason = "tests")]
#![allow(clippy::panic, reason = "tests")]
#![allow(unused_unsafe, reason = "env::set_var is unsafe only on edition 2024")]

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use phux_server::runtime::{ServerConfig, ServerRuntime};
use phux_server_testkit::{
    SERVER_JOIN_DEADLINE, SOCKET_CONNECT_DEADLINE, run_local, wait_for_socket,
};
use tempfile::TempDir;

/// Set by [`blocking_detect`] the moment it is called, so the test can prove
/// the auto-listen gate was actually open — otherwise a closed gate would
/// make every assertion below vacuously true.
static DETECT_STARTED: AtomicBool = AtomicBool::new(false);

/// The barrier [`blocking_detect`] waits on. `false` until the test releases
/// it, which happens only after the server has answered on the wire.
static DETECT_RELEASED: Mutex<bool> = Mutex::new(false);

/// Companion condvar for [`DETECT_RELEASED`].
static DETECT_RELEASE_CV: Condvar = Condvar::new();

/// Upper bound on how long the injected detector will block if the test never
/// releases it. Nothing asserts on this value: it is a safety valve. Tokio
/// waits for blocking tasks when the runtime is dropped, so a detector that
/// blocks forever would turn a *failing* assertion into a wedged test
/// process. Comfortably past `WIRE_RECV_TIMEOUT` so it never fires first on a
/// loaded machine, and no further: on a regression this ceiling *is* how long
/// the failure takes to surface, because a detector called inline blocks the
/// current-thread runtime that would otherwise raise the recv timeout.
const DETECT_CEILING: Duration = Duration::from_secs(30);

/// How long to wait for the detector to be *entered*. It runs on a blocking
/// thread, so its first instruction is not ordered against the client's HELLO
/// round trip; the submission of that thread is (it happens in the same poll
/// that first drives the accept loops). Only the non-vacuity check needs
/// this, so the deadline is generous.
const DETECT_START_DEADLINE: Duration = Duration::from_secs(10);

/// Stand-in for `phux_config::overlay::detect` that blocks until released.
///
/// Returns no addresses, so nothing is ever bound: this test is about the
/// startup path, and binding the real overlay ports 8787/8788 from a test
/// would contend with whatever else is running on the machine.
#[allow(
    clippy::significant_drop_tightening,
    reason = "a condvar wait holds its guard across the loop by construction"
)]
fn blocking_detect() -> Vec<IpAddr> {
    DETECT_STARTED.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + DETECT_CEILING;
    let mut released = DETECT_RELEASED.lock().expect("detect barrier");
    while !*released {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        let (guard, timed_out) = DETECT_RELEASE_CV
            .wait_timeout(released, remaining)
            .expect("detect barrier wait");
        released = guard;
        if timed_out.timed_out() {
            break;
        }
    }
    Vec::new()
}

/// Let [`blocking_detect`] return. The guard is dropped before the notify so
/// the woken thread does not immediately block on the lock it was woken for.
fn release_detect() {
    {
        let mut released = DETECT_RELEASED.lock().expect("detect barrier");
        *released = true;
    }
    DETECT_RELEASE_CV.notify_all();
}

/// Releases the detector on the way out, including while unwinding from a
/// failed assertion — otherwise a failure would leave a blocking task parked
/// for [`DETECT_CEILING`] and the test process would appear to hang.
struct ReleaseOnDrop;

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        release_detect();
    }
}

/// Whether the detector has been released yet.
fn detect_released() -> bool {
    *DETECT_RELEASED.lock().expect("detect barrier")
}

#[test]
fn server_serves_clients_while_overlay_detection_is_still_running() {
    // Present this process as the installed server, which is the only
    // configuration that auto-binds and therefore the only one that ever
    // detected an overlay. See the module docs. Set before the runtime
    // exists, so no other thread is reading the environment yet.
    unsafe { std::env::set_var("PHUX_PROFILE", phux_config::instance::DEFAULT_PROFILE) };

    run_local(async {
        let _release_on_unwind = ReleaseOnDrop;

        let dir = TempDir::new().expect("tempdir");
        let socket_path = dir.path().join("phux.sock");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let cfg = ServerConfig {
            socket_path: socket_path.clone(),
            pre_seeded_session: Some("overlay-startup".to_owned()),
            seed_with_pty: false,
            seed_command: None,
            ..ServerConfig::with_default_socket()
        };
        let server = tokio::task::spawn_local(async move {
            ServerRuntime::new(cfg)
                .overlay_detect(blocking_detect)
                .run_async(async move {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        // The load-bearing line. HELLO/HELLO_OK completes only if the accept
        // loop ran and spawned a client task — while the detector, which
        // nothing has released yet, is still blocked.
        let started = Instant::now();
        let stream = wait_for_socket(&socket_path, SOCKET_CONNECT_DEADLINE).await;
        let served_in = started.elapsed();
        assert!(
            !detect_released(),
            "the test released the detector before the server answered; the \
             assertion below would prove nothing",
        );

        // Non-vacuity: detection has to actually be happening, or this test
        // would keep passing after the auto-listener stopped detecting at all.
        let start_deadline = Instant::now() + DETECT_START_DEADLINE;
        while !DETECT_STARTED.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < start_deadline,
                "overlay detection never ran, so nothing here was exercised. \
                 The auto-listen gate is closed on this machine: check \
                 PHUX_NO_AUTO_LISTEN and PHUX_PROFILE.",
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !detect_released(),
            "detection returned on its own; the barrier is not holding and \
             the ordering claim is unproven (served in {served_in:?})",
        );

        // Detection finishing must not disturb a server that is already
        // running: with no address detected there is nothing to bind, and the
        // auto-listen future has to keep parking rather than resolve and let
        // the accept set unwind.
        release_detect();
        let second = wait_for_socket(&socket_path, SOCKET_CONNECT_DEADLINE).await;

        drop(second);
        drop(stream);
        shutdown_tx.send(()).expect("shutdown receiver alive");
        tokio::time::timeout(SERVER_JOIN_DEADLINE, server)
            .await
            .expect("server joined within deadline")
            .expect("server task did not panic")
            .expect("server exited cleanly");
    });
}
