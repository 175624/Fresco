//! Lock widget engine: turns a resolved [`ResolvedLock`] into actual pixels,
//! on whichever [`LockTargets`] the current host names.
//!
//! # Inputs, gathered once, refreshed on a schedule
//!
//! * **Layout** — [`lockscreen::resolve`]'s [`ResolvedLock`] maps 1:1 onto a
//!   [`LockArrangement`] ([`arrangement_for`]) and a slot list; the reserved
//!   prompt zone comes from [`ReservedZoneKind::for_host`], which is host
//!   data, not config.
//! * **Clock** — built the same way the desktop clock is:
//!   [`clock::ClockText::of`] then [`clock::ClockText::card_data`], off the
//!   resolved [`crate::clock::ClockTheme`], recomputed only when the rendered
//!   string actually changes (implicitly, via each slot's
//!   [`widgets::ContentKey`] — see "Change detection" below).
//! * **Greeting** — [`GreetingText::Auto`] calls [`userinfo::greeting`] with
//!   [`userinfo::first_name`] of a [`UserInfo`] resolved **once**, at
//!   [`LockEngine::new`] — matching `userinfo`'s own "once per lock" contract,
//!   not the render loop's cadence.
//! * **Avatar** — looked up and decoded once, into an [`Arc<RgbaImage>`] cheap
//!   to clone into a borrow every tick, and only when [`LockWidget::Avatar`]
//!   is on; a session with it off never looks for the picture at all
//!   ([`userinfo::current_identity`]). The lookup lives in [`userinfo`], the
//!   square crop and size bound in [`super::avatar`]; with no picture (or one
//!   that does not decode) the greeting's disc carries
//!   [`userinfo::initials`].
//! * **Battery** — [`battery::read`] every [`BATTERY_POLL`] (10s); `None`
//!   hides the chip rather than showing a stale or fabricated reading.
//! * **Media** — the caller's own now-playing [`widgets::Snapshot`] (the
//!   desktop widget engine's, so there is exactly one now-playing worker
//!   thread whether or not the lock screen is up — see
//!   [`widgets::WidgetEngine::set_lock_album_art`]). No lyric line is ever
//!   attached to the card here, matching the desktop's own `lyrics_tick`,
//!   which omits it (and the progress bar/elapsed/total) for the identical
//!   power reason: those move every second, and this card is meant to
//!   repaint only when the *track* changes. **v1 scope**: when
//!   [`LockWidget::Lyrics`] (or [`LockWidget::Visualizer`]) is on, this engine
//!   still draws no lyric line of its own — the desktop engine's existing
//!   lyric/visualiser overlays are left in place instead (undocumented here
//!   because it is the *caller*'s decision, not this engine's: see
//!   `daemon::mod`'s locking wiring for where that is implemented).
//!
//! # Targets
//!
//! [`LockEngine::tick`] dispatches on [`LockTargets`], set via
//! [`LockEngine::set_targets`]:
//!
//! * [`LockTargets::Desktop`] — per-slot bitmaps via
//!   [`lockscene::render_slot`], returned as [`widgets::WidgetUpdate`]s for
//!   the caller to dispatch through the *existing* desktop player handles —
//!   the same wire the desktop widget engine already uses, just with
//!   different overlay ids (see "Overlay ids" below).
//! * [`LockTargets::Sockets`] — the same per-slot bitmaps, pushed directly
//!   over one [`mpvpaper::MpvIpc`] client per output, reconnected until it
//!   appears (bounded — see [`SocketClient`]).
//! * [`LockTargets::LayerFiles`] — one composed image per output
//!   ([`lockscene::compose`], not `render_slot`: this target has no
//!   `overlay-add` substrate of its own, only a directory a foreign process
//!   polls), written atomically per `packaging/kde/README.md`'s contract.
//! * [`LockTargets::None`] — `tick` does nothing and allocates nothing.
//!
//! # Overlay ids
//!
//! [`LOCK_CLOCK_OVERLAY`]..=[`LOCK_BATTERY_OVERLAY`] (101..=104) are a
//! dedicated range that cannot collide with the desktop widget engine's
//! `widgets::LYRICS_OVERLAY`..=`widgets::DISC_OVERLAY` (1..=4) — both engines
//! can be pushing to the *same* mpv instance at once on the `Desktop` target
//! (COSMIC, locked, with lyrics left in place — see above), so a shared
//! numbering scheme is a correctness requirement, not tidiness.
//!
//! # Change detection and scheduling
//!
//! Each of the four slots tracks its own [`widgets::ContentKey`] per output
//! (content plus the output's own size, since a resize changes the rendered
//! pixels even when nothing else did) and is re-rendered only when that key
//! moves — [`LockEngine::invalidate`] forces one re-*push* (not a re-render,
//! exactly like `widgets::WidgetEngine::invalidate`) for a renderer that lost
//! its overlays. [`LockEngine::next_deadline`] mirrors
//! `widgets::WidgetEngine`'s "Smart Sleep": the next clock-text change and the
//! next battery poll are known in advance, and everything else (a media
//! change arriving on the shared worker's own schedule) is bounded by a
//! ≤1 Hz fallback rather than polled tightly.
//!
//! # Output scale
//!
//! [`ReservedZoneKind::CosmicGreeter`]'s `cosmic_greeter_zone` and every
//! per-output [`widgetkit::lockscene`] size are quoted in device pixels at a
//! *scale* — the compositor's HiDPI factor for that output — which arrives
//! per output as `OutputGeom::scale_milli` (probed in
//! `daemon::wayland_outputs`: wlr-output-management's fractional scale,
//! falling back to `wl_output`'s integer one, then 1.0). The greeter panel is
//! sized in logical pixels, so the real factor is what keeps Fresco's widgets
//! clear of it on a scaled panel.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Timelike};
use image::RgbaImage;

use crate::artwork::Bgra;
use crate::battery::{self, BatteryStatus, ChargeState};
use crate::clock::{self, ClockStyle};
use crate::config::LockPreset;
use crate::ipc::LockSocket;
use crate::lockscreen::{GreetingText, LockWidget, ResolvedLock};
use crate::userinfo::{self, UserInfo};
use crate::widgetkit::lockscene::{self, LockArrangement, LockSceneData, LockSceneSpec, LockSlot};
use crate::widgetkit::{BatteryData, FontStack, GreetingData, NowPlayingData, Rect, Size, Theme};

use super::super::mpvpaper::MpvIpc;
use super::super::widgets::{
    self, BitmapOverlay, BitmapUpdate, ContentKey, OutputGeom, Snapshot, WidgetUpdate,
    MAX_WIDGET_AREA_PX,
};
use super::hosts::{HostKind, LockTargets};

/// Overlay id of the lock clock. See the module docs' "Overlay ids".
pub const LOCK_CLOCK_OVERLAY: u32 = 101;
/// Overlay id of the lock greeting.
pub const LOCK_GREETING_OVERLAY: u32 = 102;
/// Overlay id of the lock now-playing card.
pub const LOCK_MEDIA_OVERLAY: u32 = 103;
/// Overlay id of the lock battery chip.
pub const LOCK_BATTERY_OVERLAY: u32 = 104;

fn overlay_id_for(slot: LockSlot) -> u32 {
    match slot {
        LockSlot::Clock => LOCK_CLOCK_OVERLAY,
        LockSlot::Greeting => LOCK_GREETING_OVERLAY,
        LockSlot::Media => LOCK_MEDIA_OVERLAY,
        LockSlot::Battery => LOCK_BATTERY_OVERLAY,
    }
}

/// Every slot this engine ever draws, in a fixed order — used wherever code
/// needs to visit all four rather than only the ones a particular
/// [`ResolvedLock`] turned on.
const ALL_SLOTS: [LockSlot; 4] = [
    LockSlot::Clock,
    LockSlot::Greeting,
    LockSlot::Media,
    LockSlot::Battery,
];

/// `crate::config::LockPreset` -> `LockArrangement`, 1:1 — the plan's own
/// phrase for this mapping (`docs/plan-lock-screen.md` §3.1's schema comment).
pub fn arrangement_for(preset: LockPreset) -> LockArrangement {
    match preset {
        LockPreset::Classic => LockArrangement::Classic,
        LockPreset::Minimal => LockArrangement::Minimal,
        LockPreset::Glass => LockArrangement::Glass,
        LockPreset::BigType => LockArrangement::BigType,
        LockPreset::Terminal => LockArrangement::Terminal,
    }
}

/// Which reserved-zone rule applies to a host — `docs/plan-lock-screen.md`
/// §3.4/§4's per-host prompt placement, pulled out as a pure value so
/// [`LockEngine`] doesn't have to match on [`HostKind`] at render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservedZoneKind {
    /// COSMIC's frosted greeter panel — `lockscene::cosmic_greeter_zone`.
    CosmicGreeter,
    /// A conventional centred password prompt — `lockscene::prompt_zone`.
    Prompt,
    /// No known reservation (a still-frame-only host, or one with no lock
    /// engine target at all).
    None,
}

impl ReservedZoneKind {
    pub fn for_host(kind: HostKind) -> Self {
        match kind {
            HostKind::Cosmic { .. } => ReservedZoneKind::CosmicGreeter,
            HostKind::Wlroots | HostKind::X11Wm | HostKind::Kde => ReservedZoneKind::Prompt,
            HostKind::Gnome
            | HostKind::Cinnamon
            | HostKind::Mate
            | HostKind::Xfce
            | HostKind::Deepin
            | HostKind::Unsupported => ReservedZoneKind::None,
        }
    }

    /// `scale` is the output's real HiDPI factor (`OutputGeom::scale`): the
    /// COSMIC panel is sized in logical pixels, so an under-estimate would
    /// under-reserve and let Fresco's widgets overlap it.
    pub(super) fn reserved(self, output: Size, scale: f32) -> Vec<Rect> {
        match self {
            ReservedZoneKind::CosmicGreeter => {
                vec![lockscene::cosmic_greeter_zone(output, scale)]
            }
            ReservedZoneKind::Prompt => vec![lockscene::prompt_zone(output)],
            ReservedZoneKind::None => Vec::new(),
        }
    }
}

/// How often [`battery::read`] is re-polled. See [`LockWidget::Battery`].
const BATTERY_POLL: Duration = Duration::from_secs(10);

/// Ceiling on how long [`LockEngine::next_deadline`] will ever ask the caller
/// to sleep, so a media change on the shared now-playing worker (not itself
/// schedulable in advance) is still noticed within one second — the "≤1 Hz
/// worst case" budget.
const MAX_WAIT: Duration = Duration::from_secs(1);

/// How long a [`SocketClient`] keeps retrying before giving up on a socket
/// that never appears — `docs/plan-lock-screen.md`'s "retry until it appears,
/// ≤5 s" rule for `LockTargets::Sockets`.
const SOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Per-output, per-slot state
// ---------------------------------------------------------------------------

/// What is currently believed to be on screen for one (output, slot) pair —
/// the seam that makes "re-render only on change" possible. Mirrors
/// `widgets::BitmapState`'s own `key`/`shown` pair; not shared with it
/// because that type is private to `widgets` and paired one-to-one with a
/// single overlay id, where this engine tracks four per output.
#[derive(Default)]
struct SlotState {
    key: Option<ContentKey>,
    shown: Option<BitmapOverlay>,
}

/// One output's `LayerFiles` state: only a content key is needed (no
/// `BitmapOverlay` — there is no overlay id to remember, just a file that is
/// either up to date or isn't).
#[derive(Default)]
struct LayerState {
    key: Option<ContentKey>,
}

/// One `LockTargets::Sockets` connection: the reused [`MpvIpc`] client plus
/// the bookkeeping [`SOCKET_CONNECT_TIMEOUT`] needs.
struct SocketClient {
    ipc: MpvIpc,
    /// When this client was created — the clock the 5s give-up is measured
    /// against. Not reset by a reconnect attempt: the budget is "how long
    /// since we first wanted this socket", not "how long since the last
    /// retry".
    first_seen: Instant,
    /// Logged once, not once per tick.
    gave_up: bool,
}

impl SocketClient {
    fn new(path: PathBuf, now: Instant) -> Self {
        SocketClient {
            ipc: MpvIpc::new(path),
            first_seen: now,
            gave_up: false,
        }
    }
}

/// Where [`LockEngine::tick`] sends pixels, and whatever state that requires.
enum EngineTarget {
    None,
    Desktop,
    Sockets(HashMap<String, SocketClient>),
    LayerFiles(PathBuf),
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

pub struct LockEngine {
    reserved: ReservedZoneKind,
    arrangement: LockArrangement,
    slots: Vec<LockSlot>,
    greeting: GreetingText,
    clock_style: ClockStyle,
    /// The engine's notion of "now" for the clock slot and its wake-up
    /// deadline. Always `Local::now` in production; boxed (rather than read
    /// inline) so tests can pin it, because a clock slot keyed on the real
    /// wall clock legitimately redraws when a minute boundary falls between
    /// two ticks — which made "the second tick redraws nothing" assertions
    /// flaky. See `LockEngine::set_wall_clock`.
    wall_clock: Box<dyn Fn() -> DateTime<Local> + Send>,
    wants_battery: bool,
    wants_album_art: bool,
    outputs: Vec<OutputGeom>,
    target: EngineTarget,
    theme: Theme,
    fonts: FontStack,
    user: UserInfo,
    avatar: Option<Arc<RgbaImage>>,
    battery: Option<BatteryStatus>,
    last_battery_check: Instant,
    /// Base directory for this engine's own per-slot frame files (`Desktop`/
    /// `Sockets` targets) — never the desktop widget engine's, so the two
    /// can never read or write each other's frames. See
    /// [`LockEngine::frames_dir`].
    frames_dir: PathBuf,
    bitmap_state: HashMap<(String, LockSlot), SlotState>,
    layer_state: HashMap<String, LayerState>,
    /// Config or output geometry changed: every slot must re-render on the
    /// next tick regardless of its cached key.
    dirty: bool,
    /// Re-*push* (not re-render) whatever is already believed shown — see the
    /// module docs' "Change detection and scheduling".
    repush: bool,
}

impl LockEngine {
    /// Build a fresh engine for one lock session. Does the one-time,
    /// per-lock I/O the module docs describe (resolving [`UserInfo`],
    /// decoding the avatar, priming the font stack) and nothing per-tick.
    pub fn new(
        host: HostKind,
        resolved: &ResolvedLock,
        outputs: &[OutputGeom],
        theme: Theme,
    ) -> Self {
        let wants_avatar = resolved.widgets.contains(&LockWidget::Avatar);
        // The picture is only looked for when the Avatar widget is on.
        let user = if wants_avatar {
            userinfo::current()
        } else {
            userinfo::current_identity()
        };
        let avatar = if wants_avatar {
            decode_avatar(&user)
        } else {
            None
        };
        let now = Instant::now();
        LockEngine {
            reserved: ReservedZoneKind::for_host(host),
            arrangement: arrangement_for(resolved.preset),
            slots: slots_for(resolved),
            greeting: resolved.greeting.clone(),
            clock_style: ClockStyle {
                theme: resolved.clock_theme,
                show_date: resolved.widgets.contains(&LockWidget::Date),
                ..ClockStyle::default()
            },
            wall_clock: Box::new(Local::now),
            wants_battery: resolved.widgets.contains(&LockWidget::Battery),
            wants_album_art: resolved.widgets.contains(&LockWidget::AlbumArt),
            outputs: outputs.to_vec(),
            target: EngineTarget::None,
            theme,
            fonts: FontStack::system(),
            user,
            avatar,
            battery: battery::read(),
            last_battery_check: now,
            // A scratch directory of our own, distinct from both the desktop
            // widget engine's frame files and the `LayerFiles` target's own
            // (documented, cross-process) directory — nothing here is part
            // of any external contract, so the exact name is this engine's
            // choice alone.
            frames_dir: crate::ipc::socket_dir().join("lock-frames"),
            bitmap_state: HashMap::new(),
            layer_state: HashMap::new(),
            dirty: true,
            repush: false,
        }
    }

    /// Freeze the engine's wall clock at `at`. Test-only — see the
    /// `wall_clock` field for why the real clock makes two-tick assertions
    /// flaky.
    #[cfg(test)]
    fn set_wall_clock(&mut self, at: DateTime<Local>) {
        self.wall_clock = Box::new(move || at);
    }

    /// Point this engine's per-slot frame files somewhere else. Test-only —
    /// without it a `cargo test` run writes lock frames into the developer's
    /// live `$XDG_RUNTIME_DIR/fresco/lock/`, exactly the hazard
    /// `widgets::frame_stem`'s own `#[cfg(test)]` redirection avoids for the
    /// desktop engine.
    #[cfg(test)]
    fn set_frames_dir(&mut self, dir: PathBuf) {
        self.frames_dir = dir;
        self.bitmap_state.clear();
        self.dirty = true;
    }

    /// Tell the engine where to send pixels. Call once when a lock starts
    /// (with the daemon-spawned host's own targets if it has any, else
    /// `hosts::LockHost::targets_while_locked`), and again if `LockNotify`
    /// reports a fresh socket list. Replacing the target resets every
    /// per-output/per-slot cache — the new target has drawn nothing yet.
    pub fn set_targets(&mut self, targets: LockTargets) {
        let now = Instant::now();
        self.target = match targets {
            LockTargets::None => EngineTarget::None,
            LockTargets::Desktop => EngineTarget::Desktop,
            LockTargets::Sockets(sockets) => {
                let mut clients = HashMap::new();
                for LockSocket { connector, path } in sockets {
                    clients.insert(connector, SocketClient::new(PathBuf::from(path), now));
                }
                EngineTarget::Sockets(clients)
            }
            LockTargets::LayerFiles(dir) => EngineTarget::LayerFiles(dir),
        };
        self.bitmap_state.clear();
        self.layer_state.clear();
        self.dirty = true;
    }

    /// Tell the engine every output it is drawing on, in real pixels — see
    /// `widgets::WidgetEngine::set_outputs`'s identical contract. Call before
    /// [`LockEngine::tick`], every tick.
    pub fn set_outputs(&mut self, outputs: &[OutputGeom]) {
        if outputs == self.outputs.as_slice() {
            return;
        }
        self.outputs = outputs.to_vec();
        self.dirty = true;
    }

    /// Re-*push* whatever is currently believed shown on the next
    /// [`LockEngine::tick`] — for a renderer that lost its overlays without
    /// this engine clearing them (a respawned mpvpaper, a reconnect). See
    /// `widgets::WidgetEngine::invalidate`'s identical contract and caveat:
    /// this re-sends, it does not re-render.
    pub fn invalidate(&mut self) {
        self.repush = true;
    }

    /// Advance every slot and return whatever the caller must dispatch.
    ///
    /// `np` is the desktop widget engine's own now-playing snapshot (see the
    /// module docs' "Media"); pass `None` when neither the lock screen's
    /// NowPlaying widget matters nor a snapshot is available.
    ///
    /// Only [`LockTargets::Desktop`] returns anything: `Sockets` and
    /// `LayerFiles` do their own I/O inline and always return an empty `Vec`.
    pub fn tick(&mut self, np: Option<&Snapshot>) -> Vec<WidgetUpdate> {
        if matches!(self.target, EngineTarget::None) {
            return Vec::new();
        }
        let now = Instant::now();
        if now.duration_since(self.last_battery_check) >= BATTERY_POLL {
            self.last_battery_check = now;
            self.battery = battery::read();
        }

        let owned = self.build_owned_data(np);
        let data = owned.as_scene_data(self.theme);
        let dirty = self.dirty;
        let repush = self.repush;

        let out = match &mut self.target {
            EngineTarget::None => Vec::new(),
            EngineTarget::Desktop => render_bitmaps(
                &mut self.fonts,
                self.arrangement,
                self.reserved,
                &self.slots,
                &self.outputs,
                &mut self.bitmap_state,
                &self.frames_dir,
                dirty,
                repush,
                &data,
                now,
            ),
            EngineTarget::Sockets(clients) => {
                render_sockets(
                    &mut self.fonts,
                    self.arrangement,
                    self.reserved,
                    &self.slots,
                    &self.outputs,
                    &mut self.bitmap_state,
                    &self.frames_dir,
                    dirty,
                    repush,
                    &data,
                    clients,
                    now,
                );
                Vec::new()
            }
            EngineTarget::LayerFiles(dir) => {
                render_layer_files(
                    &mut self.fonts,
                    self.arrangement,
                    self.reserved,
                    &self.slots,
                    &self.outputs,
                    &mut self.layer_state,
                    dir,
                    dirty,
                    &data,
                );
                Vec::new()
            }
        };
        self.dirty = false;
        self.repush = false;
        out
    }

    /// The earliest instant this engine has a scheduled reason to redraw —
    /// see the module docs' "Change detection and scheduling". `None` only
    /// when the target is [`LockTargets::None`] (nothing to ever wake for).
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if matches!(self.target, EngineTarget::None) {
            return None;
        }
        let mut deadline = now + MAX_WAIT;
        if self.slots.contains(&LockSlot::Clock) {
            let wall = (self.wall_clock)();
            let next = clock::next_change(wall, &self.clock_style);
            if let Ok(gap) = (next - wall).to_std() {
                deadline = deadline.min(now + gap);
            }
        }
        if self.wants_battery {
            deadline = deadline.min(self.last_battery_check + BATTERY_POLL);
        }
        Some(deadline)
    }

    /// Take every overlay this engine owns down (`Desktop`/`Sockets`) or
    /// delete its composed images (`LayerFiles`), and forget everything it
    /// believed was shown. Call on unlock — never on a config change alone,
    /// which is what [`LockEngine::invalidate`]/[`LockEngine::set_outputs`]
    /// are for.
    pub fn clear(&mut self) -> Vec<WidgetUpdate> {
        let mut out = Vec::new();
        match &mut self.target {
            EngineTarget::None => {}
            EngineTarget::Desktop => {
                for ((connector, slot), state) in self.bitmap_state.drain() {
                    if state.shown.is_some() {
                        out.push(WidgetUpdate {
                            overlay_id: overlay_id_for(slot),
                            ass: String::new(),
                            bitmap: Some(BitmapUpdate::Remove),
                            target: Some(connector),
                        });
                    }
                }
            }
            EngineTarget::Sockets(clients) => {
                for slot in ALL_SLOTS {
                    for client in clients.values_mut() {
                        client.ipc.overlay_remove(overlay_id_for(slot));
                    }
                }
                self.bitmap_state.clear();
            }
            EngineTarget::LayerFiles(dir) => {
                for connector in self.layer_state.keys() {
                    let _ = std::fs::remove_file(layer_path(dir, connector));
                }
                let _ = std::fs::remove_file(dir.join("layer.png"));
                self.layer_state.clear();
            }
        }
        out
    }

    /// Whether this engine's target is `Sockets` and every one of them has
    /// given up (see [`SOCKET_CONNECT_TIMEOUT`]) — the daemon-observable "the
    /// out-of-process renderer we were driving is gone" signal
    /// `daemon::lock::state::LockMonitor::on_sockets_disconnected` needs.
    /// Always `false` for every other target, including a `Sockets` target
    /// that was never actually populated (`LockTargets::Sockets(vec![])`) —
    /// there is nothing to have disconnected in that case, not "everything
    /// already has".
    pub fn all_sockets_disconnected(&self) -> bool {
        match &self.target {
            EngineTarget::Sockets(clients) => {
                !clients.is_empty() && clients.values().all(|c| c.gave_up)
            }
            _ => false,
        }
    }
}

pub(super) fn slots_for(resolved: &ResolvedLock) -> Vec<LockSlot> {
    let mut slots = Vec::new();
    if resolved.widgets.contains(&LockWidget::Clock) {
        slots.push(LockSlot::Clock);
    }
    if resolved.widgets.contains(&LockWidget::Greeting) {
        slots.push(LockSlot::Greeting);
    }
    if resolved.widgets.contains(&LockWidget::NowPlaying) {
        slots.push(LockSlot::Media);
    }
    if resolved.widgets.contains(&LockWidget::Battery) {
        slots.push(LockSlot::Battery);
    }
    slots
}

/// Decode the picture [`userinfo::current`] found for `user`, square-cropped
/// and bounded (see [`super::avatar`]). `None` — no picture, or one that does
/// not decode — is not an error: the greeting then draws the user's initials.
pub(super) fn decode_avatar(user: &UserInfo) -> Option<Arc<RgbaImage>> {
    super::avatar::decode_avatar_file(user.avatar.as_deref()?)
}

// ---------------------------------------------------------------------------
// Owned per-tick data (no borrow of `self`, so it can outlive a `&mut self`
// dispatch on a different field — see `LockEngine::tick`)
// ---------------------------------------------------------------------------

/// `pub(super)`: `daemon::lock::preview` builds one of these too, for its own
/// one-shot render — see [`build_owned_data`].
pub(super) struct OwnedData {
    clock_text: clock::ClockText,
    clock_style: ClockStyle,
    greeting: Option<String>,
    /// The letters the greeting's avatar disc carries when `avatar` is `None`.
    initials: String,
    avatar: Option<Arc<RgbaImage>>,
    media: Option<OwnedMedia>,
    battery: Option<BatteryData>,
}

struct OwnedMedia {
    label: String,
    title: String,
    artist: String,
    album: String,
    art: Option<Arc<RgbaImage>>,
}

impl OwnedData {
    /// Borrow this tick's owned strings/images into the widgetkit's own
    /// data shape. The lifetime is tied to `self` (an `OwnedData` built fresh
    /// every tick), not to `LockEngine`, which is exactly what lets the
    /// caller hold `&data` across a `&mut self.target` dispatch.
    pub(super) fn as_scene_data(&self, theme: Theme) -> LockSceneData<'_> {
        LockSceneData {
            clock: self.clock_text.card_data(
                &self.clock_style,
                0.0, /* excluded from the content key; see the desktop clock's own precedent */
            ),
            greeting: self.greeting.as_deref().map(|text| GreetingData {
                text,
                avatar: self.avatar.as_deref(),
                initials: &self.initials,
                text_size: 0.0, // `render_slot`/`compose` size this themselves.
            }),
            media: self.media.as_ref().map(|m| NowPlayingData {
                label: &m.label,
                title: &m.title,
                artist: &m.artist,
                album: &m.album,
                art: m.art.as_deref(),
                ..NowPlayingData::default()
            }),
            battery: self.battery,
            theme,
        }
    }
}

/// Build one instant's [`OwnedData`] from plain inputs — the shared core of
/// [`LockEngine::build_owned_data`] and `daemon::lock::preview`'s own one-shot
/// render, pulled out as a free function so the preview never has to
/// re-derive "what does the greeting say", "does the media card have
/// anything to show" or "is the battery chip visible" by hand and risk
/// disagreeing with what a real lock would draw.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_owned_data(
    slots: &[LockSlot],
    clock_style: &ClockStyle,
    greeting: &GreetingText,
    user: &UserInfo,
    avatar: &Option<Arc<RgbaImage>>,
    wants_battery: bool,
    wants_album_art: bool,
    battery: Option<BatteryStatus>,
    np: Option<&Snapshot>,
) -> OwnedData {
    build_owned_data_at(
        Local::now(),
        slots,
        clock_style,
        greeting,
        user,
        avatar,
        wants_battery,
        wants_album_art,
        battery,
        np,
    )
}

/// [`build_owned_data`] at an explicit instant, so the engine can feed its
/// injectable wall clock through while the preview keeps the real one.
#[allow(clippy::too_many_arguments)]
fn build_owned_data_at(
    wall: DateTime<Local>,
    slots: &[LockSlot],
    clock_style: &ClockStyle,
    greeting: &GreetingText,
    user: &UserInfo,
    avatar: &Option<Arc<RgbaImage>>,
    wants_battery: bool,
    wants_album_art: bool,
    battery: Option<BatteryStatus>,
    np: Option<&Snapshot>,
) -> OwnedData {
    let clock_text = clock::ClockText::of(wall, clock_style);

    let greeting = if !slots.contains(&LockSlot::Greeting) {
        None
    } else {
        match greeting {
            GreetingText::Hidden => None,
            GreetingText::Custom(s) => Some(s.clone()),
            GreetingText::Auto => Some(userinfo::greeting(
                wall.hour(),
                Some(&userinfo::first_name(user)),
            )),
        }
    };

    let media = if !slots.contains(&LockSlot::Media) {
        None
    } else {
        np.and_then(|s| s.track.as_ref()).and_then(|track| {
            let now_playing = &track.now_playing;
            if !now_playing.has_title() {
                return None;
            }
            Some(OwnedMedia {
                label: crate::t!("Now playing").to_string(),
                title: now_playing.title.clone(),
                artist: now_playing.artist_line(),
                album: now_playing.album.clone(),
                art: if wants_album_art {
                    track.art.clone()
                } else {
                    None
                },
            })
        })
    };

    let battery_data = if !wants_battery {
        None
    } else {
        battery.map(|b| BatteryData {
            percent: b.percent,
            charging: b.state == ChargeState::Charging,
            full: b.state == ChargeState::Full,
            size: 0.0,
        })
    };

    OwnedData {
        clock_text,
        clock_style: clock_style.clone(),
        greeting,
        initials: userinfo::initials(user),
        avatar: avatar.clone(),
        media,
        battery: battery_data,
    }
}

impl LockEngine {
    fn build_owned_data(&self, np: Option<&Snapshot>) -> OwnedData {
        build_owned_data_at(
            (self.wall_clock)(),
            &self.slots,
            &self.clock_style,
            &self.greeting,
            &self.user,
            &self.avatar,
            self.wants_battery,
            self.wants_album_art,
            self.battery,
            np,
        )
    }
}

// ---------------------------------------------------------------------------
// Content keys
// ---------------------------------------------------------------------------

/// One slot's key: its own content plus the output's size, since a resize
/// changes the rendered pixels even when the content is byte-identical.
/// `day_fraction`/font sizes are deliberately excluded — see the desktop
/// clock's own `clock_tick` doc comment for why a gauge value that changes
/// every instant must not be in a redraw-gating key.
fn slot_key(slot: LockSlot, data: &LockSceneData, out: (u32, u32)) -> ContentKey {
    match slot {
        LockSlot::Clock => {
            let c = &data.clock;
            ContentKey::of((c.time, c.weekday, c.date, c.secondary, out))
        }
        LockSlot::Greeting => {
            let g = data.greeting.as_ref();
            ContentKey::of((
                g.map(|g| (g.text, g.initials)),
                g.map(|g| g.avatar.is_some()),
                out,
            ))
        }
        LockSlot::Media => {
            let m = data.media.as_ref();
            ContentKey::of((
                m.map(|m| (m.label, m.title, m.artist, m.album, m.art.is_some())),
                out,
            ))
        }
        LockSlot::Battery => {
            let b = data.battery;
            ContentKey::of((b.map(|b| (b.percent, b.charging, b.full)), out))
        }
    }
}

// ---------------------------------------------------------------------------
// Per-slot rendering shared by the Desktop and Sockets targets
// ---------------------------------------------------------------------------

/// Outcome of considering one (output, slot) pair for this tick.
enum SlotOutcome {
    /// Nothing changed; the caller should do nothing (or, on `repush`,
    /// re-issue whatever is in `SlotState::shown`).
    Unchanged,
    /// Freshly rendered and written to `path`.
    Drawn(BitmapOverlay),
    /// `layout`/the data say there is nothing to draw here any more.
    Removed,
}

/// The shared "should this redraw, and if so what are the pixels" step for
/// one (output, slot) pair. Writes the frame file itself (via
/// `widgets::write_frame`, this engine's own frame-file safety net) but
/// leaves *dispatching* the result (a `WidgetUpdate`, or a direct
/// `overlay-add`) to the caller, since that differs between `Desktop` and
/// `Sockets`.
#[allow(clippy::too_many_arguments)]
fn render_one_slot(
    fonts: &mut FontStack,
    slot: LockSlot,
    spec: &LockSceneSpec,
    data: &LockSceneData,
    state: &mut SlotState,
    frame_path: &Path,
    dirty: bool,
    repush: bool,
) -> SlotOutcome {
    let out = (spec.output.w as u32, spec.output.h as u32);
    let key = slot_key(slot, data, out);
    let changed = dirty || state.key != Some(key);
    if !changed {
        return if repush {
            match &state.shown {
                Some(b) => SlotOutcome::Drawn(b.clone()),
                None => SlotOutcome::Unchanged,
            }
        } else {
            SlotOutcome::Unchanged
        };
    }
    state.key = Some(key);
    let Some((rect, bgra)) = lockscene::render_slot(slot, spec, fonts, data) else {
        let removed = state.shown.take().is_some();
        return if removed {
            SlotOutcome::Removed
        } else {
            SlotOutcome::Unchanged
        };
    };
    let area = u64::from(bgra.w) * u64::from(bgra.h);
    if area > MAX_WIDGET_AREA_PX {
        log::warn!(
            "lock engine: slot {slot:?} wanted a {}x{} bitmap ({area} px), over the \
             {MAX_WIDGET_AREA_PX} px cap — it stays hidden",
            bgra.w,
            bgra.h
        );
        return SlotOutcome::Unchanged;
    }
    if let Err(e) = widgets::write_frame(frame_path, &bgra) {
        log::warn!(
            "lock engine: cannot write the frame for slot {slot:?} to {}: {e} — it stays hidden",
            frame_path.display()
        );
        return SlotOutcome::Unchanged;
    }
    let overlay = BitmapOverlay {
        x: rect.x.round() as i32,
        y: rect.y.round() as i32,
        path: frame_path.to_path_buf(),
        w: bgra.w,
        h: bgra.h,
        stride: bgra.stride(),
    };
    state.shown = Some(overlay.clone());
    SlotOutcome::Drawn(overlay)
}

/// `{frames_dir}/{slot-name}-{sanitized-connector}.bgra` — one file per
/// (output, slot), following `widgets.rs`'s own "one file per output" rule
/// (`slot_path`'s doc comment): two outputs of different sizes must never
/// share a frame file.
fn slot_frame_path(frames_dir: &Path, slot: LockSlot, connector: &str) -> PathBuf {
    let name = match slot {
        LockSlot::Clock => "clock",
        LockSlot::Greeting => "greeting",
        LockSlot::Media => "media",
        LockSlot::Battery => "battery",
    };
    let safe: String = connector
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    frames_dir.join(format!("{name}-{safe}.bgra"))
}

#[allow(clippy::too_many_arguments)]
fn render_bitmaps(
    fonts: &mut FontStack,
    arrangement: LockArrangement,
    reserved: ReservedZoneKind,
    slots: &[LockSlot],
    outputs: &[OutputGeom],
    bitmap_state: &mut HashMap<(String, LockSlot), SlotState>,
    frames_dir: &Path,
    dirty: bool,
    repush: bool,
    data: &LockSceneData,
    _now: Instant,
) -> Vec<WidgetUpdate> {
    let mut out = Vec::new();
    let _ = std::fs::create_dir_all(frames_dir);
    for geom in outputs {
        let output = Size::new(geom.w as f32, geom.h as f32);
        let scale = geom.scale();
        let spec = LockSceneSpec {
            arrangement,
            output,
            scale,
            slots: slots.to_vec(),
            reserved: reserved.reserved(output, scale),
        };
        for &slot in slots {
            let key = (geom.connector.clone(), slot);
            let state = bitmap_state.entry(key).or_default();
            let path = slot_frame_path(frames_dir, slot, &geom.connector);
            match render_one_slot(fonts, slot, &spec, data, state, &path, dirty, repush) {
                SlotOutcome::Unchanged => {}
                SlotOutcome::Removed => out.push(WidgetUpdate {
                    overlay_id: overlay_id_for(slot),
                    ass: String::new(),
                    bitmap: Some(BitmapUpdate::Remove),
                    target: Some(geom.connector.clone()),
                }),
                SlotOutcome::Drawn(overlay) => out.push(WidgetUpdate {
                    overlay_id: overlay_id_for(slot),
                    ass: String::new(),
                    bitmap: Some(BitmapUpdate::Draw(overlay)),
                    target: Some(geom.connector.clone()),
                }),
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn render_sockets(
    fonts: &mut FontStack,
    arrangement: LockArrangement,
    reserved: ReservedZoneKind,
    slots: &[LockSlot],
    outputs: &[OutputGeom],
    bitmap_state: &mut HashMap<(String, LockSlot), SlotState>,
    frames_dir: &Path,
    dirty: bool,
    repush: bool,
    data: &LockSceneData,
    clients: &mut HashMap<String, SocketClient>,
    now: Instant,
) {
    let _ = std::fs::create_dir_all(frames_dir);
    for geom in outputs {
        let Some(client) = clients.get_mut(&geom.connector) else {
            continue;
        };
        if client.gave_up {
            continue;
        }
        if !client.ipc.is_connected()
            && now.duration_since(client.first_seen) > SOCKET_CONNECT_TIMEOUT
        {
            client.gave_up = true;
            log::warn!(
                "lock engine: mpv IPC socket for {} never appeared within {SOCKET_CONNECT_TIMEOUT:?}; \
                 giving up on this output",
                geom.connector
            );
            continue;
        }
        let output = Size::new(geom.w as f32, geom.h as f32);
        let scale = geom.scale();
        let spec = LockSceneSpec {
            arrangement,
            output,
            scale,
            slots: slots.to_vec(),
            reserved: reserved.reserved(output, scale),
        };
        for &slot in slots {
            let key = (geom.connector.clone(), slot);
            let state = bitmap_state.entry(key).or_default();
            let path = slot_frame_path(frames_dir, slot, &geom.connector);
            match render_one_slot(fonts, slot, &spec, data, state, &path, dirty, repush) {
                SlotOutcome::Unchanged => {}
                SlotOutcome::Removed => client.ipc.overlay_remove(overlay_id_for(slot)),
                SlotOutcome::Drawn(overlay) => client.ipc.overlay_add(
                    overlay_id_for(slot),
                    overlay.x,
                    overlay.y,
                    &overlay.path_str(),
                    overlay.w,
                    overlay.h,
                    overlay.stride,
                ),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// LayerFiles: one composed PNG per output
// ---------------------------------------------------------------------------

/// `dir/layer-<connector>.png` — the connector-specific file
/// `packaging/kde/README.md`'s widget-layer contract prefers.
fn layer_path(dir: &Path, connector: &str) -> PathBuf {
    dir.join(format!("layer-{connector}.png"))
}

#[allow(clippy::too_many_arguments)]
fn render_layer_files(
    fonts: &mut FontStack,
    arrangement: LockArrangement,
    reserved: ReservedZoneKind,
    slots: &[LockSlot],
    outputs: &[OutputGeom],
    layer_state: &mut HashMap<String, LayerState>,
    dir: &Path,
    dirty: bool,
    data: &LockSceneData,
) {
    if std::fs::create_dir_all(dir).is_err() {
        log::warn!(
            "lock engine: cannot create layer directory {}",
            dir.display()
        );
        return;
    }
    for (i, geom) in outputs.iter().enumerate() {
        let output = Size::new(geom.w as f32, geom.h as f32);
        let scale = geom.scale();
        let spec = LockSceneSpec {
            arrangement,
            output,
            scale,
            slots: slots.to_vec(),
            reserved: reserved.reserved(output, scale),
        };
        // Every slot's content, folded into one key for the whole composed
        // image — there is only one file to redraw or not, unlike the
        // per-slot bitmap targets.
        let key = ContentKey::of(ALL_SLOTS.map(|s| slot_key(s, data, (geom.w, geom.h))));
        let state = layer_state.entry(geom.connector.clone()).or_default();
        if !dirty && state.key == Some(key) {
            continue;
        }
        state.key = Some(key);

        let Ok(mut canvas) = crate::widgetkit::Canvas::for_logical(
            Size::new(output.w / scale, output.h / scale),
            scale,
        ) else {
            continue;
        };
        lockscene::compose(&mut canvas, fonts, &spec, data);
        let bgra = canvas.into_bgra();
        let rgba = bgra_to_rgba_image(&bgra);

        let path = layer_path(dir, &geom.connector);
        if let Err(e) = write_png_atomic(&path, &rgba) {
            log::warn!("lock engine: failed to write {}: {e}", path.display());
            continue;
        }
        // `layer.png` is the fallback for whichever output QML fails to
        // match by connector name — always the *first* output, matching
        // `packaging/kde/README.md`'s contract.
        if i == 0 {
            if let Err(e) = write_png_atomic(&dir.join("layer.png"), &rgba) {
                log::warn!("lock engine: failed to write layer.png: {e}");
            }
        }
    }
}

/// Un-premultiply [`Bgra`] into a straight-alpha [`RgbaImage`] — PNG has no
/// premultiplied-alpha convention, and a QML `Image` reading one written
/// without this conversion would show every translucent pixel too dark.
/// Reuses [`crate::widgetkit::Color::from_premul_rgba8`], the toolkit's own
/// inverse of the premultiply it does on the way *into* a [`Bgra`], rather
/// than re-deriving the divide-by-alpha arithmetic here.
pub(in crate::daemon) fn bgra_to_rgba_image(bgra: &Bgra) -> RgbaImage {
    use crate::widgetkit::Color;
    let mut img = RgbaImage::new(bgra.w, bgra.h);
    for (i, px) in img.pixels_mut().enumerate() {
        let o = i * 4;
        let Some(chunk) = bgra.data.get(o..o + 4) else {
            break;
        };
        let (b, g, r, a) = (chunk[0], chunk[1], chunk[2], chunk[3]);
        let straight = Color::from_premul_rgba8([r, g, b, a]);
        let to_u8 = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
        *px = image::Rgba([to_u8(straight.r), to_u8(straight.g), to_u8(straight.b), a]);
    }
    img
}

/// Write `img` as a PNG to `path` atomically: encode to a temp file in the
/// same directory, then `rename` over the target — `packaging/kde/README.md`'s
/// contract, so `main.qml` never reads a half-written file.
pub(in crate::daemon) fn write_png_atomic(path: &Path, img: &RgbaImage) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    // Best-effort: the directory may not exist yet (e.g. `preview_path`'s
    // `$XDG_RUNTIME_DIR/fresco` before anything else has created it, or a
    // fresh `LayerFiles` target on the first lock of a session).
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    img.save_with_format(&tmp, image::ImageFormat::Png)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LockScreen, LockWidgets};
    use crate::lockscreen::resolve;
    use crate::widgetkit::theme::Mode;

    fn theme() -> Theme {
        Theme::for_accent(Mode::Dark, crate::config::Accent::Blue)
    }

    fn geoms(pairs: &[(&str, u32, u32)]) -> Vec<OutputGeom> {
        pairs
            .iter()
            .map(|(c, w, h)| OutputGeom {
                connector: (*c).to_string(),
                w: *w,
                h: *h,
                scale_milli: 1000,
            })
            .collect()
    }

    fn test_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fresco-lock-engine-test-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fixed local instant 30 s into a minute, `minutes` minutes later.
    fn fixed_time(minutes: i64) -> DateTime<Local> {
        use chrono::TimeZone;
        Local.with_ymd_and_hms(2026, 3, 10, 12, 0, 30).unwrap() + chrono::Duration::minutes(minutes)
    }

    /// A test-isolated engine: `Desktop`/`Sockets` frame files go under a
    /// unique temp directory rather than the developer's live
    /// `$XDG_RUNTIME_DIR/fresco/lock-frames` — the same hazard
    /// `widgets::frame_stem`'s own `#[cfg(test)]` redirection exists to
    /// avoid, and why this helper (not a bare `LockEngine::new`) is what
    /// every test in this module should build an engine through.
    fn engine_for(host: HostKind, resolved: &ResolvedLock, outputs: &[OutputGeom]) -> LockEngine {
        let mut engine = LockEngine::new(host, resolved, outputs, theme());
        // Pinned mid-minute so two ticks can never straddle a minute boundary.
        engine.set_wall_clock(fixed_time(0));
        let dir = std::env::temp_dir().join(format!(
            "fresco-lock-engine-test-frames-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        engine.set_frames_dir(dir);
        engine
    }

    // -- the greeting's avatar / initials ---------------------------------------

    fn user_with(avatar: Option<PathBuf>) -> UserInfo {
        UserInfo {
            login: "roy".to_string(),
            real_name: Some("Roy Das".to_string()),
            avatar,
        }
    }

    fn greeting_data_for(user: &UserInfo, avatar: &Option<Arc<RgbaImage>>) -> OwnedData {
        // A pinned instant, so the greeting text cannot change between two
        // calls in one test because an hour boundary fell between them.
        build_owned_data_at(
            fixed_time(0),
            &[LockSlot::Greeting],
            &ClockStyle::default(),
            &GreetingText::Auto,
            user,
            avatar,
            false,
            false,
            None,
            None,
        )
    }

    #[test]
    fn a_user_with_no_picture_gets_their_own_initials_not_the_greetings_letter() {
        let user = user_with(None);
        let avatar = decode_avatar(&user);
        assert!(avatar.is_none());
        let owned = greeting_data_for(&user, &avatar);
        let scene = owned.as_scene_data(theme());
        let g = scene
            .greeting
            .expect("a greeting slot yields greeting data");
        assert!(g.avatar.is_none());
        // "Good …" would have put a "G" in the disc.
        assert_eq!(g.initials, "RD");
        assert!(g.text.contains("Roy"), "{:?}", g.text);
    }

    #[test]
    fn a_user_with_a_picture_gets_it_decoded_square_and_handed_to_the_greeting() {
        let dir = test_dir("avatar-picture");
        let path = dir.join("face.png");
        RgbaImage::from_pixel(80, 40, image::Rgba([10, 20, 30, 255]))
            .save_with_format(&path, image::ImageFormat::Png)
            .unwrap();
        let user = user_with(Some(path));
        let avatar = decode_avatar(&user).expect("a PNG decodes");
        assert_eq!(avatar.dimensions(), (40, 40), "centre-cropped to a square");
        let owned = greeting_data_for(&user, &Some(avatar));
        let scene = owned.as_scene_data(theme());
        assert!(scene.greeting.unwrap().avatar.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_avatar_path_that_will_not_decode_degrades_to_initials() {
        let dir = test_dir("avatar-svg");
        let path = dir.join("face.svg");
        std::fs::write(&path, "<svg xmlns=\"http://www.w3.org/2000/svg\"/>").unwrap();
        let user = user_with(Some(path));
        assert!(decode_avatar(&user).is_none());
        let owned = greeting_data_for(&user, &None);
        let scene = owned.as_scene_data(theme());
        let g = scene.greeting.unwrap();
        assert!(g.avatar.is_none());
        assert_eq!(g.initials, "RD");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_greeting_slot_redraws_when_the_picture_or_the_initials_change() {
        let out = (320, 200);
        let key = |user: &UserInfo, avatar: Option<Arc<RgbaImage>>| {
            let owned = greeting_data_for(user, &avatar);
            let scene = owned.as_scene_data(theme());
            slot_key(LockSlot::Greeting, &scene, out)
        };
        let roy = user_with(None);
        let mut ada = user_with(None);
        ada.real_name = Some("Ada Lovelace".to_string());
        let pic = Some(Arc::new(RgbaImage::new(8, 8)));

        assert_eq!(key(&roy, None), key(&roy, None), "stable for equal data");
        assert_ne!(key(&roy, None), key(&roy, pic.clone()));
        // Same greeting text ("Good …, <first name>") is not the whole story:
        // different people differ in their initials even if a greeting repeats.
        let mut same_first = user_with(None);
        same_first.real_name = Some("Roy Smith".to_string());
        assert_ne!(key(&roy, None), key(&same_first, None));
        assert_ne!(key(&roy, None), key(&ada, None));
    }

    // -- arrangement_for / ReservedZoneKind -----------------------------------

    #[test]
    fn arrangement_for_is_one_to_one() {
        assert_eq!(
            arrangement_for(LockPreset::Classic),
            LockArrangement::Classic
        );
        assert_eq!(
            arrangement_for(LockPreset::Minimal),
            LockArrangement::Minimal
        );
        assert_eq!(arrangement_for(LockPreset::Glass), LockArrangement::Glass);
        assert_eq!(
            arrangement_for(LockPreset::BigType),
            LockArrangement::BigType
        );
        assert_eq!(
            arrangement_for(LockPreset::Terminal),
            LockArrangement::Terminal
        );
    }

    #[test]
    fn reserved_zone_kind_matches_the_hosts_table() {
        assert_eq!(
            ReservedZoneKind::for_host(HostKind::Cosmic { live: true }),
            ReservedZoneKind::CosmicGreeter
        );
        assert_eq!(
            ReservedZoneKind::for_host(HostKind::Cosmic { live: false }),
            ReservedZoneKind::CosmicGreeter
        );
        for h in [HostKind::Wlroots, HostKind::X11Wm, HostKind::Kde] {
            assert_eq!(
                ReservedZoneKind::for_host(h),
                ReservedZoneKind::Prompt,
                "{h:?}"
            );
        }
        for h in [
            HostKind::Gnome,
            HostKind::Cinnamon,
            HostKind::Mate,
            HostKind::Xfce,
            HostKind::Deepin,
            HostKind::Unsupported,
        ] {
            assert_eq!(
                ReservedZoneKind::for_host(h),
                ReservedZoneKind::None,
                "{h:?}"
            );
        }
    }

    // -- Desktop target: overlay ids, placement, change detection -----------

    #[test]
    fn desktop_target_overlay_ids_are_in_the_dedicated_range_and_never_collide_with_the_desktop_engine(
    ) {
        for id in [
            LOCK_CLOCK_OVERLAY,
            LOCK_GREETING_OVERLAY,
            LOCK_MEDIA_OVERLAY,
            LOCK_BATTERY_OVERLAY,
        ] {
            assert!(
                id > widgets::DISC_OVERLAY,
                "{id} must be past the desktop engine's range"
            );
        }
        // And pairwise distinct.
        let ids = [
            LOCK_CLOCK_OVERLAY,
            LOCK_GREETING_OVERLAY,
            LOCK_MEDIA_OVERLAY,
            LOCK_BATTERY_OVERLAY,
        ];
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j]);
            }
        }
    }

    #[test]
    fn desktop_target_places_every_slot_inside_its_own_output_and_outside_the_reserved_zone() {
        let resolved = resolve(&LockScreen {
            widgets: LockWidgets {
                clock: true,
                date: true,
                greeting: true,
                avatar: false,
                now_playing: true,
                album_art: true,
                battery: true,
                lyrics: false,
                visualizer: false,
            },
            ..LockScreen::default()
        });
        let outs = geoms(&[("DP-1", 1920, 1080), ("HDMI-1", 2560, 1440)]);
        let mut engine = engine_for(HostKind::Cosmic { live: true }, &resolved, &outs);
        engine.set_targets(LockTargets::Desktop);
        engine.set_outputs(&outs);

        let updates = engine.tick(None);
        assert!(
            !updates.is_empty(),
            "the clock alone must always draw something"
        );
        for u in &updates {
            let Some(b) = u.frame() else { continue };
            let target = u.target.clone().expect("Desktop updates are per output");
            let geom = outs.iter().find(|g| g.connector == target).unwrap();
            assert!(b.x >= 0 && b.y >= 0, "{target}: {b:?}");
            assert!(
                u32::try_from(b.x).unwrap() + b.w <= geom.w + 1
                    && u32::try_from(b.y).unwrap() + b.h <= geom.h + 1,
                "{target}: {b:?} escapes {geom:?}"
            );
        }
    }

    #[test]
    fn desktop_target_only_redraws_a_slot_whose_content_changed() {
        let resolved = resolve(&LockScreen::default());
        let outs = geoms(&[("DP-1", 1920, 1080)]);
        let mut engine = engine_for(HostKind::Cosmic { live: true }, &resolved, &outs);
        engine.set_targets(LockTargets::Desktop);
        engine.set_outputs(&outs);

        let first = engine.tick(None);
        assert!(!first.is_empty());
        // Nothing about the resolved config, the outputs, or (within the same
        // wall-clock minute) the clock text has changed, so a second tick
        // must draw nothing at all.
        let second = engine.tick(None);
        assert!(
            second.is_empty(),
            "an unchanged tick must produce no updates, got {second:?}"
        );
    }

    /// The flip side of the pinned-clock tests above: when the injected time
    /// does cross a minute boundary the clock slot must redraw (and only it),
    /// proving those tests pass because nothing changed, not because the
    /// clock is ignored.
    #[test]
    fn clock_slot_redraws_when_the_injected_time_crosses_a_minute() {
        let resolved = resolve(&LockScreen::default());
        let outs = geoms(&[("DP-1", 1920, 1080)]);
        let mut engine = engine_for(HostKind::Cosmic { live: true }, &resolved, &outs);
        engine.set_targets(LockTargets::Desktop);
        engine.set_outputs(&outs);
        assert!(!engine.tick(None).is_empty());

        // Same minute: nothing to draw.
        engine.set_wall_clock(fixed_time(0) + chrono::Duration::seconds(10));
        assert!(engine.tick(None).is_empty());

        engine.set_wall_clock(fixed_time(1));
        let next = engine.tick(None);
        assert!(!next.is_empty(), "a new minute must redraw the clock");
        assert!(
            next.iter().all(|u| u.overlay_id == LOCK_CLOCK_OVERLAY),
            "only the clock slot changed, got {next:?}"
        );
    }

    #[test]
    fn invalidate_repushes_without_rerendering() {
        let resolved = resolve(&LockScreen::default());
        let outs = geoms(&[("DP-1", 1920, 1080)]);
        let mut engine = engine_for(HostKind::Cosmic { live: true }, &resolved, &outs);
        engine.set_targets(LockTargets::Desktop);
        engine.set_outputs(&outs);
        let first = engine.tick(None);
        assert!(!first.is_empty());

        engine.invalidate();
        let repushed = engine.tick(None);
        assert_eq!(
            repushed.len(),
            first.len(),
            "invalidate must re-send exactly what was already shown"
        );
        for (a, b) in first.iter().zip(repushed.iter()) {
            assert_eq!(a, b, "a repush must be byte-identical to the original draw");
        }
    }

    #[test]
    fn lyrics_and_visualizer_widgets_produce_no_lock_slot_of_their_own() {
        // v1 scope (module docs): even with Lyrics/Visualizer on, this
        // engine draws no lyric line and has no visualiser slot at all —
        // there is no `LockSlot` for either, so `slots_for` can never
        // produce one no matter what the config says.
        let resolved = resolve(&LockScreen {
            widgets: LockWidgets {
                lyrics: true,
                visualizer: true,
                ..LockWidgets::default()
            },
            ..LockScreen::default()
        });
        let slots = slots_for(&resolved);
        assert_eq!(slots.len(), ALL_SLOTS.len().min(slots.len()));
        assert!(slots.iter().all(|s| ALL_SLOTS.contains(s)));
    }

    #[test]
    fn clock_disabled_still_lets_other_slots_through() {
        let resolved = resolve(&LockScreen {
            widgets: LockWidgets {
                clock: false,
                date: false,
                greeting: true,
                avatar: false,
                now_playing: false,
                album_art: false,
                battery: true,
                lyrics: false,
                visualizer: false,
            },
            ..LockScreen::default()
        });
        let slots = slots_for(&resolved);
        assert!(!slots.contains(&LockSlot::Clock));
        assert!(slots.contains(&LockSlot::Greeting));
        assert!(slots.contains(&LockSlot::Battery));
    }

    // -- LayerFiles target: decodable PNGs of the right size -----------------

    #[test]
    fn layer_files_writes_decodable_pngs_of_the_output_size_per_connector_plus_fallback() {
        let resolved = resolve(&LockScreen::default());
        let outs = geoms(&[("eDP-1", 1920, 1080), ("DP-2", 2560, 1440)]);
        let mut engine = engine_for(HostKind::Kde, &resolved, &outs);
        let dir = test_dir("layerfiles");
        engine.set_targets(LockTargets::LayerFiles(dir.clone()));
        engine.set_outputs(&outs);

        let updates = engine.tick(None);
        assert!(
            updates.is_empty(),
            "LayerFiles must return nothing for the caller to dispatch"
        );

        for (connector, w, h) in [("eDP-1", 1920u32, 1080u32), ("DP-2", 2560, 1440)] {
            let path = dir.join(format!("layer-{connector}.png"));
            let img = image::open(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
            assert_eq!((img.width(), img.height()), (w, h), "{connector}");
        }
        // `layer.png` is the first output's image, byte-identical.
        let first = std::fs::read(dir.join("layer-eDP-1.png")).unwrap();
        let fallback = std::fs::read(dir.join("layer.png")).unwrap();
        assert_eq!(first, fallback);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn layer_files_are_transparent_where_nothing_is_drawn() {
        // A corner far from every arrangement's placements must stay fully
        // transparent — this is an overlay layer, not a full frame.
        let resolved = resolve(&LockScreen {
            widgets: LockWidgets {
                clock: false,
                date: false,
                greeting: false,
                avatar: false,
                now_playing: false,
                album_art: false,
                battery: false,
                lyrics: false,
                visualizer: false,
            },
            ..LockScreen::default()
        });
        let outs = geoms(&[("eDP-1", 800, 600)]);
        let mut engine = engine_for(HostKind::Kde, &resolved, &outs);
        let dir = test_dir("layerfiles-transparent");
        engine.set_targets(LockTargets::LayerFiles(dir.clone()));
        engine.set_outputs(&outs);
        engine.tick(None);

        let img = image::open(dir.join("layer.png")).unwrap().into_rgba8();
        assert_eq!(
            img.get_pixel(0, 0).0[3],
            0,
            "no widgets on: must be fully transparent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn layer_files_skip_rewriting_when_nothing_changed() {
        let resolved = resolve(&LockScreen::default());
        let outs = geoms(&[("eDP-1", 800, 600)]);
        let mut engine = engine_for(HostKind::Kde, &resolved, &outs);
        let dir = test_dir("layerfiles-unchanged");
        engine.set_targets(LockTargets::LayerFiles(dir.clone()));
        engine.set_outputs(&outs);
        engine.tick(None);
        let mtime = |p: &Path| std::fs::metadata(p).unwrap().modified().unwrap();
        let path = dir.join("layer.png");
        let before = mtime(&path);
        std::thread::sleep(Duration::from_millis(20));
        engine.tick(None);
        assert_eq!(
            before,
            mtime(&path),
            "an unchanged tick must not rewrite the file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- next_deadline ---------------------------------------------------------

    #[test]
    fn next_deadline_is_none_for_no_target_and_bounded_otherwise() {
        let resolved = resolve(&LockScreen::default());
        let outs = geoms(&[("DP-1", 1920, 1080)]);
        let mut engine = engine_for(HostKind::Cosmic { live: true }, &resolved, &outs);
        assert_eq!(
            engine.next_deadline(Instant::now()),
            None,
            "no target set yet"
        );

        engine.set_targets(LockTargets::Desktop);
        let now = Instant::now();
        let deadline = engine.next_deadline(now).expect("a target is set");
        assert!(deadline > now);
        assert!(
            deadline <= now + MAX_WAIT + Duration::from_millis(50),
            "must respect the ~1 Hz worst case, got {:?} ahead",
            deadline - now
        );
    }

    // -- clear -----------------------------------------------------------------

    #[test]
    fn clear_blanks_every_shown_desktop_overlay_exactly_once() {
        let resolved = resolve(&LockScreen::default());
        let outs = geoms(&[("DP-1", 1920, 1080)]);
        let mut engine = engine_for(HostKind::Cosmic { live: true }, &resolved, &outs);
        engine.set_targets(LockTargets::Desktop);
        engine.set_outputs(&outs);
        let drawn = engine.tick(None);
        assert!(!drawn.is_empty());

        let cleared = engine.clear();
        assert_eq!(cleared.len(), drawn.len());
        assert!(cleared.iter().all(|u| u.is_clear()));

        // A second clear (nothing left to blank) must produce nothing.
        assert!(engine.clear().is_empty());
    }

    #[test]
    fn clear_deletes_layer_files() {
        let resolved = resolve(&LockScreen::default());
        let outs = geoms(&[("eDP-1", 800, 600)]);
        let mut engine = engine_for(HostKind::Kde, &resolved, &outs);
        let dir = test_dir("layerfiles-clear");
        engine.set_targets(LockTargets::LayerFiles(dir.clone()));
        engine.set_outputs(&outs);
        engine.tick(None);
        assert!(dir.join("layer.png").exists());

        engine.clear();
        assert!(!dir.join("layer.png").exists());
        assert!(!dir.join("layer-eDP-1.png").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- bgra_to_rgba_image ------------------------------------------------

    #[test]
    fn bgra_to_rgba_unpremultiplies_and_swaps_channel_order() {
        // A fully opaque red pixel: premultiplied and straight are identical.
        let opaque_red = Bgra {
            w: 1,
            h: 1,
            data: vec![0, 0, 255, 255], // B=0 G=0 R=255 A=255
        };
        let img = bgra_to_rgba_image(&opaque_red);
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);

        // 50% alpha red, premultiplied: R channel is halved before alpha.
        let translucent_red = Bgra {
            w: 1,
            h: 1,
            data: vec![0, 0, 128, 128],
        };
        let img = bgra_to_rgba_image(&translucent_red);
        let p = img.get_pixel(0, 0).0;
        assert_eq!(p[3], 128);
        assert!(
            p[0] > 240,
            "unpremultiplying 128/128 should recover ~full red, got {p:?}"
        );
        assert_eq!(p[1], 0);
        assert_eq!(p[2], 0);

        // Fully transparent: must not divide by zero or panic.
        let clear = Bgra {
            w: 1,
            h: 1,
            data: vec![10, 20, 30, 0],
        };
        let img = bgra_to_rgba_image(&clear);
        assert_eq!(img.get_pixel(0, 0).0[3], 0);
    }
}
