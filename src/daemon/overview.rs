//! GNOME overview fallback.
//!
//! Our live wallpaper is an X11 window the GNOME Activities overview, workspace
//! switcher, and lock screen can't see — they draw `org.gnome.desktop.background`
//! instead. To keep those surfaces consistent, we extract a still frame from the
//! active wallpaper and set it as the desktop background, saving the user's
//! original first and restoring it on Stop. No-op on non-GNOME desktops.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{Kind, Wallpaper};

const GNOME_SCHEMA: &str = "org.gnome.desktop.background";
/// Cinnamon (Linux Mint) forked the background settings: muffin and the
/// Cinnamon Wayland session draw `org.cinnamon.desktop.background` and ignore
/// the GNOME schema, so writing only GNOME's key is an invisible wallpaper.
const CINNAMON_SCHEMA: &str = "org.cinnamon.desktop.background";

/// Newer Cinnamon (cinnamon commits 3f134e6, 61c1b3d — Sep 2026) replaced the
/// single `picture-uri` key with a per-monitor list. `picture-uri` still
/// exists but is now a one-shot legacy input: `backgroundManager.js` calls
/// `set_single_uri()` on it, which REPLACES the whole list with one entry,
/// forces `background-mode` to `mirror`/`spanned`, forces `picture-options`
/// to `zoom`, and resets `picture-uri` back to `''`. Writing `picture-uri`
/// the old way therefore (a) never round-trips on backup — it reads back
/// empty — and (b) destroys a user's per-monitor ("independent") layout by
/// collapsing it to a single mirrored entry.
const CINNAMON_LIST_KEY: &str = "picture-uri-list";
const CINNAMON_MODE_KEY: &str = "background-mode";

/// First line of a state file written for modern Cinnamon, distinguishing it
/// from the older one-value-per-line format so `restore` can tell them apart
/// (old state files, e.g. from a Fresco install that predates this schema,
/// must still restore correctly).
const MODERN_MARKER: &str = "#fresco-cinnamon-modern-v1";

/// MATE forked them again, and differently: `org.mate.background` keeps the
/// picture in `picture-filename` as a plain path, not a URI. Caja draws it —
/// which is what shows while its desktop is peeked at above the wallpaper.
const MATE_SCHEMA: &str = "org.mate.background";

/// A desktop's background settings: where they live, which keys hold the
/// picture (the first is the one probed for availability), and whether those
/// keys take a `file://` URI or a bare path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Schema {
    name: &'static str,
    keys: &'static [&'static str],
    uri: bool,
}

/// The background schema this session actually draws.
fn schema() -> Schema {
    schema_for(
        crate::capability::is_cinnamon(),
        crate::capability::is_mate(),
    )
}

/// Pure form of [`schema`]. Cinnamon has no `picture-uri-dark`.
fn schema_for(cinnamon: bool, mate: bool) -> Schema {
    if mate {
        Schema {
            name: MATE_SCHEMA,
            keys: &["picture-filename"],
            uri: false,
        }
    } else if cinnamon {
        Schema {
            name: CINNAMON_SCHEMA,
            keys: &["picture-uri"],
            uri: true,
        }
    } else {
        Schema {
            name: GNOME_SCHEMA,
            keys: &["picture-uri", "picture-uri-dark"],
            uri: true,
        }
    }
}

/// Whether this session's Cinnamon uses the modern `picture-uri-list` model
/// instead of the legacy single `picture-uri` key. Probed once per daemon
/// (like [`gnome_available`]'s warn-once) via `gsettings list-keys`, since the
/// installed schema cannot change while we're running.
fn cinnamon_modern() -> bool {
    static MODERN: OnceLock<bool> = OnceLock::new();
    *MODERN.get_or_init(|| {
        Command::new("gsettings")
            .args(["list-keys", CINNAMON_SCHEMA])
            .output()
            .ok()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .any(|l| l.trim() == CINNAMON_LIST_KEY)
            })
            .unwrap_or(false)
    })
}

/// Escape a string for use as a GVariant text-format string literal, e.g. for
/// `gsettings set schema key <literal>`. Only `\` and `'` need escaping
/// inside a single-quoted GVariant string.
fn gvariant_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\\' || c == '\'' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// Percent-encode `path` into a `file://` URI. Unlike the legacy schemas'
/// single-quoted-as-is path (safe there because our cache directory never
/// contains spaces or quotes), a `picture-uri-list` entry is itself embedded
/// inside a GVariant string literal, so any byte that isn't an unreserved URI
/// character must be percent-encoded up front.
fn encode_file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Whether `text` (a `picture-uri-list` `gsettings get` value) is Fresco's own
/// frame rather than something the user set. Used to avoid ever recording our
/// own frame as the "original" to restore — e.g. if the state file was lost
/// (deleted, or written by a version of Fresco predating this schema) while
/// our frame was still live.
fn is_own_frame_list(text: &str) -> bool {
    let marker = cache_dir().join("overview-");
    text.contains(&marker.to_string_lossy().into_owned())
}

/// Set a still frame of `wallpaper` as the desktop background.
///
/// Skipped while the MATE icon mirror has Caja painting its key colour
/// (`caja_mirror`): the still frame would replace the key, the mirror would
/// then find no key pixels to cut away, and the whole photograph — not just
/// the icons — would be copied over the video. The still is not needed there
/// anyway: Caja never shows its background while the mirror runs.
pub fn apply(wallpaper: &Wallpaper) {
    if super::caja_mirror::key_active() {
        return;
    }
    if !gnome_available() {
        return;
    }
    let Some(frame) = render_still(wallpaper) else {
        return;
    };
    save_original_once();
    if crate::capability::is_cinnamon() && cinnamon_modern() {
        // Never write `picture-uri`: Cinnamon's `set_single_uri()` would
        // consume it and collapse any per-monitor ("independent") layout to a
        // single mirrored entry on its own, but doing it explicitly here lets
        // us pick `picture-options` and keeps the write order predictable —
        // list first, then mode, so there's no window where `background-mode`
        // names a list state that hasn't landed yet.
        let uri = encode_file_uri(&frame);
        let list = format!(
            "[{{'picture-uri': <{}>, 'picture-options': <{}>}}]",
            gvariant_string_literal(&uri),
            gvariant_string_literal("zoom")
        );
        gset(CINNAMON_LIST_KEY, &list);
        gset(CINNAMON_MODE_KEY, "'mirror'");
        log::info!("overview background set to {}", frame.display());
        return;
    }
    // GVariant string literal: 'file:///path' (or '/path' for MATE). Our frame
    // path is a safe cache location (no spaces/quotes), so simple
    // single-quoting is sufficient.
    let s = schema();
    let gv = if s.uri {
        format!("'file://{}'", frame.display())
    } else {
        format!("'{}'", frame.display())
    };
    for key in s.keys {
        gset(key, &gv);
    }
    log::info!("overview background set to {}", frame.display());
}

/// What a saved state file tells us to restore. Split out from [`restore`] so
/// the format itself (new and old) can be unit-tested without a live
/// `gsettings`/schema.
#[derive(Debug, PartialEq, Eq)]
enum RestoreOp<'a> {
    /// Modern Cinnamon: `background-mode` and `picture-uri-list`, verbatim
    /// `gsettings get` text.
    Modern { mode: &'a str, list: &'a str },
    /// Everything else: one value per schema key, in `schema().keys` order.
    /// Also what an old-format state file (written before this schema was
    /// added) parses as, so those files keep restoring correctly.
    Legacy(Vec<&'a str>),
}

fn parse_state(text: &str) -> RestoreOp<'_> {
    if let Some(rest) = text
        .strip_prefix(MODERN_MARKER)
        .and_then(|r| r.strip_prefix('\n'))
    {
        let mut lines = rest.splitn(2, '\n');
        let mode = lines.next().unwrap_or("").trim();
        let list = lines.next().unwrap_or("").trim_end_matches('\n');
        RestoreOp::Modern { mode, list }
    } else {
        RestoreOp::Legacy(text.lines().collect())
    }
}

/// Restore the user's original background (called on Stop / shutdown).
pub fn restore() {
    let sf = state_file();
    let Ok(text) = std::fs::read_to_string(&sf) else {
        return;
    };
    match parse_state(&text) {
        // List first, then mode — the same order `apply` writes in, so there
        // is never a moment where `background-mode` names a list that hasn't
        // landed yet.
        RestoreOp::Modern { mode, list } => {
            if !list.is_empty() {
                gset(CINNAMON_LIST_KEY, list);
            }
            if !mode.is_empty() {
                gset(CINNAMON_MODE_KEY, mode);
            }
        }
        RestoreOp::Legacy(lines) => {
            for (key, v) in schema().keys.iter().zip(lines) {
                if !v.is_empty() {
                    gset(key, v);
                }
            }
        }
    }
    std::fs::remove_file(&sf).ok();
    log::info!("overview background restored");
}

/// Produce a full-size still PNG for the active wallpaper. Uses a fresh
/// timestamped filename each call so GNOME reliably reloads the new image.
///
/// `pub(super)`, not private: `daemon::cosmic_bg` reuses this exact rendering
/// (COSMIC's lock screen has the same "can't see the live wallpaper" problem
/// GNOME's overview/lock screen has — see that module's doc comment) rather
/// than duplicating the ffmpeg/ffmpegthumbnailer logic. No behavior change.
pub(super) fn render_still(w: &Wallpaper) -> Option<PathBuf> {
    let src = match w.kind {
        Kind::Slideshow => {
            let s = w.slideshow.as_ref()?;
            super::slideshow_images(s).into_iter().next()?
        }
        _ => w.effective_path()?.to_path_buf(),
    };
    if !src.exists() {
        return None;
    }
    let dir = cache_dir();
    std::fs::create_dir_all(&dir).ok();
    // Drop previous frames so the cache doesn't grow.
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            let is_frame = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("overview-"));
            if is_frame {
                std::fs::remove_file(p).ok();
            }
        }
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let out = dir.join(format!("overview-{stamp}.png"));
    extract_frame(&src, w.rotation, &out).then_some(out)
}

/// The `ffmpeg -vf` filter that undoes a clockwise `rotation` (mpv's
/// `video-rotate` convention), `None` for an upright wallpaper.
pub(super) fn transpose_filter(rotation: u16) -> Option<&'static str> {
    match rotation % 360 {
        0 => None,
        90 => Some("transpose=1"), // mpv video-rotate is clockwise
        180 => Some("transpose=1,transpose=1"),
        270 => Some("transpose=2"),
        _ => Some("null"),
    }
}

/// Write one full-size still of `src` (a video's poster frame, or the image
/// itself) to `out`, upright for `rotation`. Returns whether `out` was
/// produced. Touches no other file: [`render_still`] owns the "drop the
/// previous `overview-*` frames" bookkeeping, and a caller that must not
/// disturb the desktop's current background frame (the lock-screen preview)
/// calls this directly with its own scratch path.
///
/// The still must match what's on screen — INCLUDING the user's rotation, or
/// the workspace switcher / overview shows the unrotated frame.
/// ffmpegthumbnailer can't rotate, so rotated wallpapers go through ffmpeg
/// when available; without ffmpeg we fall back to the unrotated frame
/// (better than none) and say so in the log.
pub(super) fn extract_frame(src: &Path, rotation: u16, out: &Path) -> bool {
    if let Some(filter) = transpose_filter(rotation) {
        if ffmpeg_frame(src, None, Some(filter), out) {
            return true;
        }
        log::warn!("ffmpeg unavailable/failed; overview frame will not be rotated");
    }
    // ffmpegthumbnailer handles both video frames and images; -s 0 = full size.
    Command::new("ffmpegthumbnailer")
        .args([
            "-i",
            &src.to_string_lossy(),
            "-o",
            &out.to_string_lossy(),
            "-s",
            "0",
            "-q",
            "10",
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// One frame of `src` through plain `ffmpeg`, optionally `seek_s` seconds in
/// and through the `-vf` chain `filter`. The second-chance extractor for when
/// `ffmpegthumbnailer` is missing or hands back a black frame.
pub(super) fn ffmpeg_frame(
    src: &Path,
    seek_s: Option<f32>,
    filter: Option<&str>,
    out: &Path,
) -> bool {
    let mut cmd = Command::new("ffmpeg");
    // -nostdin + null stdio: ffmpeg reads the terminal by default, and from a
    // shell-launched daemon that SIGTTIN-stops the WHOLE process group —
    // daemon suspended, wallpaper frozen.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .args(["-nostdin", "-y", "-loglevel", "error"]);
    if let Some(s) = seek_s.filter(|s| s.is_finite() && *s > 0.0) {
        cmd.args(["-ss", &format!("{s:.2}")]);
    }
    cmd.arg("-i").arg(src).args(["-frames:v", "1"]);
    if let Some(f) = filter {
        cmd.args(["-vf", f]);
    }
    cmd.arg(out).status().map(|s| s.success()).unwrap_or(false)
}

fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("fresco")
}

fn state_file() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("fresco")
        .join("saved-background")
}

/// Save the user's current background once, so Stop can restore it. Guarded by
/// the state file's existence so we never overwrite the real original with our
/// own frame (e.g. across an apply or a re-login while active).
fn save_original_once() {
    let sf = state_file();
    if sf.exists() {
        return;
    }
    if crate::capability::is_cinnamon() && cinnamon_modern() {
        let list = gget(CINNAMON_LIST_KEY);
        let mode = gget(CINNAMON_MODE_KEY);
        // Nothing to restore, or the list is already our own frame (e.g. the
        // state file was lost while our frame was live) — in either case
        // there is no genuine "original" to record.
        if list.is_empty() || is_own_frame_list(&list) {
            return;
        }
        if let Some(d) = sf.parent() {
            std::fs::create_dir_all(d).ok();
        }
        std::fs::write(&sf, format!("{MODERN_MARKER}\n{mode}\n{list}\n")).ok();
        return;
    }
    let values: Vec<String> = schema().keys.iter().map(|k| gget(k)).collect();
    if values.iter().all(String::is_empty) {
        return;
    }
    if let Some(d) = sf.parent() {
        std::fs::create_dir_all(d).ok();
    }
    std::fs::write(&sf, values.join("\n") + "\n").ok();
}

/// Whether the GNOME background schema can be driven at all.
///
/// Two very different failures land here and must not read the same. A
/// `gsettings` that runs and says "no such schema" is simply not a GNOME
/// desktop — the documented no-op, and silent. A `gsettings` that will not
/// *spawn* is a missing package, and on GNOME Wayland that is the whole
/// wallpaper: the static frame this module paints is the only backend Mutter
/// allows, so a user who silently gets no wallpaper has nothing to go on. Name
/// the binary and the package once, at `warn`, rather than returning `false`
/// the way a KDE session does.
fn gnome_available() -> bool {
    let s = schema();
    match Command::new("gsettings")
        .args(["get", s.name, s.keys[0]])
        .output()
    {
        Ok(out) => out.status.success(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Once per daemon, not once per apply: a rotating slideshow calls
            // this on every change, and the fix does not get truer by repetition.
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                log::warn!(
                    "overview: `gsettings` is not installed — install libglib2.0-bin \
                     (Debian/Ubuntu), glib2 (Arch/Fedora) or glib2-tools (openSUSE); \
                     without it Fresco cannot set the GNOME desktop background, which \
                     is the only wallpaper surface available on GNOME Wayland"
                );
            });
            false
        }
        Err(e) => {
            log::debug!("overview: gsettings failed to run: {e}");
            false
        }
    }
}

fn gget(key: &str) -> String {
    Command::new("gsettings")
        .args(["get", schema().name, key])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn gset(key: &str, gvariant: &str) {
    let _ = Command::new("gsettings")
        .args(["set", schema().name, key, gvariant])
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_desktop_gets_the_background_keys_it_draws() {
        let gnome = schema_for(false, false);
        assert_eq!(gnome.name, GNOME_SCHEMA);
        assert!(gnome.uri && gnome.keys.contains(&"picture-uri-dark"));
        let cinnamon = schema_for(true, false);
        assert_eq!(
            (cinnamon.name, cinnamon.keys),
            (CINNAMON_SCHEMA, &["picture-uri"][..])
        );
        // MATE takes a bare path; a URI there is a picture Caja cannot find.
        let mate = schema_for(false, true);
        assert_eq!(
            (mate.name, mate.keys, mate.uri),
            (MATE_SCHEMA, &["picture-filename"][..], false)
        );
    }

    #[test]
    fn gvariant_literal_escapes_backslash_and_quote() {
        assert_eq!(gvariant_string_literal("plain"), "'plain'");
        assert_eq!(gvariant_string_literal("it's"), r"'it\'s'");
        assert_eq!(gvariant_string_literal(r"back\slash"), r"'back\\slash'");
        // Both together, and in the order they appear.
        assert_eq!(gvariant_string_literal(r"a\b'c"), r"'a\\b\'c'");
    }

    #[test]
    fn gvariant_literal_roundtrips_unicode_and_spaces() {
        let s = "日本語 with spaces and % signs";
        let lit = gvariant_string_literal(s);
        assert!(lit.starts_with('\'') && lit.ends_with('\''));
        assert_eq!(&lit[1..lit.len() - 1], s);
    }

    #[test]
    fn file_uri_percent_encodes_special_characters() {
        let uri = encode_file_uri(Path::new("/home/u/My File 100%.png"));
        assert_eq!(uri, "file:///home/u/My%20File%20100%25.png");
    }

    #[test]
    fn file_uri_percent_encodes_unicode() {
        let uri = encode_file_uri(Path::new("/tmp/日本語.png"));
        // UTF-8 bytes of "日本語" percent-encoded, ASCII parts left alone.
        assert_eq!(uri, "file:///tmp/%E6%97%A5%E6%9C%AC%E8%AA%9E.png");
    }

    #[test]
    fn file_uri_leaves_unreserved_characters_alone() {
        let uri = encode_file_uri(Path::new("/home/u/plain-file_name.99~.png"));
        assert_eq!(uri, "file:///home/u/plain-file_name.99~.png");
    }

    #[test]
    fn own_frame_detected_inside_list_text() {
        let dir = cache_dir();
        let ours = format!(
            "[{{'picture-uri': <'file://{}/overview-12345.png'>, 'picture-options': <'zoom'>}}]",
            dir.display()
        );
        assert!(is_own_frame_list(&ours));

        let users = "[{'picture-uri': <'file:///home/u/Pictures/vacation.jpg'>, \
                      'picture-options': <'zoom'>}]";
        assert!(!is_own_frame_list(users));
    }

    #[test]
    fn parse_state_reads_modern_format() {
        let text = format!("{MODERN_MARKER}\n'mirror'\n[{{'picture-uri': <'file:///a.png'>}}]\n");
        let op = parse_state(&text);
        assert_eq!(
            op,
            RestoreOp::Modern {
                mode: "'mirror'",
                list: "[{'picture-uri': <'file:///a.png'>}]",
            }
        );
    }

    #[test]
    fn parse_state_reads_modern_format_with_independent_layout() {
        // A realistic 2-monitor independent list: gsettings prints the whole
        // array literal on one line regardless of how many entries it holds.
        let list = "[{'connector': <'DP-1'>, 'index': <0>, 'picture-uri': <'file:///a.jpg'>, \
                     'picture-options': <'zoom'>}, {'connector': <'HDMI-1'>, 'index': <1>, \
                     'picture-uri': <'file:///b.jpg'>, 'picture-options': <'zoom'>}]";
        let text = format!("{MODERN_MARKER}\n'independent'\n{list}\n");
        let op = parse_state(&text);
        assert_eq!(
            op,
            RestoreOp::Modern {
                mode: "'independent'",
                list,
            }
        );
    }

    #[test]
    fn parse_state_reads_old_format_unchanged() {
        // Old (pre-modern-Cinnamon) state files are one value per line, in
        // schema-key order, with no marker line.
        let text = "'file:///home/u/a.jpg'\n'file:///home/u/b.jpg'\n";
        let op = parse_state(text);
        assert_eq!(
            op,
            RestoreOp::Legacy(vec!["'file:///home/u/a.jpg'", "'file:///home/u/b.jpg'"])
        );
    }

    #[test]
    fn parse_state_old_format_with_empty_value_is_preserved() {
        // e.g. GNOME with only `picture-uri` set and no dark variant.
        let text = "'file:///home/u/a.jpg'\n\n";
        let op = parse_state(text);
        assert_eq!(op, RestoreOp::Legacy(vec!["'file:///home/u/a.jpg'", ""]));
    }

    #[test]
    fn state_file_round_trip_modern_then_legacy_selection() {
        // Sanity: a marker-prefixed blob is never misparsed as legacy, and a
        // legacy blob that happens to start with a line matching some other
        // text is never misparsed as modern.
        let modern = format!("{MODERN_MARKER}\n'mirror'\n[{{}}]\n");
        assert!(matches!(parse_state(&modern), RestoreOp::Modern { .. }));

        let legacy = "'file:///x.png'\n";
        assert!(matches!(parse_state(legacy), RestoreOp::Legacy(_)));
    }

    /// End-to-end against a REAL compiled Cinnamon schema (no live Cinnamon
    /// session needed): `gsettings` talks to a `keyfile` backend rooted at a
    /// temp `XDG_CONFIG_HOME`, so this only ever touches files under `/tmp`.
    ///
    /// Ignored by default because it needs `glib-compile-schemas`/`gsettings`
    /// on PATH and mutates process-wide env vars (`XDG_*`,
    /// `GSETTINGS_SCHEMA_DIR`, `GSETTINGS_BACKEND`, `XDG_CURRENT_DESKTOP`),
    /// which is only safe run alone: `cargo test --all-features --locked \
    /// daemon::overview::tests::modern_cinnamon_backup_apply_restore_round_trip \
    /// -- --ignored --exact --test-threads=1`.
    ///
    /// Fetches `schemas/org.cinnamon.desktop.background.gschema.xml.in` from
    /// `linuxmint/cinnamon-desktop` at setup time (see the shell script this
    /// test's setup mirrors, captured in the PR description) — here the fixed
    /// schema text is embedded directly so the test has no network dependency
    /// at `cargo test` time.
    #[test]
    #[ignore = "needs glib-compile-schemas/gsettings and mutates process env; run explicitly"]
    fn modern_cinnamon_backup_apply_restore_round_trip() {
        use std::sync::Mutex;

        // Guards against this test ever running concurrently with another
        // `#[ignore]`d test in this file that also touches process env.
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();

        let tmp = std::env::temp_dir().join(format!(
            "fresco-overview-it-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let schema_dir = tmp.join("schemas");
        let config_home = tmp.join("config");
        let cache_home = tmp.join("cache");
        let state_home = tmp.join("state");
        for d in [&schema_dir, &config_home, &cache_home, &state_home] {
            std::fs::create_dir_all(d).unwrap();
        }

        std::fs::write(
            schema_dir.join("org.cinnamon.desktop.enums.xml"),
            include_str!("testdata/org.cinnamon.desktop.enums.xml"),
        )
        .unwrap();
        std::fs::write(
            schema_dir.join("org.cinnamon.desktop.background.gschema.xml"),
            include_str!("testdata/org.cinnamon.desktop.background.gschema.xml"),
        )
        .unwrap();
        let compiled = Command::new("glib-compile-schemas")
            .arg(&schema_dir)
            .status();
        match compiled {
            Ok(s) if s.success() => {}
            other => {
                eprintln!("skipping: glib-compile-schemas unavailable or failed: {other:?}");
                std::fs::remove_dir_all(&tmp).ok();
                return;
            }
        }

        // SAFETY: serialized by ENV_LOCK; only this process's env is touched,
        // and only under a temp dir.
        unsafe {
            std::env::set_var("GSETTINGS_SCHEMA_DIR", &schema_dir);
            std::env::set_var("GSETTINGS_BACKEND", "keyfile");
            std::env::set_var("XDG_CONFIG_HOME", &config_home);
            std::env::set_var("XDG_CACHE_HOME", &cache_home);
            std::env::set_var("XDG_STATE_HOME", &state_home);
            std::env::set_var("XDG_CURRENT_DESKTOP", "X-Cinnamon");
            std::env::remove_var("XDG_SESSION_DESKTOP");
        }

        let run = || -> bool {
            assert!(crate::capability::is_cinnamon());
            assert!(cinnamon_modern(), "modern picture-uri-list not detected");

            // Seed a real 2-monitor "independent" layout, as a user who cares
            // about per-monitor wallpapers would have.
            let original_list = "[{'connector': <'DP-1'>, 'index': <0>, 'picture-uri': \
                                  <'file:///home/u/a.jpg'>, 'picture-options': <'zoom'>}, \
                                  {'connector': <'HDMI-1'>, 'index': <1>, 'picture-uri': \
                                  <'file:///home/u/b.jpg'>, 'picture-options': <'zoom'>}]";
            let original_mode = "'independent'";
            gset(CINNAMON_LIST_KEY, original_list);
            gset(CINNAMON_MODE_KEY, original_mode);
            assert_eq!(gget(CINNAMON_LIST_KEY), original_list);
            assert_eq!(gget(CINNAMON_MODE_KEY), original_mode);

            // --- backup ---
            let sf = state_file();
            std::fs::remove_file(&sf).ok();
            save_original_once();
            let saved = std::fs::read_to_string(&sf).expect("state file written");
            match parse_state(&saved) {
                RestoreOp::Modern { mode, list } => {
                    assert_eq!(mode, original_mode);
                    assert_eq!(list, original_list);
                }
                RestoreOp::Legacy(_) => panic!("expected modern state format"),
            }
            // Second call must be a no-op (backup-once guard).
            gset(CINNAMON_LIST_KEY, "[]");
            save_original_once();
            assert_eq!(
                std::fs::read_to_string(&sf).unwrap(),
                saved,
                "save_original_once overwrote an existing backup"
            );
            gset(CINNAMON_LIST_KEY, original_list); // put it back for the apply step

            // --- apply (via the same pure helpers `apply()` uses) ---
            let frame = cache_dir().join("overview-999999.png");
            let uri = encode_file_uri(&frame);
            let list = format!(
                "[{{'picture-uri': <{}>, 'picture-options': <{}>}}]",
                gvariant_string_literal(&uri),
                gvariant_string_literal("zoom")
            );
            gset(CINNAMON_LIST_KEY, &list);
            gset(CINNAMON_MODE_KEY, "'mirror'");
            let applied_list = gget(CINNAMON_LIST_KEY);
            assert!(applied_list.contains(&frame.to_string_lossy().into_owned()));
            assert!(is_own_frame_list(&applied_list));
            assert_eq!(gget(CINNAMON_MODE_KEY), "'mirror'");

            // --- restore ---
            restore();
            assert_eq!(gget(CINNAMON_LIST_KEY), original_list);
            assert_eq!(gget(CINNAMON_MODE_KEY), original_mode);
            assert!(!sf.exists(), "restore should remove the state file");
            true
        };

        let ok = std::panic::catch_unwind(run);

        unsafe {
            std::env::remove_var("GSETTINGS_SCHEMA_DIR");
            std::env::remove_var("GSETTINGS_BACKEND");
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::remove_var("XDG_CACHE_HOME");
            std::env::remove_var("XDG_STATE_HOME");
            std::env::remove_var("XDG_CURRENT_DESKTOP");
        }
        std::fs::remove_dir_all(&tmp).ok();
        ok.unwrap();
    }
}
