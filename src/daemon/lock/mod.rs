//! Lock-mode: Fresco's presence on the real system lock screen.
//!
//! See `docs/plan-lock-screen.md` for the full plan. Fresco never handles
//! authentication anywhere in this module or any of its submodules — every
//! host below is a client, plugin, or background layer *of* that desktop's
//! own, already-audited locker, never a replacement for it (plan §5's
//! "fail closed" invariant: a host's `lock()` must end in a locked session
//! or return `Err`, never leave the user unlocked while claiming success).
//!
//! The daemon-side architecture, in the order a lock travels through it:
//!
//! 1. **Host detection** ([`hosts::detect`] / [`hosts::classify`]) — which
//!    desktop is this, and which Wayland lock-related globals does it
//!    advertise? Pure decision table over environment plus one registry
//!    roundtrip, yielding a [`hosts::HostKind`].
//! 2. **Host lock** ([`hosts::LockHost`]) — one adapter per desktop family,
//!    all implemented: `loginctl lock-session` for COSMIC/KDE/the DEs that
//!    lock themselves; `swaylock-plugin` with Fresco's mpvpaper as the
//!    per-output background on wlroots (`hosts::wlroots`); `xsecurelock` (via
//!    `xss-lock`) running the `frescod --saver` module on X11 window
//!    managers; and on KDE a Plasma wallpaper plugin whose `kscreenlockerrc`
//!    setup/undo/refresh is backed up and restored exactly. Each either ends
//!    in a locked session or returns `Err` — never "unlocked but claims
//!    success".
//!    When the desktop locks itself (a DE lock: idle timeout, lid close, the
//!    user's own shortcut) no host `lock()` runs at all; stage 3 notices.
//! 3. **Lock state** ([`state`]) — merges cosmic-greeter's lockfile, logind's
//!    `Lock`/`Unlock`/`PrepareForSleep` signals, the session-bus screensaver
//!    interfaces and the IPC/child/socket signals every host can report (the
//!    validated `LockNotify`, see [`notify`]) into one debounced
//!    Locked/Unlocked answer. A transition swaps the desktop widget set for
//!    the lock set.
//! 4. **Lock widget engine** ([`engine`]) — turns a resolved
//!    `crate::lockscreen::ResolvedLock` into pixels: per-slot bitmaps or one
//!    composed image per output, with COSMIC's greeter panel (or the generic
//!    prompt area) reserved using each output's real HiDPI scale.
//! 5. **Targets** ([`hosts::LockTargets`]) — where those pixels go, decided
//!    per host: the live desktop wallpaper surface (`Desktop`, COSMIC), an
//!    out-of-process renderer's mpv IPC sockets (`Sockets`), or files a
//!    foreign process polls (`LayerFiles`, KDE's plugin).
//!
//! [`preview`] is standalone: a still-image render of the same arrangement
//! for the GUI's "Preview lock screen" button, built so it is *incapable* of
//! calling any locking primitive (`docs/plan-lock-screen.md` §6, §8) rather
//! than merely not calling one today. It bounds the requested size before
//! allocating anything.
//!
//! All stages are wired into each of `daemon::mod`'s run loops (X11,
//! Wayland, static GNOME-style); see each `hosts` submodule's own doc
//! comment for the desktop-specific details.

mod avatar;
pub mod engine;
pub mod hosts;
pub mod notify;
pub mod preview;
pub mod state;
