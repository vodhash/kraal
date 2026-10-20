use std::path::{Path, PathBuf};

/// Longest Unix socket path both Linux (108 bytes) and macOS (104) accept, its final NUL aside.
pub(super) const SOCKET_PATH_MAX: usize = 103;
/// Longest session name kraal accepts. Never lowered: a session named when it was allowed must
/// stay reachable by its name.
pub(super) const SESSION_NAME_MAX: usize = 64;
/// Longest socket file name before `.sock`: a longer session name gets a shortened one.
pub(super) const SOCKET_STEM_MAX: usize = 32;
/// Room for the runtime base directory: `<base>/kraal/<stem>.sock` must fit.
pub(super) const RUNTIME_BASE_MAX: usize = SOCKET_PATH_MAX - "/kraal/".len() - SOCKET_STEM_MAX - ".sock".len();

/// Where sockets go when `$XDG_RUNTIME_DIR` is missing, too long or not safe: `kraal-<uid>` in
/// `$TMPDIR` when that is short and safe, else in /tmp (macOS's `$TMPDIR` is long). Other users
/// can write there, so [`private_runtime_dir`] checks it.
pub(super) fn fallback_runtime_base() -> PathBuf {
    // SAFETY: getuid cannot fail.
    let name = format!("kraal-{}", unsafe { libc::getuid() });
    let tmp = checked_base(&std::env::temp_dir(), RUNTIME_BASE_MAX - 1 - name.len());
    // /tmp itself (a link owned by root on macOS); the directory in it is checked as created.
    tmp.unwrap_or_else(|| PathBuf::from("/tmp")).join(name)
}

/// `dir` as a path checked as it is returned ([`safe_chain`]), when it is short enough: links
/// resolved, or, when that is too long, as given (a short link of ours to a deep directory).
fn checked_base(dir: &Path, max_len: usize) -> Option<PathBuf> {
    let path = short_spelling(dir, max_len)?;
    (path.is_dir() && safe_chain(&path)).then_some(path)
}

/// `path` with its links resolved when that fits in `max_len` bytes, else as given when that
/// fits (an absolute path): the spelling to check, then use.
pub(super) fn short_spelling(path: &Path, max_len: usize) -> Option<PathBuf> {
    let fits = |p: &Path| p.as_os_str().len() <= max_len;
    match path.canonicalize() {
        Ok(real) if fits(&real) => Some(real),
        Ok(_) if path.is_absolute() && fits(path) => Some(path.to_path_buf()),
        _ => None,
    }
}

/// Whether nobody else can swap `path`, or what is in it, read as it is spelt (never resolved to
/// another path first, which a swap between the two would defeat): from the root down, every
/// directory is owned by us or by root, and others cannot write to it, unless it is sticky like
/// /tmp (then they cannot move what is ours); a link on the way is ours or root's and leads to
/// a place that passes the same check. A socket under a directory another user controls could be
/// replaced by theirs.
pub(super) fn safe_chain(path: &Path) -> bool {
    safe_chain_at(path, 0)
}

fn safe_chain_at(path: &Path, depth: usize) -> bool {
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;
    if depth > 8 || !path.is_absolute() {
        return false;
    }
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let mut at = PathBuf::new();
    for part in path.components() {
        match part {
            Component::RootDir => at.push("/"),
            Component::Normal(name) => at.push(name),
            // `.` and `..` would go through links: each step of the path is named.
            _ => return false,
        }
        let Ok(m) = std::fs::symlink_metadata(&at) else { return false };
        let owner_ok = m.uid() == uid || m.uid() == 0;
        let ok = if m.file_type().is_symlink() {
            owner_ok && at.canonicalize().is_ok_and(|target| safe_chain_at(&target, depth + 1))
        } else {
            m.is_dir() && owner_ok && (m.mode() & 0o022 == 0 || m.mode() & 0o1000 != 0)
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Directory holding one control socket per running session.
pub fn runtime_dir() -> PathBuf {
    runtime_dir_in(std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
}

pub(super) fn runtime_dir_in(xdg: Option<PathBuf>) -> PathBuf {
    let base = xdg.and_then(|p| checked_base(&p, RUNTIME_BASE_MAX)).unwrap_or_else(fallback_runtime_base);
    base.join("kraal")
}

/// Why the sockets are not in `$XDG_RUNTIME_DIR` although it is set, and where they are: said
/// when a server is started (to who starts it, and in its log) and by `kraal config check`,
/// since whoever set it expects them there. `None` when it is used, or not set: the sockets
/// then go where they go without a word.
pub fn runtime_dir_refused() -> Option<String> {
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()).map(PathBuf::from)?;
    let why = base_refused(&xdg, RUNTIME_BASE_MAX)?;
    Some(format!("XDG_RUNTIME_DIR ({}) is not used: {why}. Sockets are in {}", xdg.display(), runtime_dir().display()))
}

/// Why [`checked_base`] does not take `dir`, if it does not.
fn base_refused(dir: &Path, max_len: usize) -> Option<String> {
    if checked_base(dir, max_len).is_some() {
        return None;
    }
    Some(if !dir.is_dir() {
        "it is not a directory".into()
    } else if short_spelling(dir, max_len).is_none() {
        format!("it is {} bytes long, and a socket path leaves it {max_len}", dir.as_os_str().len())
    } else {
        "another user could replace it, or what is in it".into()
    })
}

/// Create `dir` (and missing parents) and make sure it is a real directory owned by us and
/// private (0700). Refuses symlinks and directories owned by someone else: a socket in such a
/// place could be read or replaced by another user.
pub fn ensure_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    std::fs::create_dir_all(dir)?;
    let meta = std::fs::symlink_metadata(dir)?;
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    if !meta.is_dir() || meta.uid() != uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is not a directory owned by you; refusing to use it", dir.display()),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The runtime directory, made private ([`ensure_private_dir`]). Under the fallback base
/// (`kraal-<uid>` in a temporary directory every user can write to), that base is checked first:
/// another user who made it could otherwise swap the directory under it.
pub fn private_runtime_dir() -> std::io::Result<PathBuf> {
    let dir = runtime_dir();
    if dir.parent() == Some(fallback_runtime_base().as_path()) {
        ensure_private_dir(&fallback_runtime_base())?;
    }
    ensure_private_dir(&dir)?;
    Ok(dir)
}
/// The directories of kraal's own that its atomic writes go to.
pub fn written_dirs() -> Vec<PathBuf> {
    let state = state_dir();
    vec![
        state.join("sessions-v2"),
        state.join("buffers"),
        state.join("runs"),
        state.join("usage"),
        state.join("usage/by-id"),
        state,
        workspaces_dir(),
        config_dir(),
    ]
}
/// Bytes a shared SSH connection's socket name takes after its directory: `/ssh-` and kraal's
/// 16-character key (see `servers::ssh_args`), `-`, ssh's 40-character `%C`, and the
/// `.<16 random characters>` ssh adds while it sets the connection up.
const SSH_SOCKET_NAME: usize = "/ssh-".len() + 16 + 1 + 40 + 17;

/// Directory for the sockets of shared SSH connections: private (a socket another user could
/// replace would see every command), and short enough for ssh's socket names. `None` when no
/// such directory can be had: connections are then not shared.
pub fn ssh_control_dir() -> Option<PathBuf> {
    let dir = runtime_dir();
    if dir.as_os_str().len() + SSH_SOCKET_NAME <= SOCKET_PATH_MAX {
        return private_runtime_dir().ok();
    }
    // Directly in /tmp, whose sticky bit keeps others from moving it once it is ours.
    // SAFETY: getuid cannot fail.
    let short = PathBuf::from(format!("/tmp/kraal-{}", unsafe { libc::getuid() }));
    ensure_private_dir(&short).ok()?;
    Some(short)
}

/// An XDG base directory: `$var` when it is an absolute path, else `~/<fallback>`. The same on
/// macOS as on Linux: terminal tools keep their files there, not in `~/Library`.
fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home_dir().map(|h| h.join(fallback)))
        .unwrap_or_else(std::env::temp_dir)
}

/// `~/.local/state/kraal`
pub fn state_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state").join("kraal")
}

/// `~/.local/share`
pub fn data_home() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

/// The saved state of `session` (layout, panes, what each runs), which a restart restores.
pub fn session_state_path(session: &str) -> PathBuf {
    state_dir().join("sessions-v2").join(format!("{session}.json"))
}

// Keep recognizing old files after their migration is removed, so startup can refuse them.
pub fn legacy_session_state_path(session: &str) -> PathBuf {
    state_dir().join("sessions").join(format!("{session}.json"))
}

pub fn saved_sessions() -> std::io::Result<std::collections::BTreeSet<String>> {
    saved_sessions_in(&state_dir())
}

pub(super) fn saved_sessions_in(state: &Path) -> std::io::Result<std::collections::BTreeSet<String>> {
    let mut names = std::collections::BTreeSet::new();
    for dir in ["sessions-v2", "sessions"] {
        let entries = match std::fs::read_dir(state.join(dir)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json")
                && let Some(name) = path.file_stem().and_then(|s| s.to_str()).filter(|s| valid_session_name(s))
            {
                names.insert(name.to_string());
            }
        }
    }
    Ok(names)
}

/// Where the panes' saved scrollback of `session` goes, one file per pane.
pub fn scrollback_dir(session: &str) -> PathBuf {
    state_dir().join("scrollback-v2").join(session)
}

/// Where the paste buffers of `session` are kept across restarts, when they are (`[buffers]
/// persist`).
pub fn buffers_path(session: &str) -> PathBuf {
    state_dir().join("buffers").join(format!("{session}.json"))
}

/// Where a buffers' file of `session` that could not be read back may be kept aside, the first
/// free one taken: `<session>.json.bad`, then `.bad.1` to `.bad.9`.
pub fn buffers_aside_paths(session: &str) -> impl Iterator<Item = PathBuf> {
    let path = buffers_path(session);
    (0..10).map(move |n| path.with_extension(if n == 0 { "json.bad".into() } else { format!("json.bad.{n}") }))
}

/// The log of `session`'s server, which runs detached from any terminal: its standard output and
/// error, and those of a restart run on its own ([`crate::client::restart_detached`]).
pub fn log_path(session: &str) -> PathBuf {
    state_dir().join(format!("server-{session}.log"))
}

/// `~/.config/kraal`
pub fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("kraal")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn workspaces_dir() -> PathBuf {
    config_dir().join("workspaces")
}

/// The home directory: `HOME`, else the password database's. Never empty: an empty `HOME`
/// names no directory.
pub fn home_dir() -> Option<PathBuf> {
    std::env::home_dir().filter(|home| !home.as_os_str().is_empty())
}

/// Expand a leading `~` or `~/` to the home directory. `~user` is left as it is.
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// `path` with its `.` and `..` worked out from the text alone.
pub fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Replace the home directory prefix with `~` for display.
pub fn shorten_home(path: &Path) -> String {
    let Some(home) = home_dir() else { return path.display().to_string() };
    shorten_home_in(path, &home)
}

pub(super) fn shorten_home_in(path: &Path, home: &Path) -> String {
    let resolved = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    if let Ok(rest) = path.strip_prefix(home).or_else(|_| path.strip_prefix(&resolved)) {
        if rest.as_os_str().is_empty() {
            return "~".into();
        }
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// Whether `path` is the developer's while a test runs this kraal (debug builds, as
/// [`of_the_developer`] tells): their session, which a test's command may run under.
pub fn kept_out(path: &Path) -> bool {
    #[cfg(debug_assertions)]
    if let Some(keep_out) = std::env::var_os("KRAAL_TEST_KEEP_OUT") {
        return std::env::split_paths(&keep_out).any(|theirs| path.starts_with(theirs));
    }
    let _ = path;
    false
}

/// What this kraal would use of the developer's while a test runs it, if anything. Debug builds
/// only: the tests run kraal in directories of their own and name in `KRAAL_TEST_KEEP_OUT`, as
/// `PATH` names its directories, the developer's home and where their sessions are. A test that
/// lets one through (as a home, a directory of kraal's, an agent's home, a directory of `PATH`)
/// fails on this, where it would have read their accounts, run their agents or reached their
/// session.
#[cfg(debug_assertions)]
pub fn of_the_developer() -> Option<String> {
    let keep_out = std::env::var_os("KRAAL_TEST_KEEP_OUT")?;
    let followed = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let own = [
        ("HOME", home_dir()),
        ("the configuration directory", Some(config_dir())),
        ("the state directory", Some(state_dir())),
        ("the data directory", Some(data_home())),
        ("the runtime directory", Some(runtime_dir())),
    ];
    let agents = crate::accounts::agents().map(|a| (a.home_env, std::env::var_os(a.home_env).map(PathBuf::from)));
    let path = std::env::var_os("PATH").unwrap_or_default();
    let used: Vec<(&str, PathBuf)> = own
        .into_iter()
        .chain(agents)
        .filter_map(|(what, dir)| Some((what, dir?)))
        .chain(std::env::split_paths(&path).map(|dir| ("PATH", dir)))
        .collect();
    std::env::split_paths(&keep_out).find_map(|theirs| {
        let inside = |dir: &Path| dir.starts_with(&theirs) || followed(dir).starts_with(followed(&theirs));
        let (what, dir) = used.iter().find(|(_, dir)| inside(dir))?;
        Some(format!("{what} ({}) is the developer's: the tests stay out of {}", dir.display(), theirs.display()))
    })
}

/// Session names end up in file names; keep them tame and reasonably short.
pub fn valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= SESSION_NAME_MAX
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.')
}
