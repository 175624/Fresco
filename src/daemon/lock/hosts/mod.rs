//! Which desktop is locking the session, and how Fresco asks it to.
//!
//! [`classify`] is the pure decision table `docs/plan-lock-screen.md` §4
//! describes in prose; [`detect`] is the one place that actually reads the
//! environment and talks to the compositor to fill in [`HostInputs`].
//! [`LockHost`] is the per-host adapter: [`LogindHost`] and [`CosmicHost`]
//! are real (both just ask `loginctl` to lock the session — see
//! [`loginctl_lock_session`]); [`hosts::wlroots`](wlroots), [`hosts::x11`](x11)
//! and [`hosts::kde`](kde) are the full per-desktop adapters — see each
//! submodule's own doc comment for exactly what it does.

pub(in crate::daemon) mod kde;
mod wlroots;
mod x11;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::ipc::{LockSetupState, LockSocket};

/// Which desktop (or bare compositor family) is asking to lock the session.
///
/// [`HostKind::Cosmic`] carries its own `live` flag rather than being a
/// second, separate variant, because "is this COSMIC" and "does this COSMIC
/// support show-on-lock" are two independent facts [`classify`] learns from
/// two different sources (a desktop-name match vs. a Wayland global) — see
/// [`HostInputs::has_cosmic_lock_layer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKind {
    /// COSMIC 1.9+. `live` is `cosmic_session_lock_layer_manager_v1`'s
    /// presence: whether Fresco's mpvpaper can mark itself show-on-lock and
    /// so keep playing behind cosmic-greeter's own lock surface.
    Cosmic {
        live: bool,
    },
    Kde,
    Gnome,
    Cinnamon,
    Mate,
    Xfce,
    Deepin,
    /// A layer-shell Wayland compositor with no known desktop environment —
    /// Sway, Hyprland, niri, river, labwc, Wayfire, ….
    Wlroots,
    /// An X11 window manager with no known desktop environment.
    X11Wm,
    /// Neither a recognised desktop nor a session Fresco can safely drive a
    /// locker on (e.g. a Wayland compositor with no session-lock protocol).
    Unsupported,
}

impl HostKind {
    /// Stable id — exactly [`crate::ipc::LockStatus::host`]'s vocabulary.
    /// [`HostKind::X11Wm`] is reported as `"x11"`, not `"x11wm"`: the
    /// IPC-level id names the session type a person recognizes, not this
    /// enum's Rust identifier.
    pub fn id(&self) -> &'static str {
        match self {
            HostKind::Cosmic { .. } => "cosmic",
            HostKind::Kde => "kde",
            HostKind::Gnome => "gnome",
            HostKind::Cinnamon => "cinnamon",
            HostKind::Mate => "mate",
            HostKind::Xfce => "xfce",
            HostKind::Deepin => "deepin",
            HostKind::Wlroots => "wlroots",
            HostKind::X11Wm => "x11",
            HostKind::Unsupported => "unsupported",
        }
    }

    /// Whether this host's real lock screen shows at least a still frame of
    /// Fresco's wallpaper today — [`crate::ipc::LockStatus::still_frame`], the
    /// datum the Lock Screen page uses to tell "still frame only" from
    /// "nothing reaches the lock screen yet".
    ///
    /// Exhaustive on purpose (no `_` arm): a host that gains a still-frame
    /// writer must be flipped here, in the one place the GUI's claim comes
    /// from, and a new [`HostKind`] cannot compile without being decided.
    /// [`capabilities`] reads this rather than keeping a second table.
    /// `Unsupported` has no still-frame writer, so its lock screen keeps the
    /// desktop's own background.
    pub fn shows_still_frame(&self) -> bool {
        match self {
            // Live video / widgets hosts (Cosmic's still frame via
            // `cosmic_bg` is unconditional, live layer or not).
            HostKind::Cosmic { .. } | HostKind::Kde | HostKind::Wlroots | HostKind::X11Wm => true,
            // Still-frame hosts: the `overview` background sync (GNOME,
            // Cinnamon, MATE), the screensaver-theme route (Xfce) and, since
            // issue #37, `dde_lock` (Deepin: a still set as the greeter
            // background through Appearance1).
            HostKind::Gnome
            | HostKind::Cinnamon
            | HostKind::Mate
            | HostKind::Xfce
            | HostKind::Deepin => true,
            HostKind::Unsupported => false,
        }
    }
}

/// Everything [`classify`] needs to decide a [`HostKind`] — gathered by
/// [`detect`] in one place so `classify` itself stays a pure function over
/// plain data, testable with no display server, D-Bus, or environment
/// variables at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInputs {
    /// `XDG_SESSION_TYPE` verbatim (`"wayland"` / `"x11"` / anything else).
    pub session_type: Option<String>,
    /// `XDG_CURRENT_DESKTOP` verbatim — a colon-separated, case-insensitive
    /// list (e.g. `"ubuntu:GNOME"`, `"X-Cinnamon"`).
    pub current_desktop: Option<String>,
    /// `zwlr_layer_shell_v1` advertised by the compositor.
    pub has_layer_shell: bool,
    /// `ext_session_lock_manager_v1` advertised by the compositor.
    pub has_session_lock: bool,
    /// `cosmic_session_lock_layer_manager_v1` advertised by the compositor.
    pub has_cosmic_lock_layer: bool,
}

/// Pure host classification: [`HostInputs`] in, a [`HostKind`] out.
///
/// Order mirrors `capability::classify`'s own shape: a recognised desktop
/// name wins first, regardless of session type — GNOME, Cinnamon, MATE,
/// Xfce, KDE and Deepin all run on both X11 and Wayland sessions, and the
/// locker each of those hosts wraps (`loginctl lock-session`, or KDE's own
/// greeter) already handles whichever one is live. Only once nothing names a
/// known desktop do we fall back to a bare session-type-plus-protocol guess:
/// a Wayland session needs BOTH `zwlr_layer_shell_v1` (somewhere to draw)
/// AND `ext_session_lock_manager_v1` (somewhere to lock) before it is worth
/// calling [`HostKind::Wlroots`] — either alone is not enough to fail-close
/// safely, so that case falls through to [`HostKind::Unsupported`] instead.
pub fn classify(inputs: &HostInputs) -> HostKind {
    let segments: Vec<String> = inputs
        .current_desktop
        .as_deref()
        .unwrap_or("")
        .split(':')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    let has = |seg: &str| segments.iter().any(|s| s == seg);
    let contains = |needle: &str| segments.iter().any(|s| s.contains(needle));

    if contains("cosmic") {
        return HostKind::Cosmic {
            live: inputs.has_cosmic_lock_layer,
        };
    }
    if has("kde") {
        return HostKind::Kde;
    }
    if has("mate") {
        return HostKind::Mate;
    }
    if has("xfce") {
        return HostKind::Xfce;
    }
    if contains("deepin") || has("dde") {
        return HostKind::Deepin;
    }
    if contains("cinnamon") {
        return HostKind::Cinnamon;
    }
    if contains("gnome") {
        return HostKind::Gnome;
    }

    match inputs.session_type.as_deref() {
        Some("wayland") if inputs.has_layer_shell && inputs.has_session_lock => HostKind::Wlroots,
        Some("x11") => HostKind::X11Wm,
        _ => HostKind::Unsupported,
    }
}

/// Detect the live session's [`HostKind`]: environment plus one Wayland
/// registry roundtrip for the globals [`classify`] needs
/// (`capability::probe_wayland_globals`) — mirroring how
/// `capability::detect` layers a real registry probe over the same kind of
/// desktop-name heuristic this module's [`classify`] encodes.
pub fn detect() -> HostKind {
    let globals = crate::capability::probe_wayland_globals().unwrap_or_default();
    let inputs = HostInputs {
        session_type: std::env::var("XDG_SESSION_TYPE").ok(),
        current_desktop: std::env::var("XDG_CURRENT_DESKTOP").ok(),
        has_layer_shell: globals.layer_shell,
        has_session_lock: globals.session_lock_manager,
        has_cosmic_lock_layer: globals.cosmic_lock_layer_manager,
    };
    classify(&inputs)
}

/// Where the lock widget engine paints while the session is locked.
#[derive(Debug, Clone, PartialEq)]
pub enum LockTargets {
    /// Nothing to draw into on this host right now.
    None,
    /// The live desktop wallpaper surface itself — COSMIC's show-on-lock
    /// layer, where Fresco's own mpvpaper stays on screen through the lock
    /// with no separate process to hand widgets to.
    Desktop,
    /// One mpv IPC socket per output, driving an out-of-process renderer
    /// (wlroots' swaylock-plugin child, or the X11 saver reporting back via
    /// `crate::ipc::Request::LockNotify`).
    Sockets(Vec<LockSocket>),
    /// A directory of widget bitmaps a foreign process polls — KDE's Plasma
    /// wallpaper plugin reading PNGs `frescod` refreshes here.
    LayerFiles(PathBuf),
}

/// Everything a [`LockHost`] needs to do its job, borrowed for the duration
/// of one call rather than owned — hosts have no state of their own beyond
/// which [`HostKind`] they are.
pub struct HostCtx<'a> {
    pub config: &'a crate::config::Config,
    pub outputs: &'a [crate::daemon::widgets::OutputGeom],
    /// `$XDG_RUNTIME_DIR/fresco` — where a host that hands widgets to a
    /// foreign process (KDE's plugin) or to a child renderer (wlroots'
    /// swaylock-plugin) reads or writes them.
    pub runtime_dir: PathBuf,
}

/// What a successful [`LockHost::lock`] leaves behind: a child process to
/// supervise, if Fresco spawned one directly (`None` for every host in this
/// file — `loginctl lock-session` asks logind to do the locking and returns,
/// it is not itself the locker), and where the lock widget engine should
/// draw.
pub struct RunningHost {
    pub child: Option<std::process::Child>,
    pub targets: LockTargets,
}

/// One desktop's lock-screen adapter. Every implementation is a client,
/// plugin, or background layer *of* that desktop's own, already-audited
/// locker — never a replacement for it (`docs/plan-lock-screen.md` §5).
pub trait LockHost {
    fn kind(&self) -> HostKind;

    /// Lock now. Must end in a locked session or return `Err` — never leave
    /// the user unlocked while claiming success.
    fn lock(&self, ctx: &HostCtx) -> Result<RunningHost, String>;

    /// Where the lock widget engine draws while the session is locked by the
    /// DE itself (not by [`LockHost::lock`]): `Cosmic { live: true }` →
    /// [`LockTargets::Desktop`], [`HostKind::Kde`] →
    /// `LockTargets::LayerFiles(runtime_dir.join("lock"))`, everything else
    /// → [`LockTargets::None`].
    fn targets_while_locked(&self, ctx: &HostCtx) -> LockTargets;

    fn setup(&self, _ctx: &HostCtx) -> Result<(), String> {
        Ok(())
    }
    fn undo(&self, _ctx: &HostCtx) -> Result<(), String> {
        Ok(())
    }
    fn setup_state(&self, _ctx: &HostCtx) -> LockSetupState {
        LockSetupState::NotNeeded
    }
    fn notes(&self, _ctx: &HostCtx) -> Vec<String> {
        Vec::new()
    }
}

/// Longest [`loginctl_lock_session`] waits for `loginctl` to exit before
/// treating it as hung and failing loudly instead of blocking the caller
/// forever. `loginctl lock-session` only sends one D-Bus method call and
/// normally returns in well under a second, so this bound is generous, not
/// tight.
const LOGINCTL_TIMEOUT: Duration = Duration::from_secs(5);

/// Shared `lock()` body for every host whose own locker is reached through
/// `loginctl lock-session` — [`LogindHost`] (GNOME, Cinnamon, MATE, Xfce,
/// Deepin, and the `Unsupported` catch-all, per
/// `docs/plan-lock-screen.md` §4.6/§4.2's fallback chain), [`CosmicHost`],
/// and KDE's host (`hosts::kde`, wave 2). One implementation so the
/// process-spawn, bounded-wait, and stderr-on-failure handling can never
/// drift between call sites — see `battery.rs`'s module doc for why this
/// codebase treats that kind of duplication as a bug waiting to happen, not
/// a style preference.
///
/// A bounded wait via `try_wait` polling, not a plain blocking `wait`: a
/// wedged or missing `loginctl` must fail loudly within
/// [`LOGINCTL_TIMEOUT`], never hang the daemon's command loop indefinitely.
/// `loginctl`'s exit status is the only success signal — a non-zero exit
/// (logind refused, no active session, `loginctl` missing) is always an
/// `Err`, never papered over as `Ok`, matching [`LockHost::lock`]'s own
/// "never leave the user unlocked while claiming success" invariant.
///
/// Shells out rather than talking to logind over D-Bus directly, matching
/// `userinfo.rs`/`mpris.rs`/`daemon::dde`'s own "no D-Bus crate" convention
/// — `loginctl` ships with systemd on every host this feature targets.
///
/// `pub(crate)`, not private: `daemon::mod`'s own `Request::Lock` handling
/// falls back to this exact call when `host_for(kind).lock(ctx)` fails (e.g.
/// the wlroots/X11 hosts' fallback chains, still stubs as of this contract
/// wave) — see its `Lock` request-handling doc comment. The trait shape
/// (`LockHost`) is unchanged; this is a visibility-only fix so that fallback
/// does not need a second, duplicate spawn-and-wait implementation.
pub(crate) fn loginctl_lock_session() -> Result<RunningHost, String> {
    let mut child = Command::new("loginctl")
        .arg("lock-session")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to run loginctl lock-session: {e}"))?;

    let deadline = Instant::now() + LOGINCTL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(RunningHost {
                        child: None,
                        targets: LockTargets::None,
                    });
                }
                let mut stderr = String::new();
                if let Some(mut s) = child.stderr.take() {
                    use std::io::Read;
                    let _ = s.read_to_string(&mut stderr);
                }
                return Err(format!(
                    "loginctl lock-session failed ({status}): {}",
                    stderr.trim()
                ));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "loginctl lock-session timed out after {LOGINCTL_TIMEOUT:?}"
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("failed to wait on loginctl lock-session: {e}")),
        }
    }
}

/// Every desktop with no Fresco-specific lock-screen integration yet, and no
/// special relationship to a lock-related Wayland global either: GNOME,
/// Cinnamon, MATE, Xfce, Deepin, and the catch-all [`HostKind::Unsupported`]
/// (`docs/plan-lock-screen.md` §4.6). `loginctl lock-session` is a plain
/// systemd-logind call with no desktop-specific behaviour, so one struct
/// covers all of them; [`targets_while_locked`](LockHost::targets_while_locked)
/// is always [`LockTargets::None`] because none of these hosts have a
/// Fresco-owned surface to paint into while their own locker is up — the
/// still-frame wallpaper sync each of them gets instead
/// (`daemon::overview`/`daemon::cinnamon_bg`/`daemon::dde`) is separate,
/// existing machinery, not this trait.
pub struct LogindHost {
    kind: HostKind,
}

impl LogindHost {
    /// `kind` should be one of the hosts this struct documents itself as
    /// covering; nothing here enforces that beyond [`host_for`] only ever
    /// constructing one that way.
    pub fn new(kind: HostKind) -> Self {
        LogindHost { kind }
    }
}

impl LockHost for LogindHost {
    fn kind(&self) -> HostKind {
        self.kind
    }

    fn lock(&self, _ctx: &HostCtx) -> Result<RunningHost, String> {
        loginctl_lock_session()
    }

    fn targets_while_locked(&self, _ctx: &HostCtx) -> LockTargets {
        LockTargets::None
    }
}

/// COSMIC 1.9+. `lock()` still goes through `loginctl lock-session` like
/// every other host in this file — replacing cosmic-greeter itself was
/// rejected (`docs/plan-lock-screen.md` §4.1: cosmic-session hardcodes and
/// restarts it, and a lock race during a swap would be a real lockout risk),
/// so wave 2 here is only ever "ask logind to lock", the same as
/// [`LogindHost`]. What's COSMIC-specific is
/// [`targets_while_locked`](LockHost::targets_while_locked): when `live`
/// (this session's cosmic-comp advertises
/// `cosmic_session_lock_layer_manager_v1`), Fresco's own mpvpaper marks its
/// surface show-on-lock and stays on screen through the lock, so the lock
/// widget engine paints directly onto that live desktop surface. When not
/// `live`, there is nothing to paint into yet — the still-frame fallback
/// (`daemon::cosmic_bg`) is unconditional, separate machinery, already
/// shipped in wave 1.
pub struct CosmicHost {
    live: bool,
}

impl CosmicHost {
    pub fn new(live: bool) -> Self {
        CosmicHost { live }
    }
}

impl LockHost for CosmicHost {
    fn kind(&self) -> HostKind {
        HostKind::Cosmic { live: self.live }
    }

    fn lock(&self, _ctx: &HostCtx) -> Result<RunningHost, String> {
        loginctl_lock_session()
    }

    fn targets_while_locked(&self, _ctx: &HostCtx) -> LockTargets {
        if self.live {
            LockTargets::Desktop
        } else {
            LockTargets::None
        }
    }
}

/// What Fresco can put on a host's lock screen — the source of the
/// `Live video` / `Widgets` / `Still frame` tags the settings page shows
/// ([`crate::ipc::LockStatus`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockCaps {
    /// The wallpaper keeps playing as video behind the lock screen.
    pub live_video: bool,
    /// Fresco's own widgets are drawn on the lock screen.
    pub widgets: bool,
    /// At least a still frame of the wallpaper is what the lock screen shows.
    /// True wherever `live_video` is, since those hosts fall back to a still
    /// on battery; the point of the flag is the hosts where it is *all* there
    /// is.
    pub still_frame: bool,
}

/// The capability matrix, per host. Live video and widgets need a real
/// surface to draw on — see `docs/plan-lock-screen.md` §4: COSMIC needs the
/// show-on-lock layer specifically (not just "is COSMIC"). The still frame is
/// [`HostKind::shows_still_frame`], the one place that says which hosts have a
/// writer handing the desktop's own locker a picture.
pub fn capabilities(kind: HostKind) -> LockCaps {
    let live = matches!(
        kind,
        HostKind::Cosmic { live: true } | HostKind::Wlroots | HostKind::X11Wm | HostKind::Kde
    );
    LockCaps {
        live_video: live,
        widgets: live,
        still_frame: kind.shows_still_frame(),
    }
}

/// Construct the [`LockHost`] for an already-[`classify`]d [`HostKind`].
pub fn host_for(kind: HostKind) -> Box<dyn LockHost> {
    match kind {
        HostKind::Cosmic { live } => Box::new(CosmicHost::new(live)),
        HostKind::Kde => Box::new(kde::KdeHost),
        HostKind::Wlroots => Box::new(wlroots::WlrootsHost),
        HostKind::X11Wm => Box::new(x11::X11Host),
        other => Box::new(LogindHost::new(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(session_type: Option<&str>, current_desktop: Option<&str>) -> HostInputs {
        HostInputs {
            session_type: session_type.map(str::to_string),
            current_desktop: current_desktop.map(str::to_string),
            has_layer_shell: false,
            has_session_lock: false,
            has_cosmic_lock_layer: false,
        }
    }

    // -- classify(): known desktops, any session type ------------------------

    #[test]
    fn classify_gnome() {
        for d in ["pop:GNOME", "ubuntu:GNOME", "GNOME", "gnome"] {
            assert_eq!(
                classify(&inputs(Some("wayland"), Some(d))),
                HostKind::Gnome,
                "{d}"
            );
            // GNOME on Xorg exists too; the desktop name wins either way.
            assert_eq!(
                classify(&inputs(Some("x11"), Some(d))),
                HostKind::Gnome,
                "{d} (x11)"
            );
        }
    }

    #[test]
    fn classify_cinnamon() {
        for d in ["X-Cinnamon", "Cinnamon", "cinnamon"] {
            assert_eq!(
                classify(&inputs(Some("wayland"), Some(d))),
                HostKind::Cinnamon,
                "{d}"
            );
        }
    }

    #[test]
    fn classify_kde() {
        for d in ["KDE", "kde", "plasma:KDE"] {
            assert_eq!(
                classify(&inputs(Some("wayland"), Some(d))),
                HostKind::Kde,
                "{d}"
            );
            assert_eq!(
                classify(&inputs(Some("x11"), Some(d))),
                HostKind::Kde,
                "{d} (x11)"
            );
        }
    }

    #[test]
    fn classify_mate() {
        for d in ["MATE", "mate", "X-Generic:MATE"] {
            assert_eq!(
                classify(&inputs(Some("x11"), Some(d))),
                HostKind::Mate,
                "{d}"
            );
        }
        // Substring only (no whole-segment match) must NOT trigger MATE,
        // mirroring `capability::classify_mate`'s own guard.
        for d in ["ultimate", "mate-ish"] {
            assert_ne!(
                classify(&inputs(Some("x11"), Some(d))),
                HostKind::Mate,
                "{d}"
            );
        }
    }

    #[test]
    fn classify_xfce() {
        for d in ["XFCE", "xfce", "XFCE:GNOME"] {
            assert_eq!(
                classify(&inputs(Some("x11"), Some(d))),
                HostKind::Xfce,
                "{d}"
            );
        }
    }

    #[test]
    fn classify_deepin() {
        for d in ["Deepin", "deepin", "DDE", "dde", "X-Deepin", "Deepin:GNOME"] {
            assert_eq!(
                classify(&inputs(Some("wayland"), Some(d))),
                HostKind::Deepin,
                "{d}"
            );
        }
    }

    #[test]
    fn classify_cosmic_with_and_without_the_lock_layer_global() {
        for d in ["COSMIC", "cosmic", "custom:COSMIC", "COSMIC:GNOME"] {
            let mut i = inputs(Some("wayland"), Some(d));
            i.has_cosmic_lock_layer = true;
            assert_eq!(classify(&i), HostKind::Cosmic { live: true }, "{d} live");

            i.has_cosmic_lock_layer = false;
            assert_eq!(
                classify(&i),
                HostKind::Cosmic { live: false },
                "{d} not live"
            );
        }
    }

    #[test]
    fn classify_mixed_case_and_colon_lists() {
        assert_eq!(
            classify(&inputs(Some("wayland"), Some("ubuntu:GNOME"))),
            HostKind::Gnome
        );
        assert_eq!(
            classify(&inputs(Some("wayland"), Some("  cosmic  :GNOME"))),
            HostKind::Cosmic { live: false }
        );
        assert_eq!(
            classify(&inputs(Some("x11"), Some("kde:unknownthing"))),
            HostKind::Kde
        );
    }

    // -- classify(): Wayland/X11 fallbacks when no DE is recognised ----------

    #[test]
    fn classify_wayland_fallback_needs_both_protocols() {
        let mut i = inputs(Some("wayland"), None);
        assert_eq!(classify(&i), HostKind::Unsupported, "neither protocol");

        i.has_layer_shell = true;
        assert_eq!(classify(&i), HostKind::Unsupported, "layer-shell only");

        i.has_layer_shell = false;
        i.has_session_lock = true;
        assert_eq!(classify(&i), HostKind::Unsupported, "session-lock only");

        i.has_layer_shell = true;
        assert_eq!(classify(&i), HostKind::Wlroots, "both protocols");
    }

    #[test]
    fn classify_x11_fallback_with_no_known_de() {
        assert_eq!(classify(&inputs(Some("x11"), None)), HostKind::X11Wm);
        assert_eq!(classify(&inputs(Some("x11"), Some("i3"))), HostKind::X11Wm);
    }

    #[test]
    fn classify_unknown_session_type_is_unsupported() {
        assert_eq!(classify(&inputs(None, None)), HostKind::Unsupported);
        assert_eq!(
            classify(&inputs(Some("something-else"), None)),
            HostKind::Unsupported
        );
    }

    // -- HostKind::id() --------------------------------------------------------

    #[test]
    fn host_kind_ids() {
        assert_eq!(HostKind::Cosmic { live: true }.id(), "cosmic");
        assert_eq!(HostKind::Cosmic { live: false }.id(), "cosmic");
        assert_eq!(HostKind::Kde.id(), "kde");
        assert_eq!(HostKind::Gnome.id(), "gnome");
        assert_eq!(HostKind::Cinnamon.id(), "cinnamon");
        assert_eq!(HostKind::Mate.id(), "mate");
        assert_eq!(HostKind::Xfce.id(), "xfce");
        assert_eq!(HostKind::Deepin.id(), "deepin");
        assert_eq!(HostKind::Wlroots.id(), "wlroots");
        assert_eq!(HostKind::X11Wm.id(), "x11");
        assert_eq!(HostKind::Unsupported.id(), "unsupported");
    }

    // -- HostKind::shows_still_frame() ---------------------------------------

    #[test]
    fn shows_still_frame_follows_what_each_host_actually_writes() {
        // Every host that can play live video shows at least a frame of it.
        for k in [
            HostKind::Cosmic { live: true },
            HostKind::Wlroots,
            HostKind::X11Wm,
            HostKind::Kde,
        ] {
            assert!(k.shows_still_frame(), "{k:?}");
        }
        // The still-frame-only hosts, COSMIC without the show-on-lock layer
        // included (its `cosmic_bg` sync does not depend on it). Deepin is
        // one since issue #37 (`dde_lock`).
        for k in [
            HostKind::Cosmic { live: false },
            HostKind::Gnome,
            HostKind::Cinnamon,
            HostKind::Mate,
            HostKind::Xfce,
            HostKind::Deepin,
        ] {
            assert!(k.shows_still_frame(), "{k:?}");
        }
        // No still-frame writer: the desktop keeps its own lock background.
        assert!(!HostKind::Unsupported.shows_still_frame());
    }

    // -- LogindHost / CosmicHost: the pure half (never calls loginctl) ------

    fn ctx<'a>(
        config: &'a crate::config::Config,
        outputs: &'a [crate::daemon::widgets::OutputGeom],
    ) -> HostCtx<'a> {
        HostCtx {
            config,
            outputs,
            runtime_dir: PathBuf::from("/run/user/1000/fresco"),
        }
    }

    #[test]
    fn logind_host_reports_its_own_kind_and_paints_nowhere_while_locked() {
        let config = crate::config::Config::default();
        for kind in [
            HostKind::Gnome,
            HostKind::Cinnamon,
            HostKind::Mate,
            HostKind::Xfce,
            HostKind::Deepin,
            HostKind::Unsupported,
        ] {
            let host = LogindHost::new(kind);
            assert_eq!(host.kind(), kind);
            assert_eq!(
                host.targets_while_locked(&ctx(&config, &[])),
                LockTargets::None
            );
            assert_eq!(
                host.setup_state(&ctx(&config, &[])),
                LockSetupState::NotNeeded
            );
            assert!(host.notes(&ctx(&config, &[])).is_empty());
        }
    }

    #[test]
    fn cosmic_host_targets_desktop_only_when_live() {
        let config = crate::config::Config::default();
        let live = CosmicHost::new(true);
        assert_eq!(live.kind(), HostKind::Cosmic { live: true });
        assert_eq!(
            live.targets_while_locked(&ctx(&config, &[])),
            LockTargets::Desktop
        );

        let not_live = CosmicHost::new(false);
        assert_eq!(not_live.kind(), HostKind::Cosmic { live: false });
        assert_eq!(
            not_live.targets_while_locked(&ctx(&config, &[])),
            LockTargets::None
        );
    }

    #[test]
    fn host_for_maps_every_kind_to_the_matching_host() {
        let config = crate::config::Config::default();
        let c = ctx(&config, &[]);
        assert_eq!(
            host_for(HostKind::Cosmic { live: true }).kind(),
            HostKind::Cosmic { live: true }
        );
        assert_eq!(host_for(HostKind::Kde).kind(), HostKind::Kde);
        assert_eq!(host_for(HostKind::Gnome).kind(), HostKind::Gnome);
        assert_eq!(host_for(HostKind::Cinnamon).kind(), HostKind::Cinnamon);
        assert_eq!(host_for(HostKind::Mate).kind(), HostKind::Mate);
        assert_eq!(host_for(HostKind::Xfce).kind(), HostKind::Xfce);
        assert_eq!(host_for(HostKind::Deepin).kind(), HostKind::Deepin);
        assert_eq!(host_for(HostKind::Wlroots).kind(), HostKind::Wlroots);
        assert_eq!(host_for(HostKind::X11Wm).kind(), HostKind::X11Wm);
        assert_eq!(
            host_for(HostKind::Unsupported).kind(),
            HostKind::Unsupported
        );

        // KDE's `targets_while_locked` always names the same layer-files
        // directory under this ctx's runtime_dir, regardless of setup state.
        assert_eq!(
            host_for(HostKind::Kde).targets_while_locked(&c),
            LockTargets::LayerFiles(PathBuf::from("/run/user/1000/fresco/lock"))
        );
    }

    #[test]
    fn capabilities_matrix_per_host() {
        let caps = |live_video, widgets, still_frame| LockCaps {
            live_video,
            widgets,
            still_frame,
        };
        // Live hosts: everything, a still included.
        for kind in [
            HostKind::Cosmic { live: true },
            HostKind::Kde,
            HostKind::Wlroots,
            HostKind::X11Wm,
        ] {
            assert_eq!(capabilities(kind), caps(true, true, true), "{kind:?}");
        }
        // Still-frame hosts: something hands their locker a picture (see
        // `HostKind::shows_still_frame`). Deepin is one since issue #37.
        for kind in [
            HostKind::Cosmic { live: false },
            HostKind::Gnome,
            HostKind::Cinnamon,
            HostKind::Mate,
            HostKind::Xfce,
            HostKind::Deepin,
        ] {
            assert_eq!(capabilities(kind), caps(false, false, true), "{kind:?}");
        }
        // Nothing sets what this one's locker shows.
        assert_eq!(
            capabilities(HostKind::Unsupported),
            caps(false, false, false)
        );
    }
}
