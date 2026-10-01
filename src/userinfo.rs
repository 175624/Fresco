//! Who is locked out: login, display name and avatar for the lock-screen
//! greeting/avatar widgets (lock-screen aesthetics feature, wave 1 — the
//! daemon-side glue that turns this into a card lands separately, in wave 2;
//! this module only owns the data).
//!
//! # Two identity sources, in priority order
//!
//! AccountsService (`org.freedesktop.Accounts`, system bus) is asked first for
//! both the display name and the avatar, because it is what every desktop's
//! own greeter and user-switcher already read — GNOME, KDE and COSMIC all let
//! a user set a "full name" and a picture through it, and neither is
//! guaranteed to appear anywhere in `/etc/passwd`. `/etc/passwd` is the
//! fallback for both: the GECOS field's first comma-separated entry is the
//! traditional home of a real name, and it exists on every Linux system with
//! no service required.
//!
//! No D-Bus crate, matching `src/mpris.rs` and `src/daemon/dde.rs`: we shell
//! out to `gdbus`, which both Flatpak runtimes ship. Unlike those two modules
//! this one talks to the **system** bus, not the session bus — accounts are
//! system-wide state, not a per-session one — and it only ever needs to parse
//! a single string variant, never a dictionary, so it gets its own tiny
//! parser, `parse_gvariant_string`, rather than reusing
//! [`crate::mpris::parse_gvariant`]. Two reasons, not one: that parser handles
//! a much bigger grammar than this needs, and reusing it would pull the
//! `daemon` feature gate `mpris` lives behind into a module that otherwise
//! needs nothing beyond `std` and the crate's own i18n macros.
//!
//! # Bounded time, never panics
//!
//! [`current`] does real I/O — two `gdbus --system --timeout 2` round trips in
//! the worst case, plus a handful of local file reads — but it runs once per
//! lock, not on a render loop, and every failure mode (`gdbus` missing, no
//! system bus, no AccountsService, an unreadable `/etc/passwd`) degrades to a
//! fallback rather than an error or a hang.
//!
//! # Pure helpers
//!
//! [`first_name`] and [`greeting`] take already-resolved data and do no I/O at
//! all, so the lock-screen card layout (wave 2) can call them straight from a
//! render function without worrying about blocking.

use std::path::PathBuf;

/// `gdbus --timeout`, seconds. Matches `mpris.rs`'s reasoning: short enough
/// that a wedged or absent AccountsService cannot hold up the lock screen.
const CALL_TIMEOUT_SECS: &str = "2";

/// Who is logged in, resolved once per lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    /// Unix login name. Always set — see [`current`]'s fallback chain.
    pub login: String,
    /// Display name, when one could be found. Never `Some("")` or
    /// whitespace-only; see [`current`]'s filtering.
    pub real_name: Option<String>,
    /// Path to an avatar image that exists and could be opened for reading,
    /// when one could be found.
    pub avatar: Option<PathBuf>,
}

/// Resolve [`UserInfo`] for the process's own user. Does I/O; never panics;
/// bounded time (see the module docs).
pub fn current() -> UserInfo {
    let uid = current_uid();
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let entry = uid.and_then(|u| passwd_entry_for_uid(&passwd, u));

    let login = entry
        .as_ref()
        .map(|e| e.name.clone())
        .or_else(|| non_empty_env("USER"))
        .or_else(|| non_empty_env("LOGNAME"))
        .unwrap_or_default();

    let real_name = uid
        .and_then(accounts_service_real_name)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| entry.as_ref().and_then(|e| gecos_real_name(&e.gecos)));

    let avatar = uid
        .and_then(accounts_service_icon_file)
        .and_then(readable_file)
        .or_else(|| readable_file(PathBuf::from("/var/lib/AccountsService/icons").join(&login)))
        .or_else(|| home_file(".face"))
        .or_else(|| home_file(".face.icon"));

    UserInfo {
        login,
        real_name,
        avatar,
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// The process's own uid, with no `libc` dependency — the same trick as
/// `ipc.rs`'s private `libc_getuid`, but kept as `Option` rather than
/// defaulting to 0: a wrong uid would look up the wrong (or root's) passwd
/// row, which is worse than simply having none to look up with.
pub(crate) fn current_uid() -> Option<u32> {
    std::fs::metadata("/proc/self")
        .ok()
        .map(|m| std::os::unix::fs::MetadataExt::uid(&m))
}

// ---------------------------------------------------------------------------
// /etc/passwd
// ---------------------------------------------------------------------------

/// The fields this module needs from one `/etc/passwd` row.
#[derive(Debug, PartialEq, Eq)]
struct PasswdEntry {
    name: String,
    uid: u32,
    /// Raw GECOS field, comma-separated; index 0 is the real name.
    gecos: String,
}

/// Parse `/etc/passwd` text (`name:passwd:uid:gid:gecos:home:shell`) into
/// rows. Blank lines and `#`-comments are skipped (not part of the format,
/// but tolerated the way glibc's own reader is); a line with too few fields or
/// a non-numeric uid is dropped rather than guessed at — a parser run over
/// this file must never panic on it, however it got hand-edited.
fn parse_passwd(text: &str) -> Vec<PasswdEntry> {
    text.lines()
        .filter_map(|line| {
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let mut fields = line.split(':');
            let name = fields.next()?.to_string();
            let _password = fields.next()?;
            let uid = fields.next()?.parse().ok()?;
            let _gid = fields.next()?;
            let gecos = fields.next().unwrap_or("").to_string();
            Some(PasswdEntry { name, uid, gecos })
        })
        .collect()
}

/// The first row matching `uid`. "First" is a deliberate, tested choice for a
/// hand-edited file with duplicate uids — real systems never have two rows
/// for the same uid, but a parser over untrusted text must still answer
/// *something* rather than pick arbitrarily or panic.
fn passwd_entry_for_uid(text: &str, uid: u32) -> Option<PasswdEntry> {
    parse_passwd(text).into_iter().find(|e| e.uid == uid)
}

/// The real name out of a GECOS field: its first comma-separated entry,
/// trimmed. Empty (a bare `,office,phone` or an empty field entirely) is
/// `None`, not an empty string — matching [`current`]'s contract that
/// `real_name` is never `Some("")`.
fn gecos_real_name(gecos: &str) -> Option<String> {
    let name = gecos.split(',').next().unwrap_or("").trim();
    (!name.is_empty()).then(|| name.to_string())
}

// ---------------------------------------------------------------------------
// AccountsService (system bus)
// ---------------------------------------------------------------------------

/// One `Properties.Get` round trip against
/// `org.freedesktop.Accounts.User<uid>`.
fn accounts_service_property(uid: u32, property: &str) -> Option<String> {
    let object_path = format!("/org/freedesktop/Accounts/User{uid}");
    let out = std::process::Command::new("gdbus")
        .args(["call", "--system", "--timeout", CALL_TIMEOUT_SECS])
        .args([
            "--dest",
            "org.freedesktop.Accounts",
            "--object-path",
            &object_path,
        ])
        .args([
            "--method",
            "org.freedesktop.DBus.Properties.Get",
            "org.freedesktop.Accounts.User",
            property,
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        // Expected whenever AccountsService is absent (minimal distros,
        // containers, most CI) — never worth a log line on every lock.
        return None;
    }
    parse_gvariant_string(&String::from_utf8_lossy(&out.stdout))
}

fn accounts_service_real_name(uid: u32) -> Option<String> {
    accounts_service_property(uid, "RealName")
}

fn accounts_service_icon_file(uid: u32) -> Option<PathBuf> {
    accounts_service_property(uid, "IconFile").map(PathBuf::from)
}

/// Parse the one reply shape every call above can produce: a 1-tuple wrapping
/// a single string variant, e.g. `(<'Roy Das'>,)` or, once the value itself
/// contains an apostrophe, `(<"Roy O'Das">,)`. `g_variant_print` picks
/// whichever quote character the content does not already contain and escapes
/// only that quote and a literal backslash — see `mpris.rs`'s
/// [`crate::mpris::GVal`] docs for the general rule this is a narrow slice of.
/// Anything that is not exactly this shape — an error message, empty output, a
/// dictionary, a non-string scalar — returns `None`.
fn parse_gvariant_string(out: &str) -> Option<String> {
    let b = out.trim().as_bytes();
    let mut i = 0usize;
    if *b.get(i)? != b'(' {
        return None;
    }
    i += 1;
    if *b.get(i)? != b'<' {
        return None;
    }
    i += 1;
    let quote = *b.get(i)?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    i += 1;
    let mut content: Vec<u8> = Vec::with_capacity(b.len());
    loop {
        let c = *b.get(i)?;
        i += 1;
        if c == quote {
            break;
        }
        if c == b'\\' {
            content.push(*b.get(i)?);
            i += 1;
        } else {
            content.push(c);
        }
    }
    if b.get(i) != Some(&b'>') || b.get(i + 1) != Some(&b',') || b.get(i + 2) != Some(&b')') {
        return None;
    }
    String::from_utf8(content).ok()
}

// ---------------------------------------------------------------------------
// Avatar files
// ---------------------------------------------------------------------------

/// `path` if it exists, is a regular file, and can actually be opened for
/// reading — stricter than metadata alone, so a lock-screen card is never
/// handed a path that will only fail later when it tries to decode it.
fn readable_file(path: PathBuf) -> Option<PathBuf> {
    if !std::fs::metadata(&path)
        .map(|m| m.is_file())
        .unwrap_or(false)
    {
        return None;
    }
    std::fs::File::open(&path).ok()?;
    Some(path)
}

fn home_file(name: &str) -> Option<PathBuf> {
    readable_file(dirs::home_dir()?.join(name))
}

// ---------------------------------------------------------------------------
// Pure presentation helpers
// ---------------------------------------------------------------------------

/// The name to greet with: the first word of `real_name`, or the whole thing
/// when it has none (as most CJK names do — there is no family/given split to
/// make), or the login with its first letter upper-cased when there is no
/// real name at all.
pub fn first_name(info: &UserInfo) -> String {
    let real = info
        .real_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match real {
        Some(name) => name.split_whitespace().next().unwrap_or(name).to_string(),
        None => capitalize_first(&info.login),
    }
}

/// Upper-case just the first character. Unicode-aware — not every login is
/// ASCII — and total: empty input returns empty rather than panicking.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The lock-screen greeting for `hour` (0..=23, local time), optionally naming
/// `name`. A `name` of `None` or all-whitespace gets the bare phrase.
///
/// Boundaries: 05:00–11:59 morning, 12:00–16:59 afternoon, 17:00–21:59
/// evening, everything else (including an out-of-range `hour`, which cannot
/// happen from a real clock but must not panic here) night.
pub fn greeting(hour: u32, name: Option<&str>) -> String {
    let name = name.map(str::trim).filter(|s| !s.is_empty());
    match (hour, name) {
        (5..=11, Some(n)) => crate::tf!("Good morning, {name}", "name" => n),
        (5..=11, None) => crate::t!("Good morning").to_string(),
        (12..=16, Some(n)) => crate::tf!("Good afternoon, {name}", "name" => n),
        (12..=16, None) => crate::t!("Good afternoon").to_string(),
        (17..=21, Some(n)) => crate::tf!("Good evening, {name}", "name" => n),
        (17..=21, None) => crate::t!("Good evening").to_string(),
        (_, Some(n)) => crate::tf!("Good night, {name}", "name" => n),
        (_, None) => crate::t!("Good night").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- /etc/passwd -----------------------------------------------------

    #[test]
    fn parses_normal_rows() {
        let text =
            "root:x:0:0:root:/root:/bin/bash\nroy:x:1000:1000:Roy Das,,,:/home/roy:/bin/zsh\n";
        let rows = parse_passwd(text);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].name, "roy");
        assert_eq!(rows[1].uid, 1000);
        assert_eq!(rows[1].gecos, "Roy Das,,,");
        assert_eq!(gecos_real_name(&rows[1].gecos).as_deref(), Some("Roy Das"));
    }

    #[test]
    fn skips_comments_and_blank_lines() {
        let text = "# a comment\n\nroy:x:1000:1000:Roy Das:/home/roy:/bin/zsh\n";
        let rows = parse_passwd(text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "roy");
    }

    #[test]
    fn drops_malformed_lines_without_panicking() {
        let text = "no-colons-at-all\n\
             roy:x:notanumber:1000:Roy:/home/roy:/bin/zsh\n\
             tooshort:x:1000\n\
             valid:x:42:42:Valid User:/home/valid:/bin/sh\n";
        let rows = parse_passwd(text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "valid");
    }

    #[test]
    fn uid_collisions_resolve_to_the_first_row() {
        let text = "first:x:1000:1000:First Person:/home/first:/bin/sh\n\
             second:x:1000:1000:Second Person:/home/second:/bin/sh\n";
        let entry = passwd_entry_for_uid(text, 1000).unwrap();
        assert_eq!(entry.name, "first");
    }

    #[test]
    fn gecos_edge_cases() {
        assert_eq!(gecos_real_name(""), None);
        assert_eq!(gecos_real_name(",office,555"), None);
        assert_eq!(gecos_real_name("   "), None);
        assert_eq!(gecos_real_name("Only Name"), Some("Only Name".to_string()));
        assert_eq!(
            gecos_real_name("Roy Das,Building 2,555-1234"),
            Some("Roy Das".to_string())
        );
    }

    // -- GVariant string parsing ------------------------------------------

    #[test]
    fn gvariant_string_plain() {
        assert_eq!(
            parse_gvariant_string("(<'Roy Das'>,)\n"),
            Some("Roy Das".to_string())
        );
        assert_eq!(
            parse_gvariant_string("(<'/var/lib/AccountsService/icons/roy'>,)"),
            Some("/var/lib/AccountsService/icons/roy".to_string())
        );
    }

    #[test]
    fn gvariant_string_escaped_quote() {
        // Contains an apostrophe, so g_variant_print delimits with double
        // quotes; the literal double quotes around "Big" then have to be
        // escaped so they don't end the string early.
        assert_eq!(
            parse_gvariant_string(r#"(<"O'Brien \"Big\" Roy">,)"#),
            Some("O'Brien \"Big\" Roy".to_string())
        );
        // A literal backslash must round-trip too.
        assert_eq!(
            parse_gvariant_string(r"(<'Roy\\Das'>,)"),
            Some(r"Roy\Das".to_string())
        );
    }

    #[test]
    fn gvariant_string_unicode() {
        assert_eq!(
            parse_gvariant_string("(<'小明'>,)"),
            Some("小明".to_string())
        );
    }

    #[test]
    fn gvariant_string_empty() {
        assert_eq!(parse_gvariant_string("(<''>,)"), Some(String::new()));
    }

    #[test]
    fn gvariant_string_error_output_is_none() {
        assert_eq!(
            parse_gvariant_string(
                "Error: GDBus.Error:org.freedesktop.DBus.Error.ServiceUnknown: \
                 The name org.freedesktop.Accounts was not provided by any .service files\n"
            ),
            None
        );
        assert_eq!(parse_gvariant_string(""), None);
        assert_eq!(parse_gvariant_string("()"), None);
        assert_eq!(parse_gvariant_string("(<42>,)"), None); // not a string variant
    }

    // -- first_name --------------------------------------------------------

    fn info(real_name: Option<&str>, login: &str) -> UserInfo {
        UserInfo {
            login: login.to_string(),
            real_name: real_name.map(str::to_string),
            avatar: None,
        }
    }

    #[test]
    fn first_name_western_takes_the_first_word() {
        assert_eq!(first_name(&info(Some("Roy Das"), "roy")), "Roy");
    }

    #[test]
    fn first_name_cjk_has_no_spaces_so_uses_the_whole_name() {
        assert_eq!(first_name(&info(Some("小明"), "xiaoming")), "小明");
    }

    #[test]
    fn first_name_falls_back_to_capitalized_login() {
        assert_eq!(first_name(&info(None, "royd")), "Royd");
        assert_eq!(first_name(&info(Some("   "), "royd")), "Royd");
        assert_eq!(first_name(&info(None, "")), "");
    }

    // -- greeting ------------------------------------------------------------

    #[test]
    fn greeting_covers_every_hour_boundary() {
        for h in 0..24u32 {
            let (plain, named) = match h {
                5..=11 => ("Good morning", "Good morning, Roy"),
                12..=16 => ("Good afternoon", "Good afternoon, Roy"),
                17..=21 => ("Good evening", "Good evening, Roy"),
                _ => ("Good night", "Good night, Roy"),
            };
            assert_eq!(greeting(h, None), plain, "hour {h}");
            assert_eq!(greeting(h, Some("Roy")), named, "hour {h}");
        }
    }

    #[test]
    fn greeting_treats_blank_name_as_no_name() {
        assert_eq!(greeting(9, Some("   ")), "Good morning");
        assert_eq!(greeting(9, Some("")), "Good morning");
    }

    #[test]
    fn greeting_out_of_range_hour_is_night() {
        assert_eq!(greeting(24, None), "Good night");
        assert_eq!(greeting(100, None), "Good night");
    }

    #[test]
    fn current_does_not_panic() {
        // Whatever this machine/CI container actually is, resolving the
        // current user must never panic — only degrade. Not panicking here
        // *is* the assertion; there is no fixed expected value to compare to.
        let info = current();
        let _ = first_name(&info);
    }
}
