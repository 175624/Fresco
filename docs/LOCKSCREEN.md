# Lock screen

Fresco can show your wallpaper, plus a handful of its own widgets — clock,
date, greeting, avatar, now playing, album art, battery, and optionally
lyrics or the audio visualiser — on your real system lock screen, in place of
your distro's stock lock-screen furniture.

It is **off by default** (`[lockscreen] enabled = false` in `config.toml`).
Turning it on is one switch; what actually happens after that depends
entirely on your desktop — some get live video and widgets, some get a still
frame with widgets, and a few get a still frame and nothing else. Each case
is covered below.

## Privacy & security

**Fresco never sees your password.** This feature only ever supplies a
wallpaper and a widget layer to *someone else's* locker — your desktop's own,
already-audited authentication stack keeps 100% of the password prompt, the
PAM conversation, and the decision that your session is actually unlocked.
Fresco has no code path that could read or intercept a credential even if it
wanted to; the config in `[lockscreen]` has no field that could hold one.

Who actually checks your password:

| Desktop | Password is checked by |
|---|---|
| COSMIC | `cosmic-greeter` (COSMIC's own locker) |
| KDE Plasma 6 | `kscreenlocker_greet` (KDE's own greeter) |
| Sway, Hyprland, niri, river, labwc, Wayfire | `swaylock-plugin`'s own PAM conversation |
| X11 window managers | `xsecurelock`'s own PAM conversation |
| MATE / Xfce | `mate-screensaver` / `xfce4-screensaver`'s own lock dialog |
| GNOME, Cinnamon, Deepin | the desktop's own lock screen, unchanged |

**Privacy defaults.** A lock screen is semi-public — anyone standing at the
machine sees it, not just its owner — so its defaults are more conservative
than the desktop overlay's:

- There is no notification or calendar widget at all, on or off. Every
  mainstream platform hides exactly this content on the lock screen by
  default; Fresco has no notification or calendar data of its own to draw in
  the first place, so this isn't a missing feature, it's one that was never
  built.
- **Lyrics are off by default**, the same as the desktop overlay's lyrics
  widget, but for a stronger reason: a lock screen is visible to anyone
  walking past a machine its owner can't see, so lyrics don't turn on there
  just because they're already on for the desktop.
- **Avatar is off by default** — a photo identifies you more directly than a
  first name does, and unlike the greeting there's no "leave it blank"
  middle ground, so Fresco asks first.
- The audio visualiser stays off unless you've already given audio-capture
  consent (the same one-time consent the desktop visualiser asks for); it's
  refused even if you flip the config file's switch by hand without that
  consent on record.
- Every widget that reveals more than the time of day carries its own
  one-line privacy note next to its toggle in the Lock Screen settings
  window — what it shows, in plain words, right where you decide whether to
  turn it on.

## Turning it on

Open Fresco, then either:

- the hamburger menu → **Lock Screen…**, or
- **Ctrl+K** → search for **Lock Screen settings**

Flip **Show Fresco on the lock screen** at the top. Everything else on the
page — look, widgets, motion — only matters once that switch is on, and
turning it off again leaves your distro's normal lock screen exactly as it
was.

The **Status** section on the same page shows what your desktop actually
supports (live video or not, widgets or not), any one-click setup a host
needs (see COSMIC/KDE below), and the exact commands to bind on hosts that
need a keybinding instead. It re-checks itself every few seconds, so it
reflects reality rather than what a click merely hoped would happen.

**Preview lock screen** at the bottom renders a full-screen, live-updating
preview on your current display — the clock ticks and, on AC power, the
video plays — without ever locking anything. It's a safe way to check a
preset or a widget choice before it's actually staring back at you from a
locked screen.

## Customizing it

### Presets

A preset fixes a clock look and a widget arrangement together, so picking
one is a single decision rather than a pile of layout knobs:

| Preset | Look |
|---|---|
| **Classic** (default) | Date above a large, centred clock — the familiar lock-screen look |
| **Minimal** | Just the time, small and out of the way |
| **Glass** | A frosted card carrying the clock and your widgets |
| **Big type** | One oversized time that fills the screen |
| **Terminal** | Dot-matrix digits in a quiet, monochrome readout |

The **Clock style** dropdown overrides just the preset's clock face, if you
want a different one without changing the rest of the layout.

### Widgets

Nine widgets, each its own switch:

| Widget | Default | Shows |
|---|---|---|
| Clock | On | The time |
| Date | On | The date |
| Greeting | On | "Good morning, `<your first name>`" (or your own text — see below) |
| Avatar | **Off** | Your profile picture |
| Now playing | On | The playing track's title, artist and transport state |
| Album art | On | The playing track's cover |
| Battery | On | Battery level and charge state (hidden automatically on a desktop with no battery) |
| Lyrics | **Off** | Synced lyrics for the playing track |
| Visualiser | **Off** | An audio-spectrum visualiser (needs audio-capture consent) |

The greeting has its own entry field and a **Hide greeting** switch: leave
the field blank for the automatic "Good morning, …" line, type your own text
to show that verbatim, or hide the line entirely.

### Live video and battery

**Live video** (Motion & power) has three settings:

- **On AC power only** (default) — plays while plugged in, falls back to a
  still frame with the widgets still ticking over once unplugged. A locked
  screen is routinely left up for hours — a laptop closed in a bag stays
  *locked*, not suspended, on plenty of desktops — so decoding and
  compositing a full video frame the whole time is a real, sustained battery
  cost that this default exists to avoid.
- **Always** — plays regardless of power source.
- **Never** — always a still frame, even on mains power.

**Dim** darkens the wallpaper under the widgets (0–0.8; clamped short of
fully black, since past that point the wallpaper isn't doing anything a
plain black background wouldn't). **Blur** softens a still frame; the slider
reads 0–100 %. It is not linear: the blur radius grows with the square of the
slider position, so the first half is gentle and fine-grained and the very end
is a soft wash of colour. Blur only applies when a still frame is actually
showing — a playing video is never blurred, since that would mean re-filtering
every decoded frame for as long as the screen stays locked.

## Per-desktop setup

### COSMIC 1.9+

Nothing to click — it works automatically once the master switch is on, on
COSMIC 1.9 and newer. COSMIC's compositor (`cosmic-comp`) exposes a
show-on-lock protocol that Fresco's bundled, patched `mpvpaper` uses to keep
your live wallpaper on screen behind `cosmic-greeter`'s own lock surface,
instead of freezing or disappearing the moment the screen locks.

COSMIC keeps its own frosted panel — time, name, battery, and the password
field — near the top of the screen; Fresco's widgets fill the rest. Since
COSMIC's panel already shows a clock, a name, and a battery, you may want to
turn off Fresco's own **Clock**, **Greeting**, and **Battery** widgets here
to avoid showing each one twice — the Status section reminds you of this
directly under COSMIC.

If your COSMIC is older than 1.9, or the show-on-lock protocol isn't
available for some other reason, the live-video-and-widgets layer doesn't
appear. Your lock screen still shows a still frame of your current
wallpaper instead of the stock image either way — that part is unconditional
and independent of the `[lockscreen]` switch above; see
[GNOME, Cinnamon, and Deepin](#gnome-cinnamon-and-deepin) for the same idea
applied elsewhere.

### KDE Plasma 6

Fresco ships its own Plasma lock-screen wallpaper plugin. In the Lock Screen
window's Status section, click **Set up**:

- backs up whatever `~/.config/kscreenlockerrc` already has, byte for byte;
- installs the plugin package if it isn't already present system-wide or
  per-user;
- points KDE's greeter at Fresco's plugin and writes its config (video/still
  path, whether to play video, dim).

**Undo** restores exactly what was there before — the original plugin
selection and every key Fresco touched, deleted again if it was never there
to begin with.

Changes take effect **from the next lock, not the current session** —
`kscreenlocker_greet` is a fresh process spawned each time you lock, and it
only reads its config at startup.

Video playback needs the Qt6 Multimedia QML module. It's a recommended, not
required, package — without it the plugin falls back to a still image
automatically, no error, nothing broken:

- Debian/Ubuntu: `qml6-module-qtmultimedia`
- Arch: `qt6-multimedia`
- Fedora: `qt6-qtmultimedia`

### Sway, Hyprland, niri, river, labwc, Wayfire

`fresco lock` runs **swaylock-plugin** (a fork of swaylock that can host an
arbitrary background program) with Fresco's own `mpvpaper` as its
per-output background, so your wallpaper — and the widget layer, once the
compositor confirms the lock — shows behind swaylock's familiar password
ring.

This needs `swaylock-plugin` installed **with its PAM file in place**
(`/etc/pam.d/swaylock-plugin`, or `/usr/lib/pam.d`/`/usr/etc/pam.d` on
distros that lay out PAM config differently). Fresco checks for that file
before ever handing it your session: a locker with no PAM service file can
*never* successfully authenticate, so rather than lock you out with a
password that's actually correct, Fresco refuses to use it and reports why
in the Status section instead.

Bind a key or an idle daemon to `fresco lock`:

```
Sway / Hyprland / niri:  bindsym $mod+Escape exec fresco lock
hypridle:                lock_cmd = fresco lock
swayidle:                timeout 300 'fresco lock' before-sleep 'fresco lock'
```

If `frescod` isn't running or its own attempt fails, `fresco lock` falls
back to whichever of `swaylock`, `hyprlock`, or `gtklock` is installed
(without Fresco's wallpaper), and if none of those are available either, to
`loginctl lock-session` — see [`fresco lock`](#fresco-lock) below.

### X11 window managers

`fresco lock` runs **`xsecurelock`** with Fresco's own saver module as its
background. Trigger it from your idle daemon or lock keybinding:

```
xss-lock -- fresco lock
```

This needs `xsecurelock` installed; Fresco's saver helper ships as part of
the package, so there's nothing else to set up. The password dialog's font
and colours follow Fresco's own theme.

> The wallpaper itself shows whenever `xsecurelock` is actually pointed at
> Fresco's saver (via `fresco lock`), regardless of the `[lockscreen]`
> master switch — that switch is what adds the widget layer on top of it.

### MATE and Xfce

Open your screensaver settings (**mate-screensaver-preferences** or
**xfce4-screensaver-preferences**) and pick **Fresco** from the theme list —
it appears there automatically once Fresco is installed, no extra setup.

This reuses the same saver module as the X11 case above, through the
xscreensaver-style theme protocol both `mate-screensaver` and
`xfce4-screensaver` implement. As with X11, picking Fresco's theme shows
your wallpaper regardless of the `[lockscreen]` switch; the switch adds the
widget layer.

**This pairing is experimental**, and honestly so: whether Fresco's
wallpaper stays visible *under* the password dialog or gets replaced by it
varies by `mate-screensaver`/`xfce4-screensaver` version, since the two
diverged from a common ancestor and evolved differently. On some
`mate-screensaver` versions specifically, the widget layer may not appear
at all even though the wallpaper does — that screensaver doesn't forward
`$XDG_RUNTIME_DIR` to the saver process it spawns, which can stop Fresco's
daemon from ever learning that a lock is in progress. `xfce4-screensaver`
does forward it. If widgets don't show up for you on MATE, the wallpaper
itself is still expected to work; that's the known gap.

### GNOME, Cinnamon, and Deepin

Still frame only, automatically — there's nothing to turn on for this part.
These lock screens have no supported way for an outside application to add
a widget layer today, so Fresco doesn't attempt one; the same limit that
already applies to Fresco's desktop widgets on GNOME Wayland. What you do
get, for free, is a still frame of your current wallpaper on the lock
screen instead of your distro's stock image, through the same background
sync that already keeps these desktops' own wallpaper setting pointed at
whatever Fresco is playing.

On Deepin that sync only runs while **Show Fresco on the lock screen** is on.
Deepin keeps a separate lock-screen picture per user (not the desktop
wallpaper), so Fresco saves yours the first time, sets a still frame as the
lock-screen picture, and puts yours back when you stop Fresco or turn the
setting off. If you pick a different lock-screen picture in the meantime,
yours is kept. Fresco applies your Dim and Blur settings to that frame, so the
lock screen picks them up; Deepin may add a blur of its own on top. Widgets are
not part of the frame (they would be frozen at one moment), so they appear in
the preview only. The frame is kept in `~/.cache/fresco`, or in
`/var/tmp/fresco-<uid>` when that folder is not readable by other users —
Deepin's login screen and blur service run as other users and fall back to
the stock picture if they cannot read it.

### Flatpak

Flatpak builds are no longer provided. The lock screen needs files and
protocols outside what the Flatpak sandbox can reach — the Lock Screen window
says so directly and greys itself out if you open it from an old Flatpak
install.

## `fresco lock`

```
fresco lock       Lock the screen now, through your desktop's own locker
```

Run without a running `frescod`, or without a working host adapter, this
still locks your session — it falls back to a short, independent chain of
progressively more generic lockers, and always ends at `loginctl
lock-session`, which every host in this list already has. `fresco lock`
either prints confirmation of a real, running locker and exits 0, or prints
an error and exits 1; it never exits 0 without something actually holding
the lock.

The fallback chain, by session type, each entry tried only if it's actually
installed:

- **A recognised desktop** (GNOME, Cinnamon, MATE, Xfce, KDE, COSMIC,
  Deepin): straight to `loginctl lock-session` — these already wire their
  own locker to it, so nothing here races a second one against it.
- **A bare wlroots session** (Sway, Hyprland, niri, and friends with no
  frescod running): `swaylock`, then `hyprlock`, then `gtklock`, then
  `loginctl lock-session`.
- **A bare X11 session**: `xsecurelock`, then `i3lock`, then `loginctl
  lock-session`.

Note that a normal, successful lock through `frescod` can itself take a few
seconds — `swaylock-plugin` waits for the compositor to actually confirm
the lock before it reports back — so `fresco lock` waits up to 12 seconds
before deciding the daemon is unreachable and moving to the fallback chain.
That's expected, not a hang.

## Configuration reference

Everything lives under `[lockscreen]` in `config.toml`. The table is absent
until you turn the feature on from the GUI; every key below shows its
default.

```toml
[lockscreen]
enabled     = false     # master switch
preset      = "classic" # classic | minimal | glass | bigtype | terminal
live_video  = "ac"      # ac | always | never
dim         = 0.2       # 0.0-0.8, clamped
blur        = 0.0       # 0.0-1.0 slider position (curved), clamped; still-frame hosts only
blur_curve  = 1         # written by Fresco; a file without it holds the old linear blur
# clock_theme = "..."   # unset = the preset's own clock; same spellings as
                         # the desktop clock widget ("lockscreen" selects
                         # Classic's own look — not "classic")
# greeting = "..."      # unset = auto "Good morning, <name>"; "" = hidden;
                         # anything else is shown verbatim

[lockscreen.widgets]
clock       = true
date        = true
greeting    = true
avatar      = false
now_playing = true
album_art   = true
battery     = true
lyrics      = false
visualizer  = false     # also needs audio_capture_consented = true, top-level
```

| Key | Default | Notes |
|---|---|---|
| `enabled` | `false` | Nothing here takes effect until this is `true` |
| `preset` | `"classic"` | See [Presets](#presets) |
| `live_video` | `"ac"` | See [Live video and battery](#live-video-and-battery) |
| `dim` | `0.2` | Clamped to `0.0..=0.8` |
| `blur` | `0.0` | Slider position, clamped to `0.0..=1.0`; radius = 0.3 × position² of the screen height; ignored while video is actually playing |
| `blur_curve` | `1` | Which scale `blur` is on. Absent (configs from 1.1.46 and earlier) means the old linear blur, which Fresco converts once on load so the look doesn't change |
| `clock_theme` | unset | Overrides the preset's clock face |
| `greeting` | unset | `""` hides the line; any other text is shown as-is |
| `widgets.*` | see above | One bool per widget; see [Widgets](#widgets) |

A hand-edited `config.toml` is always safe to load: an out-of-range `dim`
or `blur` is clamped rather than rejected, and an unset `[lockscreen]`
table entirely is the same as the feature never existing.

## Troubleshooting

**The Status section says a host isn't available.** Read the note it
prints — it names the exact missing piece (`swaylock-plugin` itself, its
PAM file, `xsecurelock`, KDE's `kreadconfig6`/`kwriteconfig6`) rather than
failing silently.

**KDE: my change didn't show up.** Expected — KDE only reads the plugin's
config when `kscreenlocker_greet` starts, i.e. the *next* time you lock, not
the session that's currently unlocked. Lock and check again.

**KDE: video doesn't play, only a still image.** Almost always the Qt6
Multimedia QML module isn't installed (see [KDE Plasma 6](#kde-plasma-6)
above) — this is a documented, graceful fallback, not a bug.

**MATE: the wallpaper shows but the widgets don't.** See the note under
[MATE and Xfce](#mate-and-xfce) — some `mate-screensaver` versions don't
forward `$XDG_RUNTIME_DIR` to the saver process, which can stop the widget
layer from ever hearing that a lock started.

**A lock screen is stuck — frozen, black, or unresponsive, and you can't
unlock.** This is a known risk with any third-party lock-screen tooling, not
something specific to Fresco, and both Sway and Hyprland document their own
recovery path:

- *Sway, and any other wlroots compositor `fresco lock` targets*:
  switch to a free VT (e.g. Ctrl+Alt+F3), log in there, and start a fresh
  locker against the same session's Wayland socket. `swaylock-plugin`'s own
  README gives exactly this as the documented recovery: "switching to a
  different virtual terminal, running `killall swaylock-plugin` and
  running swaylock-plugin, and restarting with e.g. `WAYLAND_DISPLAY=wayland-1
  swaylock-plugin`." List the actual socket name first with `ls
  /run/user/$(id -u)/wayland-*` if you have more than one — then either
  repeat that command with `fresco lock` in place of `swaylock-plugin` (to
  get your wallpaper back too), or run `swaylock-plugin`/`swaylock` alone if
  `fresco` itself can't reach its daemon from that shell.
  (Source: [swaylock-plugin README](https://github.com/mstoeckl/swaylock-plugin/blob/main/README.md).)

- *Hyprland specifically*: Hyprland's compositor refuses a second lock
  client while it still considers the session locked by the first
  (crashed) one, unless `misc:allow_session_lock_restore` is `true` in your
  `hyprland.conf` — worth setting ahead of time for exactly this situation.
  If it wasn't set, `hyprctl` still reaches the compositor over its own
  socket independently of whatever is (or isn't) on screen, so from a free
  VT you can turn it on and restart the locker live:

  ```
  hyprctl -i 0 keyword misc:allow_session_lock_restore true
  hyprctl -i 0 dispatch exec "fresco lock"
  ```

  `-i`/`--instance` targets a running Hyprland instance directly, which
  matters here since `$HYPRLAND_INSTANCE_SIGNATURE` won't be set in a shell
  on a different VT; `0` addresses the first (usually only) running
  instance. (Source: `misc:allow_session_lock_restore` and `hyprctl`'s
  `-i`/`--instance` flag are both documented on the
  [Hyprland Wiki](https://wiki.hypr.land/Configuring/Basics/Variables/).)

If neither applies to your setup, `loginctl lock-session` from any other
active session, or a straightforward reboot from a VT (`sudo reboot`), are
the universal last resorts — this feature never removes your ability to do
either.
