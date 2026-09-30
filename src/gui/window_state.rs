//! Remembered size of the main window.
//!
//! GUI-only state, stored next to `view.json` for the same reason it lives
//! there and not in `Config`: `config.toml` is shared with the daemon, and a
//! window geometry is not configuration.
//!
//! Only size and the maximized flag are kept. GTK4 gives a client no way to
//! place a toplevel (that is the compositor's call on Wayland, and X11 support
//! for it was removed along with `gtk_window_move`), so position cannot be
//! restored and is deliberately not recorded.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Size the window opens at when nothing usable is saved.
pub const DEFAULT_SIZE: (i32, i32) = (880, 660);
/// The window's `size_request`; a saved size below it is meaningless.
pub const MIN_SIZE: (i32, i32) = (420, 480);
/// Anything beyond this is not a window size, it is a corrupt file.
const SANE_MAX: i32 = 16_384;
/// Room left around a restored window: horizontal, and vertical (panel and
/// title bar). GTK4 cannot tell which monitor the window will land on, so the
/// restore is capped by the largest one.
const MARGIN: (i32, i32) = (16, 64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowState {
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            width: DEFAULT_SIZE.0,
            height: DEFAULT_SIZE.1,
            maximized: false,
        }
    }
}

fn state_path() -> PathBuf {
    super::library::library_dir().join("window.json")
}

/// Read the saved window state, falling back to the default.
///
/// Infallible by design, like `library::load_view`: a missing or unreadable
/// file has one right answer, the default size, and none worth failing a
/// launch over.
pub fn load() -> WindowState {
    fs::read_to_string(state_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Persist the window state atomically, like the other GUI stores.
pub fn save(state: &WindowState) -> Result<()> {
    let path = state_path();
    let dir = path.parent().map(PathBuf::from).unwrap_or_default();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(state)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// The size to open at, given what was saved and the largest connected monitor
/// (logical pixels, `None` if the display could not be queried).
///
/// Out-of-range values mean a corrupt or hand-edited file and fall back to the
/// default rather than being clamped into something the user never chose.
pub fn restore_size(saved: &WindowState, largest_monitor: Option<(i32, i32)>) -> (i32, i32) {
    let in_range = |v: i32| (1..=SANE_MAX).contains(&v);
    if !in_range(saved.width) || !in_range(saved.height) {
        return DEFAULT_SIZE;
    }
    let (max_w, max_h) = match largest_monitor {
        Some((mw, mh)) => (
            (mw - MARGIN.0).max(MIN_SIZE.0),
            (mh - MARGIN.1).max(MIN_SIZE.1),
        ),
        None => (SANE_MAX, SANE_MAX),
    };
    (
        saved.width.clamp(MIN_SIZE.0, max_w),
        saved.height.clamp(MIN_SIZE.1, max_h),
    )
}

/// Whether a live window size may be persisted.
///
/// Refuses non-positive values (GTK reports -1 for "unset") and anything larger
/// than the monitor the window is on: a window that has ballooned past the
/// screen (issue #31) must never become the size it reopens at.
pub fn size_is_saveable(width: i32, height: i32, monitor: Option<(i32, i32)>) -> bool {
    if width <= 0 || height <= 0 {
        return false;
    }
    match monitor {
        Some((mw, mh)) => width <= mw && height <= mh,
        None => width <= SANE_MAX && height <= SANE_MAX,
    }
}

/// The state to write on close. A maximized window reports the screen-sized
/// geometry, which is not the size to come back to after un-maximizing, so
/// then only the flag changes and the previous un-maximized size is kept.
pub fn next_state(
    prev: &WindowState,
    width: i32,
    height: i32,
    maximized: bool,
    monitor: Option<(i32, i32)>,
) -> WindowState {
    if maximized {
        return WindowState {
            maximized: true,
            ..prev.clone()
        };
    }
    if size_is_saveable(width, height, monitor) {
        WindowState {
            width,
            height,
            maximized: false,
        }
    } else {
        WindowState {
            maximized: false,
            ..prev.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(width: i32, height: i32, maximized: bool) -> WindowState {
        WindowState {
            width,
            height,
            maximized,
        }
    }

    #[test]
    fn restore_keeps_a_sane_size() {
        assert_eq!(
            restore_size(&ws(1000, 700, false), Some((1920, 1080))),
            (1000, 700)
        );
    }

    #[test]
    fn restore_falls_back_on_garbage() {
        for bad in [ws(0, 600, false), ws(-1, -1, false), ws(99_999, 500, false)] {
            assert_eq!(restore_size(&bad, Some((1920, 1080))), DEFAULT_SIZE);
        }
    }

    #[test]
    fn restore_clamps_to_min_and_monitor() {
        assert_eq!(restore_size(&ws(100, 100, false), None), MIN_SIZE);
        assert_eq!(
            restore_size(&ws(5000, 4000, false), Some((1920, 1080))),
            (1920 - MARGIN.0, 1080 - MARGIN.1)
        );
    }

    #[test]
    fn restore_never_goes_below_min_on_a_tiny_monitor() {
        assert_eq!(
            restore_size(&ws(800, 600, false), Some((400, 300))),
            MIN_SIZE
        );
    }

    #[test]
    fn save_guard_rejects_oversize_and_unset() {
        let mon = Some((1920, 1080));
        assert!(size_is_saveable(880, 660, mon));
        assert!(size_is_saveable(1920, 1080, mon));
        assert!(!size_is_saveable(1921, 700, mon));
        assert!(!size_is_saveable(800, 2000, mon));
        assert!(!size_is_saveable(-1, 660, mon));
        assert!(!size_is_saveable(0, 0, None));
        assert!(size_is_saveable(900, 700, None));
    }

    #[test]
    fn maximized_keeps_previous_size() {
        let prev = ws(900, 700, false);
        assert_eq!(
            next_state(&prev, 1920, 1080, true, Some((1920, 1080))),
            ws(900, 700, true)
        );
    }

    #[test]
    fn unmaximized_saves_new_size_and_clears_flag() {
        let prev = ws(900, 700, true);
        assert_eq!(
            next_state(&prev, 1000, 650, false, Some((1920, 1080))),
            ws(1000, 650, false)
        );
    }

    #[test]
    fn ballooned_window_is_not_persisted() {
        let prev = ws(900, 700, false);
        assert_eq!(
            next_state(&prev, 3000, 2000, false, Some((1920, 1080))),
            prev
        );
    }

    #[test]
    fn corrupt_json_is_default() {
        let parsed: Option<WindowState> = serde_json::from_str("nope").ok();
        assert!(parsed.is_none());
        let partial: WindowState = serde_json::from_str(r#"{"maximized":true}"#).unwrap();
        assert_eq!(partial, ws(880, 660, true));
    }
}
