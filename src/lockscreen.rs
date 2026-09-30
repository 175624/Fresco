//! Lock-screen presentation model: *what* to draw on the OS lock screen,
//! never *how* to get there.
//!
//! # Fresco draws; it never authenticates
//!
//! The hard boundary this whole feature is built around: Fresco supplies a
//! wallpaper and a handful of its own widgets laid over it, and nothing more.
//! Every password prompt, every PAM conversation, and the decision that a
//! session is actually unlocked stay exactly where they already live —
//! `swaylock-plugin` on wlroots/COSMIC, `xsecurelock` or the desktop's own
//! screensaver on X11, `kscreenlocker` on KDE. This module has no access to
//! any of that even in principle: it is pure functions from
//! [`crate::config::LockScreen`] to [`ResolvedLock`], and neither type can
//! hold a credential, because neither ever asks for one.
//!
//! # A second, smaller widget list
//!
//! [`LockWidget`] is not [`crate::config::Widgets`] restated. A lock screen is
//! **semi-public** — anyone standing at the machine sees it, not only the
//! person who owns it — so both the *set* of widgets on offer and their
//! *defaults* are more conservative than the desktop overlay's:
//!
//! * No [`LockWidget::Lyrics`] or [`LockWidget::Avatar`] unless the user
//!   explicitly turns them on — see [`resolve`]'s privacy invariant.
//! * No notification or calendar widget exists at all, on or off — see
//!   [`LockWidget`]'s own docs for why that is a deliberate omission and not a
//!   gap left to fill in later.
//!
//! # Pure, so both the GUI and the daemon can use it
//!
//! Exactly the bargain [`crate::clock`] makes: no I/O, no globals, nothing
//! platform-specific — so [`resolve`] is *total* (every
//! [`crate::config::LockScreen`], however a person hand-edited it, produces
//! some [`ResolvedLock`] rather than an error) and cheap enough to call on
//! every keystroke of a settings preview. The GUI calls it to show a live
//! preview of a preset before it is applied; the daemon calls the very same
//! function to decide what to paint once one of the platform lockers above has
//! handed it the background. Neither host re-derives a clock theme, re-clamps
//! a slider, or re-decides what a blank greeting field means — that logic
//! lives here exactly once.

use crate::clock::ClockTheme;
use crate::config::{LiveVideo, LockPreset, LockScreen};

/// One widget the lock screen can draw, in the order [`resolve`] always lists
/// them — the canonical order a host lays them out in when it has no reason to
/// do otherwise.
///
/// Deliberately **no `Notification` or `Calendar` variant**, even though both
/// are usually the first thing a platform's own lock-screen widget picker
/// offers. A lock screen is semi-public: it is shown to whoever is standing at
/// the machine, not only to whoever unlocks it, and every mainstream platform
/// treats that as a reason to hide exactly this content there — iOS, Android
/// and GNOME all hide notification previews and calendar agendas on the lock
/// screen by default. Fresco has no notification or calendar data of its own
/// to draw in the first place, so leaving these two out is not a missing
/// feature to add later; it is refusing to build the one kind of lock-screen
/// widget every platform above ships turned off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockWidget {
    Clock,
    Date,
    Greeting,
    Avatar,
    NowPlaying,
    AlbumArt,
    Battery,
    Lyrics,
    Visualizer,
}

impl LockWidget {
    /// Every lock-screen widget kind, in the order [`resolve`] lists them —
    /// the order a settings page should render their toggles in, so the
    /// on-screen list and the resolved arrangement agree without either
    /// copying the other's order by hand.
    pub const ALL: [LockWidget; 9] = [
        LockWidget::Clock,
        LockWidget::Date,
        LockWidget::Greeting,
        LockWidget::Avatar,
        LockWidget::NowPlaying,
        LockWidget::AlbumArt,
        LockWidget::Battery,
        LockWidget::Lyrics,
        LockWidget::Visualizer,
    ];

    /// Display name for a picker.
    pub fn label(self) -> String {
        match self {
            LockWidget::Clock => crate::t!("Clock"),
            LockWidget::Date => crate::t!("Date"),
            LockWidget::Greeting => crate::t!("Greeting"),
            LockWidget::Avatar => crate::t!("Avatar"),
            LockWidget::NowPlaying => crate::t!("Now playing"),
            LockWidget::AlbumArt => crate::t!("Album art"),
            LockWidget::Battery => crate::t!("Battery"),
            LockWidget::Lyrics => crate::t!("Lyrics"),
            // "Visualiser" (British spelling), matching every other
            // user-visible mention of this widget in the UI — the Rust
            // identifier stays `Visualizer` to match `config::Visualizer`.
            LockWidget::Visualizer => crate::t!("Visualiser"),
        }
        .to_string()
    }

    /// A one-line disclosure for a widget that shows something a lock screen
    /// would not otherwise reveal to someone who has not unlocked the machine.
    /// `None` for a widget that reveals nothing beyond the time of day.
    ///
    /// Surfaced next to each toggle in the settings page precisely because the
    /// defaults in [`crate::config::LockWidgets`] cannot speak for themselves:
    /// a switch that defaults to off still needs to say *why*, for the person
    /// deciding whether to turn it on; a switch that defaults to on deserves
    /// the same disclosure for the person deciding whether to turn it off.
    pub fn privacy_note(self) -> Option<String> {
        let note = match self {
            LockWidget::NowPlaying => crate::t!("Shows the song title and artist while locked"),
            LockWidget::AlbumArt => crate::t!("Shows the album cover while locked"),
            LockWidget::Lyrics => crate::t!("Shows song lyrics while locked"),
            LockWidget::Greeting => crate::t!("Shows your first name"),
            LockWidget::Avatar => crate::t!("Shows your profile picture"),
            LockWidget::Clock | LockWidget::Date | LockWidget::Battery | LockWidget::Visualizer => {
                return None
            }
        };
        Some(note.to_string())
    }
}

impl LockPreset {
    /// Every preset, in the order a picker should list them.
    pub const ALL: [LockPreset; 5] = [
        LockPreset::Classic,
        LockPreset::Minimal,
        LockPreset::Glass,
        LockPreset::BigType,
        LockPreset::Terminal,
    ];

    /// The clock look this preset draws unless
    /// [`crate::config::LockScreen::clock_theme`] overrides it.
    ///
    /// Each mapping reuses an existing [`ClockTheme`] rather than inventing a
    /// lock-only look: [`ClockTheme::Lock`] — the theme already named and
    /// designed for exactly this job, a date over a large centred time with no
    /// card — for [`LockPreset::Classic`], and then one theme per remaining
    /// preset picked for how it already reads at a glance: the quiet
    /// [`ClockTheme::Minimal`] for [`LockPreset::Minimal`], the decorative
    /// [`ClockTheme::Card`] for [`LockPreset::Glass`], the oversized
    /// [`ClockTheme::Stacked`] for [`LockPreset::BigType`], and the
    /// dot-matrix [`ClockTheme::Nos`] for [`LockPreset::Terminal`].
    pub fn default_clock_theme(self) -> ClockTheme {
        match self {
            LockPreset::Classic => ClockTheme::Lock,
            LockPreset::Minimal => ClockTheme::Minimal,
            LockPreset::Glass => ClockTheme::Card,
            LockPreset::BigType => ClockTheme::Stacked,
            LockPreset::Terminal => ClockTheme::Nos,
        }
    }

    /// Display name for a picker.
    pub fn label(self) -> String {
        match self {
            LockPreset::Classic => crate::t!("Classic"),
            LockPreset::Minimal => crate::t!("Minimal"),
            LockPreset::Glass => crate::t!("Glass"),
            LockPreset::BigType => crate::t!("Big type"),
            LockPreset::Terminal => crate::t!("Terminal"),
        }
        .to_string()
    }

    /// One-line description for a picker, e.g. under the preset's thumbnail.
    pub fn blurb(self) -> String {
        match self {
            LockPreset::Classic => {
                crate::t!("Date above a large, centred clock — the familiar lock-screen look")
            }
            LockPreset::Minimal => crate::t!("Just the time, small and out of the way"),
            LockPreset::Glass => crate::t!("A frosted card carrying the clock and your widgets"),
            LockPreset::BigType => crate::t!("One oversized time that fills the screen"),
            LockPreset::Terminal => crate::t!("Dot-matrix digits in a quiet, monochrome readout"),
        }
        .to_string()
    }
}

impl LiveVideo {
    /// Whether the wallpaper plays as a live video behind the lock screen, or
    /// falls back to a still frame, given whether the machine is currently
    /// running on battery.
    ///
    /// [`LiveVideo::Ac`] is the default on [`crate::config::LockScreen::live_video`]
    /// for a reason specific to a lock screen rather than to wallpapers in
    /// general: a lock screen is routinely left up for hours at a stretch — a
    /// laptop closed in a bag or set aside on a desk stays *locked*, not
    /// suspended, on plenty of desktops — so a video wallpaper decoding and
    /// compositing a full frame the entire time is a sustained battery cost
    /// that the desktop wallpaper rarely pays in the same way: a desktop is
    /// usually covered by windows, or watched by someone who will notice and
    /// act. A locked screen is watched by no one for most of the time it is
    /// up. On battery, [`LiveVideo::Ac`] falls back to a still frame with the
    /// widgets still repainting at their own cheap cadence (a clock once a
    /// minute, a battery level rarer still) — most of the look, for a fraction
    /// of the cost.
    pub fn plays(self, on_battery: bool) -> bool {
        match self {
            LiveVideo::Ac => !on_battery,
            LiveVideo::Always => true,
            LiveVideo::Never => false,
        }
    }
}

/// What the greeting line resolves to.
///
/// A closed set rather than the raw `Option<String>` carried straight through,
/// because [`resolve`] has already done the one piece of interpretation a host
/// should never redo, and never redo differently: deciding what an *empty*
/// string means. See [`crate::config::LockScreen::greeting`] for the on-disk
/// rule this encodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GreetingText {
    /// Build "Good morning, `<first name>`" (or the equivalent for the time of
    /// day) from the host's own clock and user-info provider. Neither lives
    /// here: this module keeps no clock of its own — see [`crate::clock`]'s
    /// same rule — and has no access to the user's name.
    Auto,
    /// Show exactly this text, verbatim — no time-of-day or name
    /// substitution.
    Custom(String),
    /// Show no greeting line at all.
    Hidden,
}

/// A [`crate::config::LockScreen`], fully resolved: every default filled in,
/// every slider clamped, every ambiguity settled — the one struct a host needs
/// in order to actually draw the lock screen, or to preview it.
///
/// Producing this is the whole job of [`resolve`]. Nothing in these fields
/// needs further interpretation or carries a sentinel value; a host reads it
/// top to bottom and draws exactly what it says.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedLock {
    /// The preset the arrangement was resolved from — hosts that map presets
    /// 1:1 onto their own layout enum (e.g. `widgetkit::lockscene::LockArrangement`)
    /// read this field and nothing else to pick a layout.
    pub preset: LockPreset,
    /// The clock look to draw: either [`crate::config::LockScreen::clock_theme`]
    /// verbatim, or the preset's own [`LockPreset::default_clock_theme`].
    pub clock_theme: ClockTheme,
    /// Enabled widgets, in [`LockWidget::ALL`] order — never the order the
    /// user happened to flip the switches in, so a host can lay them out with
    /// a fixed, predictable arrangement.
    pub widgets: Vec<LockWidget>,
    /// Whether the wallpaper plays as live video; see [`LiveVideo::plays`].
    pub live_video: LiveVideo,
    /// Wallpaper darkening under the widgets, already clamped to `0.0..=0.8`.
    pub dim: f32,
    /// Still-frame blur, already clamped to `0.0..=1.0`. Meaningful only when
    /// [`ResolvedLock::live_video`] is not currently playing — see
    /// [`crate::config::LockScreen::blur`].
    pub blur: f32,
    /// What the greeting widget should say, if it is present in
    /// [`ResolvedLock::widgets`] at all.
    pub greeting: GreetingText,
}

/// Upper bound [`crate::config::LockScreen::dim`] is clamped to on resolve.
/// Capped short of `1.0`: past this point the wallpaper underneath is not
/// contributing anything a flat black background would not, which is a worse
/// result than the plain lock screen this feature exists to replace.
const MAX_DIM: f32 = 0.8;

/// Upper bound [`crate::config::LockScreen::blur`] is clamped to on resolve.
const MAX_BLUR: f32 = 1.0;

/// Resolve a [`crate::config::LockScreen`] into everything a host needs to
/// draw it.
///
/// Pure and total: every input — including a hand-edited `dim = 5.0` or
/// `blur = nan` (`nan` is a valid TOML float literal) — produces some
/// [`ResolvedLock`] rather than an error or a panic.
pub fn resolve(cfg: &LockScreen) -> ResolvedLock {
    let w = &cfg.widgets;
    // One row per `LockWidget` variant, in the enum's own declaration order —
    // this array *is* the canonical order every host lays widgets out in.
    let widgets = [
        (w.clock, LockWidget::Clock),
        (w.date, LockWidget::Date),
        (w.greeting, LockWidget::Greeting),
        (w.avatar, LockWidget::Avatar),
        (w.now_playing, LockWidget::NowPlaying),
        (w.album_art, LockWidget::AlbumArt),
        (w.battery, LockWidget::Battery),
        (w.lyrics, LockWidget::Lyrics),
        (w.visualizer, LockWidget::Visualizer),
    ]
    .into_iter()
    .filter_map(|(on, widget)| on.then_some(widget))
    .collect();

    let greeting = match cfg.greeting.as_deref() {
        None => GreetingText::Auto,
        Some("") => GreetingText::Hidden,
        Some(s) => GreetingText::Custom(s.to_string()),
    };

    ResolvedLock {
        preset: cfg.preset,
        clock_theme: cfg
            .clock_theme
            .unwrap_or_else(|| cfg.preset.default_clock_theme()),
        widgets,
        live_video: cfg.live_video,
        dim: clamp_finite(cfg.dim, 0.0, MAX_DIM),
        blur: clamp_finite(cfg.blur, 0.0, MAX_BLUR),
        greeting,
    }
}

/// Clamp a hand-editable float into `lo..=hi`, treating `NaN` — a valid TOML
/// float literal, spelled `nan` — as `lo` rather than letting it through
/// unclamped.
///
/// `f32::clamp` already does the right thing with `inf`/`-inf`: they compare
/// greater/less than every finite bound, so they saturate to `hi`/`lo` like
/// any other out-of-range value. `NaN` is the one case it cannot save us from
/// — it compares false against both bounds, so neither arm of the clamp ever
/// fires and it comes back exactly as it went in.
fn clamp_finite(v: f32, lo: f32, hi: f32) -> f32 {
    if v.is_nan() {
        lo
    } else {
        v.clamp(lo, hi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LockWidgets;

    fn cfg() -> LockScreen {
        LockScreen::default()
    }

    // -- resolve(): defaults and the privacy invariant -----------------------

    #[test]
    fn resolve_of_default_matches_the_privacy_contract() {
        let r = resolve(&cfg());
        assert_eq!(r.preset, LockPreset::Classic);
        assert_eq!(r.clock_theme, ClockTheme::Lock);
        assert_eq!(r.live_video, LiveVideo::Ac);
        assert_eq!(r.dim, 0.2);
        assert_eq!(r.blur, 0.0);
        assert_eq!(r.greeting, GreetingText::Auto);
        // Exactly the six on-by-default widgets, in declaration order.
        assert_eq!(
            r.widgets,
            vec![
                LockWidget::Clock,
                LockWidget::Date,
                LockWidget::Greeting,
                LockWidget::NowPlaying,
                LockWidget::AlbumArt,
                LockWidget::Battery,
            ]
        );
    }

    #[test]
    fn privacy_invariant_lyrics_avatar_and_visualizer_need_an_explicit_yes() {
        // The default resolve must never carry the three privacy-sensitive
        // widgets, and each one must appear the instant — and only when — its
        // own flag is explicitly set, independent of the other two.
        let r = resolve(&cfg());
        assert!(!r.widgets.contains(&LockWidget::Lyrics));
        assert!(!r.widgets.contains(&LockWidget::Avatar));
        assert!(!r.widgets.contains(&LockWidget::Visualizer));

        let lyrics_on = resolve(&LockScreen {
            widgets: LockWidgets {
                lyrics: true,
                ..LockWidgets::default()
            },
            ..cfg()
        });
        assert!(lyrics_on.widgets.contains(&LockWidget::Lyrics));
        assert!(!lyrics_on.widgets.contains(&LockWidget::Avatar));
        assert!(!lyrics_on.widgets.contains(&LockWidget::Visualizer));

        let avatar_on = resolve(&LockScreen {
            widgets: LockWidgets {
                avatar: true,
                ..LockWidgets::default()
            },
            ..cfg()
        });
        assert!(avatar_on.widgets.contains(&LockWidget::Avatar));
        assert!(!avatar_on.widgets.contains(&LockWidget::Lyrics));

        let visualizer_on = resolve(&LockScreen {
            widgets: LockWidgets {
                visualizer: true,
                ..LockWidgets::default()
            },
            ..cfg()
        });
        assert!(visualizer_on.widgets.contains(&LockWidget::Visualizer));
        assert!(!visualizer_on.widgets.contains(&LockWidget::Lyrics));
    }

    // -- resolve(): widget ordering and filtering -----------------------------

    #[test]
    fn resolve_orders_widgets_by_declaration_never_by_config_order() {
        let all_on = resolve(&LockScreen {
            widgets: LockWidgets {
                clock: true,
                date: true,
                greeting: true,
                avatar: true,
                now_playing: true,
                album_art: true,
                battery: true,
                lyrics: true,
                visualizer: true,
            },
            ..cfg()
        });
        assert_eq!(all_on.widgets, LockWidget::ALL.to_vec());

        let all_off = resolve(&LockScreen {
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
            ..cfg()
        });
        assert!(all_off.widgets.is_empty());

        // A scattered subset still comes back in enum order, not flip order.
        let subset = resolve(&LockScreen {
            widgets: LockWidgets {
                clock: false,
                date: true,
                greeting: false,
                avatar: true,
                now_playing: false,
                album_art: false,
                battery: true,
                lyrics: false,
                visualizer: false,
            },
            ..cfg()
        });
        assert_eq!(
            subset.widgets,
            vec![LockWidget::Date, LockWidget::Avatar, LockWidget::Battery]
        );
    }

    // -- resolve(): clamping --------------------------------------------------

    #[test]
    fn resolve_clamps_dim_and_blur_into_their_documented_ranges() {
        let at = |dim: f32, blur: f32| resolve(&LockScreen { dim, blur, ..cfg() });
        assert_eq!(at(-5.0, -5.0).dim, 0.0);
        assert_eq!(at(-5.0, -5.0).blur, 0.0);
        assert_eq!(at(5.0, 5.0).dim, MAX_DIM);
        assert_eq!(at(5.0, 5.0).blur, MAX_BLUR);
        assert_eq!(at(0.4, 0.6).dim, 0.4, "inside the range must pass through");
        assert_eq!(at(0.4, 0.6).blur, 0.6);
        // Boundary values are not pushed inward.
        assert_eq!(at(0.0, 0.0).dim, 0.0);
        assert_eq!(at(MAX_DIM, MAX_BLUR).dim, MAX_DIM);
        assert_eq!(at(MAX_DIM, MAX_BLUR).blur, MAX_BLUR);
    }

    #[test]
    fn resolve_never_panics_or_produces_nan_on_non_finite_input() {
        // `nan`, `inf` and `-inf` are all valid TOML float literals, so a
        // hand-edited config can produce any of them here.
        for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let r = resolve(&LockScreen {
                dim: v,
                blur: v,
                ..cfg()
            });
            assert!(!r.dim.is_nan(), "dim must never resolve to NaN");
            assert!(!r.blur.is_nan(), "blur must never resolve to NaN");
            assert!((0.0..=MAX_DIM).contains(&r.dim), "{v} -> dim {}", r.dim);
            assert!((0.0..=MAX_BLUR).contains(&r.blur), "{v} -> blur {}", r.blur);
        }
        // Infinities saturate like any other out-of-range value; NaN falls
        // back to the bottom of the range (the least intrusive outcome) rather
        // than to an arbitrary corner.
        assert_eq!(clamp_finite(f32::INFINITY, 0.0, MAX_DIM), MAX_DIM);
        assert_eq!(clamp_finite(f32::NEG_INFINITY, 0.0, MAX_DIM), 0.0);
        assert_eq!(clamp_finite(f32::NAN, 0.0, MAX_DIM), 0.0);
    }

    // -- resolve(): clock theme overrides --------------------------------------

    #[test]
    fn resolve_uses_the_presets_clock_theme_unless_overridden() {
        for preset in LockPreset::ALL {
            let r = resolve(&LockScreen {
                preset,
                clock_theme: None,
                ..cfg()
            });
            assert_eq!(r.clock_theme, preset.default_clock_theme(), "{preset:?}");

            // An explicit override always wins, regardless of preset.
            let r = resolve(&LockScreen {
                preset,
                clock_theme: Some(ClockTheme::Segment),
                ..cfg()
            });
            assert_eq!(r.clock_theme, ClockTheme::Segment, "{preset:?}");
        }
    }

    #[test]
    fn preset_default_themes_are_pinned() {
        // The pairing is a design decision, not an implementation detail, so
        // it is pinned by value rather than merely "does not panic".
        assert_eq!(LockPreset::Classic.default_clock_theme(), ClockTheme::Lock);
        assert_eq!(
            LockPreset::Minimal.default_clock_theme(),
            ClockTheme::Minimal
        );
        assert_eq!(LockPreset::Glass.default_clock_theme(), ClockTheme::Card);
        assert_eq!(
            LockPreset::BigType.default_clock_theme(),
            ClockTheme::Stacked
        );
        assert_eq!(LockPreset::Terminal.default_clock_theme(), ClockTheme::Nos);
    }

    // -- resolve(): greeting ----------------------------------------------------

    #[test]
    fn resolve_greeting_modes() {
        assert_eq!(
            resolve(&LockScreen {
                greeting: None,
                ..cfg()
            })
            .greeting,
            GreetingText::Auto
        );
        assert_eq!(
            resolve(&LockScreen {
                greeting: Some(String::new()),
                ..cfg()
            })
            .greeting,
            GreetingText::Hidden
        );
        assert_eq!(
            resolve(&LockScreen {
                greeting: Some("Welcome back".to_string()),
                ..cfg()
            })
            .greeting,
            GreetingText::Custom("Welcome back".to_string())
        );
    }

    // -- LiveVideo::plays --------------------------------------------------------

    #[test]
    fn live_video_plays_truth_table() {
        assert!(LiveVideo::Ac.plays(false), "AC power: plays");
        assert!(!LiveVideo::Ac.plays(true), "on battery: falls back");
        assert!(LiveVideo::Always.plays(false));
        assert!(LiveVideo::Always.plays(true), "Always ignores battery");
        assert!(!LiveVideo::Never.plays(false), "Never ignores AC too");
        assert!(!LiveVideo::Never.plays(true));
    }

    // -- labels and blurbs -------------------------------------------------------

    #[test]
    fn preset_labels_and_blurbs_are_never_empty_and_never_say_ios_or_apple() {
        for preset in LockPreset::ALL {
            let label = preset.label();
            let blurb = preset.blurb();
            assert!(!label.trim().is_empty(), "{preset:?} label");
            assert!(!blurb.trim().is_empty(), "{preset:?} blurb");
            for text in [&label, &blurb] {
                let lower = text.to_lowercase();
                assert!(!lower.contains("ios"), "{preset:?}: {text:?}");
                assert!(!lower.contains("apple"), "{preset:?}: {text:?}");
            }
        }
        // Every preset reads as a distinct choice, not five copies of one text.
        let labels: std::collections::HashSet<String> =
            LockPreset::ALL.iter().map(|p| p.label()).collect();
        assert_eq!(labels.len(), LockPreset::ALL.len());
    }

    #[test]
    fn widget_labels_are_never_empty() {
        for widget in LockWidget::ALL {
            assert!(!widget.label().trim().is_empty(), "{widget:?}");
        }
    }

    #[test]
    fn privacy_notes_exist_only_for_widgets_that_reveal_something_extra() {
        for widget in LockWidget::ALL {
            let note = widget.privacy_note();
            match widget {
                LockWidget::NowPlaying
                | LockWidget::AlbumArt
                | LockWidget::Lyrics
                | LockWidget::Greeting
                | LockWidget::Avatar => {
                    assert!(
                        note.is_some_and(|n| !n.trim().is_empty()),
                        "{widget:?} must disclose what it shows"
                    );
                }
                LockWidget::Clock | LockWidget::Date | LockWidget::Battery => {
                    assert_eq!(note, None, "{widget:?} reveals nothing extra");
                }
                LockWidget::Visualizer => {
                    assert_eq!(
                        note, None,
                        "Visualizer has its own audio-capture consent dialog; \
                         this note is for lock-screen-specific disclosures"
                    );
                }
            }
        }
    }

    #[test]
    fn lock_widget_all_matches_the_resolve_order() {
        // `resolve`'s own ordering and the public `ALL` constant must never
        // drift apart, or a settings page built from one and a preview built
        // from the other would silently disagree.
        let all_on = LockWidgets {
            clock: true,
            date: true,
            greeting: true,
            avatar: true,
            now_playing: true,
            album_art: true,
            battery: true,
            lyrics: true,
            visualizer: true,
        };
        let r = resolve(&LockScreen {
            widgets: all_on,
            ..cfg()
        });
        assert_eq!(r.widgets, LockWidget::ALL.to_vec());
    }
}
