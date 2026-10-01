//! Live "now playing" status pill + pause/resume toggle, backed by
//! `ipc::request(&Request::Status)`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{glib, glib::ControlFlow};

use crate::ipc::{self, MonitorInfo, Request, StatusReply};
use crate::{t, tf};

use super::lockscreen::{lock_support_of, LockSupport};

thread_local! {
    /// Last `monitors_info` this poll loop saw, so other GUI code (e.g. the
    /// card menu's "move to display" list) can read connected displays
    /// without its own blocking IPC round trip — see `cached_monitors`.
    static LAST_MONITORS: RefCell<Vec<MonitorInfo>> = const { RefCell::new(Vec::new()) };
}

/// Connected displays as of the last status poll. Empty before the first
/// poll lands, or while the daemon isn't running. Never blocks: it's a plain
/// read of state `poll_once` already fetched in the background.
pub(crate) fn cached_monitors() -> Vec<MonitorInfo> {
    LAST_MONITORS.with(|m| m.borrow().clone())
}

thread_local! {
    /// What the last status poll said reaches this desktop's real lock screen,
    /// for the app menu's "Lock Screen…" row — see `cached_lock_support`.
    static LAST_LOCK_SUPPORT: Cell<Option<LockSupport>> = const { Cell::new(None) };
}

/// What reaches the real lock screen on this desktop, as of the last status
/// poll. `None` before the first poll lands, while the daemon isn't running,
/// or when it predates the capability data. Never blocks, like
/// [`cached_monitors`].
pub(crate) fn cached_lock_support() -> Option<LockSupport> {
    LAST_LOCK_SUPPORT.with(Cell::get)
}

/// Callback that shows/hides the service notice from a reachability result.
type NoticeHook = Rc<dyn Fn(bool)>;

thread_local! {
    /// Updates the "service not running" notice from a reachability result.
    /// Set once by `build_service_notice`; the status poll and
    /// `refresh_service_notice` feed it.
    static NOTICE_HOOK: RefCell<Option<NoticeHook>> = const { RefCell::new(None) };
}

fn notify_reachable(reachable: bool) {
    let hook = NOTICE_HOOK.with(|h| h.borrow().clone());
    if let Some(hook) = hook {
        hook(reachable);
    }
}

/// Ask whether the daemon is alive without blocking the GTK thread
/// (`daemon_alive` is a blocking IPC round trip); `done` runs on the GTK
/// thread with the answer.
pub(crate) fn probe_daemon(done: impl FnOnce(bool) + 'static) {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(ipc::daemon_alive());
    });
    glib::spawn_future_local(async move {
        if let Ok(alive) = rx.recv().await {
            done(alive);
        }
    });
}

/// Re-evaluate the main-window notice now (after the schedule was edited or
/// the service was started) instead of waiting for the next status poll.
pub(crate) fn refresh_service_notice() {
    probe_daemon(notify_reachable);
}

/// Compact dismissible "service not running" notice for the main window.
/// `schedule_active` says whether a schedule is enabled and not paused (the
/// only case where a dead daemon silently breaks something); `start` runs the
/// Start button's action. Deliberately no auto-start: the user may have
/// pressed Stop on purpose. Not an `adw::Banner` (needs libadwaita 1.3).
pub(crate) fn build_service_notice(
    schedule_active: Rc<dyn Fn() -> bool>,
    start: Rc<dyn Fn()>,
) -> gtk4::Widget {
    let revealer = gtk4::Revealer::new();
    revealer.set_transition_type(gtk4::RevealerTransitionType::SlideDown);

    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    row.add_css_class("capability-banner");
    row.set_margin_start(12);
    row.set_margin_end(12);
    row.set_margin_top(10);
    let icon = gtk4::Image::from_icon_name("dialog-warning-symbolic");
    icon.set_valign(gtk4::Align::Center);
    let label = gtk4::Label::new(Some(t!(
        "Fresco's background service isn't running, so the schedule can't switch wallpapers."
    )));
    label.set_wrap(true);
    label.set_xalign(0.0);
    label.set_hexpand(true);
    let start_btn = gtk4::Button::with_label(t!("Start"));
    start_btn.set_valign(gtk4::Align::Center);
    let dismiss = gtk4::Button::from_icon_name("window-close-symbolic");
    dismiss.add_css_class("flat");
    dismiss.set_valign(gtk4::Align::Center);
    row.append(&icon);
    row.append(&label);
    row.append(&start_btn);
    row.append(&dismiss);
    revealer.set_child(Some(&row));

    // Dismissal lasts until the daemon has been seen running again, so it
    // does not nag every 4 s poll but does return after the next real outage.
    let dismissed = Rc::new(Cell::new(false));
    {
        let dismissed = dismissed.clone();
        let revealer = revealer.clone();
        dismiss.connect_clicked(move |_| {
            dismissed.set(true);
            revealer.set_reveal_child(false);
        });
    }
    start_btn.connect_clicked(move |_| start());
    let hook: Rc<dyn Fn(bool)> = {
        let revealer = revealer.clone();
        Rc::new(move |reachable| {
            if reachable {
                dismissed.set(false);
            }
            revealer.set_reveal_child(!reachable && !dismissed.get() && schedule_active());
        })
    };
    NOTICE_HOOK.with(|h| *h.borrow_mut() = Some(hook));
    revealer.upcast()
}

/// How often to poll the daemon while the window is open. A live status
/// surface doesn't need sub-second freshness, and this keeps the background
/// thread churn low.
const POLL_INTERVAL_S: u32 = 4;

/// Widgets the polling loop updates in place.
struct PillWidgets {
    dot: gtk4::Label,
    /// Tiny "PLAYING" / "PAUSED" overline above the name.
    overline: gtk4::Label,
    /// Prettified wallpaper name (CPU% lives in the tooltip).
    label: gtk4::Label,
    hwdec: gtk4::Label,
    toggle: gtk4::Button,
    pill: gtk4::Box,
}

/// Build the status pill (dot + wallpaper name + hwdec badge + CPU% + a
/// pause/resume toggle) and start polling the daemon in the background.
/// Returns the root widget to place in the header.
pub fn build_status_pill() -> gtk4::Widget {
    let pill = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    pill.add_css_class("status-pill");

    let dot = gtk4::Label::new(Some("●"));
    dot.add_css_class("dot-off");
    pill.append(&dot);

    // Two stacked lines: a tiny state overline + the wallpaper name.
    let text_col = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    text_col.set_valign(gtk4::Align::Center);
    let overline = gtk4::Label::new(None);
    overline.add_css_class("pill-overline");
    overline.set_xalign(0.0);
    overline.set_visible(false);
    text_col.append(&overline);
    let label = gtk4::Label::new(Some(t!("Not running")));
    label.add_css_class("pill-name");
    label.set_xalign(0.0);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    label.set_max_width_chars(24);
    text_col.append(&label);
    pill.append(&text_col);

    let hwdec = gtk4::Label::new(None);
    hwdec.add_css_class("dim");
    hwdec.set_visible(false);
    pill.append(&hwdec);

    let toggle = gtk4::Button::from_icon_name("media-playback-pause-symbolic");
    toggle.add_css_class("flat");
    toggle.set_tooltip_text(Some(t!("Pause")));
    toggle.set_visible(false);
    pill.append(&toggle);

    let widgets = Rc::new(PillWidgets {
        dot,
        overline,
        label,
        hwdec,
        toggle: toggle.clone(),
        pill: pill.clone(),
    });

    {
        let widgets = widgets.clone();
        toggle.connect_clicked(move |btn| {
            let paused = btn.icon_name().as_deref() == Some("media-playback-start-symbolic");
            let req = if paused {
                Request::Resume
            } else {
                Request::Pause
            };
            send_fire_and_forget(req);
            // Re-poll shortly after so the pill reflects the new state without
            // waiting a full interval.
            let widgets = widgets.clone();
            glib::timeout_add_local_once(Duration::from_millis(400), move || {
                poll_once(widgets);
            });
        });
    }

    poll_once(widgets.clone());
    // Runs for the life of the process: the single window (build_ui guards
    // against duplicates) closing quits the app, taking the timer with it.
    glib::timeout_add_local(Duration::from_secs(POLL_INTERVAL_S as u64), move || {
        poll_once(widgets.clone());
        ControlFlow::Continue
    });

    pill.upcast()
}

/// Fetch `Request::Status` on a background thread and apply the result to the
/// pill once it lands back on the main thread. Mirrors the
/// thread + `async_channel` + `glib::spawn_future_local` pattern used by
/// `poll_notifications` / `check_for_updates` — never blocks the GTK thread.
fn poll_once(widgets: Rc<PillWidgets>) {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = ipc::request(&Request::Status);
        let _ = tx.send_blocking(result);
    });

    glib::spawn_future_local(async move {
        let Ok(result) = rx.recv().await else {
            return;
        };
        match result {
            Ok(crate::ipc::Response::Status(status)) => {
                LAST_MONITORS.with(|m| *m.borrow_mut() = status.monitors_info.clone());
                LAST_LOCK_SUPPORT
                    .with(|c| c.set(status.lockscreen.as_ref().and_then(lock_support_of)));
                apply_status(&widgets, &status);
                notify_reachable(status.running);
            }
            Ok(_) => {}
            Err(e) => {
                // Daemon not running — expected and common, not an error.
                log::debug!("status poll: daemon unreachable: {e:#}");
                LAST_MONITORS.with(|m| m.borrow_mut().clear());
                LAST_LOCK_SUPPORT.with(|c| c.set(None));
                apply_off(&widgets);
                notify_reachable(false);
            }
        }
    });
}

/// Fire a Pause/Resume request on a background thread; the result isn't
/// awaited (the next poll picks up the new state), matching the plan's
/// "fire-and-forget is fine" guidance for the toggle.
fn send_fire_and_forget(req: Request) {
    std::thread::spawn(move || {
        if let Err(e) = ipc::request(&req) {
            log::warn!("pause/resume request failed: {e:#}");
        }
    });
}

fn apply_off(w: &PillWidgets) {
    w.dot.remove_css_class("dot-ok");
    w.dot.remove_css_class("dot-warn");
    w.dot.add_css_class("dot-off");
    w.overline.set_visible(false);
    w.label.set_label(t!("Not running"));
    w.hwdec.set_visible(false);
    w.toggle.set_visible(false);
    w.pill.set_tooltip_text(None);
}

fn apply_status(w: &PillWidgets, status: &StatusReply) {
    if !status.running {
        apply_off(w);
        return;
    }

    let warn = status.paused || status.error.is_some();
    w.dot.remove_css_class("dot-ok");
    w.dot.remove_css_class("dot-warn");
    w.dot.remove_css_class("dot-off");
    w.dot
        .add_css_class(if warn { "dot-warn" } else { "dot-ok" });

    // Presentation-only restyle: overline state + prettified name in the pill,
    // CPU% relegated to the tooltip (with any daemon error). A renderer that
    // gave up on live playback and is holding a paused static frame (see
    // `daemon::WlOutput::supervise`) must not say PLAYING — the dot already
    // turns amber for it via `status.error`, but the overline used to keep
    // claiming the wallpaper was animating right through a give-up.
    w.overline.set_label(if !status.gave_up.is_empty() {
        t!("NEEDS ATTENTION")
    } else if status.paused {
        t!("PAUSED")
    } else {
        t!("PLAYING")
    });
    w.overline.set_visible(true);
    let name = status
        .wallpaper
        .as_deref()
        .unwrap_or(t!("Wallpaper active"));
    w.label.set_label(&pretty_status_name(name));

    match status.hwdec.as_deref() {
        Some(raw) if raw != "no" => {
            w.hwdec.set_label(hwdec_label(raw));
            w.hwdec.set_visible(true);
        }
        _ => w.hwdec.set_visible(false),
    }

    w.toggle.set_visible(true);
    if status.paused {
        w.toggle.set_icon_name("media-playback-start-symbolic");
        w.toggle.set_tooltip_text(Some(t!("Resume")));
    } else {
        w.toggle.set_icon_name("media-playback-pause-symbolic");
        w.toggle.set_tooltip_text(Some(t!("Pause")));
    }

    let mut tip = tf!("CPU {percent}%", "percent" => format!("{:.0}", status.cpu_percent));
    if let Some(err) = status.error.as_deref() {
        tip.push('\n');
        tip.push_str(err);
    }
    w.pill.set_tooltip_text(Some(&tip));
}

/// Prettify the daemon-reported wallpaper name for the pill: strip a trailing
/// media extension ("CAR.mp4" → "CAR") and middle-truncate very long names.
fn pretty_status_name(raw: &str) -> String {
    let mut n = raw.trim().to_string();
    if let Some((stem, ext)) = n.rsplit_once('.') {
        if !stem.is_empty()
            && matches!(
                ext.to_ascii_lowercase().as_str(),
                "mp4"
                    | "webm"
                    | "mkv"
                    | "avi"
                    | "mov"
                    | "flv"
                    | "gif"
                    | "jpg"
                    | "jpeg"
                    | "png"
                    | "webp"
                    | "bmp"
                    | "tiff"
            )
        {
            n = stem.to_string();
        }
    }
    let chars: Vec<char> = n.chars().collect();
    if chars.len() > 28 {
        let head: String = chars[..18].iter().collect();
        let tail: String = chars[chars.len() - 8..].iter().collect();
        n = format!("{}…{}", head.trim_end(), tail.trim_start());
    }
    n
}

/// Map a raw hwdec value from frescod to a friendly badge label.
fn hwdec_label(raw: &str) -> &str {
    match raw {
        "vaapi" => "VA-API",
        "nvdec" => "NVDEC",
        "vdpau" => "VDPAU",
        "drm" => "DRM",
        _ => raw,
    }
}
