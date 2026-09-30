//! X11 window manager lock host: spawns `xsecurelock` running Fresco's own
//! saver module (`daemon::saver`, `frescod --saver`) as its background.
//!
//! `xsecurelock`'s own small, audited core keeps sole ownership of
//! authentication and the input grab; the saver it spawns has none of that
//! capability (`docs/plan-lock-screen.md` §4.3) — see `daemon::saver`'s own
//! module docs for the full split. This file's only job is getting
//! `xsecurelock` running with the right saver and the right look; the video
//! and widgets are entirely the saver's problem, reported back to the daemon
//! over [`crate::ipc::Request::LockNotify`] once it starts (this `lock()`
//! never learns the mpv socket directly — see
//! [`super::LockTargets::Sockets`]'s own doc comment).
//!
//! # Verified against xsecurelock's own source
//!
//! Everything below is checked against `github.com/google/xsecurelock`
//! (master branch, fetched with `curl`), not assumed from its README alone:
//!
//! - **An absolute `XSECURELOCK_SAVER` works**, no `HELPER_PATH` symlink
//!   needed. `GetExecutablePathSetting` (`env_settings.c`) rejects a bare
//!   name containing a `/` that isn't itself absolute, but explicitly exempts
//!   `value[0] == '/'` from that check, and its `access(value, X_OK)`
//!   executability check runs on that same absolute path directly — no
//!   dependency on the caller's current directory or on xsecurelock's own
//!   (distro-specific) helper directory. [`SAVER_WRAPPER_PATH`] is therefore
//!   the plain absolute path this package installs the wrapper to.
//! - **`xss-lock -- fresco lock` is the documented trigger**: xsecurelock's
//!   own README's "Automatic Locking" section recommends
//!   `xss-lock -n <dimmer> -l -- xsecurelock` and warns against invoking
//!   `xsecurelock` a second, different way — [`X11Host::notes`] points at the
//!   `fresco lock` equivalent of that same line.
//! - **Only README-documented `XSECURELOCK_*` variables are set here**:
//!   `XSECURELOCK_FONT`, `XSECURELOCK_AUTH_BACKGROUND_COLOR`,
//!   `XSECURELOCK_AUTH_FOREGROUND_COLOR` — each confirmed present, with this
//!   exact spelling, in the "Options" section of xsecurelock's README.
//! - **`xsecurelock [options] -- notify-command...` proves the lock**: the
//!   README states plainly that "command line arguments following a `--`
//!   argument will be executed via `execvp` once locking is successful; this
//!   can be used to notify a calling process of successful locking", and
//!   `main.c` confirms the exact trigger: `NotifyOfLock` runs only once,
//!   gated on `background_window_mapped && background_window_visible &&
//!   saver_window_mapped && !xss_lock_notified` — i.e. only after the saver
//!   and background windows are actually mapped and visible, which is as
//!   real as proof-of-lock gets short of reading xsecurelock's own memory.
//!   `NotifyOfLock` **forks** the notify command (`ForkWithoutSigHandlers` +
//!   `execvp` in the child) and returns immediately; the parent keeps running
//!   its normal event loop for as long as the session stays locked, reaping
//!   the notify child via a plain `WaitProc` a few lines later in that same
//!   loop. So waiting for the notify file is waiting for a real, gated
//!   signal from xsecurelock itself, not a race against it exiting.
//! - **The notify command is a raw `argv`, not a shell line**: arg parsing
//!   (`ParseArgs`) does `notify_command = argv + i + 1;` at the `--` token
//!   and `execvp(notify_command[0], notify_command)` later — no shell
//!   involved at that layer, so [`X11Host::lock`] hands xsecurelock
//!   `sh -c '<script>' sh <path>` ([`NOTIFY_SCRIPT`]) as five separate argv
//!   elements (`sh`'s own `$0`/`$1` trick) rather than building one
//!   shell-quoted string, which means the confirmation path never needs
//!   escaping here at all.
//! - **`XSECURELOCK_PAM_SERVICE`'s compiled-in default is not a fixed,
//!   knowable string** — unlike swaylock-plugin's hardcoded
//!   `"swaylock-plugin"` (`hosts::wlroots`'s own doc comment).
//!   `authproto_pam.c` reads it as
//!   `GetStringSetting("XSECURELOCK_PAM_SERVICE", PAM_SERVICE_NAME)`, and
//!   `PAM_SERVICE_NAME` is a `-D`-defined macro (`Makefile.am`) filled in from
//!   `configure.ac`'s `--with-pam-service-name`, which that same script marks
//!   **mandatory** (`AC_MSG_ERROR([--with-pam-service-name is mandatory.])`
//!   if omitted) with no fallback default of its own — the README's
//!   installation instructions just recommend distro packagers pass
//!   `xscreensaver` or `common-auth`, whichever fits. Fresco cannot read what
//!   a given distro's package compiled in, so it can never assume leaving
//!   this variable unset is safe — see [`pam_service_env`].
//!
//! # Bounded wait for proof of lock
//!
//! `xsecurelock` blocks for as long as the session stays locked — potentially
//! hours — so [`X11Host::lock`] spawns it detached, in its own process group
//! (a signal aimed at `frescod` must never also reach the locker), exactly as
//! before. What changed: `lock` no longer returns the instant `spawn`
//! succeeds. A crashed-at-once `xsecurelock` (bad `$DISPLAY`, no X11
//! authority, a missing PAM file) would otherwise make Fresco report a
//! locked session that is not actually locked — the one outcome
//! `docs/plan-lock-screen.md` §5's fail-closed invariant forbids outright.
//! [`wait_for_lock_confirmation`] instead polls, for up to
//! [`LOCK_CONFIRM_TIMEOUT`], for either the notify file to appear (real
//! proof, see above) or the child to have already exited (proof of the
//! opposite); only on confirmed-locked does `lock` hand the still-running
//! `Child` back to the daemon's lock-state machine to supervise for the rest
//! of the session, unchanged from before. A timeout or an early exit kills
//! the child (if it is even still alive) and returns `Err` — this is the one
//! place this host now differs from `hosts::loginctl_lock_session`'s bounded
//! wait: that call *finishes* in milliseconds by design, this one only
//! *proves itself* within a bound and then keeps running for hours.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{HostCtx, HostKind, LockHost, LockTargets, RunningHost};
use crate::config::{Config, ThemeMode};
use crate::ipc::LockSetupState;

/// Absolute path this package installs the saver wrapper to — see
/// `packaging/debian`, `packaging/aur/*/PKGBUILD` and `install.sh`, and
/// `data/saver_fresco` for the wrapper's own (one-line) source. See the
/// module docs for why an absolute path needs no further packaging trick to
/// work with `XSECURELOCK_SAVER`.
pub const SAVER_WRAPPER_PATH: &str = "/usr/lib/fresco/saver_fresco";

/// Font family passed as `XSECURELOCK_FONT`. Matches the font this package
/// already recommends everywhere else (`Cargo.toml`'s `[package.metadata.deb]`
/// `recommends = "... fonts-inter ..."`), so the auth dialog reads as part of
/// the same visual identity as the rest of Fresco rather than a mismatched
/// system default — and degrades harmlessly to fontconfig's own fallback on
/// a system that never installed that recommendation.
const AUTH_FONT: &str = "Inter";

/// Longest [`wait_for_lock_confirmation`] waits for xsecurelock's own notify
/// command (see the module doc) before giving up and treating the lock as
/// failed. Bounded so a wedged/never-mapping xsecurelock can never hang
/// `Request::Lock` itself — a prompt, honest failure is always better than
/// that, per `docs/plan-lock-screen.md` §5.
const LOCK_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);

/// How often [`wait_for_lock_confirmation`] polls for the confirmation file
/// / the child's exit. Short enough that the common case (a healthy
/// xsecurelock mapping its windows in well under a second) is barely
/// delayed; long enough not to busy-loop a core while it waits.
const LOCK_CONFIRM_POLL: Duration = Duration::from_millis(50);

/// Argv appended after xsecurelock's own `--`: `sh -c '<script>' sh <path>`.
/// `sh`'s own convention makes the first word after the script itself become
/// `$0` (conventionally `sh`, unused) and every word after that `$1`, `$2`,
/// …, so the confirmation path arrives as a real argv element — never
/// interpolated into the script text — and needs no shell quoting here at
/// all, however odd the path (a `$XDG_RUNTIME_DIR` under a username with a
/// space in it, say).
const NOTIFY_SCRIPT: &str = "printf locked > \"$1\"";

/// Env var xsecurelock reads for its PAM service name
/// (`authproto_pam.c`: `GetStringSetting("XSECURELOCK_PAM_SERVICE",
/// PAM_SERVICE_NAME)`) — see the module doc for why Fresco always sets this
/// explicitly rather than trusting whatever a given distro compiled in as
/// `PAM_SERVICE_NAME`.
const PAM_SERVICE_ENV: &str = "XSECURELOCK_PAM_SERVICE";

/// PAM service files tried, in order, when the user has not already set
/// [`PAM_SERVICE_ENV`] themselves. `"xsecurelock"` first (a distro or the
/// user may ship one specifically for it), then the two general-purpose
/// service names xsecurelock's own README suggests packagers fall back to
/// (`"common-auth"` on Debian/Ubuntu, `"system-auth"` on the RPM family).
const AUTO_PAM_SERVICES: [&str; 3] = ["xsecurelock", "common-auth", "system-auth"];

/// Where a PAM service file might live, checked in this order. Mirrors
/// `hosts::wlroots::PAM_ROOTS` exactly (same reasoning: `/etc/pam.d` is
/// where every mainstream distro's package puts it; `/usr/lib/pam.d` covers
/// distros that ship factory PAM config there instead; `/usr/etc/pam.d`
/// covers a read-only-`/usr` layout that symlinks `/etc` under `/usr/etc`) —
/// duplicated rather than shared because neither lock-host module depends on
/// the other and this fix's scope does not extend to introducing a shared
/// home for the constant.
const PAM_ROOTS: [&str; 3] = ["/etc/pam.d", "/usr/lib/pam.d", "/usr/etc/pam.d"];

pub struct X11Host;

impl LockHost for X11Host {
    fn kind(&self) -> HostKind {
        HostKind::X11Wm
    }

    /// Spawn `xsecurelock` detached, in its own process group, with
    /// [`SAVER_WRAPPER_PATH`] as its saver and Fresco's own theme as the auth
    /// dialog's colors/font, then wait (bounded) for its own notify command to
    /// prove the screen actually locked before returning — see the module
    /// doc's "Bounded wait for proof of lock" section. Errs (never leaves a
    /// half-started locker behind, and kills the child first if the wait
    /// itself is what failed) if `xsecurelock`, the saver wrapper, or a usable
    /// PAM service is missing, or if the wait times out — the daemon's
    /// fallback chain (`docs/plan-lock-screen.md` §4.2/§7.2's AC) takes over
    /// from there.
    fn lock(&self, ctx: &HostCtx) -> Result<RunningHost, String> {
        let xsecurelock = find_xsecurelock().ok_or_else(|| {
            "xsecurelock not found on PATH — install it to lock this session through Fresco"
                .to_string()
        })?;
        if !is_executable(Path::new(SAVER_WRAPPER_PATH)) {
            return Err(format!(
                "Fresco's saver wrapper is missing ({SAVER_WRAPPER_PATH}) — reinstall Fresco"
            ));
        }
        let pam_service = pam_service_env()?;

        let confirm_path = lock_confirm_path(&ctx.runtime_dir);
        // Defensive cleanup only: a fresh nonce (see `lock_confirm_path`)
        // never legitimately collides with a leftover file, but a stale file
        // at this exact path (however unlikely) would otherwise make the
        // very first poll below read as an instant, false confirmation.
        let _ = std::fs::remove_file(&confirm_path);

        let mut cmd = Command::new(&xsecurelock);
        cmd.env("XSECURELOCK_SAVER", SAVER_WRAPPER_PATH);
        cmd.env(PAM_SERVICE_ENV, &pam_service);
        for (key, value) in auth_dialog_env(ctx.config) {
            cmd.env(key, value);
        }
        cmd.arg("--")
            .arg("sh")
            .arg("-c")
            .arg(NOTIFY_SCRIPT)
            .arg("sh")
            .arg(&confirm_path);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
        // New process group (not just a new session): see the module docs.
        // `process_group(0)` is the stable, safe std API for `setpgid(0, 0)`
        // — no `pre_exec`/`unsafe` needed.
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }

        let child = cmd
            .spawn()
            .map_err(|e| format!("failed to start xsecurelock: {e}"))?;
        let child = wait_for_lock_confirmation(child, &confirm_path, LOCK_CONFIRM_TIMEOUT)?;
        Ok(RunningHost {
            child: Some(child),
            targets: LockTargets::None,
        })
    }

    /// Always [`LockTargets::None`]: this host has no surface of its own to
    /// paint into. The saver `lock()` just launched reports its own mpv
    /// sockets back once it starts, via `LockNotify` — a completely separate
    /// path from this trait method, which only ever runs inside `frescod`.
    fn targets_while_locked(&self, _ctx: &HostCtx) -> LockTargets {
        LockTargets::None
    }

    /// [`LockSetupState::NotNeeded`] when `xsecurelock` and the saver wrapper
    /// are both present and executable (nothing to install — `Request::Lock`
    /// alone already works), [`LockSetupState::Unavailable`] otherwise. There
    /// is no `Needed`/`Done` state on this host: unlike KDE, nothing here
    /// persists a config-file edit a one-click "Set up" could install and an
    /// "Undo" could revert — see [`LockHost::setup`]/[`LockHost::undo`]'s
    /// unmodified (`Ok(())`, do-nothing) defaults.
    fn setup_state(&self, _ctx: &HostCtx) -> LockSetupState {
        if usable() {
            LockSetupState::NotNeeded
        } else {
            LockSetupState::Unavailable
        }
    }

    fn notes(&self, _ctx: &HostCtx) -> Vec<String> {
        let mut notes = Vec::new();
        if find_xsecurelock().is_none() {
            notes.push(
                crate::t!(
                    "xsecurelock is not installed — install it to use Fresco on this lock screen"
                )
                .to_string(),
            );
        } else if !is_executable(Path::new(SAVER_WRAPPER_PATH)) {
            notes.push(crate::tf!(
                "Fresco's saver helper is missing ({path}) — reinstall Fresco",
                "path" => SAVER_WRAPPER_PATH
            ));
        } else if pam_service_env().is_err() {
            notes.push(
                crate::t!(
                    "xsecurelock has no usable PAM configuration on this system, so Fresco won't lock with it"
                )
                .to_string(),
            );
        }
        // Always shown, even when everything above is fine: unlike COSMIC/KDE,
        // nothing on this host triggers a lock automatically — see
        // xsecurelock's own README ("Automatic Locking") for why the
        // `xss-lock` wrapper, not a raw idle timer, is what should call this.
        notes.push(
            crate::t!("Bind your idle-lock trigger to `xss-lock -- fresco lock`").to_string(),
        );
        notes
    }
}

fn usable() -> bool {
    find_xsecurelock().is_some()
        && is_executable(Path::new(SAVER_WRAPPER_PATH))
        && pam_service_env().is_ok()
}

/// First executable named `xsecurelock` on `$PATH` — mirrors how a shell
/// itself would resolve the bare command name `X11Host::notes`/README's own
/// `xss-lock -- xsecurelock` line assumes.
fn find_xsecurelock() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("xsecurelock"))
            .find(|p| is_executable(p))
    })
}

/// `path` names a regular file with at least one executable bit set.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// `XSECURELOCK_FONT`/`XSECURELOCK_AUTH_BACKGROUND_COLOR`/
/// `XSECURELOCK_AUTH_FOREGROUND_COLOR` for `config`'s theme — see the module
/// docs for why only these three, README-verified, variables are set.
fn auth_dialog_env(config: &Config) -> [(&'static str, &'static str); 3] {
    let (background, foreground) = auth_colors(config.theme_mode);
    [
        ("XSECURELOCK_FONT", AUTH_FONT),
        ("XSECURELOCK_AUTH_BACKGROUND_COLOR", background),
        ("XSECURELOCK_AUTH_FOREGROUND_COLOR", foreground),
    ]
}

/// `(background, foreground)` X11 colors (`XParseColor` hex, per
/// `XSECURELOCK_AUTH_*_COLOR`'s own documented format) for `mode`.
///
/// [`ThemeMode::System`] resolves to the dark pair: the GTK/libadwaita theme
/// resolution that would normally answer "which does the desktop actually
/// prefer" lives entirely behind the `gui` feature, unreachable from here
/// (`daemon` builds without it), and a lock screen reads acceptably in a dark
/// palette regardless of the desktop's own light/dark preference — a
/// reasonable default for a cosmetic setting, not a correctness one.
fn auth_colors(mode: ThemeMode) -> (&'static str, &'static str) {
    match mode {
        ThemeMode::Light => ("#f5f5f5", "#1a1a1a"),
        ThemeMode::Dark | ThemeMode::System => ("#1a1a1a", "#f5f5f5"),
    }
}

// ── proof-of-lock: notify file + bounded wait ───────────────────────────────

/// A confirmation-file path under `runtime_dir` unique enough that no earlier
/// (or concurrent, in principle — `Request::Lock` is not reentrant in
/// practice, but this costs nothing) lock attempt could have left a file
/// behind at the same name: pid + a nanosecond timestamp, the same "cheap
/// uniqueness" recipe this codebase's own tests already use for scratch
/// directories (e.g. `cosmic_bg`'s `tempdir` test helper). Living under
/// `runtime_dir` (`$XDG_RUNTIME_DIR/fresco`, hardened by
/// `ipc::ensure_safe_socket_dir` before the daemon ever binds its control
/// socket there — see `ipc.rs`) means no other local user's process can read
/// or create anything at this path in the first place, so the nonce only
/// needs to guard against Fresco's own reuse, not against another uid.
fn lock_confirm_path(runtime_dir: &Path) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    runtime_dir.join(format!("lock-confirm-{}-{nonce}", std::process::id()))
}

/// What one [`wait_for_lock_confirmation`] poll observed. Kept as plain data
/// (no `Child`/`Path` borrowed inside it) so [`evaluate_tick`] — the actual
/// decision — can be a pure function with nothing real to fake.
struct LockObservation {
    /// The notify file exists — xsecurelock's own `NotifyOfLock` only execs
    /// it once the saver/background windows are mapped and visible (see the
    /// module doc), so this is real proof the screen locked.
    confirmed: bool,
    /// `Some(description)` once the child has already exited — always a
    /// failure to report, since a lock this function never saw confirmed
    /// before the process died is not a lock at all.
    child_exited: Option<String>,
}

/// One [`wait_for_lock_confirmation`] tick's outcome.
enum LockWaitTick {
    Done(Result<(), String>),
    KeepPolling,
}

/// Pure state machine for one polling tick: given how long the wait has run
/// and what this tick observed, decide whether to report success, fail now,
/// or poll again. Free of `Instant`/`Child`/real sleeping entirely, so every
/// branch — confirmed, child died first, timed out, still waiting — is a
/// one-line test with no real process spawned and no real time elapsed.
fn evaluate_tick(elapsed: Duration, timeout: Duration, obs: &LockObservation) -> LockWaitTick {
    if obs.confirmed {
        return LockWaitTick::Done(Ok(()));
    }
    if let Some(reason) = &obs.child_exited {
        return LockWaitTick::Done(Err(format!(
            "xsecurelock exited before confirming a lock: {reason}"
        )));
    }
    if elapsed >= timeout {
        return LockWaitTick::Done(Err(format!(
            "xsecurelock did not confirm a lock within {timeout:?}"
        )));
    }
    LockWaitTick::KeepPolling
}

/// Thin real driver around [`evaluate_tick`]: poll `confirm_path` and
/// `child`'s status every [`LOCK_CONFIRM_POLL`] until confirmed, the child
/// exits, or `timeout` elapses. On any non-confirmed outcome, kills `child`
/// first if it is still alive (never leave an ambiguous xsecurelock process
/// running that Fresco is about to report as failed) and always removes
/// `confirm_path` (whether or not it ever appeared) so no stale file can
/// affect a later lock attempt. Returns the still-running `child` on success,
/// for [`X11Host::lock`] to hand to the daemon's lock-state machine exactly
/// as before this fix.
fn wait_for_lock_confirmation(
    mut child: Child,
    confirm_path: &Path,
    timeout: Duration,
) -> Result<Child, String> {
    let start = Instant::now();
    loop {
        let obs = LockObservation {
            confirmed: confirm_path.exists(),
            child_exited: match child.try_wait() {
                Ok(Some(status)) => Some(status.to_string()),
                Ok(None) => None,
                Err(e) => Some(format!("failed to check xsecurelock's status: {e}")),
            },
        };
        if let LockWaitTick::Done(result) = evaluate_tick(start.elapsed(), timeout, &obs) {
            let _ = std::fs::remove_file(confirm_path);
            return match result {
                Ok(()) => Ok(child),
                Err(e) => {
                    if obs.child_exited.is_none() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    Err(e)
                }
            };
        }
        std::thread::sleep(LOCK_CONFIRM_POLL);
    }
}

// ── PAM gate ─────────────────────────────────────────────────────────────────

/// Pure core: does any of `roots` contain a PAM file named `service`?
/// Injected `is_file` so this is testable against a temp directory instead of
/// the real `/etc`. Mirrors `hosts::wlroots::pam_service_present` exactly.
fn pam_service_file_exists(
    roots: &[&Path],
    service: &str,
    is_file: &dyn Fn(&Path) -> bool,
) -> bool {
    roots.iter().any(|r| is_file(&r.join(service)))
}

/// Pure core of [`pam_service_env`]: decide the `XSECURELOCK_PAM_SERVICE`
/// value to set (or refuse to lock at all) from an explicit user override —
/// `None` when the env var is unset — and injected filesystem access, so
/// every branch (user set it and it exists; user set it and it's missing;
/// nothing set and an auto candidate exists; nothing set and none do) is a
/// plain unit test against a temp dir.
///
/// If the user has already set [`PAM_SERVICE_ENV`], that choice is
/// authoritative — Fresco only checks that the file exists, it does not
/// second-guess which service the user wants. Otherwise, [`AUTO_PAM_SERVICES`]
/// is tried in order, since (see the module doc) xsecurelock's own compiled-in
/// default cannot be assumed to be any particular name.
fn resolve_pam_service(
    user_override: Option<&str>,
    roots: &[&Path],
    is_file: &dyn Fn(&Path) -> bool,
) -> Result<String, String> {
    if let Some(service) = user_override {
        return if pam_service_file_exists(roots, service, is_file) {
            Ok(service.to_string())
        } else {
            let dirs = roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "{PAM_SERVICE_ENV} is set to \"{service}\" but no such file exists under {dirs} \
                 — xsecurelock would have no PAM configuration to authenticate against"
            ))
        };
    }
    for candidate in AUTO_PAM_SERVICES {
        if pam_service_file_exists(roots, candidate, is_file) {
            return Ok(candidate.to_string());
        }
    }
    Err("xsecurelock would have no PAM configuration — refusing to lock with it".to_string())
}

/// The `XSECURELOCK_PAM_SERVICE` value [`X11Host::lock`] should set, or the
/// reason it refuses to lock at all. Always resolved and set explicitly —
/// never left unset — because (see the module doc) xsecurelock's own
/// compiled-in default is not something Fresco can assume, and a locker with
/// no working PAM service can never be unlocked with a correct password.
fn pam_service_env() -> Result<String, String> {
    let roots: Vec<&Path> = PAM_ROOTS.iter().map(Path::new).collect();
    let user_override = std::env::var(PAM_SERVICE_ENV).ok();
    resolve_pam_service(user_override.as_deref(), &roots, &|p: &Path| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn ctx(config: &Config) -> HostCtx<'_> {
        HostCtx {
            config,
            outputs: &[],
            runtime_dir: PathBuf::from("/run/user/1000/fresco"),
        }
    }

    // -- kind / targets_while_locked -------------------------------------------

    #[test]
    fn kind_and_targets() {
        let config = Config::default();
        let host = X11Host;
        assert_eq!(host.kind(), HostKind::X11Wm);
        assert_eq!(host.targets_while_locked(&ctx(&config)), LockTargets::None);
    }

    // -- auth_colors / auth_dialog_env (pure) -----------------------------------

    #[test]
    fn auth_colors_light_and_dark() {
        assert_eq!(auth_colors(ThemeMode::Light), ("#f5f5f5", "#1a1a1a"));
        assert_eq!(auth_colors(ThemeMode::Dark), ("#1a1a1a", "#f5f5f5"));
        assert_eq!(
            auth_colors(ThemeMode::System),
            auth_colors(ThemeMode::Dark),
            "System falls back to the same pair as Dark — see the doc comment"
        );
    }

    #[test]
    fn auth_dialog_env_only_uses_documented_variables() {
        let config = Config {
            theme_mode: ThemeMode::Light,
            ..Config::default()
        };
        let env = auth_dialog_env(&config);
        let keys: Vec<&str> = env.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            [
                "XSECURELOCK_FONT",
                "XSECURELOCK_AUTH_BACKGROUND_COLOR",
                "XSECURELOCK_AUTH_FOREGROUND_COLOR",
            ]
        );
        for (_, v) in env {
            assert!(!v.is_empty());
        }
    }

    // -- is_executable ------------------------------------------------------

    #[test]
    fn is_executable_checks_the_permission_bits() {
        let dir = std::env::temp_dir().join(format!("fresco-x11host-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let exe = dir.join("exe");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable(&exe));

        let not_exe = dir.join("not-exe");
        std::fs::write(&not_exe, b"data").unwrap();
        std::fs::set_permissions(&not_exe, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_executable(&not_exe));

        assert!(!is_executable(&dir.join("does-not-exist")));
        // A directory is never "executable" in this module's sense, even
        // though the x bit means something else for directories on POSIX.
        assert!(!is_executable(&dir));

        std::fs::remove_dir_all(&dir).ok();
    }

    // -- find_xsecurelock / usable / setup_state / notes, with a fake $PATH ----

    /// Point `$PATH` at a throwaway directory for the duration of `f`,
    /// serialized against every other test in this binary that mutates
    /// process-wide environment state (`crate::ENV_LOCK`, the same guard
    /// `daemon::mpvpaper`'s own env-touching tests use) and restored
    /// afterwards regardless of how `f` returns.
    fn with_fake_path<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let original = std::env::var_os("PATH");
        std::env::set_var("PATH", dir);
        let result = f();
        match original {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        result
    }

    #[test]
    fn find_xsecurelock_via_fake_path() {
        let dir =
            std::env::temp_dir().join(format!("fresco-x11host-path-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("xsecurelock");
        std::fs::write(&fake, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        with_fake_path(&dir, || {
            assert_eq!(find_xsecurelock(), Some(fake.clone()));
        });

        // An empty PATH must find nothing, not panic.
        let empty_dir =
            std::env::temp_dir().join(format!("fresco-x11host-empty-path-{}", std::process::id()));
        std::fs::create_dir_all(&empty_dir).unwrap();
        with_fake_path(&empty_dir, || {
            assert_eq!(find_xsecurelock(), None);
        });

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&empty_dir).ok();
    }

    #[test]
    fn setup_state_and_notes_are_unavailable_when_xsecurelock_is_missing() {
        // Deterministic regardless of whether `SAVER_WRAPPER_PATH` happens to
        // exist on the machine running this test: `usable()` is an AND of
        // both checks, so forcing xsecurelock alone to be absent is already
        // enough to force `Unavailable`.
        let empty_dir =
            std::env::temp_dir().join(format!("fresco-x11host-setup-state-{}", std::process::id()));
        std::fs::create_dir_all(&empty_dir).unwrap();
        let config = Config::default();
        with_fake_path(&empty_dir, || {
            let host = X11Host;
            assert_eq!(host.setup_state(&ctx(&config)), LockSetupState::Unavailable);
            let notes = host.notes(&ctx(&config));
            assert!(
                notes
                    .iter()
                    .any(|n| n.to_lowercase().contains("xsecurelock")),
                "{notes:?}"
            );
            // The `xss-lock -- fresco lock` note is unconditional.
            assert!(notes.iter().any(|n| n.contains("fresco lock")), "{notes:?}");
        });
        std::fs::remove_dir_all(&empty_dir).ok();
    }

    #[test]
    fn lock_errs_clearly_when_xsecurelock_is_missing() {
        let empty_dir = std::env::temp_dir().join(format!(
            "fresco-x11host-lock-no-xsecurelock-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&empty_dir).unwrap();
        let config = Config::default();
        with_fake_path(&empty_dir, || {
            // Not `.unwrap_err()`: `RunningHost` (the `Ok` side) holds a
            // `std::process::Child`, which has no `Debug` impl for that
            // method's bound to reach for.
            match X11Host.lock(&ctx(&config)) {
                Err(err) => assert!(err.contains("xsecurelock"), "{err}"),
                Ok(_) => panic!("expected an error when xsecurelock is missing from PATH"),
            }
        });
        std::fs::remove_dir_all(&empty_dir).ok();
    }

    // -- evaluate_tick: pure proof-of-lock state machine ---------------------

    fn obs(confirmed: bool, child_exited: Option<&str>) -> LockObservation {
        LockObservation {
            confirmed,
            child_exited: child_exited.map(str::to_string),
        }
    }

    #[test]
    fn evaluate_tick_confirmed_wins_even_alongside_other_signals() {
        // Confirmation is checked first: even a child that also happens to
        // look exited, or a tick past the timeout, must still report success
        // once the notify file is there — the file is real proof, so nothing
        // else gets a vote once it exists.
        assert!(matches!(
            evaluate_tick(Duration::ZERO, Duration::from_secs(5), &obs(true, None)),
            LockWaitTick::Done(Ok(()))
        ));
        assert!(matches!(
            evaluate_tick(
                Duration::from_secs(99),
                Duration::from_secs(5),
                &obs(true, Some("exited"))
            ),
            LockWaitTick::Done(Ok(()))
        ));
    }

    #[test]
    fn evaluate_tick_child_exit_without_confirmation_is_err() {
        match evaluate_tick(
            Duration::from_millis(1),
            Duration::from_secs(5),
            &obs(false, Some("exit status: 1")),
        ) {
            LockWaitTick::Done(Err(e)) => assert!(e.contains("exited"), "{e}"),
            _ => panic!("expected Done(Err(..)) for a child that exited without confirming"),
        }
    }

    #[test]
    fn evaluate_tick_timeout_without_confirmation_or_exit_is_err() {
        let timeout = Duration::from_secs(5);
        match evaluate_tick(timeout, timeout, &obs(false, None)) {
            LockWaitTick::Done(Err(e)) => assert!(e.contains("did not confirm"), "{e}"),
            _ => panic!("expected a timeout error at elapsed == timeout"),
        }
    }

    #[test]
    fn evaluate_tick_keeps_polling_when_nothing_has_happened_yet() {
        assert!(matches!(
            evaluate_tick(
                Duration::from_millis(1),
                Duration::from_secs(5),
                &obs(false, None)
            ),
            LockWaitTick::KeepPolling
        ));
    }

    // -- lock_confirm_path -----------------------------------------------------

    #[test]
    fn lock_confirm_path_lives_under_runtime_dir_and_is_unique() {
        let runtime_dir = PathBuf::from("/run/user/1000/fresco");
        let a = lock_confirm_path(&runtime_dir);
        let b = lock_confirm_path(&runtime_dir);
        assert!(a.starts_with(&runtime_dir));
        assert!(a
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("lock-confirm-"));
        assert_ne!(a, b, "two calls must not collide");
    }

    // -- wait_for_lock_confirmation: thin real driver, tiny real durations ---

    /// `/bin/sh`, not a bare `"sh"`: `Command::new("sh")` would resolve it
    /// through this process's own `$PATH`, which races against
    /// `with_fake_path`'s tests elsewhere in this file (cargo runs tests in
    /// this binary concurrently, and `PATH` is process-wide, not per-thread)
    /// — a real, observed flake, not a theoretical one. An absolute path
    /// needs no `$PATH` lookup at all, so it is immune to whatever any other
    /// concurrently-running test does to that variable.
    fn spawn_sh(script: &str) -> Child {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            // The child's own `sleep` lookup must not see a fake PATH
            // another test installed process-wide via `with_fake_path`.
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("/bin/sh must be available to run this test")
    }

    fn scratch_confirm_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "fresco-x11host-confirm-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn wait_for_lock_confirmation_ok_when_file_already_present() {
        let path = scratch_confirm_path("already-there");
        std::fs::write(&path, b"locked").unwrap();
        // A long-lived child: proves success comes from the file, not from
        // the child happening to exit around the same time.
        let child = spawn_sh("sleep 5");
        let result = wait_for_lock_confirmation(child, &path, Duration::from_secs(5));
        match result {
            Ok(mut child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
            Err(e) => panic!("expected Ok, got {e}"),
        }
        assert!(
            !path.exists(),
            "confirm file must be cleaned up after a successful wait"
        );
    }

    #[test]
    fn wait_for_lock_confirmation_ok_once_file_appears_mid_wait() {
        let path = scratch_confirm_path("appears-later");
        let _ = std::fs::remove_file(&path);
        let path_str = path.to_str().unwrap();
        // Simulates xsecurelock's own notify command firing shortly after
        // start, while the "locker" itself keeps running well past that.
        let child = spawn_sh(&format!("sleep 0.1; printf locked > '{path_str}'; sleep 5"));
        let result = wait_for_lock_confirmation(child, &path, Duration::from_secs(5));
        match result {
            Ok(mut child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
            Err(e) => panic!("expected Ok, got {e}"),
        }
    }

    #[test]
    fn wait_for_lock_confirmation_err_when_child_exits_before_confirming() {
        let path = scratch_confirm_path("never-confirms");
        let _ = std::fs::remove_file(&path);
        let child = spawn_sh("exit 1");
        let err = wait_for_lock_confirmation(child, &path, Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("exited before confirming"), "{err}");
        assert!(!path.exists());
    }

    #[test]
    fn wait_for_lock_confirmation_err_on_timeout_is_bounded_and_kills_the_child() {
        let path = scratch_confirm_path("times-out");
        let _ = std::fs::remove_file(&path);
        let child = spawn_sh("sleep 5");
        let pid = child.id();
        let started = Instant::now();
        let err = wait_for_lock_confirmation(child, &path, Duration::from_millis(150)).unwrap_err();
        assert!(err.contains("did not confirm"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "must not wait anywhere near the child's `sleep 5`"
        );
        // The child must actually have been killed, not left running.
        std::thread::sleep(Duration::from_millis(50));
        let still_alive = std::fs::metadata(format!("/proc/{pid}")).is_ok();
        assert!(
            !still_alive,
            "timed-out child must be killed, not left running"
        );
    }

    // -- pam_service_file_exists / resolve_pam_service: PAM gate -------------

    fn pam_tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fresco-x11host-pam-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pam_service_file_exists_true_when_present_in_any_root() {
        let dir = pam_tempdir("present");
        std::fs::write(dir.join("xsecurelock"), "auth include login\n").unwrap();
        let other = dir.join("does-not-exist-root");
        let roots = [other.as_path(), dir.as_path()];
        assert!(pam_service_file_exists(&roots, "xsecurelock", &|p| p.is_file()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pam_service_file_exists_false_when_absent_everywhere() {
        let dir = pam_tempdir("absent");
        let roots = [dir.as_path()];
        assert!(!pam_service_file_exists(&roots, "xsecurelock", &|p| p.is_file()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_pam_service_uses_user_override_when_its_file_exists() {
        let dir = pam_tempdir("override-ok");
        std::fs::write(dir.join("my-custom-service"), "auth include login\n").unwrap();
        let roots = [dir.as_path()];
        assert_eq!(
            resolve_pam_service(Some("my-custom-service"), &roots, &|p| p.is_file()),
            Ok("my-custom-service".to_string())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_pam_service_errs_when_user_override_file_is_missing() {
        let dir = pam_tempdir("override-missing");
        let roots = [dir.as_path()];
        let err =
            resolve_pam_service(Some("no-such-service"), &roots, &|p| p.is_file()).unwrap_err();
        assert!(err.contains("no-such-service"), "{err}");
        assert!(err.contains(PAM_SERVICE_ENV), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_pam_service_falls_back_to_first_auto_candidate_present() {
        let dir = pam_tempdir("auto-common-auth");
        // Only the second candidate in AUTO_PAM_SERVICES exists.
        std::fs::write(dir.join("common-auth"), "auth include login\n").unwrap();
        let roots = [dir.as_path()];
        assert_eq!(
            resolve_pam_service(None, &roots, &|p| p.is_file()),
            Ok("common-auth".to_string())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_pam_service_prefers_earlier_auto_candidates() {
        let dir = pam_tempdir("auto-order");
        // Both "xsecurelock" and "system-auth" exist: the first
        // AUTO_PAM_SERVICES entry must win.
        std::fs::write(dir.join("xsecurelock"), "auth include login\n").unwrap();
        std::fs::write(dir.join("system-auth"), "auth include login\n").unwrap();
        let roots = [dir.as_path()];
        assert_eq!(
            resolve_pam_service(None, &roots, &|p| p.is_file()),
            Ok("xsecurelock".to_string())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_pam_service_errs_with_the_documented_message_when_nothing_is_available() {
        let dir = pam_tempdir("auto-none");
        let roots = [dir.as_path()];
        assert_eq!(
            resolve_pam_service(None, &roots, &|p| p.is_file()),
            Err(
                "xsecurelock would have no PAM configuration — refusing to lock with it"
                    .to_string()
            )
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
