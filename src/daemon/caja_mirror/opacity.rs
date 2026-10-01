//! Deepin: keep dde-shell's desktop window invisible to the compositor.
//!
//! # Why
//!
//! The mirror copies DDE's icons from the window's *offscreen* pixmap, so the
//! window itself never needs to be seen — yet every time the user clicks an
//! empty part of the desktop KWin raises it above the wallpaper, and KWin
//! composites on its own schedule: for at least one frame the opaque,
//! key-coloured window sits over the video. That is the flicker. Putting the
//! window back down is a request KWin may or may not honour and is in any case
//! too late for a frame already queued.
//!
//! What stops it at the source is `_NET_WM_WINDOW_OPACITY` = 0 on the DDE
//! client window: KWin then paints nothing for it wherever it is stacked. The
//! Composite pixmap we mirror from is the window's own contents and does not
//! depend on the opacity, so mirroring carries on.
//!
//! # Safety net
//!
//! Left at 0, DDE's desktop would be invisible. So the original value (or its
//! absence) is written to a state file *before* the property is touched, and
//! restored on every way out: normal stop, falling back to restack, and — for
//! a crash — at the next start ([`restore_saved`], reached from
//! `dde::restore`). DDE sets this property itself too (0.99), so a change on
//! the window is watched and put back to 0; a window that keeps being reset is
//! given up on and handed back, leaving the raise-based fallback in charge.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;

use super::Desktop;

/// The EWMH property KWin reads a client's opacity from.
pub(super) const ATOM_NAME: &[u8] = b"_NET_WM_WINDOW_OPACITY";

/// More resets than this inside [`RESET_WINDOW`] and DDE is considered to be
/// fighting us.
const MAX_RESETS: usize = 12;
const RESET_WINDOW: Duration = Duration::from_secs(2);

/// `FRESCO_DDE_MIRROR_OPACITY=off` turns the whole trick off, for comparing
/// behaviour on a tester's machine.
pub(super) fn enabled() -> bool {
    !std::env::var("FRESCO_DDE_MIRROR_OPACITY")
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "no"))
}

/// What a state file records: which window, and what its opacity was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Saved {
    pub window: Window,
    /// `None`: the property was absent.
    pub original: Option<u32>,
}

pub(super) fn format_saved(s: Saved) -> String {
    let original = s
        .original
        .map_or_else(|| "absent".to_string(), |v| v.to_string());
    format!("window={}\noriginal={original}\n", s.window)
}

pub(super) fn parse_saved(text: &str) -> Option<Saved> {
    let mut window = None;
    let mut original = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "window" => window = value.trim().parse::<u32>().ok(),
            "original" => {
                original = match value.trim() {
                    "absent" => Some(None),
                    v => Some(Some(v.parse::<u32>().ok()?)),
                }
            }
            _ => {}
        }
    }
    Some(Saved {
        window: window?,
        original: original?,
    })
}

pub(super) fn state_file() -> PathBuf {
    super::super::dde::state_dir().join("dde-window-opacity")
}

fn save(s: Saved) -> Result<()> {
    let path = state_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(&path, format_saved(s)).with_context(|| format!("writing {}", path.display()))
}

/// The window's opacity property: `None` when absent.
fn read<C: Connection>(conn: &C, window: Window, atom: Atom) -> Result<Option<u32>> {
    let reply = conn
        .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1)?
        .reply()?;
    Ok(reply.value32().and_then(|mut v| v.next()))
}

fn write<C: Connection>(conn: &C, window: Window, atom: Atom, value: Option<u32>) -> Result<()> {
    match value {
        Some(v) => {
            conn.change_property32(PropMode::REPLACE, window, atom, AtomEnum::CARDINAL, &[v])?;
        }
        None => {
            conn.delete_property(window, atom)?;
        }
    }
    Ok(())
}

/// The desktop window while the mirror is hiding it.
pub(super) struct Hider {
    window: Window,
    atom: Atom,
    original: Option<u32>,
    recent: VecDeque<Instant>,
    gave_up: bool,
}

impl Hider {
    /// Save the window's opacity and set it to 0. Fails — leaving the window
    /// untouched — when the state file cannot be written, since then a crash
    /// could not be undone.
    pub(super) fn engage<C: Connection>(conn: &C, atom: Atom, window: Window) -> Result<Hider> {
        let current = read(conn, window, atom)?;
        // A file for this very window means an earlier run died while hiding
        // it: the 0 read now is ours, and the file holds the true original.
        let original = match std::fs::read_to_string(state_file())
            .ok()
            .and_then(|t| parse_saved(&t))
        {
            Some(s) if s.window == window => s.original,
            _ => current,
        };
        save(Saved { window, original })?;
        write(conn, window, atom, Some(0))?;
        Ok(Hider {
            window,
            atom,
            original,
            recent: VecDeque::new(),
            gave_up: false,
        })
    }

    pub(super) fn window(&self) -> Window {
        self.window
    }

    /// The property changed on the desktop window. Put 0 back if it is not 0
    /// — DDE does reset it — unless that is happening so often that the window
    /// is being fought over; then hand it back for good.
    pub(super) fn on_change<C: Connection>(&mut self, conn: &C, now: Instant) {
        if self.gave_up {
            return;
        }
        let Ok(value) = read(conn, self.window, self.atom) else {
            return; // gone; the DestroyNotify deals with it
        };
        if value == Some(0) {
            return;
        }
        self.recent.push_back(now);
        while self
            .recent
            .front()
            .is_some_and(|&t| now.duration_since(t) > RESET_WINDOW)
        {
            self.recent.pop_front();
        }
        if self.recent.len() > MAX_RESETS {
            log::warn!(
                "DDE: the desktop window's opacity keeps being reset; giving up hiding it \
                 (the wallpaper is raised over it instead)"
            );
            self.gave_up = true;
            let _ = write(conn, self.window, self.atom, self.original);
            std::fs::remove_file(state_file()).ok();
            return;
        }
        log::debug!("DDE: the desktop window's opacity was reset ({value:?}); hiding it again");
        // The state file may have been taken away by a restore elsewhere.
        let _ = save(Saved {
            window: self.window,
            original: self.original,
        });
        let _ = write(conn, self.window, self.atom, Some(0));
    }

    /// Put the original opacity back and drop the state file.
    pub(super) fn restore<C: Connection>(&mut self, conn: &C) {
        if !self.gave_up {
            let _ = write(conn, self.window, self.atom, self.original);
        }
        self.gave_up = true;
        std::fs::remove_file(state_file()).ok();
    }

    /// The window is gone and took its property with it; only the file is left.
    pub(super) fn forget(self) {
        std::fs::remove_file(state_file()).ok();
    }
}

/// Undo a hide that an earlier run did not get to undo (a crash, a `kill -9`).
/// Idempotent, and a no-op without a state file, so it is safe at every start
/// and on every desktop. Opens its own connection: no mirror is running when
/// this matters.
pub(super) fn restore_saved() {
    let path = state_file();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Some(saved) = parse_saved(&text) else {
        log::warn!("DDE: unreadable desktop-window state at {}", path.display());
        std::fs::remove_file(&path).ok();
        return;
    };
    let Ok((conn, _)) = x11rb::connect(None) else {
        log::warn!(
            "DDE: an earlier run left the desktop window hidden, and there is no display to \
             restore it on; will try again at the next start"
        );
        return;
    };
    let atom = conn
        .intern_atom(true, ATOM_NAME)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom)
        .filter(|&a| a != x11rb::NONE);
    // Only touch a window that still is DDE's desktop: ids are reused after a
    // DDE restart, and a new window starts without our change anyway.
    let still_dde = conn
        .get_property(
            false,
            saved.window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            0,
            1024,
        )
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|p| Desktop::Dde.matches(&p.value));
    if let (Some(atom), true) = (atom, still_dde) {
        if write(&conn, saved.window, atom, saved.original).is_ok() {
            log::info!("DDE: restored the desktop window's opacity");
        }
        // Wait until the server has applied it before the caller moves on.
        let _ = conn.get_input_focus().map(|c| c.reply());
    }
    std::fs::remove_file(&path).ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_with_and_without_an_original() {
        for s in [
            Saved {
                window: 0x1a00003,
                original: Some(4252017623),
            },
            Saved {
                window: 7,
                original: None,
            },
            Saved {
                window: 9,
                original: Some(0),
            },
        ] {
            assert_eq!(parse_saved(&format_saved(s)), Some(s), "{s:?}");
        }
    }

    #[test]
    fn damaged_state_is_rejected_rather_than_guessed_at() {
        assert_eq!(parse_saved(""), None);
        assert_eq!(parse_saved("window=5\n"), None);
        assert_eq!(parse_saved("original=absent\n"), None);
        assert_eq!(parse_saved("window=x\noriginal=absent\n"), None);
        assert_eq!(parse_saved("window=5\noriginal=banana\n"), None);
        // Unknown keys are ignored, order does not matter.
        assert_eq!(
            parse_saved("future=1\noriginal=absent\nwindow=5\n"),
            Some(Saved {
                window: 5,
                original: None
            })
        );
    }
}
