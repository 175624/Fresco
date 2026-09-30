//! Session capability detection — which wallpaper backend can run here.
//!
//! X11 sessions use the embedded-mpv backend. Wayland sessions are split:
//!  - GNOME/Mutter has no `wlr-layer-shell`, so we fall back to a static frame.
//!  - Cinnamon's muffin *used to* have none either, but as of muffin PR #803
//!    it now implements `zwlr_layer_shell_v1` for every client — so a current
//!    Cinnamon session gets the same live layer-shell backend as everything
//!    else. See `daemon::cinnamon_bg` for the restack this newer muffin needs
//!    (it stacks new BACKGROUND surfaces under old ones, hiding mpvpaper
//!    behind `cinnamon-background-daemon`'s own window unless that daemon is
//!    restarted after mpvpaper comes up).
//!  - Everything else (wlroots, KDE Plasma 6, COSMIC, …) uses the mpvpaper
//!    layer-shell backend for live wallpapers.
//!
//! On Wayland we probe the live registry for `zwlr_layer_shell_v1` ourselves (no
//! external tools) and trust that over the desktop-name heuristic below, which
//! only runs when no Wayland connection could be made at all (so a real probe
//! is impossible) — there we still have to guess, and guessing layer-shell for
//! GNOME or an old Cinnamon means mpvpaper fails outright at login, so both
//! keep defaulting to the static fallback in that fallback path only.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// X11 session — the existing in-process mpv backend.
    X11,
    /// Wayland with a layer-shell compositor — live wallpaper backend.
    WaylandLayerShell,
    /// Wayland on GNOME (no layer-shell) — static-frame fallback.
    WaylandGnomeStatic,
}

impl Capability {
    /// Short stable identifier for logs and diagnostics.
    pub fn id(self) -> &'static str {
        match self {
            Capability::X11 => "x11",
            Capability::WaylandLayerShell => "wayland-layer-shell",
            Capability::WaylandGnomeStatic => "wayland-gnome-static",
        }
    }
}

/// Detect the capability of the current session from the environment.
pub fn detect() -> Capability {
    let session_type = std::env::var("XDG_SESSION_TYPE").ok();
    let wayland_display = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let current_desktop = std::env::var("XDG_CURRENT_DESKTOP").ok();
    let session_desktop = std::env::var("XDG_SESSION_DESKTOP").ok();

    let is_wayland = match session_type.as_deref() {
        Some("wayland") => true,
        Some("x11") => false,
        // Session type unset/unknown: trust WAYLAND_DISPLAY.
        _ => wayland_display,
    };
    if !is_wayland {
        return Capability::X11;
    }

    // Prefer a real registry probe when available.
    if let Some(has_layer) = probe_layer_shell() {
        return if has_layer {
            Capability::WaylandLayerShell
        } else {
            // No layer-shell → treat like GNOME (static fallback) even if we
            // can't identify the compositor by name.
            Capability::WaylandGnomeStatic
        };
    }

    classify(
        session_type.as_deref(),
        wayland_display,
        current_desktop.as_deref().or(session_desktop.as_deref()),
    )
}

/// Pure desktop-name classification, testable without touching the process
/// environment. `detect()` may override this with a layer-shell registry probe.
fn classify(
    session_type: Option<&str>,
    wayland_display: bool,
    current_desktop: Option<&str>,
) -> Capability {
    let is_wayland = match session_type {
        Some("wayland") => true,
        Some("x11") => false,
        // Session type unset/unknown: trust WAYLAND_DISPLAY.
        _ => wayland_display,
    };
    if !is_wayland {
        return Capability::X11;
    }
    // Name-only fallback, used only when no Wayland connection could be made
    // at all (so `probe_layer_shell` returned `None`) — a real Cinnamon
    // session almost always reaches the probe above instead. We cannot tell
    // an old muffin (no layer-shell) from a current one (has it, PR #803)
    // by name alone, and guessing layer-shell for either GNOME or Cinnamon
    // means mpvpaper fails outright at login if we guess wrong — so both
    // still default to the static fallback here.
    if is_gnome(current_desktop) || is_cinnamon_name(current_desktop) {
        Capability::WaylandGnomeStatic
    } else {
        Capability::WaylandLayerShell
    }
}

/// Is this session Deepin's DDE? Its `dde-shell` paints an opaque desktop
/// window that covers other DESKTOP-type windows, so the X11 backend applies
/// extra quirks (see `daemon::dde`).
pub fn is_deepin_dde() -> bool {
    classify_deepin_dde(
        std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref(),
        std::env::var("XDG_SESSION_DESKTOP").ok().as_deref(),
    )
}

/// Pure DDE classification, testable without touching the process environment.
/// Desktop vars are colon-separated lists (e.g. "Deepin:GNOME"); a segment
/// containing "deepin" or equal to "dde" (case-insensitive) means DDE.
fn classify_deepin_dde(current_desktop: Option<&str>, session_desktop: Option<&str>) -> bool {
    [current_desktop, session_desktop]
        .into_iter()
        .flatten()
        .any(|v| {
            v.split(':').any(|seg| {
                let s = seg.trim().to_ascii_lowercase();
                s.contains("deepin") || s == "dde"
            })
        })
}

/// Is this session MATE? Caja, MATE's file manager, draws the desktop — its
/// icons and its own copy of the background — into one opaque full-screen
/// window that covers any other DESKTOP-type window, so a wallpaper stacked the
/// ordinary way is never seen (issue #18). The X11 backend raises the wallpaper
/// above that window instead; see `daemon::dde`.
pub fn is_mate() -> bool {
    classify_mate(
        std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref(),
        std::env::var("XDG_SESSION_DESKTOP").ok().as_deref(),
    )
}

/// Pure MATE classification. A whole segment of the colon-separated list must
/// be `MATE`, so no desktop whose name merely *contains* those letters matches.
fn classify_mate(current_desktop: Option<&str>, session_desktop: Option<&str>) -> bool {
    [current_desktop, session_desktop]
        .into_iter()
        .flatten()
        .any(|v| {
            v.split(':')
                .any(|seg| seg.trim().eq_ignore_ascii_case("mate"))
        })
}

/// Is this session Cinnamon (Linux Mint)? Its muffin compositor has no
/// layer-shell on Wayland and reads its own background schema.
pub fn is_cinnamon() -> bool {
    [
        std::env::var("XDG_CURRENT_DESKTOP").ok(),
        std::env::var("XDG_SESSION_DESKTOP").ok(),
    ]
    .iter()
    .flatten()
    .any(|v| is_cinnamon_name(Some(v)))
}

fn is_cinnamon_name(desktop: Option<&str>) -> bool {
    desktop
        .map(|d| d.to_ascii_lowercase().contains("cinnamon"))
        .unwrap_or(false)
}

fn is_gnome(desktop: Option<&str>) -> bool {
    desktop
        .map(|d| d.to_ascii_lowercase().contains("gnome"))
        .unwrap_or(false)
}

/// Probe the live Wayland registry for `zwlr_layer_shell_v1` — no external tools.
/// `Some(true/false)` when we could talk to the compositor; `None` only if we
/// couldn't connect at all, leaving the decision to the desktop-name heuristic.
fn probe_layer_shell() -> Option<bool> {
    probe_wayland_globals().map(|g| g.layer_shell)
}

/// Which lock-related Wayland globals this compositor advertises, as probed
/// by [`probe_wayland_globals`] in a single registry roundtrip.
///
/// `bool` fields, not `Option`: a successful roundtrip that simply never
/// sees a given global IS the answer "not present" — it is
/// [`probe_wayland_globals`]'s own `Option<WaylandGlobals>` return type that
/// carries "couldn't even connect" (`None`), same contract the private
/// `probe_layer_shell` already had before this struct existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WaylandGlobals {
    /// `zwlr_layer_shell_v1` — live wallpaper backend (see [`Capability`]).
    pub layer_shell: bool,
    /// `ext_session_lock_manager_v1` — lets a client implement a real
    /// session locker (swaylock-plugin, and Fresco's own wave-2b wlroots
    /// lock host); see `daemon::lock::hosts::HostKind::Wlroots`.
    pub session_lock_manager: bool,
    /// `cosmic_session_lock_layer_manager_v1` — cosmic-comp's opt-in
    /// show-on-lock flag for a layer-shell surface; see
    /// `daemon::lock::hosts::HostKind::Cosmic`.
    pub cosmic_lock_layer_manager: bool,
}

/// Probe the live Wayland registry for every lock-related global Fresco cares
/// about, in one roundtrip — no external tools, and no new dependency on a
/// protocol-bindings crate for the two globals besides `zwlr_layer_shell_v1`:
/// telling whether a global is *advertised at all* only ever needs its
/// interface *name* (`wl_registry::Event::Global`'s `interface: String`), so
/// there is nothing here `wayland-client` (already a dependency) cannot do on
/// its own — `wayland-protocols`/`cosmic-protocols`'s lock-layer extension
/// would only earn their keep once something actually *binds* one of these
/// globals to create an object from it, which is wave 2/2b's job, not this
/// probe's.
///
/// `None` only if no Wayland connection could be made at all — same contract
/// the private `probe_layer_shell` (now built on this) always had.
#[cfg(feature = "daemon")]
pub fn probe_wayland_globals() -> Option<WaylandGlobals> {
    use wayland_client::protocol::wl_registry;
    use wayland_client::{Connection, Dispatch, QueueHandle};

    #[derive(Default)]
    struct Probe {
        globals: WaylandGlobals,
    }
    impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
        fn event(
            state: &mut Self,
            _: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global { interface, .. } = event {
                match interface.as_str() {
                    "zwlr_layer_shell_v1" => state.globals.layer_shell = true,
                    "ext_session_lock_manager_v1" => state.globals.session_lock_manager = true,
                    "cosmic_session_lock_layer_manager_v1" => {
                        state.globals.cosmic_lock_layer_manager = true
                    }
                    _ => {}
                }
            }
        }
    }

    let conn = Connection::connect_to_env().ok()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut probe = Probe::default();
    queue.roundtrip(&mut probe).ok()?;
    Some(probe.globals)
}

/// GUI-only builds don't link `wayland-client`; fall back to "couldn't connect".
#[cfg(not(feature = "daemon"))]
pub fn probe_wayland_globals() -> Option<WaylandGlobals> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x11_session_is_x11() {
        assert_eq!(
            classify(Some("x11"), false, Some("pop:GNOME")),
            Capability::X11
        );
        // Session type wins even if WAYLAND_DISPLAY leaks into an X11 session.
        assert_eq!(classify(Some("x11"), true, Some("GNOME")), Capability::X11);
    }

    #[test]
    fn wayland_gnome_is_static() {
        for d in ["pop:GNOME", "ubuntu:GNOME", "GNOME", "gnome"] {
            assert_eq!(
                classify(Some("wayland"), true, Some(d)),
                Capability::WaylandGnomeStatic,
                "desktop {d}"
            );
        }
    }

    #[test]
    fn wayland_cinnamon_is_static_in_the_name_only_fallback() {
        // Only reached when no Wayland connection could be made at all; a
        // live probe (see `daemon::cinnamon_bg`) is what actually tells a
        // current, layer-shell-capable muffin apart from an old one.
        for d in ["X-Cinnamon", "Cinnamon", "cinnamon"] {
            assert_eq!(
                classify(Some("wayland"), true, Some(d)),
                Capability::WaylandGnomeStatic,
                "desktop {d}"
            );
            assert!(is_cinnamon_name(Some(d)));
        }
        assert!(!is_cinnamon_name(Some("GNOME")));
    }

    #[test]
    fn wayland_non_gnome_is_layer_shell() {
        for d in ["Hyprland", "sway", "KDE", "wlroots", "COSMIC", "river"] {
            assert_eq!(
                classify(Some("wayland"), true, Some(d)),
                Capability::WaylandLayerShell,
                "desktop {d}"
            );
        }
    }

    #[test]
    fn deepin_dde_detection() {
        for d in ["Deepin", "deepin", "DDE", "dde", "X-Deepin", "Deepin:GNOME"] {
            assert!(classify_deepin_dde(Some(d), None), "current {d}");
            assert!(classify_deepin_dde(None, Some(d)), "session {d}");
        }
        for d in [
            "GNOME",
            "KDE",
            "pop:GNOME",
            "ubuntu:GNOME",
            "kddesomething",
            "",
        ] {
            assert!(!classify_deepin_dde(Some(d), None), "current {d}");
        }
        assert!(!classify_deepin_dde(None, None));
        // Second var still detected when the first is a non-DDE desktop.
        assert!(classify_deepin_dde(Some("GNOME"), Some("dde")));
    }

    #[test]
    fn mate_detection() {
        for d in ["MATE", "mate", "X-Generic:MATE"] {
            assert!(classify_mate(Some(d), None), "current {d}");
            assert!(classify_mate(None, Some(d)), "session {d}");
        }
        for d in ["GNOME", "X-Cinnamon", "ultimate", "mate-ish", "Deepin", ""] {
            assert!(!classify_mate(Some(d), None), "current {d}");
        }
        assert!(!classify_mate(None, None));
    }

    #[test]
    fn falls_back_to_wayland_display_when_session_type_unset() {
        assert_eq!(
            classify(None, true, Some("sway")),
            Capability::WaylandLayerShell
        );
        assert_eq!(
            classify(None, true, Some("GNOME")),
            Capability::WaylandGnomeStatic
        );
        assert_eq!(classify(None, false, None), Capability::X11);
    }
}
