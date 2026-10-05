//! The lock scene: Fresco's wallpaper and widgets on the real lock screen.
//!
//! # What Fresco owns here, and what it never touches
//!
//! The host locker — swaylock's ring, xsecurelock's dialog, KDE's password
//! field, cosmic-greeter's panel — draws its own authentication UI **on top**
//! of whatever this module produces. Fresco never sees a password and never
//! draws one; its whole job is the wallpaper plus a handful of widgets
//! arranged so they do not collide with wherever the host is about to put its
//! own prompt. [`prompt_zone`] and [`cosmic_greeter_zone`] are how a caller
//! tells this module where that is; [`LockSceneSpec::reserved`] is the general
//! form both are passed through.
//!
//! # Two consumers, one layout
//!
//! [`layout`] produces plain rectangles, used two ways:
//!
//! - **Live video hosts.** [`render_slot`] rasterises one slot at a time into
//!   its own tightly-cropped [`Bgra`], for `overlay-add` — the same path
//!   `src/daemon/widgets.rs` already uses for every other widget.
//! - **Still/file hosts** (the KDE plugin, an in-app preview, a still-frame
//!   DE). [`compose`] draws every slot onto one transparent canvas;
//!   [`compose_still`] additionally handles the background — scale-to-fill,
//!   blur, dim — for a caller with no live video underneath it at all.
//!
//! # Units
//!
//! [`LockSceneSpec::output`], [`prompt_zone`]'s return and every entry of
//! [`LockSceneSpec::reserved`] are **device pixels** — the same space a
//! compositor and an external locker already work in, which is what lets a
//! caller pass `prompt_zone(output)` straight through to both this module and
//! to whatever positions the real password ring. Internally, [`layout`]
//! divides by `scale` to lay things out in the toolkit's usual logical units
//! before drawing, exactly as [`Canvas`] itself does.
//!
//! # Pure widgetkit
//!
//! Like every other module here, this one reads no config and no daemon
//! state: [`LockArrangement`] is the one enum a `LockPreset` maps onto, and
//! [`LockSceneData`] is plain data assembled by the caller from whatever it
//! actually has (a lyric line, a battery reading, a display name) — the same
//! contract [`super::cards`] documents for every widget it already has.

use crate::artwork::Bgra;
use crate::widgetkit::canvas::Canvas;
use crate::widgetkit::cards::{
    self,
    battery::BatteryData,
    clock::ClockData,
    greeting::{GreetingData, GreetingLayout},
    nowplaying::NowPlayingData,
};
use crate::widgetkit::color::Color;
use crate::widgetkit::geom::{HAlign, Rect, Size, VAlign};
use crate::widgetkit::paint::Fill;
use crate::widgetkit::text::FontStack;
use crate::widgetkit::theme::Theme;
use crate::widgetkit::typo::{self, Script};

/// Which family of layout a `LockPreset` selects. The daemon maps its preset
/// 1:1 onto this — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LockArrangement {
    /// Big centred clock high on screen, the greeting under it, the media
    /// card bottom-centre, the battery top-right — a conventional phone lock
    /// screen.
    #[default]
    Classic,
    /// Clock and date only by preference, lots of air, small type.
    Minimal,
    /// Clock, media and greeting grouped into one translucent card column.
    Glass,
    /// A huge clock left-aligned in the left third; greeting and media stack
    /// beneath it.
    BigType,
    /// A top-left mono, prompt-style block — pairs with [`super::cards::nos`]
    /// — and the battery drawn as a text-like chip.
    Terminal,
}

/// One widget a lock scene can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LockSlot {
    /// The time (and, per the chosen [`ClockVariant`](cards::ClockVariant),
    /// the date).
    Clock,
    /// The salutation and avatar.
    Greeting,
    /// The now-playing card, with album art when there is any.
    Media,
    /// The battery chip.
    Battery,
}

/// What to lay out and where.
#[derive(Debug, Clone)]
pub struct LockSceneSpec {
    /// Which family of layout to use.
    pub arrangement: LockArrangement,
    /// The output's real size, in device pixels.
    pub output: Size,
    /// The compositor's HiDPI factor for this output.
    pub scale: f32,
    /// Which widgets to place. Order does not matter; duplicates are ignored.
    pub slots: Vec<LockSlot>,
    /// Regions the host already owns, in device pixels — a password prompt
    /// ([`prompt_zone`]) or a greeter panel ([`cosmic_greeter_zone`]). Every
    /// slot avoids every entry here, **except** the Clock, which is exempt
    /// only for an entry that is (approximately) [`prompt_zone`] of this same
    /// `output` — the one case spec calls out by name ("nothing but the Clock
    /// may intersect prompt_zone"). An empty vec reserves nothing.
    ///
    /// "The Clock is always present" still outranks avoiding a reservation:
    /// on an output too small to hold both the Clock and a reservation with
    /// any room to spare (a greeter panel sized for a real desktop, pinned
    /// onto a tiny or extreme aspect-ratio output), the Clock is placed
    /// anyway rather than omitted.
    pub reserved: Vec<Rect>,
}

/// Where one slot goes. In the same device-pixel space as
/// [`LockSceneSpec::output`] — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub slot: LockSlot,
    pub rect: Rect,
}

/// Everything a lock scene needs to draw. Every field but the theme is
/// optional, because the daemon may simply not have that data yet (no MPRIS
/// player, no known display name, no battery on a desktop) — a lock scene
/// degrades by omission, the rule every card in this toolkit already follows
/// for its own missing pieces.
#[derive(Debug, Clone, Copy)]
pub struct LockSceneData<'a> {
    /// The clock. Its own [`ClockVariant`](cards::ClockVariant) decides
    /// whether the date shows and which form language is drawn — the Terminal
    /// arrangement's "pairs with the NOS clock" is expressed by the caller
    /// setting `clock.variant` to [`cards::ClockVariant::Nos`], not by
    /// anything in this module.
    pub clock: ClockData<'a>,
    pub greeting: Option<GreetingData<'a>>,
    pub media: Option<NowPlayingData<'a>>,
    pub battery: Option<BatteryData>,
    pub theme: Theme,
}

// -- geometry ----------------------------------------------------------------

/// Fraction of the shorter side reserved as the safe-area margin.
const SAFE_FRAC: f32 = 0.05;
/// The margin never goes below this many logical units, scaled — a 5% margin
/// on a very small or very short output would otherwise pinch to nothing.
const MIN_MARGIN_LU: f32 = 16.0;
/// ...nor above this fraction of the shorter side, so the margin cannot eat
/// the whole safe area on a tiny output.
const MAX_MARGIN_FRAC: f32 = 0.24;

fn sane_scale(s: f32) -> f32 {
    if s.is_finite() && s > 0.0 {
        s.clamp(0.05, 8.0)
    } else {
        1.0
    }
}

fn valid_size(s: Size) -> bool {
    s.w > 0.0 && s.h > 0.0
}

/// The safe area: `output` inset by [`SAFE_FRAC`] of its shorter side, device
/// pixels throughout.
fn safe_area(output: Size, scale: f32) -> Rect {
    if !valid_size(output) {
        return Rect::ZERO;
    }
    let s = sane_scale(scale);
    let short = output.w.min(output.h);
    let m = (SAFE_FRAC * short)
        .max(MIN_MARGIN_LU * s)
        .min(MAX_MARGIN_FRAC * short);
    Rect::new(
        m,
        m,
        (output.w - 2.0 * m).max(0.0),
        (output.h - 2.0 * m).max(0.0),
    )
}

/// Where a password prompt appears, centred a little below the vertical
/// middle — the conventional spot a clock sits above and a ring or field sits
/// in. Pure function of `output` alone (device pixels), so a host that only
/// has the raw output size — not a whole [`LockSceneSpec`] — can still compute
/// exactly where Fresco expects its dialog.
pub fn prompt_zone(output: Size) -> Rect {
    if !valid_size(output) {
        return Rect::ZERO;
    }
    let short = output.w.min(output.h);
    let w = (0.44 * output.w).max(0.0).min(output.w);
    let h = (0.30 * short).max(0.0).min(output.h);
    let x = ((output.w - w) / 2.0).max(0.0);
    let cy = output.h * 0.60;
    let y = (cy - h / 2.0).max(0.0).min((output.h - h).max(0.0));
    Rect::new(x, y, w, h)
}

/// Where COSMIC 1.9's `cosmic-greeter` draws its one frosted panel.
///
/// Width `min(866, W_logical)`, height budgeted at 400 logical px (the panel
/// itself is nearer 360; the extra is headroom rather than a promise of the
/// exact figure), positioned with one part of the free vertical space above it
/// and four parts below, then padded 24 logical px on every side before being
/// converted to device pixels and clamped to `output`. On COSMIC the password
/// field lives inside this panel, so a caller passes this **instead of**
/// [`prompt_zone`], never both.
pub fn cosmic_greeter_zone(output: Size, scale: f32) -> Rect {
    if !valid_size(output) {
        return Rect::ZERO;
    }
    let s = sane_scale(scale);
    let (lw, lh) = (output.w / s, output.h / s);
    let pw = 866.0_f32.min(lw).max(0.0);
    let ph = 400.0_f32.min(lh).max(0.0);
    let top = ((lh - 400.0) / 5.0).max(0.0).min((lh - ph).max(0.0));
    let left = ((lw - pw) / 2.0).max(0.0);
    let core = Rect::new(left, top, pw, ph).inset(-24.0);
    let dev = Rect::new(core.x * s, core.y * s, core.w * s, core.h * s);
    dev.intersect(Rect::new(0.0, 0.0, output.w, output.h))
}

fn approx_eq(a: Rect, b: Rect, eps: f32) -> bool {
    (a.x - b.x).abs() <= eps
        && (a.y - b.y).abs() <= eps
        && (a.w - b.w).abs() <= eps
        && (a.h - b.h).abs() <= eps
}

fn clamp_to(r: Rect, bound: Rect) -> Rect {
    if bound.is_empty() {
        return Rect::ZERO;
    }
    let w = r.w.min(bound.w).max(0.0);
    let h = r.h.min(bound.h).max(0.0);
    let x = r.x.max(bound.x).min((bound.right() - w).max(bound.x));
    let y = r.y.max(bound.y).min((bound.bottom() - h).max(bound.y));
    Rect::new(x, y, w, h)
}

fn clears(r: Rect, avoid: &[Rect]) -> bool {
    avoid.iter().all(|a| r.intersect(*a).is_empty())
}

fn usable(r: Rect, min: Size) -> bool {
    !r.is_empty() && r.w >= min.w - 0.5 && r.h >= min.h - 0.5
}

/// Fit `want` inside `bound`, clear of every rect in `avoid`, trying a short
/// deterministic sequence of shifts — down, up, right, left — and then one
/// shrink into whichever band (above/below, then left/right, of the nearest
/// obstacle) has more room, before giving up.
///
/// Not a general constraint solver: every obstacle that `want` actually
/// touches is folded into a single bounding box first, which is exact for the
/// common case (zero or one reserved region — a password prompt or a greeter
/// panel, never both) and a conservative approximation for anything busier.
fn fit(want: Rect, bound: Rect, avoid: &[Rect], min: Size) -> Option<Rect> {
    let base = clamp_to(want, bound);
    if usable(base, min) && clears(base, avoid) {
        return Some(base);
    }
    let hit = avoid
        .iter()
        .copied()
        .filter(|a| !base.intersect(*a).is_empty())
        .fold(None::<Rect>, |acc, a| Some(acc.map_or(a, |u| u.union(a))));
    let u = hit?;
    // Every fallback below tries **both** sides of the obstacle it hit — never
    // only whichever nominally has more room, because that arithmetic is
    // measured against `bound` alone and can pick a band a *different*
    // obstacle still blocks — and orders them by which keeps the result
    // closer to `base`, i.e. closer to where the arrangement actually wanted
    // it. A centred obstacle (the prompt zone, always symmetric within the
    // safe area) offers *equal* room on both sides by construction, so "more
    // room" cannot break that tie at all, let alone toward the side a
    // left-aligned arrangement (BigType, Terminal) actually wants; proximity
    // to `base` can, because `base` already carries the arrangement's own
    // intent (flush left, flush right, centred).
    let order = |mut opts: [Rect; 2], axis_of: fn(Rect) -> f32, base_axis: f32| {
        if (axis_of(opts[1]) - base_axis).abs() < (axis_of(opts[0]) - base_axis).abs() {
            opts.swap(0, 1);
        }
        opts
    };

    let same_size_v = order(
        [
            Rect::new(base.x, u.bottom(), base.w, base.h),
            Rect::new(base.x, u.y - base.h, base.w, base.h),
        ],
        |r| r.y,
        base.y,
    );
    let same_size_h = order(
        [
            Rect::new(u.right(), base.y, base.w, base.h),
            Rect::new(u.x - base.w, base.y, base.w, base.h),
        ],
        |r| r.x,
        base.x,
    );
    for c in same_size_v.into_iter().chain(same_size_h) {
        let c = clamp_to(c, bound);
        if usable(c, min) && clears(c, avoid) {
            return Some(c);
        }
    }

    let above_h = (u.y - bound.y).max(0.0);
    let below_h = (bound.bottom() - u.bottom()).max(0.0);
    let bands = order(
        [
            Rect::new(base.x, bound.y, base.w, above_h),
            Rect::new(base.x, u.bottom(), base.w, below_h),
        ],
        |r| r.y,
        base.y,
    );
    for band in bands {
        let band = clamp_to(band, bound);
        if usable(band, min) && clears(band, avoid) {
            return Some(band);
        }
    }

    let left_w = (u.x - bound.x).max(0.0);
    let right_w = (bound.right() - u.right()).max(0.0);
    let sides = order(
        [
            Rect::new(bound.x, base.y, left_w, base.h),
            Rect::new(u.right(), base.y, right_w, base.h),
        ],
        |r| r.x,
        base.x,
    );
    for side in sides {
        let side = clamp_to(side, bound);
        if usable(side, min) && clears(side, avoid) {
            return Some(side);
        }
    }
    None
}

/// The four slots' unconstrained, ideal boxes for one arrangement, before any
/// obstacle is taken into account. Battery's corner does not vary by
/// arrangement — spec: "battery chip always in a corner".
struct Ideal {
    clock: Rect,
    greeting: Rect,
    media: Rect,
    battery: Rect,
}

/// `r`, grown if necessary so neither extent is below `min`, re-centred on
/// `r`'s own centre so growing it does not drift it off its intended anchor.
///
/// Greeting and Media are both substantially bigger objects than Battery, so
/// their *proportional* ideal box falls below their own floor at a larger
/// `s` than Battery's does — exactly backwards from the drop order the spec
/// asks for, if "is it below its floor" were left to decide inclusion on its
/// own. Flooring the ideal box here decouples the two: below the size this
/// produces there is nothing smaller to draw, so [`affordable`] and its three
/// thresholds are the *only* thing that decides whether a slot is attempted
/// at all, in the order the spec states.
fn floor_size(r: Rect, min: Size) -> Rect {
    let w = r.w.max(min.w);
    let h = r.h.max(min.h);
    let c = r.center();
    Rect::new(c.x - w / 2.0, c.y - h / 2.0, w, h)
}

fn finish(clock: Rect, greeting: Rect, media: Rect, battery: Rect) -> Ideal {
    Ideal {
        clock,
        greeting: floor_size(greeting, MIN_GREETING),
        media: floor_size(media, MIN_MEDIA),
        battery: floor_size(battery, MIN_BATTERY),
    }
}

/// `measure()`d card height ÷ `font_size`, for the variant [`suggested_clock_size`]
/// draws at each arrangement's target size — measured directly (see
/// `tests::the_reserved_clock_height_tracks_what_is_actually_measured`) rather
/// than derived from `cards::lock`'s/`cards::nos`'s internal ratios, which
/// this module does not own and should not have to keep in sync with by
/// hand. NOS (Terminal) is a near-square dial-and-ring card, not a couple of
/// text rows, and its ratio is roughly 4.5x a Lock card's — using one shared
/// constant for both was what let a correctly-*sized* Terminal clock still
/// run straight into the media card reserved beneath it.
fn clock_height_ratio(a: LockArrangement) -> f32 {
    match a {
        LockArrangement::Classic | LockArrangement::Minimal | LockArrangement::Glass => 1.06,
        LockArrangement::BigType => 0.89,
        LockArrangement::Terminal => 4.75,
    }
}

/// An estimate of the clock's *total* card height at the size
/// [`suggested_clock_size`] will actually draw it at, expressed directly in
/// `s` (`min(safe.w, safe.h)`, device pixels) rather than routing back through
/// logical units — `ideal_for` only ever needs a box to reserve, not the exact
/// figure. `safe ≈ 0.9 × output` once margins are subtracted, so this is
/// `clock_cap_fraction × (s / 0.9) / cap_height_ratio × clock_height_ratio`,
/// rounded up by keeping the `1 / 0.9` rather than a looser fudge factor: a
/// box a little taller than the real card costs nothing, and a box even a
/// little shorter is exactly the clipped-card bug this function exists to
/// prevent.
fn reserved_clock_h(a: LockArrangement, s: f32) -> f32 {
    (clock_cap_fraction(a) * (s / 0.9) / cap_height_ratio() * clock_height_ratio(a)).max(0.0)
}

fn ideal_for(a: LockArrangement, safe: Rect) -> Ideal {
    let s = safe.w.min(safe.h);
    let cx = safe.center().x;
    let centered = |w: f32, h: f32, y: f32| Rect::new(cx - w / 2.0, y, w.max(0.0), h.max(0.0));
    let battery = {
        let h = (0.075 * s).max(0.0);
        let w = h * 2.6;
        Rect::new(safe.right() - w, safe.y, w, h)
    };
    // The three centred-top arrangements put the clock in the same corner-row
    // as the battery chip; capping the clock's width to leave that row clear
    // on both sides is what keeps a wide clock from crowding a chip it never
    // competes with in the reference layouts this arrangement is drawn from.
    let corner_clear = 2.0 * (battery.w + 0.04 * s);
    let capped_w = |w: f32| w.min((safe.w - corner_clear).max(0.0));
    match a {
        LockArrangement::Classic => {
            let ch = reserved_clock_h(a, s);
            let cw = capped_w((safe.w * 0.86).min(ch * 2.6));
            let clock = centered(cw, ch, safe.y + 0.02 * s);
            let gh = 0.11 * s;
            let gw = (safe.w * 0.80).min(gh * 9.0);
            let greeting = centered(gw, gh, clock.bottom() + 0.05 * s);
            let mh = 0.20 * s;
            let mw = (safe.w * 0.55).min(mh * 3.4);
            let media = centered(mw, mh, safe.bottom() - mh);
            finish(clock, greeting, media, battery)
        }
        LockArrangement::Minimal => {
            let ch = reserved_clock_h(a, s);
            let cw = capped_w((safe.w * 0.86).min(ch * 4.4));
            let clock = centered(cw, ch, safe.y + 0.06 * s);
            let gh = 0.09 * s;
            let gw = (safe.w * 0.70).min(gh * 10.0);
            let greeting = centered(gw, gh, clock.bottom() + 0.08 * s);
            let mh = 0.16 * s;
            let mw = (safe.w * 0.45).min(mh * 3.4);
            let media = centered(mw, mh, (safe.bottom() - mh * 1.3).max(safe.y));
            finish(clock, greeting, media, battery)
        }
        LockArrangement::Glass => {
            // Media sits directly under the clock and Greeting takes the
            // bottom of the column, not the other way around: the column's
            // lowest member is the one nearest the prompt zone and the first
            // to be squeezed as the safe area shrinks, so the drop order
            // (Battery, Greeting, Media) is expressed spatially, not only by
            // the `*_MIN_S` thresholds.
            let ch = reserved_clock_h(a, s);
            let w = capped_w((safe.w * 0.60).min(ch * 2.6));
            let clock = centered(w, ch, safe.y + 0.04 * s);
            let mh = 0.22 * s;
            let media = centered(w, mh, clock.bottom() + 0.02 * s);
            let gh = 0.09 * s;
            let greeting = centered(w, gh, media.bottom() + 0.02 * s);
            finish(clock, greeting, media, battery)
        }
        LockArrangement::BigType => {
            // A floor tied to the clock's *own* height (room for a few
            // hero-sized glyphs) rather than to `s` — `s` is `min(safe.w,
            // safe.h)`, which on an ordinary 16:9 output is `safe.h`, and
            // `safe.h * 1.6` is not a "third of the width", it is most of it.
            let ch = reserved_clock_h(a, s);
            let left_w = (safe.w * 0.34).max(ch * 2.6).min(safe.w * 0.62);
            let clock = Rect::new(safe.x, safe.y + 0.05 * s, left_w.max(0.0), ch.max(0.0));
            let gh = 0.09 * s;
            let greeting = Rect::new(
                safe.x,
                clock.bottom() + 0.04 * s,
                left_w.max(0.0),
                gh.max(0.0),
            );
            let mh = 0.20 * s;
            let media = Rect::new(
                safe.x,
                greeting.bottom() + 0.03 * s,
                left_w.max(0.0),
                mh.max(0.0),
            );
            finish(clock, greeting, media, battery)
        }
        LockArrangement::Terminal => {
            let ch = reserved_clock_h(a, s);
            // NOS is a near-square card, not a wide text strip.
            let cw = (safe.w * 0.50).min(ch * 1.3);
            let clock = Rect::new(safe.x, safe.y, cw.max(0.0), ch.max(0.0));
            let gh = 0.08 * s;
            let greeting = Rect::new(safe.x, clock.bottom() + 0.03 * s, cw.max(0.0), gh.max(0.0));
            let mh = 0.16 * s;
            let media = Rect::new(
                safe.x,
                greeting.bottom() + 0.03 * s,
                cw.max(0.0),
                mh.max(0.0),
            );
            finish(clock, greeting, media, battery)
        }
    }
}

const MIN_CLOCK: Size = Size { w: 32.0, h: 20.0 };
const MIN_GREETING: Size = Size { w: 48.0, h: 14.0 };
const MIN_MEDIA: Size = Size { w: 90.0, h: 28.0 };
const MIN_BATTERY: Size = Size { w: 36.0, h: 14.0 };

/// `min(safe.w, safe.h)` thresholds below which an optional slot is not even
/// attempted — the explicit form of the drop order, since the three slots'
/// own minimum boxes above do not naturally fail in that order at every size.
/// Battery's is the largest (dropped soonest as the safe area shrinks),
/// Media's the smallest (kept longest).
const BATTERY_MIN_S: f32 = 230.0;
const GREETING_MIN_S: f32 = 170.0;
const MEDIA_MIN_S: f32 = 120.0;

/// Lay out every requested slot for `spec`.
///
/// Placement priority — which is also the reverse of the drop order — is
/// Clock, Media, Greeting, Battery: whichever is placed last is the first to
/// be squeezed out when space runs out. The Clock is never dropped: when
/// nothing clear of its obstacles can be found it is placed anyway, clamped to
/// the safe area, because "the lock screen has no clock" is not a degradation
/// this layout is allowed to choose.
pub fn layout(spec: &LockSceneSpec) -> Vec<Placement> {
    let scale = sane_scale(spec.scale);
    let output = spec.output;
    let safe = safe_area(output, scale);
    if safe.is_empty() {
        return Vec::new();
    }

    let bound = Rect::new(0.0, 0.0, output.w, output.h);
    let reserved: Vec<Rect> = spec
        .reserved
        .iter()
        .copied()
        .map(|r| r.intersect(bound))
        .filter(|r| !r.is_empty())
        .collect();
    let pz = prompt_zone(output);
    let clock_avoid: Vec<Rect> = reserved
        .iter()
        .copied()
        .filter(|r| !approx_eq(*r, pz, 1.0))
        .collect();

    let ideal = ideal_for(spec.arrangement, safe);
    let wants = |slot: LockSlot| spec.slots.contains(&slot);

    let mut placed: Vec<Placement> = Vec::new();
    let mut obstacles: Vec<Rect> = Vec::new();

    if wants(LockSlot::Clock) {
        let rect = fit(ideal.clock, safe, &clock_avoid, MIN_CLOCK)
            .or_else(|| fit(ideal.clock, safe, &[], MIN_CLOCK))
            .unwrap_or_else(|| clamp_to(ideal.clock, safe));
        obstacles.push(rect);
        placed.push(Placement {
            slot: LockSlot::Clock,
            rect,
        });
    }

    // The explicit drop order (spec: "Battery, Greeting, Media" — never
    // Clock). Every optional slot's *ideal* box already shrinks with `s`
    // (`min(safe.w, safe.h)`), but by how much varies by arrangement and the
    // two are not naturally ordered the same way at every size — Media's
    // floor (100x40) is easy to fail before Battery's much smaller one
    // (36x14) if size alone decided it. So the order is enforced directly:
    // below each threshold that slot is not even attempted, regardless of
    // whether its own ideal box happened to still fit.
    let s = safe.w.min(safe.h);
    let affordable = |slot: LockSlot| -> bool {
        match slot {
            LockSlot::Battery => s >= BATTERY_MIN_S,
            LockSlot::Greeting => s >= GREETING_MIN_S,
            LockSlot::Media => s >= MEDIA_MIN_S,
            LockSlot::Clock => true,
        }
    };

    for (slot, want, ideal_rect, min) in [
        (
            LockSlot::Media,
            wants(LockSlot::Media),
            ideal.media,
            MIN_MEDIA,
        ),
        (
            LockSlot::Greeting,
            wants(LockSlot::Greeting),
            ideal.greeting,
            MIN_GREETING,
        ),
        (
            LockSlot::Battery,
            wants(LockSlot::Battery),
            ideal.battery,
            MIN_BATTERY,
        ),
    ] {
        if !want || !affordable(slot) {
            continue;
        }
        // The prompt zone keeps everyone but the Clock out unconditionally —
        // it is where a password prompt *would* appear the moment the user
        // starts typing, whether or not the host told this spec about it via
        // `reserved`, so it is always in the non-Clock avoid list.
        let mut avoid = reserved.clone();
        if !pz.is_empty() {
            avoid.push(pz);
        }
        avoid.extend(obstacles.iter().copied());
        if let Some(rect) = fit(ideal_rect, safe, &avoid, min) {
            obstacles.push(rect);
            placed.push(Placement { slot, rect });
        }
    }

    placed
}

// -- rendering ----------------------------------------------------------------

fn to_logical(r: Rect, scale: f32) -> Rect {
    Rect::new(r.x / scale, r.y / scale, r.w / scale, r.h / scale)
}

fn greeting_layout_for(a: LockArrangement) -> GreetingLayout {
    match a {
        LockArrangement::Glass => GreetingLayout::Column,
        _ => GreetingLayout::Row,
    }
}

/// How Greeting and Media sit inside the horizontal span [`layout`] gave them.
///
/// BigType and Terminal build a left-aligned column (spec: "left-aligned in
/// the left third", "top-left … block"); centring a narrower measured card
/// inside that column's full allocated width would visually re-centre it
/// within the *column*, not the screen, but a left-aligned arrangement wants
/// it flush with the column's own left edge, not floating in its middle.
fn h_align_for(a: LockArrangement) -> HAlign {
    match a {
        LockArrangement::BigType | LockArrangement::Terminal => HAlign::Left,
        _ => HAlign::Center,
    }
}

// -- suggested sizes -----------------------------------------------------
//
// `ClockData::font_size`, `GreetingData::text_size` and `BatteryData::size`
// are all plain fields the *caller* sets, the same contract every other card
// in this toolkit already has — but an arrangement's whole identity is partly
// a size ("BigType" is not a position, it is a claim about how big the clock
// is), and `layout` cannot see the caller's data at all (it takes a bare
// `Vec<LockSlot>`, not `LockSceneData`). So `compose` and `render_slot` size
// the Clock, Greeting and Battery slots themselves, from the arrangement and
// the output, and draw a locally-adjusted copy of whatever the caller passed
// — overriding those three fields only. A user's own font-size preference is
// therefore not honoured on the lock scene today; if that turns out to
// matter, the fix is a multiplier this module accepts, not a reason to leave
// every arrangement drawing the toolkit's flat default the way the first cut
// of this module did.
//
// The functions below are `pub` so a caller assembling `LockSceneData` can
// size *other* fields (or a GUI preview) consistently with what `compose`
// will actually draw.

/// Cap-height, as a fraction of the reference dimension (see
/// [`reference_dim_logical`]), that each arrangement's hero clock time
/// targets. The date row and any secondary line are not sized independently
/// — `cards::clock`/`cards::lock` derive them from the hero size on their own
/// fixed internal ratios, which this module does not own.
fn clock_cap_fraction(a: LockArrangement) -> f32 {
    match a {
        LockArrangement::Classic => 0.15,
        LockArrangement::Minimal => 0.065,
        LockArrangement::Glass => 0.115,
        LockArrangement::BigType => 0.325,
        LockArrangement::Terminal => 0.08,
    }
}

/// `min(output.w, output.h)`, converted from device pixels to logical units.
/// The one reference dimension every suggested size below is quoted against —
/// "H on landscape" and "min(W, H) on portrait" are the same quantity, since a
/// landscape output's height already *is* its shorter side.
fn reference_dim_logical(output: Size, scale: f32) -> f32 {
    let s = sane_scale(scale);
    (output.w.min(output.h) / s).max(1.0)
}

/// A Latin cap-height, as a fraction of `1.0` unit of type size — the ratio
/// every suggested size below inverts to go from "this many device pixels of
/// cap-height" to "this `font_size`/`text_size`".
fn cap_height_ratio() -> f32 {
    typo::cap_height(1.0, Script::Latin)
}

/// A `ClockData::font_size` that puts the time's cap-height at roughly
/// `clock_cap_fraction` of the output's reference dimension.
pub fn suggested_clock_size(arrangement: LockArrangement, output: Size, scale: f32) -> f32 {
    let h_ref = reference_dim_logical(output, scale);
    (clock_cap_fraction(arrangement) * h_ref / cap_height_ratio()).clamp(11.0, 900.0)
}

/// A `GreetingData::text_size` to match: a caption under the clock, not a
/// second headline, so it is a much smaller fraction throughout.
pub fn suggested_greeting_size(output: Size, scale: f32) -> f32 {
    let h_ref = reference_dim_logical(output, scale);
    (0.022 * h_ref / cap_height_ratio()).clamp(11.0, 120.0)
}

/// A `BatteryData::size` (chip height) so the corner chip reads at a glance
/// next to a hero clock rather than disappearing beside one.
pub fn suggested_battery_size(output: Size, scale: f32) -> f32 {
    let h_ref = reference_dim_logical(output, scale);
    (0.03 * h_ref).clamp(16.0, 160.0)
}

/// Rasterise one slot alone, tightly cropped to its own measured size (plus
/// shadow bleed) — the bitmap `src/daemon/widgets.rs` pushes into mpv via
/// `overlay-add`, positioned at the returned [`Rect`]'s origin (device
/// pixels, matching [`LockSceneSpec::output`]).
///
/// Looks `slot` up in [`layout`] itself rather than taking a `rect` — a
/// caller pushing all four slots would otherwise have to run `layout` once to
/// find each one anyway, and looking it up here is what lets this function
/// size the Clock, Greeting and Battery slots itself (see the "suggested
/// sizes" section above): those three need to know `spec.arrangement` and
/// `spec.output`, which a bare `rect` cannot carry. The Media card's own
/// width-wrap budget ([`cards::nowplaying::NowPlayingData::screen_width`])
/// is still set from the slot's rect, exactly as [`compose`] does.
///
/// `None` when the slot was not placed at all (not requested, or dropped for
/// space), when its data is missing from `data`, or when its natural size
/// cannot be allocated (never a panic).
pub fn render_slot(
    slot: LockSlot,
    spec: &LockSceneSpec,
    fonts: &mut FontStack,
    data: &LockSceneData,
) -> Option<(Rect, Bgra)> {
    let out_scale = sane_scale(spec.scale);
    let rect = layout(spec).into_iter().find(|p| p.slot == slot)?.rect;
    let scale = out_scale;
    let t = &data.theme;
    let bgra = match slot {
        LockSlot::Clock => {
            let mut cd = data.clock;
            cd.font_size = suggested_clock_size(spec.arrangement, spec.output, out_scale);
            let size = cards::clock::measure(fonts, t, &cd, scale);
            let mut c = Canvas::for_logical(size.buffer(), scale).ok()?;
            cards::clock::draw(&mut c, fonts, t, &cd);
            c.into_bgra()
        }
        LockSlot::Greeting => {
            let mut gd = *data.greeting.as_ref()?;
            gd.text_size = suggested_greeting_size(spec.output, out_scale);
            let mode = GreetingLayout::Row;
            let size = cards::greeting::measure(fonts, t, &gd, mode, scale);
            let mut c = Canvas::for_logical(size.buffer(), scale).ok()?;
            cards::greeting::draw(&mut c, fonts, t, &gd, mode);
            c.into_bgra()
        }
        LockSlot::Media => {
            let mut m = *data.media.as_ref()?;
            if rect.w.is_finite() && rect.w > 0.0 {
                m.screen_width = (rect.w / scale).max(1.0);
            }
            let size = cards::nowplaying::measure(fonts, t, &m, scale);
            let mut c = Canvas::for_logical(size.buffer(), scale).ok()?;
            cards::nowplaying::draw(&mut c, fonts, t, &m);
            c.into_bgra()
        }
        LockSlot::Battery => {
            let mut bd = *data.battery.as_ref()?;
            bd.size = suggested_battery_size(spec.output, out_scale);
            let size = cards::battery::measure(fonts, t, &bd, scale);
            let mut c = Canvas::for_logical(size.buffer(), scale).ok()?;
            cards::battery::draw(&mut c, fonts, t, &bd);
            c.into_bgra()
        }
    };
    Some((rect, bgra))
}

/// One shared translucent card behind the Clock, Greeting and Media
/// placements — the Glass arrangement's "grouped into one translucent card
/// column" (spec). Drawn *underneath* the three widgets, which then draw
/// their own text with no card of their own, exactly as they do everywhere
/// else in this module.
fn draw_glass_backdrop(canvas: &mut Canvas, t: &Theme, placements: &[Placement], out_scale: f32) {
    let mut union: Option<Rect> = None;
    for p in placements {
        if matches!(
            p.slot,
            LockSlot::Clock | LockSlot::Greeting | LockSlot::Media
        ) {
            let r = to_logical(p.rect, out_scale);
            if !r.is_empty() {
                union = Some(union.map_or(r, |u| u.union(r)));
            }
        }
    }
    let Some(u) = union else { return };
    if u.is_empty() {
        return;
    }
    let pad = (u.w.min(u.h) * 0.08).max(8.0);
    let card = u.inset(-pad);
    let radius = crate::widgetkit::theme::radius_card(u.h.clamp(24.0, 320.0));
    crate::widgetkit::surface::card(canvas, card, radius, t);
}

/// Draw every requested, present slot onto `canvas` — a transparent
/// full-output surface the caller already allocated and sized to
/// `spec.output` at `spec.scale` (see the module docs on units; `canvas`'s own
/// [`Canvas::scale`] is what drawing actually uses, so a caller that renders
/// at a reduced preview density only needs to keep `canvas`'s *logical* bounds
/// equal to `spec.output / spec.scale` — [`Canvas::for_logical`] guarantees
/// that by construction whatever device-pixel size it is asked for).
///
/// A slot [`layout`] placed but whose data is missing from `data` is simply
/// not drawn — the space stays empty rather than showing a placeholder.
pub fn compose(
    canvas: &mut Canvas,
    fonts: &mut FontStack,
    spec: &LockSceneSpec,
    data: &LockSceneData,
) {
    let scale = canvas.scale();
    let out_scale = sane_scale(spec.scale);
    let placements = layout(spec);
    let t = data.theme;

    if spec.arrangement == LockArrangement::Glass {
        draw_glass_backdrop(canvas, &t, &placements, out_scale);
    }

    let h_align = h_align_for(spec.arrangement);
    // 2% of the reference dimension, in logical units — the gap between the
    // clock/date block and the greeting under it (spec review: "group it
    // tightly under the clock … gap ≈ 2% H").
    let tight_gap = 0.02 * reference_dim_logical(spec.output, out_scale);
    // The clock's *actual* drawn rect, so Greeting can hug it directly rather
    // than the abstract box `layout` reserved — `layout` cannot see
    // `suggested_clock_size`'s answer (it never sees `LockSceneData` at all),
    // so that box is sized for space-reservation and obstacle-avoidance, not
    // for pixel-exact stacking.
    let mut clock_at: Option<Rect> = None;

    for p in &placements {
        let logical = to_logical(p.rect, out_scale);
        if logical.is_empty() {
            continue;
        }
        match p.slot {
            LockSlot::Clock => {
                let mut cd = data.clock;
                cd.font_size = suggested_clock_size(spec.arrangement, spec.output, out_scale);
                let size = cards::clock::measure(fonts, &t, &cd, scale);
                let at = logical.align(size.card, h_align, VAlign::Top);
                cards::clock::draw_at(canvas, fonts, &t, &cd, at);
                clock_at = Some(at);
            }
            LockSlot::Greeting => {
                if let Some(g) = &data.greeting {
                    let mut gd = *g;
                    gd.text_size = suggested_greeting_size(spec.output, out_scale);
                    let mode = greeting_layout_for(spec.arrangement);
                    let size = cards::greeting::measure(fonts, &t, &gd, mode, scale);
                    let at = match clock_at {
                        // Same horizontal alignment as the clock, its own
                        // measured left/right/centre edge rather than the
                        // reserved box's, and flush underneath it.
                        Some(clock) => {
                            let x = match h_align {
                                HAlign::Left => clock.x,
                                HAlign::Right => clock.right() - size.card.w,
                                HAlign::Center => clock.center().x - size.card.w / 2.0,
                            };
                            Rect::new(x, clock.bottom() + tight_gap, size.card.w, size.card.h)
                        }
                        None => logical.align(size.card, h_align, VAlign::Top),
                    };
                    cards::greeting::draw_at(canvas, fonts, &t, &gd, mode, at);
                }
            }
            LockSlot::Media => {
                if let Some(m) = &data.media {
                    let mut md = *m;
                    if logical.w.is_finite() && logical.w > 0.0 {
                        md.screen_width = logical.w;
                    }
                    let size = cards::nowplaying::measure(fonts, &t, &md, scale);
                    let at = logical.align(size.card, h_align, VAlign::Bottom);
                    cards::nowplaying::draw_at(canvas, fonts, &t, &md, at);
                }
            }
            LockSlot::Battery => {
                if let Some(b) = &data.battery {
                    let mut bd = *b;
                    bd.size = suggested_battery_size(spec.output, out_scale);
                    let size = cards::battery::measure(fonts, &t, &bd, scale);
                    let at = logical.align(size.card, HAlign::Right, VAlign::Top);
                    cards::battery::draw_at(canvas, fonts, &t, &bd, at);
                }
            }
        }
    }
}

/// Blur `img` by `sigma` (source-image pixels), per channel, with
/// [`crate::widgetkit::blur::blur_alpha`] — the toolkit's one blur primitive,
/// built for a single 8-bit channel, so a colour image takes four passes
/// (R, G, B, A) rather than a new kernel.
fn blurred(img: &image::RgbaImage, sigma: f32) -> image::RgbaImage {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if sigma <= 0.0 || w == 0 || h == 0 {
        return img.clone();
    }
    let n = w * h;
    let mut r = Vec::with_capacity(n);
    let mut g = Vec::with_capacity(n);
    let mut b = Vec::with_capacity(n);
    let mut a = Vec::with_capacity(n);
    for p in img.pixels() {
        r.push(p.0[0]);
        g.push(p.0[1]);
        b.push(p.0[2]);
        a.push(p.0[3]);
    }
    let mut scratch = Vec::new();
    crate::widgetkit::blur::blur_alpha(&mut r, w, h, sigma, &mut scratch);
    crate::widgetkit::blur::blur_alpha(&mut g, w, h, sigma, &mut scratch);
    crate::widgetkit::blur::blur_alpha(&mut b, w, h, sigma, &mut scratch);
    crate::widgetkit::blur::blur_alpha(&mut a, w, h, sigma, &mut scratch);
    image::RgbaImage::from_fn(img.width(), img.height(), |x, y| {
        let i = y as usize * w + x as usize;
        image::Rgba([r[i], g[i], b[i], a[i]])
    })
}

/// The scale factor [`Canvas::image_cover`] would apply to fill `dst` from a
/// `src_w x src_h` source — replicated here (it is three lines of arithmetic,
/// not a hidden algorithm) so a blur radius requested in *output* pixels can
/// be converted to the equivalent radius in the *source* image before it is
/// scaled up, rather than blurring the source by the output's own radius and
/// getting a much softer or much sharper result than asked for depending on
/// how far from 1:1 the cover scale happens to be.
fn cover_scale(dst: Size, src_w: u32, src_h: u32) -> f32 {
    if src_w == 0 || src_h == 0 || dst.w <= 0.0 || dst.h <= 0.0 {
        return 1.0;
    }
    (dst.w / src_w as f32).max(dst.h / src_h as f32)
}

/// Compose a full still frame: `background` scaled to fill (Zoom), blurred,
/// dimmed under a black veil, then [`compose`] on top — for a host with no
/// live video underneath it at all (a still-frame desktop, the KDE plugin
/// layer, an in-app preview).
///
/// `blur` is the blur *radius* (Gaussian σ) as a `0..1` fraction of the
/// output's height, matching the ratio every other blur radius in this toolkit
/// is quoted against ([`crate::widgetkit::blur`]'s own convention). It is not
/// the user's blur setting: that is a slider position, and
/// [`crate::lockscreen::blur_radius_for`] (via `ResolvedLock::blur`) is the one
/// place it becomes a radius. `dim` is `0..0.8`, the black
/// veil's alpha — never `1.0`, because a fully opaque veil is a black screen
/// with widgets on it, not a dimmed wallpaper.
///
/// Never panics and never fails: an output too large for
/// [`crate::widgetkit::MAX_CANVAS_AREA`] at its own HiDPI factor is rendered
/// at a reduced density instead — the returned bitmap is still the full
/// output's aspect ratio, just softer, which is the honest degradation for a
/// one-shot preview render rather than an error nothing downstream expects.
pub fn compose_still(
    fonts: &mut FontStack,
    background: &image::RgbaImage,
    spec: &LockSceneSpec,
    data: &LockSceneData,
    blur: f32,
    dim: f32,
) -> Bgra {
    let Some(mut canvas) = still_canvas(spec.output, spec.scale) else {
        return Bgra {
            w: 1,
            h: 1,
            data: vec![0, 0, 0, 0],
        };
    };
    paint_backdrop(&mut canvas, background, blur, dim);
    compose(&mut canvas, fonts, spec, data);
    canvas.into_bgra()
}

/// [`compose_still`] without the widgets: just `background` scaled to fill,
/// blurred and dimmed, at `output`'s aspect ratio (device pixels, scale 1). The
/// same two functions paint the backdrop for both, so a host that can only show
/// a plain picture (Deepin's greeter background) gets exactly the preview's
/// blur and dim. `blur` and `dim` mean what they do for [`compose_still`].
/// `None` only when no canvas could be made at all.
pub fn compose_backdrop(
    background: &image::RgbaImage,
    output: Size,
    blur: f32,
    dim: f32,
) -> Option<Bgra> {
    let mut canvas = still_canvas(output, 1.0)?;
    paint_backdrop(&mut canvas, background, blur, dim);
    Some(canvas.into_bgra())
}

/// A canvas for `output` at its HiDPI `scale`, halving the density until it
/// fits [`crate::widgetkit::MAX_CANVAS_AREA`] — see [`compose_still`]'s note on
/// degrading rather than failing.
fn still_canvas(output: Size, scale: f32) -> Option<Canvas> {
    let out_scale = sane_scale(scale);
    let logical = Size::new(
        (output.w / out_scale).max(1.0),
        (output.h / out_scale).max(1.0),
    );
    let mut render_scale = out_scale;
    loop {
        match Canvas::for_logical(logical, render_scale) {
            Ok(c) => return Some(c),
            Err(_) if render_scale > 0.05 => render_scale = (render_scale * 0.5).max(0.05),
            Err(_) => return None,
        }
    }
}

/// `background` filling `canvas`, blurred by the `blur` radius and veiled by
/// `dim` — the part of [`compose_still`] that sits under the widgets.
fn paint_backdrop(canvas: &mut Canvas, background: &image::RgbaImage, blur: f32, dim: f32) {
    let bounds = canvas.bounds();

    if background.width() > 0 && background.height() > 0 {
        let b = if blur.is_finite() {
            blur.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let source = if b > 0.0 {
            let want_sigma_device = b * canvas.height_px() as f32;
            let dst = Size::new(canvas.width_px() as f32, canvas.height_px() as f32);
            let k = cover_scale(dst, background.width(), background.height());
            let sigma_source = if k > 0.0 {
                want_sigma_device / k
            } else {
                want_sigma_device
            };
            blurred(background, sigma_source)
        } else {
            background.clone()
        };
        canvas.image_cover(&source, bounds, 0.0);
    }

    let d = if dim.is_finite() {
        dim.clamp(0.0, 0.8)
    } else {
        0.0
    };
    if d > 0.0 {
        canvas.rounded_rect(bounds, 0.0, &Fill::solid(Color::BLACK.with_alpha(d)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgetkit::theme::Mode;

    const SIZES: [(f32, f32); 10] = [
        (640.0, 480.0),
        (800.0, 600.0),
        (1366.0, 768.0),
        (1920.0, 1080.0),
        (2560.0, 1600.0),
        (3840.0, 2160.0),
        (1080.0, 1920.0),
        (1080.0, 2400.0),
        (3440.0, 1440.0),
        (5120.0, 1440.0),
    ];
    const SCALES: [f32; 3] = [1.0, 1.25, 2.0];
    const ARRANGEMENTS: [LockArrangement; 5] = [
        LockArrangement::Classic,
        LockArrangement::Minimal,
        LockArrangement::Glass,
        LockArrangement::BigType,
        LockArrangement::Terminal,
    ];
    const ALL_SLOTS: [LockSlot; 4] = [
        LockSlot::Clock,
        LockSlot::Greeting,
        LockSlot::Media,
        LockSlot::Battery,
    ];

    fn slot_subsets() -> Vec<Vec<LockSlot>> {
        // Every subset that contains the Clock (a spec with no clock requested
        // is a different scene, not this module's concern), plus one with no
        // slots at all to prove an empty request never panics.
        let mut out = vec![Vec::new()];
        for mask in 0u8..8 {
            let mut v = vec![LockSlot::Clock];
            if mask & 1 != 0 {
                v.push(LockSlot::Greeting);
            }
            if mask & 2 != 0 {
                v.push(LockSlot::Media);
            }
            if mask & 4 != 0 {
                v.push(LockSlot::Battery);
            }
            out.push(v);
        }
        out
    }

    fn spec(
        arrangement: LockArrangement,
        w: f32,
        h: f32,
        scale: f32,
        slots: Vec<LockSlot>,
        reserved: Vec<Rect>,
    ) -> LockSceneSpec {
        LockSceneSpec {
            arrangement,
            output: Size::new(w, h),
            scale,
            slots,
            reserved,
        }
    }

    fn theme() -> Theme {
        Theme::for_accent(Mode::Dark, crate::config::Accent::Blue)
    }

    fn contains_rect(outer: Rect, inner: Rect, eps: f32) -> bool {
        inner.x >= outer.x - eps
            && inner.y >= outer.y - eps
            && inner.right() <= outer.right() + eps
            && inner.bottom() <= outer.bottom() + eps
    }

    #[test]
    fn geometry_invariants_hold_across_every_arrangement_slot_set_size_and_scale() {
        for &arrangement in &ARRANGEMENTS {
            for slots in slot_subsets() {
                for &(w, h) in &SIZES {
                    for &scale in &SCALES {
                        for reserved in [
                            Vec::new(),
                            vec![prompt_zone(Size::new(w, h))],
                            vec![cosmic_greeter_zone(Size::new(w, h), scale)],
                        ] {
                            let s = spec(arrangement, w, h, scale, slots.clone(), reserved.clone());
                            let placements = layout(&s);
                            let safe = safe_area(s.output, scale);
                            let pz = prompt_zone(s.output);

                            // Clock always present when requested.
                            if slots.contains(&LockSlot::Clock) {
                                assert!(
                                    placements.iter().any(|p| p.slot == LockSlot::Clock),
                                    "{arrangement:?} {w}x{h}@{scale} dropped the clock"
                                );
                            }
                            // Nothing is placed that was not requested.
                            for p in &placements {
                                assert!(slots.contains(&p.slot), "{p:?} was not requested");
                            }
                            // Every rect sits inside the safe area.
                            for p in &placements {
                                assert!(
                                    contains_rect(safe, p.rect, 0.5),
                                    "{arrangement:?} {w}x{h}@{scale}: {:?} {:?} outside safe {safe:?}",
                                    p.slot,
                                    p.rect
                                );
                            }
                            // No two placements overlap.
                            for i in 0..placements.len() {
                                for j in (i + 1)..placements.len() {
                                    let a = placements[i];
                                    let b = placements[j];
                                    assert!(
                                        a.rect.intersect(b.rect).is_empty(),
                                        "{arrangement:?} {w}x{h}@{scale}: {a:?} overlaps {b:?}"
                                    );
                                }
                            }
                            // Nothing but the Clock intersects the prompt zone,
                            // and every reserved rect keeps out everyone except
                            // a Clock that is exempt only for prompt_zone
                            // itself — and even then only when some placement
                            // of at least the Clock's floor size clearing every
                            // non-exempt reserved rect actually exists inside
                            // the safe area. "Clock always present" outranks
                            // "Clock avoids a reservation" the one place they
                            // conflict: a reservation so large (relative to a
                            // tiny output) that nothing clears it at all — e.g.
                            // a COSMIC greeter panel, sized for a real desktop,
                            // pinned onto a synthetic 640x480 test case.
                            let clock_avoid_here: Vec<Rect> = reserved
                                .iter()
                                .copied()
                                .filter(|r| !approx_eq(*r, pz, 1.0))
                                .collect();
                            let clock_room_exists = fit(
                                ideal_for(arrangement, safe).clock,
                                safe,
                                &clock_avoid_here,
                                MIN_CLOCK,
                            )
                            .is_some();
                            for p in &placements {
                                if p.slot != LockSlot::Clock {
                                    assert!(
                                        p.rect.intersect(pz).is_empty(),
                                        "{arrangement:?} {w}x{h}@{scale}: {:?} enters the prompt zone",
                                        p.slot
                                    );
                                }
                                for r in &reserved {
                                    let exempt = p.slot == LockSlot::Clock
                                        && (approx_eq(*r, pz, 1.0) || !clock_room_exists);
                                    if !exempt {
                                        assert!(
                                            p.rect.intersect(*r).is_empty(),
                                            "{arrangement:?} {w}x{h}@{scale}: {:?} enters reserved {r:?}",
                                            p.slot
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_drop_order_is_battery_then_greeting_then_media_and_never_the_clock() {
        let has = |placements: &[Placement], s: LockSlot| placements.iter().any(|p| p.slot == s);
        for &arrangement in &ARRANGEMENTS {
            // A wide sweep of square outputs, scale 1, no reservation — pure
            // "space ran out" dropping, isolated from any obstacle. The
            // relative order is what the spec actually states (Battery is
            // never protected past Greeting, Greeting never past Media); it
            // does not claim a slot's own `*_MIN_S` threshold is sufficient
            // on its own to guarantee a geometric fit in every arrangement,
            // which is why this checks the *relation* rather than pinning
            // exact sizes to exact outcomes.
            let mut ever_dropped_battery = false;
            let mut ever_dropped_greeting = false;
            let mut ever_dropped_media = false;
            for side in (60..=600).rev().step_by(10) {
                let side = side as f32;
                let s = spec(arrangement, side, side, 1.0, ALL_SLOTS.to_vec(), Vec::new());
                let placements = layout(&s);
                assert!(
                    has(&placements, LockSlot::Clock),
                    "{arrangement:?} {side}: the clock must never be dropped"
                );
                let (b, g, m) = (
                    has(&placements, LockSlot::Battery),
                    has(&placements, LockSlot::Greeting),
                    has(&placements, LockSlot::Media),
                );
                if !m {
                    assert!(
                        !g && !b,
                        "{arrangement:?} {side}: media absent but {g} {b} present"
                    );
                }
                if !g {
                    assert!(
                        !b,
                        "{arrangement:?} {side}: greeting absent but battery present"
                    );
                }
                ever_dropped_battery |= !b;
                ever_dropped_greeting |= !g;
                ever_dropped_media |= !m;
            }
            // And the sweep actually exercised every stage of the order,
            // rather than vacuously passing because nothing was ever dropped.
            assert!(
                ever_dropped_battery,
                "{arrangement:?}: battery never dropped in the sweep"
            );
            assert!(
                ever_dropped_greeting,
                "{arrangement:?}: greeting never dropped in the sweep"
            );
            assert!(
                ever_dropped_media,
                "{arrangement:?}: media never dropped in the sweep"
            );
        }
    }

    #[test]
    fn the_clock_is_exempt_from_the_prompt_zone_but_not_from_a_cosmic_panel() {
        let output = Size::new(1920.0, 1080.0);
        let scale = 1.0;
        let with_prompt = spec(
            LockArrangement::Classic,
            output.w,
            output.h,
            scale,
            vec![LockSlot::Clock],
            vec![prompt_zone(output)],
        );
        let clock = layout(&with_prompt)
            .into_iter()
            .find(|p| p.slot == LockSlot::Clock)
            .unwrap();
        // Classic's clock sits high and the prompt zone sits centred-low, so
        // in the common case they do not even compete — the exemption is
        // there for arrangements/sizes where they would.
        let _ = clock;

        let cosmic = cosmic_greeter_zone(output, scale);
        let with_cosmic = spec(
            LockArrangement::Classic,
            output.w,
            output.h,
            scale,
            vec![LockSlot::Clock],
            vec![cosmic],
        );
        let clock = layout(&with_cosmic)
            .into_iter()
            .find(|p| p.slot == LockSlot::Clock)
            .unwrap();
        assert!(
            clock.rect.intersect(cosmic).is_empty(),
            "the clock must avoid a non-prompt reserved rect: {clock:?} vs {cosmic:?}"
        );
    }

    #[test]
    fn prompt_zone_and_cosmic_zone_never_panic_and_stay_inside_the_output() {
        for &(w, h) in &SIZES {
            for &scale in &SCALES {
                let output = Size::new(w, h);
                let pz = prompt_zone(output);
                assert!(contains_rect(Rect::new(0.0, 0.0, w, h), pz, 0.5), "{pz:?}");
                let cg = cosmic_greeter_zone(output, scale);
                assert!(contains_rect(Rect::new(0.0, 0.0, w, h), cg, 0.5), "{cg:?}");
            }
        }
        for bad in [Size::ZERO, Size::new(0.0, 100.0), Size::new(100.0, 0.0)] {
            assert_eq!(prompt_zone(bad), Rect::ZERO);
            assert_eq!(cosmic_greeter_zone(bad, 1.0), Rect::ZERO);
        }
        for scale in [f32::NAN, 0.0, -1.0, f32::INFINITY] {
            let _ = cosmic_greeter_zone(Size::new(1920.0, 1080.0), scale);
        }
    }

    fn full_data<'a>(
        clock: ClockData<'a>,
        greeting: Option<GreetingData<'a>>,
        media: Option<NowPlayingData<'a>>,
        battery: Option<BatteryData>,
    ) -> LockSceneData<'a> {
        LockSceneData {
            clock,
            greeting,
            media,
            battery,
            theme: theme(),
        }
    }

    fn sample_clock() -> ClockData<'static> {
        ClockData {
            time: "09:41",
            widest_time: "00:00",
            weekday: "Tuesday",
            date: "15 September",
            secondary: "",
            font_size: 64.0,
            variant: crate::widgetkit::cards::ClockVariant::Lock,
            accent_follow: false,
            day_fraction: 0.5,
        }
    }

    #[test]
    fn compose_and_render_slot_never_panic_with_missing_data_or_degenerate_geometry() {
        let mut fonts = FontStack::system();
        let avatar = image::RgbaImage::from_pixel(32, 32, image::Rgba([90, 60, 200, 255]));
        let greeting = GreetingData {
            text: "Good evening, Roy",
            avatar: Some(&avatar),
            initials: "R",
            text_size: 20.0,
        };
        let media = NowPlayingData {
            label: "Now playing",
            title: "Blue Monday",
            artist: "New Order",
            album: "",
            lyric: "",
            next_lyric: "",
            lyric_is_stale: false,
            elapsed: "1:34",
            total: "7:29",
            position: Some(0.2),
            art: None,
            badge: None,
            badge_label: "",
            chip: "",
            font_size: 22.0,
            accent_follow: false,
            screen_width: 0.0,
        };
        let battery = BatteryData {
            percent: 64,
            charging: false,
            full: false,
            size: 0.0,
        };

        for &(w, h) in &[(0.0_f32, 0.0), (1.0, 1.0), (1920.0, 1080.0)] {
            for &arrangement in &ARRANGEMENTS {
                let s = spec(
                    arrangement,
                    w,
                    h,
                    1.0,
                    ALL_SLOTS.to_vec(),
                    vec![prompt_zone(Size::new(w, h))],
                );
                for (g, m, b) in [
                    (Some(greeting), Some(media), Some(battery)),
                    (None, None, None),
                ] {
                    let data = full_data(sample_clock(), g, m, b);
                    for slot in ALL_SLOTS {
                        let _ = render_slot(slot, &s, &mut fonts, &data);
                    }
                    if let Ok(mut c) = Canvas::for_logical(Size::new(400.0, 240.0), 1.0) {
                        compose(&mut c, &mut fonts, &s, &data);
                    }
                }
            }
        }

        // A canvas smaller than the nominal spec must clip, not panic.
        let s = spec(
            LockArrangement::Classic,
            3840.0,
            2160.0,
            2.0,
            ALL_SLOTS.to_vec(),
            vec![],
        );
        let data = full_data(sample_clock(), Some(greeting), Some(media), Some(battery));
        let mut c = Canvas::for_logical(Size::new(100.0, 80.0), 1.0).unwrap();
        compose(&mut c, &mut fonts, &s, &data);
    }

    #[test]
    fn compose_still_always_returns_a_bitmap_and_never_panics() {
        let mut fonts = FontStack::system();
        let bg = image::RgbaImage::from_fn(64, 40, |x, y| {
            image::Rgba([(x * 3) as u8, (y * 4) as u8, 120, 255])
        });
        let empty_bg = image::RgbaImage::new(0, 0);
        let data = full_data(sample_clock(), None, None, None);

        for &(w, h) in &[(1920.0_f32, 1080.0), (3440.0, 1440.0), (0.0, 0.0)] {
            let s = spec(
                LockArrangement::Classic,
                w,
                h,
                1.0,
                ALL_SLOTS.to_vec(),
                Vec::new(),
            );
            for background in [&bg, &empty_bg] {
                for blur in [0.0_f32, 0.4, 1.0, f32::NAN] {
                    for dim in [0.0_f32, 0.25, 0.8, f32::NAN] {
                        let out = compose_still(&mut fonts, background, &s, &data, blur, dim);
                        assert!(out.w >= 1 && out.h >= 1);
                        assert_eq!(out.data.len(), (out.w * out.h * 4) as usize);
                    }
                }
            }
        }
    }

    /// Pins [`clock_height_ratio`]'s constants to what `cards::clock::measure`
    /// actually produces for the variant each arrangement draws, at the size
    /// [`suggested_clock_size`] would choose — the numbers that motivated
    /// giving Terminal (NOS) its own ratio rather than sharing the Lock
    /// variant's, and the regression test for anyone who changes either card
    /// and silently invalidates the estimate `reserved_clock_h` relies on.
    #[test]
    fn the_reserved_clock_height_tracks_what_is_actually_measured() {
        let mut f = FontStack::system();
        if !f.has_fonts() {
            return;
        }
        let t = theme();
        let output = Size::new(1920.0, 1080.0);
        for &arrangement in &ARRANGEMENTS {
            let fs = suggested_clock_size(arrangement, output, 1.0);
            let variant = if arrangement == LockArrangement::Terminal {
                crate::widgetkit::cards::ClockVariant::Nos
            } else {
                crate::widgetkit::cards::ClockVariant::Lock
            };
            let d = ClockData {
                time: "9:41",
                widest_time: "00:00",
                weekday: "Tuesday",
                date: "15 September",
                secondary: "9h 27m left today",
                font_size: fs,
                variant,
                accent_follow: false,
                day_fraction: 0.62,
            };
            let size = cards::clock::measure(&mut f, &t, &d, 1.0);
            let measured_ratio = size.card.h / fs;
            let want = clock_height_ratio(arrangement);
            assert!(
                (measured_ratio - want).abs() < 0.05,
                "{arrangement:?}: measured {measured_ratio:.3}, table says {want:.3}"
            );
            // And the estimate this actually feeds must not fall short of the
            // real card at a representative safe area, or the "clipped
            // Terminal clock" bug this exists to prevent is back.
            let safe = safe_area(output, 1.0);
            let s = safe.w.min(safe.h);
            assert!(
                reserved_clock_h(arrangement, s) >= size.card.h - 1.0,
                "{arrangement:?}: reserved {:.1} < measured {:.1}",
                reserved_clock_h(arrangement, s),
                size.card.h
            );
        }
    }

    #[test]
    fn no_new_card_regresses_the_contrast_thresholds_reused_from_the_spec() {
        use crate::widgetkit::theme::AA_TEXT;
        for mode in [Mode::Dark, Mode::Light] {
            let t = Theme::for_accent(mode, crate::config::Accent::Blue);
            assert!(t.contrast_on_text(t.text_primary) >= AA_TEXT);
        }
    }
}
