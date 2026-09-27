//! Restack Cinnamon's background daemon after mpvpaper starts (issue #28).
//!
//! Newer muffin (Cinnamon's compositor) implements `zwlr_layer_shell_v1` for
//! every client, so `capability::detect()`'s registry probe sees layer-shell
//! and the daemon runs the ordinary Wayland mpvpaper path there — good. But
//! muffin inserts each newly mapped BACKGROUND-layer surface at index 0 of its
//! background window group (`clutter_actor_insert_child_at_index(..., 0)`),
//! i.e. *newer* surfaces paint *under* older ones, and nothing ever reorders
//! that group. `cinnamon-background-daemon` maps one opaque BACKGROUND surface
//! per monitor at login and keeps it — so an mpvpaper started after login is
//! silently hidden underneath Cinnamon's own wallpaper.
//!
//! The fix: after a new mpvpaper surface is confirmed live, restart Cinnamon's
//! background daemon. Its windows get recreated *after* mpvpaper's, so muffin
//! inserts them at index 0 — now underneath mpvpaper, where they belong.
//!
//! This only matters on Cinnamon with the newer, layer-shell-capable muffin;
//! see [`should_restack`]. Every subprocess call here uses `gdbus`, is bounded
//! by a timeout, and never reads stdin.
//!
//! Cinnamon's own `backgroundManager.js` restarts the daemon itself if its bus
//! name vanishes unexpectedly, but only once per 60s (`RESTART_LIMIT = 1`,
//! `RESTART_WINDOW_US`) — it cannot tell our deliberate SIGTERM from a crash.
//! Every SIGTERM we send spends that budget, so [`restack`] coalesces into one
//! restart per burst (see [`RestackScheduler`]) and always re-activates the
//! daemon itself afterwards (`Start`), with retries — Fresco must never leave
//! the session without its wallpaper daemon.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BUS_NAME: &str = "org.Cinnamon.Background";
const OBJECT_PATH: &str = "/org/Cinnamon/Background";
const IFACE: &str = "org.Cinnamon.Background";
const CALL_TIMEOUT_SECS: &str = "2";

/// Debounce: wait this long after the *last* newly-confirmed player before
/// acting, so a burst of spawns (all outputs starting at once, or a hotplug
/// that respawns several) collapses into a single restack.
const DEBOUNCE: Duration = Duration::from_millis(1500);
/// Minimum time between two restacks — each one spends Cinnamon's own 60s
/// restart budget, so we must stay well under it.
const MIN_INTERVAL: Duration = Duration::from_secs(10);
/// How long to wait for the daemon to report READY (State == 1) before giving
/// up on this attempt — mirrors Cinnamon's own `READY_FALLBACK_MS`.
const READY_TIMEOUT: Duration = Duration::from_millis(4000);
/// How long to wait for the bus name to vanish after SIGTERM, or to reappear
/// after `Start`.
const NAME_CHANGE_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const START_RETRIES: u32 = 3;

/// A pure, timestamp-driven scheduler: coalesces a burst of "a player just
/// went live" events into a single restack, and rate-limits restacks overall.
///
/// Never dropped: if a restack is needed but the rate limit hasn't cleared
/// yet, the request stays pending and fires as soon as it does — see
/// [`RestackScheduler::poll`].
#[derive(Debug, Default)]
pub struct RestackScheduler {
    /// Set on the first event of a burst and refreshed by every later one;
    /// cleared once a restack fires for it.
    pending_since: Option<Instant>,
    last_restack: Option<Instant>,
}

impl RestackScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a new mpvpaper surface was just confirmed live.
    pub fn note_spawn(&mut self, now: Instant) {
        self.pending_since = Some(now);
    }

    /// Whether a restack should run now. Consumes the pending request (resets
    /// the debounce and starts the rate-limit window) when it returns `true`.
    #[must_use]
    pub fn poll(&mut self, now: Instant) -> bool {
        let Some(since) = self.pending_since else {
            return false;
        };
        if now.saturating_duration_since(since) < DEBOUNCE {
            return false; // still inside the debounce window — more may come
        }
        if let Some(last) = self.last_restack {
            if now.saturating_duration_since(last) < MIN_INTERVAL {
                return false; // rate-limited; stays pending for the next poll
            }
        }
        self.pending_since = None;
        self.last_restack = Some(now);
        true
    }
}

/// Whether restacking applies to this session at all: Wayland, the registry
/// probe chose the layer-shell backend, the desktop is Cinnamon, and — the
/// part that tells old muffin (no layer-shell, static-frame fallback, this
/// module never even runs) apart from new — the background daemon's bus name
/// currently has an owner. An older Cinnamon without the D-Bus-activatable
/// daemon simply never owns that name, and we do nothing.
pub fn should_restack(capability: crate::capability::Capability) -> bool {
    should_restack_core(
        capability,
        crate::capability::is_cinnamon(),
        name_has_owner(BUS_NAME).unwrap_or(false),
    )
}

fn should_restack_core(
    capability: crate::capability::Capability,
    is_cinnamon: bool,
    daemon_owns_name: bool,
) -> bool {
    capability == crate::capability::Capability::WaylandLayerShell
        && is_cinnamon
        && daemon_owns_name
}

/// Make sure the daemon is reachable, activating it if its name has no owner.
/// Idempotent — `Start` on an already-running (or already-activating)
/// `G_APPLICATION_IS_SERVICE` daemon is a documented no-op. Callers should
/// rate-limit this themselves (it's meant to run on an occasional supervisor
/// tick, not every tick).
pub fn ensure_daemon_running() {
    if name_has_owner(BUS_NAME) == Some(false) {
        log::info!("cinnamon: background daemon has no owner; activating it");
        let _ = call_start();
    }
}

/// Restart Cinnamon's background daemon so its window group gets recreated
/// after mpvpaper's surfaces — see the module doc comment. Safe to call from
/// any thread; performs several blocking, timeout-bounded `gdbus` round
/// trips, so callers on the daemon's main loop should run it on a spawned
/// thread instead of blocking the tick loop.
pub fn restack() {
    let Some(pid) = get_daemon_pid() else {
        log::warn!("cinnamon: could not find the background daemon's PID; skipping restack");
        return;
    };
    if !looks_like_background_daemon(pid) {
        log::warn!(
            "cinnamon: pid {pid} owning {BUS_NAME} doesn't look like \
             cinnamon-background-daemon; refusing to signal it"
        );
        return;
    }

    // Never kill an initializing daemon — it may be mid-startup (e.g. Fresco
    // autostarted during login, racing Cinnamon's own reveal), and killing it
    // then can delay that reveal. Wait for READY first; if it never gets
    // there, bail out and let a later spawn retry.
    if !wait_for_state(1, READY_TIMEOUT) {
        log::warn!(
            "cinnamon: background daemon (pid {pid}) not READY within {:?}; \
             skipping this restack attempt",
            READY_TIMEOUT
        );
        return;
    }

    if let Err(e) = signal_terminate(pid) {
        log::warn!("cinnamon: failed to signal background daemon (pid {pid}): {e}");
        return;
    }
    if !wait_for_name_owner(false, NAME_CHANGE_TIMEOUT) {
        log::warn!(
            "cinnamon: background daemon (pid {pid}) did not exit within {:?}",
            NAME_CHANGE_TIMEOUT
        );
        // Fall through anyway and try to (re)activate it — Fresco must never
        // leave the user without a wallpaper daemon.
    }

    let mut started = false;
    for attempt in 1..=START_RETRIES {
        if call_start() {
            started = true;
            break;
        }
        log::warn!("cinnamon: Start attempt {attempt}/{START_RETRIES} failed; retrying");
        std::thread::sleep(Duration::from_millis(300 * u64::from(attempt)));
    }
    if !started {
        log::error!(
            "cinnamon: could not reactivate the background daemon after {START_RETRIES} \
             attempts — the session may be left without its wallpaper daemon"
        );
        return;
    }

    if !wait_for_state(1, READY_TIMEOUT) {
        log::warn!(
            "cinnamon: reactivated background daemon did not report READY within {:?}",
            READY_TIMEOUT
        );
        return;
    }

    let new_pid = get_daemon_pid();
    match new_pid {
        Some(new_pid) => {
            log::info!("cinnamon: restacked background daemon (pid {pid} -> {new_pid})")
        }
        None => log::info!("cinnamon: restacked background daemon (pid {pid} -> ?)"),
    }
}

// ---------------------------------------------------------------------------
// gdbus plumbing
// ---------------------------------------------------------------------------

fn gdbus_call(dest: &str, path: &str, method: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("gdbus")
        .args(["call", "--session", "--timeout", CALL_TIMEOUT_SECS])
        .args(["--dest", dest, "--object-path", path, "--method", method])
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `NameHasOwner` — `None` only if the call itself couldn't be made (no
/// session bus, `gdbus` missing).
fn name_has_owner(name: &str) -> Option<bool> {
    let out = gdbus_call(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus.NameHasOwner",
        &[name],
    )?;
    parse_bool_reply(&out)
}

fn get_daemon_pid() -> Option<u32> {
    let out = gdbus_call(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus.GetConnectionUnixProcessID",
        &[BUS_NAME],
    )?;
    parse_pid_reply(&out)
}

fn get_state() -> Option<u32> {
    let out = gdbus_call(
        BUS_NAME,
        OBJECT_PATH,
        "org.freedesktop.DBus.Properties.Get",
        &[IFACE, "State"],
    )?;
    parse_state_reply(&out)
}

fn call_start() -> bool {
    gdbus_call(BUS_NAME, OBJECT_PATH, &format!("{IFACE}.Start"), &[]).is_some()
}

fn wait_for_state(want: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if get_state() == Some(want) {
            return true;
        }
        if Instant::now() >= deadline {
            return get_state() == Some(want);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn wait_for_name_owner(want: bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if name_has_owner(BUS_NAME) == Some(want) {
            return true;
        }
        if Instant::now() >= deadline {
            return name_has_owner(BUS_NAME) == Some(want);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Executable basename of the real daemon. `/proc/<pid>/comm` is truncated by
/// the kernel to 15 bytes, so a straight substring match against this name
/// would never match the real daemon either — see [`comm_matches_daemon`].
const DAEMON_EXE_NAME: &str = "cinnamon-background-daemon";

/// Never signal a PID that doesn't at least look like the real daemon —
/// belt-and-braces against a stale/reused PID. Checks `/proc/<pid>/comm`
/// (truncation-tolerant) and falls back to the `exe` symlink's basename.
fn looks_like_background_daemon(pid: u32) -> bool {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string());
    let exe_name = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    comm_matches_daemon(comm.as_deref(), exe_name.as_deref())
}

/// Pure core of [`looks_like_background_daemon`]: `comm` matches if it is a
/// (possibly kernel-truncated-to-15-bytes) prefix of [`DAEMON_EXE_NAME`];
/// `exe_name` matches if it merely contains that name (it is never truncated).
fn comm_matches_daemon(comm: Option<&str>, exe_name: Option<&str>) -> bool {
    if let Some(comm) = comm {
        if !comm.is_empty() && DAEMON_EXE_NAME.starts_with(comm) {
            return true;
        }
    }
    exe_name.is_some_and(|n| n.contains(DAEMON_EXE_NAME))
}

fn signal_terminate(pid: u32) -> Result<(), nix::errno::Errno> {
    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;
    signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM)
}

// ---------------------------------------------------------------------------
// Pure parsers for gdbus's text output
// ---------------------------------------------------------------------------

/// Parse `gdbus call`'s reply to `GetConnectionUnixProcessID`, e.g.
/// `(uint32 1234,)`.
fn parse_pid_reply(out: &str) -> Option<u32> {
    parse_uint32_after_marker(out)
}

/// Find the first `uint32` type marker and parse the digits right after it.
///
/// A naive "skip to the first digit" parser breaks here: gdbus's own type
/// annotation, the word `uint32`, *contains* a digit (`3`, `2`) before the
/// actual value ever appears, e.g. `(uint32 305288,)` — skipping to the first
/// digit lands inside "uint32" itself and yields `32`, not `305288`.
fn parse_uint32_after_marker(out: &str) -> Option<u32> {
    let idx = out.find("uint32")?;
    let rest = &out[idx + "uint32".len()..];
    let digits: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Parse `gdbus call`'s reply to `NameHasOwner`, e.g. `(true,)` / `(false,)`.
fn parse_bool_reply(out: &str) -> Option<bool> {
    let out = out.trim();
    if out.contains("true") {
        Some(true)
    } else if out.contains("false") {
        Some(false)
    } else {
        None
    }
}

/// Parse the `Properties.Get State` reply, e.g. `(<uint32 1>,)`.
fn parse_state_reply(out: &str) -> Option<u32> {
    parse_uint32_after_marker(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;

    #[test]
    fn parses_pid_reply() {
        assert_eq!(parse_pid_reply("(uint32 1234,)\n"), Some(1234));
        assert_eq!(parse_pid_reply("(uint32 0,)"), Some(0));
        assert_eq!(parse_pid_reply(""), None);
        assert_eq!(parse_pid_reply("garbage"), None);
        // Regression: "uint32" itself contains digits ("3", "2") before the
        // real value — a PID whose value could be confused with those must
        // still parse correctly.
        assert_eq!(parse_pid_reply("(uint32 305288,)\n"), Some(305288));
        assert_eq!(parse_pid_reply("(uint32 32,)\n"), Some(32));
    }

    #[test]
    fn parses_bool_reply() {
        assert_eq!(parse_bool_reply("(true,)\n"), Some(true));
        assert_eq!(parse_bool_reply("(false,)\n"), Some(false));
        assert_eq!(parse_bool_reply(""), None);
    }

    #[test]
    fn parses_state_reply() {
        assert_eq!(parse_state_reply("(<uint32 1>,)\n"), Some(1));
        assert_eq!(parse_state_reply("(<uint32 0>,)\n"), Some(0));
        assert_eq!(parse_state_reply("error: no such property"), None);
    }

    #[test]
    fn gate_requires_layer_shell_and_cinnamon_and_owned_name() {
        assert!(should_restack_core(
            Capability::WaylandLayerShell,
            true,
            true
        ));
        assert!(!should_restack_core(
            Capability::WaylandLayerShell,
            true,
            false
        ));
        assert!(!should_restack_core(
            Capability::WaylandLayerShell,
            false,
            true
        ));
        assert!(!should_restack_core(
            Capability::WaylandGnomeStatic,
            true,
            true
        ));
        assert!(!should_restack_core(Capability::X11, true, true));
    }

    #[test]
    fn scheduler_debounces_a_burst() {
        let mut s = RestackScheduler::new();
        let t0 = Instant::now();
        s.note_spawn(t0);
        assert!(!s.poll(t0)); // still inside debounce
                              // A second spawn shortly after refreshes the debounce window.
        s.note_spawn(t0 + Duration::from_millis(500));
        assert!(!s.poll(t0 + Duration::from_millis(1000))); // < 1500ms since refresh
        assert!(s.poll(t0 + Duration::from_millis(2100))); // 1600ms since refresh: fires
        assert!(!s.poll(t0 + Duration::from_millis(2200))); // consumed
    }

    #[test]
    fn scheduler_rate_limits_and_keeps_pending_request() {
        let mut s = RestackScheduler::new();
        let t0 = Instant::now();
        s.note_spawn(t0);
        assert!(s.poll(t0 + DEBOUNCE + Duration::from_millis(1)));
        // Immediately need another restack — but rate limit hasn't cleared.
        let t1 = t0 + DEBOUNCE + Duration::from_millis(1);
        s.note_spawn(t1);
        assert!(!s.poll(t1 + DEBOUNCE + Duration::from_millis(1))); // rate-limited
                                                                    // Still pending once the minimum interval (measured from the first
                                                                    // restack, at t1) has passed.
        assert!(s.poll(t1 + MIN_INTERVAL + Duration::from_millis(10)));
    }

    #[test]
    fn scheduler_does_nothing_without_a_pending_request() {
        let mut s = RestackScheduler::new();
        assert!(!s.poll(Instant::now()));
        assert!(!s.poll(Instant::now() + Duration::from_secs(100)));
    }

    #[test]
    fn refuses_to_signal_a_non_daemon_pid() {
        // pid 1 (init/systemd) must never look like the background daemon.
        assert!(!looks_like_background_daemon(1));
    }

    #[test]
    fn comm_matching_tolerates_kernel_truncation() {
        // The real daemon's comm, truncated to 15 bytes by the kernel.
        assert!(comm_matches_daemon(Some("cinnamon-backgr"), None)); // 15-byte truncation
        assert!(comm_matches_daemon(
            Some("cinnamon-background-daemon"),
            None
        ));
        assert!(comm_matches_daemon(
            None,
            Some("/usr/bin/cinnamon-background-daemon")
        ));
        assert!(!comm_matches_daemon(Some("python3"), None));
        assert!(!comm_matches_daemon(Some(""), None));
        assert!(!comm_matches_daemon(
            Some("bash"),
            Some("/usr/bin/python3.12")
        ));
        // A comm that merely starts similarly but diverges must not match.
        assert!(!comm_matches_daemon(Some("cinnamon-backgammon"), None));
    }

    /// Full integration test against a fake `org.Cinnamon.Background` service
    /// on an isolated session bus (`dbus-run-session`), calling the real
    /// [`restack`] and [`ensure_daemon_running`] through the
    /// `cinnamon_restack_probe` example binary. Asserts the exact sequence:
    /// wait for READY, SIGTERM the old pid, wait for it to exit, `Start` a
    /// new instance, wait for READY again — and that `ensure_daemon_running`
    /// is a no-op while the daemon already owns the name. The burst-coalescing
    /// and non-daemon-pid-refusal properties are covered by the pure unit
    /// tests above ([`scheduler_debounces_a_burst`],
    /// [`refuses_to_signal_a_non_daemon_pid`]) rather than repeated here.
    /// Needs `dbus-run-session` and `python3` with `gi` (PyGObject); both are
    /// checked and the test skips itself (does not fail the suite) if either
    /// is missing.
    ///
    /// Run with:
    ///   cargo test --features daemon --locked -- --ignored --nocapture \
    ///     daemon::cinnamon_bg::tests::restack_against_fake_daemon
    #[test]
    #[ignore = "needs dbus-run-session + python3 gi; see doc comment"]
    fn restack_against_fake_daemon() {
        let script = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/cinnamon_bg_fake_daemon.sh"
        );
        let out = Command::new("bash")
            .arg(script)
            .output()
            .expect("failed to run tests/cinnamon_bg_fake_daemon.sh");
        print!("{}", String::from_utf8_lossy(&out.stdout));
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "fake-daemon integration test failed");
    }
}
