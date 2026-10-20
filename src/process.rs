//! What other processes are doing: command line, environment, working directory, the TCP
//! ports listening on this machine, and who is behind a Unix socket. Linux reads `/proc`; macOS
//! asks the kernel (sysctl and libproc) and `lsof` for ports.

use std::collections::{HashMap, HashSet};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// Ownership of a terminal session across a server crash. The token is inherited by new
/// children, so a surviving member can identify the session after its leader has been reaped.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionOwner {
    pub leader: u32,
    pub started: u64,
    pub token: String,
}

impl SessionOwner {
    pub fn members(&self) -> Vec<u32> {
        let members: Vec<_> = session_members(self.leader as libc::pid_t).into_iter().map(|p| p as u32).collect();
        let owned = match start_time(self.leader) {
            Some(started) => started == self.started,
            None => members.iter().any(|p| environ(*p, &["KRAAL_PROCESS"]).get("KRAAL_PROCESS") == Some(&self.token)),
        };
        if owned { members } else { Vec::new() }
    }
}

/// The command line of a process; `None` when it cannot be read, or is empty (a kernel thread).
pub fn argv(pid: u32) -> Option<Vec<String>> {
    #[cfg(test)]
    if FAIL_ARGV.with(|fail| fail.replace(false)) {
        return None;
    }
    let (argv, _) = imp::args_and_env(pid)?;
    (!argv.is_empty()).then_some(argv)
}

/// Some variables of a process's environment, as it was started.
pub fn environ(pid: u32, names: &[&str]) -> HashMap<String, String> {
    command_env(pid, names).map(|(_, env)| env).unwrap_or_default()
}

/// Read the command and selected environment together; unreadable is not an empty environment.
pub fn command_env(pid: u32, names: &[&str]) -> Option<(Vec<String>, HashMap<String, String>)> {
    #[cfg(test)]
    if FAIL_COMMAND_ENV.with(|fail| fail.replace(false)) {
        return None;
    }
    #[cfg(target_os = "linux")]
    let (argv, env) = (
        nul_strings(&std::fs::read(format!("/proc/{pid}/cmdline")).ok()?),
        nul_strings(&std::fs::read(format!("/proc/{pid}/environ")).ok()?),
    );
    #[cfg(not(target_os = "linux"))]
    let (argv, env) = imp::args_and_env(pid)?;
    let env = env
        .into_iter()
        .filter_map(|e| {
            let (k, v) = e.split_once('=')?;
            names.contains(&k).then(|| (k.to_string(), v.to_string()))
        })
        .collect();
    Some((argv, env))
}

/// The kraal variables of `pid`'s environment, or of the nearest process it runs under that
/// has any: what places a command in a kraal pane, even when it dropped them itself. A session
/// of the developer's, which tests may be run from, does not count while they run.
pub fn kraal_env(pid: u32) -> HashMap<String, String> {
    let names = ["KRAAL_SESSION", "KRAAL_PANE", "KRAAL_SOCKET"];
    let theirs =
        |env: &HashMap<String, String>| env.get("KRAAL_SOCKET").is_some_and(|s| crate::paths::kept_out(Path::new(s)));
    lineage(pid).into_iter().map(|p| environ(p, &names)).find(|env| !env.is_empty() && !theirs(env)).unwrap_or_default()
}

#[cfg(test)]
thread_local! {
    pub(crate) static FAIL_ARGV: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(crate) static FAIL_COMMAND_ENV: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    #[cfg(target_os = "linux")]
    pub(crate) static AFTER_GROUP_ENUMERATION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// The working directory of a process; `None` when the system does not show it (a process of
/// another user, or one that is gone).
pub fn cwd(pid: u32) -> Option<PathBuf> {
    imp::cwd(pid)
}

/// TCP ports something listens on, on this machine.
pub fn listening_ports() -> HashSet<u16> {
    imp::listening_ports()
}

/// Process id and user of the process that listens on the other end of `stream`.
pub fn socket_peer(stream: &UnixStream) -> std::io::Result<(libc::pid_t, libc::uid_t)> {
    imp::socket_peer(stream.as_raw_fd())
}

/// Whether a process with this id still runs. One that exited but was not reaped yet counts as
/// gone.
pub fn running(pid: libc::pid_t) -> bool {
    imp::running(pid)
}

/// The processes (id, program name) whose working directory is `dir` or inside it, or that hold
/// a file open there: what may still write there (a shell, an agent, an editor, a
/// build writing through absolute paths). Not this process.
pub fn working_in(dir: &Path) -> Vec<(u32, String)> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let me = std::process::id();
    imp::pids()
        .into_iter()
        .filter(|p| {
            *p != me
                && (cwd(*p).is_some_and(|c| c.starts_with(&dir))
                    || imp::open_files(*p).iter().any(|f| f.starts_with(&dir)))
        })
        .map(|p| {
            let name = argv(p).and_then(|a| a.into_iter().next()).unwrap_or_default();
            (p, name.rsplit('/').next().unwrap_or(&name).to_string())
        })
        .collect()
}

/// The processes started with `name=value` in their environment (inherited from whatever kraal
/// started with it), even those that left their parent, group and session: a daemon started by
/// a wrapper keeps the environment it was started with. Not this process.
pub fn tagged(name: &str, value: &str) -> Vec<u32> {
    let me = std::process::id();
    imp::pids().into_iter().filter(|p| *p != me && environ(*p, &[name]).get(name).is_some_and(|v| v == value)).collect()
}

/// Every process descending from `pid` (children, theirs, and so on), even those that left its
/// process group or session (`setsid`): while `pid` is alive, they are still its descendants.
pub fn descendants(pid: u32) -> Vec<u32> {
    let parents: Vec<(u32, u32)> = imp::pids().into_iter().filter_map(|p| Some((p, imp::ppid(p)?))).collect();
    let mut found = vec![pid];
    let mut i = 0;
    while i < found.len() {
        let parent = found[i];
        found.extend(parents.iter().filter(|(_, pp)| *pp == parent).map(|(p, _)| *p));
        i += 1;
    }
    found.remove(0);
    found
}

/// The parent of a process; `None` once it is gone.
pub fn ppid(pid: u32) -> Option<u32> {
    imp::ppid(pid)
}

/// `pid` and the processes it descends from (the one that started it, the one that started
/// that one, and so on), nearest first.
pub fn lineage(pid: u32) -> Vec<u32> {
    let mut line = vec![pid];
    let mut next = ppid(pid);
    while let Some(parent) = next.filter(|p| *p > 1 && !line.contains(p)) {
        line.push(parent);
        next = ppid(parent);
    }
    line
}

/// Whether the process that had this id and start time (see [`start_time`]) still runs: not
/// another one that got its id since.
pub fn still_running(pid: u32, started: u64) -> bool {
    libc::pid_t::try_from(pid).is_ok_and(running) && start_time(pid) == Some(started)
}

/// The processes `pid` started that still run as its children.
pub fn children(pid: u32) -> Vec<u32> {
    imp::pids().into_iter().filter(|p| imp::ppid(*p) == Some(pid)).collect()
}

/// The process group in the foreground of the terminal `pid` runs in (the one a shell gives
/// the keyboard to while it runs a command), if it has a terminal.
pub fn foreground_group(pid: u32) -> Option<u32> {
    imp::foreground_group(pid).filter(|g| *g > 0)
}

/// Every process running on this machine that the system shows us.
pub fn pids() -> Vec<u32> {
    imp::pids()
}

/// When a process started, as a number that, with its id, names it and no other process of this
/// boot (Linux: clock ticks after boot, see [`boot_id`]; macOS: microseconds since 1970).
pub fn start_time(pid: u32) -> Option<u64> {
    imp::start_time(pid)
}

/// This machine as its system names it (Linux's machine-id, macOS's host UUID), if it does.
pub fn machine_id() -> Option<String> {
    imp::machine_id()
}

/// The id of the machine's current boot on Linux, where [`start_time`] counts from the boot: a
/// start time saved with it names a process only while the id is the same. Empty on macOS, whose
/// start times do not depend on the boot.
pub fn boot_id() -> String {
    imp::boot_id()
}

/// Whether something still runs in process group `pgid`. A member that ended but was not reaped
/// yet does not count: a pane keeps its ended leader unreaped (see `pane::pty::Process`).
pub fn group_running(pgid: libc::pid_t) -> bool {
    pgid > 1 && exists(-pgid) && group_running_in(|| imp::group_snapshot(pgid))
}

fn group_running_in(mut snapshot: impl FnMut() -> Option<Vec<(libc::pid_t, bool)>>) -> bool {
    let Some(mut before) = snapshot() else { return true };
    if before.iter().any(|(_, running)| *running) {
        return true;
    }
    // A member may fork after enumeration, then exit before its state is read. Only complete,
    // matching lists of dead members establish completion: dead members cannot fork again.
    let Some(mut after) = snapshot() else { return true };
    before.sort_unstable();
    after.sort_unstable();
    before != after
}

/// The processes of session `sid` (a pane's program leads one, with everything started from
/// it that did not leave), zombies left out.
pub fn session_members(sid: libc::pid_t) -> Vec<libc::pid_t> {
    if sid <= 1 {
        return Vec::new();
    }
    imp::session_parents(sid).into_iter().map(|(pid, _)| pid).collect()
}

/// Whether stopping this terminal would affect only its shell and the foreground agent's children.
/// `None` means the process table could not establish the ancestry of every member.
pub fn sole_foreground_job(leader: u32, foreground: u32) -> Option<bool> {
    sole_foreground_job_in(leader, foreground, imp::session_parents(leader as libc::pid_t))
}

fn sole_foreground_job_in(leader: u32, foreground: u32, members: Vec<(libc::pid_t, u32)>) -> Option<bool> {
    // Keep the parents read with the session membership. A child's ancestry may no longer be
    // readable after it exits, even while kill(pid, 0) still sees its unreaped process on macOS.
    let parents: HashMap<_, _> = members.into_iter().map(|(pid, parent)| (pid as u32, parent)).collect();
    if !parents.contains_key(&leader) || !parents.contains_key(&foreground) {
        return None;
    }
    for &pid in parents.keys().filter(|&&pid| pid != leader) {
        let mut next = pid;
        let mut seen = HashSet::new();
        while next != foreground {
            if next == leader || next <= 1 {
                return Some(false);
            }
            if !seen.insert(next) {
                return None;
            }
            next = *parents.get(&next)?;
        }
    }
    Some(true)
}

/// The jobs left in the background of the terminal of the session `leader` leads (a shell's
/// `&`): the process groups of the session, but the leader's own and the one in the terminal's
/// foreground, with processes that have the terminal open; each by the command line of one of
/// those, its leader when it is one. Hanging the session up stops them. What keeps off the
/// terminal (a prompt's daemon, `nohup`) is no job of the user's, nor are the processes `not`
/// (whoever asks, when it runs there itself): a group of those alone is no job.
pub fn background_jobs(leader: u32, not: &[u32]) -> Vec<Vec<String>> {
    background_processes(leader, not).into_iter().filter_map(argv).collect()
}

/// The processes representing the terminal's background jobs (see [`background_jobs`]).
pub fn background_processes(leader: u32, not: &[u32]) -> Vec<u32> {
    let (Ok(sid), Some(terminal)) = (libc::pid_t::try_from(leader), imp::terminal_of(leader)) else {
        return Vec::new();
    };
    let foreground = foreground_group(leader).and_then(|g| libc::pid_t::try_from(g).ok());
    // The process shown for each job, by group.
    let mut shown: std::collections::BTreeMap<libc::pid_t, libc::pid_t> = Default::default();
    for pid in session_members(sid) {
        let Some(group) = imp::group_of(pid).filter(|g| *g != sid && Some(*g) != foreground) else { continue };
        if not.contains(&(pid as u32)) || !imp::open_files(pid as u32).contains(&terminal) {
            continue;
        }
        let process = shown.entry(group).or_insert(pid);
        if pid == group {
            *process = pid;
        }
    }
    shown.into_values().map(|pid| pid as u32).collect()
}

/// Whether the process `pid` exists, or the group `-pgid` has members, zombies included. One of
/// another user's (a program run through sudo) exists too: it only may not be signalled.
pub fn exists(target: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the process or group exists and may be signalled.
    unsafe { libc::kill(target, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

/// Split NUL-separated strings, skipping empty ones (Linux `/proc` files).
#[cfg_attr(all(not(target_os = "linux"), not(test)), expect(dead_code, reason = "Linux only, tested everywhere"))]
fn nul_strings(raw: &[u8]) -> Vec<String> {
    raw.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect()
}

/// Parse a `KERN_PROCARGS2` buffer (macOS): argc, the executable path, padding, argv, then
/// the environment.
#[cfg_attr(all(not(target_os = "macos"), not(test)), expect(dead_code, reason = "macOS only, tested everywhere"))]
fn parse_procargs2(buf: &[u8]) -> Option<(Vec<String>, Vec<String>)> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let rest = &buf[4..];
    // Skip the executable path and the NULs that pad it.
    let path_end = rest.iter().position(|b| *b == 0)?;
    let start = path_end + rest[path_end..].iter().position(|b| *b != 0)?;
    let strings: Vec<String> =
        rest[start..].split(|b| *b == 0).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
    let argv: Vec<String> = strings.iter().take(argc).cloned().collect();
    let env: Vec<String> = strings.into_iter().skip(argc).take_while(|s| !s.is_empty()).collect();
    Some((argv, env))
}

/// Ports in `lsof -Fn` output (macOS).
#[cfg_attr(all(not(target_os = "macos"), not(test)), expect(dead_code, reason = "macOS only, tested everywhere"))]
fn parse_lsof(text: &str) -> HashSet<u16> {
    text.lines()
        .filter_map(|l| l.strip_prefix('n'))
        .filter_map(|addr| addr.rsplit_once(':'))
        .filter_map(|(_, port)| port.parse().ok())
        .collect()
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    pub fn args_and_env(pid: u32) -> Option<(Vec<String>, Vec<String>)> {
        let argv = nul_strings(&std::fs::read(format!("/proc/{pid}/cmdline")).ok()?);
        let env = std::fs::read(format!("/proc/{pid}/environ")).map(|r| nul_strings(&r)).unwrap_or_default();
        Some((argv, env))
    }

    pub fn cwd(pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    /// The files a process holds open. Only our own processes' descriptors can be read, and only
    /// those matter here.
    pub fn open_files(pid: u32) -> Vec<PathBuf> {
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { return Vec::new() };
        fds.flatten().filter_map(|fd| std::fs::read_link(fd.path()).ok()).filter(|p| p.is_absolute()).collect()
    }

    pub fn pids() -> Vec<u32> {
        let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
        dir.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect()
    }

    pub fn start_time(pid: u32) -> Option<u64> {
        // After the command name: the 20th field is the start time, in clock ticks after boot.
        let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
        let end = stat.iter().rposition(|b| *b == b')')?;
        let rest = String::from_utf8_lossy(&stat[end + 1..]).into_owned();
        rest.split_whitespace().nth(19)?.parse().ok()
    }

    pub fn machine_id() -> Option<String> {
        ["/etc/machine-id", "/var/lib/dbus/machine-id"]
            .iter()
            .filter_map(|f| std::fs::read_to_string(f).ok())
            .map(|s| s.trim().to_string())
            .find(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric()))
    }

    pub fn boot_id() -> String {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
            .unwrap_or_default()
    }

    pub fn ppid(pid: u32) -> Option<u32> {
        // After the command name (in parentheses, any bytes): state, then the parent's id.
        let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
        let end = stat.iter().rposition(|b| *b == b')')?;
        let rest = String::from_utf8_lossy(&stat[end + 1..]).into_owned();
        rest.split_whitespace().nth(1)?.parse().ok()
    }

    pub fn foreground_group(pid: u32) -> Option<u32> {
        // After the command name: state, parent, group, session, terminal, its foreground group
        // (-1 without a terminal).
        let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
        let end = stat.iter().rposition(|b| *b == b')')?;
        let rest = String::from_utf8_lossy(&stat[end + 1..]).into_owned();
        rest.split_whitespace().nth(5)?.parse::<i64>().ok().and_then(|g| u32::try_from(g).ok())
    }

    pub fn socket_peer(fd: std::os::unix::io::RawFd) -> std::io::Result<(libc::pid_t, libc::uid_t)> {
        // SAFETY: plain C data (integers, arrays): all zero bytes are a valid value.
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: SO_PEERCRED writes at most `len` bytes into `cred`.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast::<libc::c_void>(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((cred.pid, cred.uid))
    }

    pub fn running(pid: libc::pid_t) -> bool {
        std::fs::read(format!("/proc/{pid}/stat")).is_ok_and(|stat| runs(&stat))
    }

    /// Whether `stat`, a process's line in `/proc`, is that of one that still runs: not one that
    /// ended and waits to be reaped (Z), nor one being reaped (X), which is there for a moment
    /// between the two.
    pub fn runs(stat: &[u8]) -> bool {
        // The state follows the command name, which is in parentheses and may hold any byte.
        let Some(end) = stat.iter().rposition(|b| *b == b')') else { return false };
        !matches!(stat[end + 1..].iter().find(|b| !b.is_ascii_whitespace()), Some(b'Z' | b'X'))
    }

    pub fn session_parents(sid: libc::pid_t) -> Vec<(libc::pid_t, u32)> {
        let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
        dir.filter_map(|e| e.ok())
            .filter_map(|e| {
                let pid: libc::pid_t = e.file_name().to_str()?.parse().ok()?;
                let stat = std::fs::read(e.path().join("stat")).ok()?;
                let end = stat.iter().rposition(|b| *b == b')')?;
                // After the name: state, parent, process group, session.
                let fields: Vec<&[u8]> =
                    stat[end + 1..].split(u8::is_ascii_whitespace).filter(|f| !f.is_empty()).take(4).collect();
                let session = std::str::from_utf8(fields.get(3)?).ok()?.parse::<libc::pid_t>().ok()?;
                let parent = std::str::from_utf8(fields.get(1)?).ok()?.parse().ok()?;
                (session == sid && !matches!(fields[0], b"Z" | b"X")).then_some((pid, parent))
            })
            .collect()
    }

    pub fn group_of(pid: libc::pid_t) -> Option<libc::pid_t> {
        // After the command name: state, parent, process group.
        let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
        let end = stat.iter().rposition(|b| *b == b')')?;
        let rest = String::from_utf8_lossy(&stat[end + 1..]).into_owned();
        rest.split_whitespace().nth(2)?.parse().ok()
    }

    /// The terminal a process runs on: its controlling terminal, when that is a pseudo-terminal
    /// (a pane's is). Not what its standard input is: a shell has that elsewhere for a while
    /// when it redirects itself (dash does, while a command given `< file` runs).
    pub fn terminal_of(pid: u32) -> Option<PathBuf> {
        use std::os::unix::fs::MetadataExt;
        // After the command name: state, parent, group, session, then the terminal's device (0
        // without one) as the kernel packs it: the major number above the low byte of the minor,
        // the rest of the minor above both.
        let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
        let end = stat.iter().rposition(|b| *b == b')')?;
        let rest = String::from_utf8_lossy(&stat[end + 1..]).into_owned();
        let device: u32 = rest.split_whitespace().nth(4)?.parse().ok()?;
        let (major, minor) = ((device >> 8) & 0xfff, (device & 0xff) | ((device >> 12) & 0xfff00));
        // A pseudo-terminal's name in /dev/pts is its minor number, under this major.
        const PTS_MAJOR: u32 = 136;
        let path = PathBuf::from(format!("/dev/pts/{minor}"));
        let named =
            major == PTS_MAJOR && std::fs::metadata(&path).is_ok_and(|m| m.rdev() == libc::makedev(major, minor));
        named.then_some(path)
    }

    pub fn group_snapshot(pgid: libc::pid_t) -> Option<Vec<(libc::pid_t, bool)>> {
        let mut pids = Vec::new();
        for entry in std::fs::read_dir("/proc").ok()? {
            let entry = entry.ok()?;
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<libc::pid_t>().ok()) else {
                continue;
            };
            // SAFETY: getpgid only reads the process table. Select the group before reading
            // states, so unrelated processes exiting cannot invalidate its snapshot.
            let group = unsafe { libc::getpgid(pid) };
            if group == pgid {
                pids.push(pid);
            } else if group < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                return None;
            }
        }
        #[cfg(test)]
        if let Some(after) = AFTER_GROUP_ENUMERATION.with(|after| after.borrow_mut().take()) {
            after();
        }
        let mut members = Vec::new();
        for pid in pids {
            let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
            let end = stat.iter().rposition(|b| *b == b')')?;
            // After the name: state, parent, process group.
            let mut fields = stat[end + 1..].split(u8::is_ascii_whitespace).filter(|f| !f.is_empty());
            let state = fields.next()?;
            fields.next()?;
            let group = std::str::from_utf8(fields.next()?).ok()?.parse::<libc::pid_t>().ok()?;
            if group != pgid {
                return None;
            }
            members.push((pid, !matches!(state, b"Z" | b"X")));
        }
        Some(members)
    }

    pub fn listening_ports() -> HashSet<u16> {
        let mut out = HashSet::new();
        for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
            let Ok(text) = std::fs::read_to_string(f) else { continue };
            for line in text.lines().skip(1) {
                let cols: Vec<&str> = line.split_whitespace().collect();
                // local_address is "ADDR:PORT" in hex; state 0A is LISTEN.
                if cols.len() > 3
                    && cols[3] == "0A"
                    && let Some((_, port)) = cols[1].rsplit_once(':')
                    && let Ok(p) = u16::from_str_radix(port, 16)
                {
                    out.insert(p);
                }
            }
        }
        out
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    /// `struct proc_fileinfo` and `struct vnode_fdinfowithpath` of `<sys/proc_info.h>`, which
    /// libc does not have.
    #[repr(C)]
    struct ProcFileInfo {
        fi_openflags: u32,
        fi_status: u32,
        fi_offset: i64,
        fi_type: i32,
        fi_guardflags: u32,
    }
    #[repr(C)]
    struct VnodeFdInfoWithPath {
        pfi: ProcFileInfo,
        pvip: libc::vnode_info_path,
    }
    /// `PROC_PIDFDVNODEPATHINFO`.
    const PIDFD_VNODE_PATH: libc::c_int = 2;
    // The size the kernel checks the buffer against (`PROC_PIDFDVNODEPATHINFO_SIZE`): 24 bytes
    // of file info, then the 1176 of `vnode_info_path`.
    const _: () = assert!(std::mem::size_of::<VnodeFdInfoWithPath>() == 1200);

    /// The paths of the files process `pid` holds open (vnodes: files and directories).
    pub fn open_files(pid: u32) -> Vec<PathBuf> {
        let entry = std::mem::size_of::<libc::proc_fdinfo>();
        // SAFETY: a null buffer only asks for the size needed.
        let need = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if need <= 0 {
            return Vec::new();
        }
        // Room for the descriptors opened between the two calls.
        let mut fds: Vec<libc::proc_fdinfo> =
            (0..need as usize / entry + 16).map(|_| libc::proc_fdinfo { proc_fd: 0, proc_fdtype: 0 }).collect();
        let size = (fds.len() * entry) as libc::c_int;
        // SAFETY: the buffer is `size` bytes long.
        let n =
            unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), size) };
        fds.truncate(n.max(0) as usize / entry);
        fds.iter()
            .filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
            .filter_map(|f| {
                // SAFETY: plain C data (integers, arrays): all zero bytes are a valid value.
                let mut info: VnodeFdInfoWithPath = unsafe { std::mem::zeroed() };
                let size = std::mem::size_of::<VnodeFdInfoWithPath>() as libc::c_int;
                // SAFETY: proc_pidfdinfo writes at most `size` bytes into `info`.
                let n = unsafe {
                    libc::proc_pidfdinfo(
                        pid as libc::c_int,
                        f.proc_fd,
                        PIDFD_VNODE_PATH,
                        (&mut info as *mut VnodeFdInfoWithPath).cast::<libc::c_void>(),
                        size,
                    )
                };
                if n != size {
                    return None;
                }
                let bytes: Vec<u8> =
                    info.pvip.vip_path.iter().flatten().take_while(|c| **c != 0).map(|c| *c as u8).collect();
                (!bytes.is_empty()).then(|| PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()))
            })
            .collect()
    }
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    pub fn args_and_env(pid: u32) -> Option<(Vec<String>, Vec<String>)> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
        let mut size: libc::size_t = 0;
        // SAFETY: sysctl with a null buffer only reports the size needed.
        let rc = unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) };
        if rc != 0 || size < 4 {
            return None;
        }
        let mut buf = vec![0u8; size];
        // SAFETY: the buffer is `size` bytes long, as the kernel asked.
        let rc =
            unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) };
        if rc != 0 {
            return None;
        }
        buf.truncate(size);
        parse_procargs2(&buf)
    }

    pub fn cwd(pid: u32) -> Option<PathBuf> {
        // SAFETY: plain C data (integers, arrays): all zero bytes are a valid value.
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&mut info as *mut libc::proc_vnodepathinfo).cast::<libc::c_void>(),
                size,
            )
        };
        if n != size {
            return None;
        }
        // libc spells the MAXPATHLEN buffer as 32 rows of 32 characters.
        let bytes: Vec<u8> =
            info.pvi_cdir.vip_path.iter().flatten().take_while(|c| **c != 0).map(|c| *c as u8).collect();
        (!bytes.is_empty()).then(|| PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()))
    }

    pub fn socket_peer(fd: std::os::unix::io::RawFd) -> std::io::Result<(libc::pid_t, libc::uid_t)> {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: LOCAL_PEERPID writes at most `len` bytes into `pid`.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast::<libc::c_void>(),
                &mut len,
            )
        };
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: getpeereid only writes the two ids.
        if rc != 0 || unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((pid, uid))
    }

    pub fn running(pid: libc::pid_t) -> bool {
        // A zombie still exists for kill(), but no longer runs.
        exists(pid) && short_info(pid).is_some_and(|info| info.pbsi_status != libc::SZOMB)
    }

    /// The kernel's short description of a process, including zombies, when it gives one.
    fn short_info(pid: libc::pid_t) -> Option<libc::proc_bsdshortinfo> {
        // SAFETY: plain C data (integers, arrays): all zero bytes are a valid value.
        let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
        // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDT_SHORTBSDINFO,
                // Include zombies: group snapshots must distinguish them from unreadable members.
                1,
                (&mut info as *mut libc::proc_bsdshortinfo).cast::<libc::c_void>(),
                size,
            )
        };
        (n == size).then_some(info)
    }

    pub fn session_parents(sid: libc::pid_t) -> Vec<(libc::pid_t, u32)> {
        let mut pids = vec![0 as libc::pid_t; 8192];
        let size = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: the buffer is `size` bytes long; unused entries stay 0.
        if unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) } <= 0 {
            return Vec::new();
        }
        pids.into_iter()
            .filter(|p| *p > 0)
            // SAFETY: getsid only reads the process table.
            .filter(|p| unsafe { libc::getsid(*p) } == sid)
            .filter_map(|p| {
                let info = short_info(p)?;
                (info.pbsi_status != libc::SZOMB).then_some((p, info.pbsi_ppid))
            })
            .collect()
    }

    pub fn group_of(pid: libc::pid_t) -> Option<libc::pid_t> {
        short_info(pid).and_then(|info| libc::pid_t::try_from(info.pbsi_pgid).ok())
    }

    pub fn group_snapshot(pgid: libc::pid_t) -> Option<Vec<(libc::pid_t, bool)>> {
        let mut pids = vec![0 as libc::pid_t; 1024];
        let size = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: the buffer is `size` bytes long. A full buffer may omit members.
        let n = unsafe { libc::proc_listpgrppids(pgid, pids.as_mut_ptr().cast(), size) };
        if n <= 0 || n as usize >= pids.len() {
            return None;
        }
        pids.truncate(n as usize);
        pids.into_iter()
            .map(|pid| {
                let info = short_info(pid)?;
                (info.pbsi_pgid == pgid as u32).then_some((pid, info.pbsi_status != libc::SZOMB))
            })
            .collect()
    }

    /// The kernel's description of a process, when it gives one.
    fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
        // SAFETY: plain C data (integers, arrays): all zero bytes are a valid value.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast::<libc::c_void>(),
                size,
            )
        };
        (n == size).then_some(info)
    }

    pub fn start_time(pid: u32) -> Option<u64> {
        bsd_info(pid).map(|info| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }

    pub fn foreground_group(pid: u32) -> Option<u32> {
        bsd_info(pid).map(|info| info.e_tpgid)
    }

    /// The terminal a process runs on: its controlling terminal.
    pub fn terminal_of(pid: u32) -> Option<PathBuf> {
        let tdev = bsd_info(pid)?.e_tdev;
        // No terminal: NODEV.
        if tdev == u32::MAX {
            return None;
        }
        // SAFETY: devname gives a pointer to a NUL-terminated name it keeps, or null.
        let name = unsafe { libc::devname(tdev as libc::dev_t, libc::S_IFCHR) };
        if name.is_null() {
            return None;
        }
        // SAFETY: not null, NUL-terminated, read before any other call to devname.
        let name = unsafe { std::ffi::CStr::from_ptr(name) };
        Some(Path::new("/dev").join(name.to_str().ok()?))
    }

    pub fn machine_id() -> Option<String> {
        let mut id = [0u8; 16];
        let wait = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: gethostuuid writes the 16 bytes of the host's UUID.
        let rc = unsafe { libc::gethostuuid(id.as_mut_ptr(), &wait) };
        (rc == 0).then(|| crate::identity::hex(&id))
    }

    pub fn boot_id() -> String {
        String::new()
    }

    pub fn pids() -> Vec<u32> {
        // SAFETY: a null buffer only asks how many there are.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if n <= 0 {
            return Vec::new();
        }
        let mut pids = vec![0 as libc::pid_t; n as usize + 64];
        let size = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: the buffer is `size` bytes long.
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
        pids.truncate(n.max(0) as usize);
        pids.into_iter().filter(|p| *p > 0).map(|p| p as u32).collect()
    }

    pub fn ppid(pid: u32) -> Option<u32> {
        // SAFETY: plain C data (integers, arrays): all zero bytes are a valid value.
        let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
        // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDT_SHORTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdshortinfo).cast::<libc::c_void>(),
                size,
            )
        };
        (n == size).then_some(info.pbsi_ppid)
    }

    /// `lsof` is slow: it runs on its own thread, at most every two seconds, and callers get
    /// the last answer at once (empty until the first one).
    pub fn listening_ports() -> HashSet<u16> {
        use std::sync::atomic::{AtomicBool, Ordering};
        static CACHE: Mutex<Option<(Instant, HashSet<u16>)>> = Mutex::new(None);
        static ASKING: AtomicBool = AtomicBool::new(false);
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = cache.as_ref().is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(2));
        let ports = cache.as_ref().map(|(_, p)| p.clone()).unwrap_or_default();
        drop(cache);
        if !fresh && !ASKING.swap(true, Ordering::SeqCst) {
            std::thread::spawn(|| {
                let mut cmd = std::process::Command::new("lsof");
                cmd.args(["-nP", "-iTCP", "-sTCP:LISTEN", "-Fn"]);
                let out = crate::exec::run(&mut cmd, Duration::from_secs(5), 1 << 20)
                    .map(|o| o.stdout_text())
                    .unwrap_or_default();
                *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), parse_lsof(&out)));
                ASKING.store(false, Ordering::SeqCst);
            });
        }
        ports
    }
}

// kraal supports Linux and macOS only (docs/install.md): `imp`, here and in watch.rs, exists
// for these two. Elsewhere the build fails with this reason, not only with unresolved names.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("kraal builds for Linux and macOS only");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_completion_requires_complete_stable_dead_membership() {
        let dead = vec![(10, false), (20, false)];
        for snapshots in [
            vec![None],
            vec![Some(dead.clone()), None],
            vec![Some(vec![(10, true)])],
            vec![Some(dead.clone()), Some(vec![(10, false)])],
            vec![Some(dead.clone()), Some(vec![(10, false), (20, false), (30, false)])],
            vec![Some(dead.clone()), Some(vec![(10, false), (20, true)])],
        ] {
            let mut snapshots = snapshots.into_iter();
            assert!(group_running_in(|| snapshots.next().unwrap()));
        }
        let mut snapshots = [Some(dead), Some(vec![(20, false), (10, false)])].into_iter();
        assert!(!group_running_in(|| snapshots.next().unwrap()), "enumeration order does not matter");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unrelated_exits_do_not_invalidate_a_group_snapshot() {
        use std::cell::RefCell;
        use std::os::unix::process::CommandExt;
        use std::process::{Child, Command};
        use std::rc::Rc;

        fn end_during_scan(children: Rc<RefCell<Vec<Child>>>) {
            AFTER_GROUP_ENUMERATION.with(|after| {
                *after.borrow_mut() = Some(Box::new(move || {
                    let mut child = children.borrow_mut().pop().unwrap();
                    child.kill().unwrap();
                    child.wait().unwrap();
                    if !children.borrow().is_empty() {
                        end_during_scan(children);
                    }
                }));
            });
        }

        let mut leader = Command::new("true").process_group(0).spawn().unwrap();
        let start = std::time::Instant::now();
        while !crate::exec::has_exited(&leader) && start.elapsed() < crate::testing::PATIENCE {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(crate::exec::has_exited(&leader));
        // Each retry loses another unrelated process, so retries cannot hide the race.
        let unrelated =
            Rc::new(RefCell::new((0..3).map(|_| Command::new("sleep").arg("60").spawn().unwrap()).collect::<Vec<_>>()));
        end_during_scan(unrelated.clone());
        let snapshot = imp::group_snapshot(leader.id() as libc::pid_t);
        AFTER_GROUP_ENUMERATION.with(|after| after.borrow_mut().take());
        for mut child in unrelated.borrow_mut().drain(..) {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        leader.wait().unwrap();
        assert_eq!(snapshot, Some(vec![(leader.id() as libc::pid_t, false)]));
    }

    #[test]
    fn a_foreground_check_uses_the_parents_read_with_the_session() {
        let members = vec![(10, 1), (20, 10), (30, 20), (40, 30)];
        assert_eq!(sole_foreground_job_in(10, 20, members.clone()), Some(true));
        let mut unrelated = members.clone();
        unrelated.push((50, 10));
        assert_eq!(sole_foreground_job_in(10, 20, unrelated), Some(false));
        let mut missing_parent = members.clone();
        missing_parent.retain(|&(pid, _)| pid != 30);
        assert_eq!(sole_foreground_job_in(10, 20, missing_parent), None);
        let mut missing_foreground = members.clone();
        missing_foreground.retain(|&(pid, _)| pid != 20);
        assert_eq!(sole_foreground_job_in(10, 20, missing_foreground), None);
        let mut cycle = members;
        cycle.push((50, 60));
        cycle.push((60, 50));
        assert_eq!(sole_foreground_job_in(10, 20, cycle), None);
    }

    #[test]
    fn this_process() {
        let me = std::process::id();
        let argv = argv(me).expect("own command line");
        assert!(!argv.is_empty());
        assert_eq!(cwd(me).unwrap(), std::env::current_dir().unwrap().canonicalize().unwrap());
        // The environment a program started with (the tests have changed their own since). The
        // system shows it once the program is loaded, a moment after it is started.
        let mut child =
            std::process::Command::new("sleep").arg("60").env("KRAAL_SEEN", "at the start").spawn().unwrap();
        let start = std::time::Instant::now();
        let seen = loop {
            let seen = environ(child.id(), &["KRAAL_SEEN", "KRAAL_NOT_THERE"]);
            if !seen.is_empty() || start.elapsed() > crate::testing::PATIENCE {
                break seen;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(seen.get("KRAAL_SEEN").map(String::as_str), Some("at the start"));
        assert_eq!(seen.len(), 1);
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        // macOS answers from a cache that `lsof` fills in the background.
        let start = std::time::Instant::now();
        while !listening_ports().contains(&port) && start.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(listening_ports().contains(&port));
    }

    /// A process that ended runs no more, whether it waits to be reaped or is being reaped: the
    /// second is shown for a moment after the first, and is no process that came back.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_being_reaped_runs_no_more() {
        assert!(imp::runs(b"4242 (sleep) S 1 4242 4242 0 -1 4194304"));
        assert!(imp::runs(b"4242 (a) Z (b) R 1 4242 4242 0 -1 4194304"), "the state follows the whole name");
        assert!(!imp::runs(b"4242 (sleep) Z 1 4242 4242 0 -1 4194304"));
        assert!(!imp::runs(b"4242 (sleep) X 1 4242 4242 0 -1 4194304"));
        assert!(!imp::runs(b"4242 (S) X 1 4242 4242 0 -1 4194304"));
        assert!(!imp::runs(b""));
    }

    #[test]
    fn a_group_left_with_its_ended_leader_runs_nothing() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sh").args(["-c", "sleep 0.3"]).process_group(0).spawn().unwrap();
        let pgid = child.id() as libc::pid_t;
        assert!(group_running(pgid));
        // Ended, not reaped: the group still has its zombie leader, and nothing running.
        let start = std::time::Instant::now();
        while group_running(pgid) && start.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // SAFETY: signal 0 only checks the group still has a member. Linux counts the zombie
        // leader as one; macOS does not.
        if cfg!(target_os = "linux") {
            assert_eq!(unsafe { libc::kill(-pgid, 0) }, 0, "the zombie is still in its group");
        }
        assert!(!group_running(pgid));
        child.wait().unwrap();
        assert!(!group_running(pgid));
    }

    #[test]
    fn socket_peer_and_running() {
        let (a, b) = UnixStream::pair().unwrap();
        let (pid, uid) = socket_peer(&a).unwrap();
        assert_eq!(pid as u32, std::process::id());
        // SAFETY: getuid cannot fail.
        assert_eq!(uid, unsafe { libc::getuid() });
        drop(b);
        assert!(running(pid));
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let child_pid = child.id() as libc::pid_t;
        // Exited but not reaped yet: told apart from a running process.
        let start = std::time::Instant::now();
        while running(child_pid) && start.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!running(child_pid), "a zombie does not run");
        child.wait().unwrap();
        assert!(!running(child_pid));
    }

    #[test]
    fn what_works_in_a_directory_is_found() {
        let dir = std::env::temp_dir().join(format!("kraal-working-in-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("deep")).unwrap();
        // In its own process group, as a build an agent started would be.
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .current_dir(dir.join("deep"))
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let start = std::time::Instant::now();
        while !working_in(&dir).iter().any(|(p, _)| *p == pid) && start.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let found = working_in(&dir);
        assert!(found.iter().any(|(p, name)| *p == pid && name == "sleep"), "{found:?}");
        assert!(!working_in(&dir.join("other")).iter().any(|(p, _)| *p == pid));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!working_in(&dir).iter().any(|(p, _)| *p == pid));
        // Elsewhere, but writing there through an absolute path (Linux sees open files).
        if cfg!(target_os = "linux") {
            let log = dir.join("deep/build.log");
            let mut child = std::process::Command::new("sh")
                .args(["-c", &format!("exec 3>>'{}'; exec sleep 5", log.display())])
                .current_dir(std::env::temp_dir())
                .process_group(0)
                .spawn()
                .unwrap();
            let pid = child.id();
            let start = std::time::Instant::now();
            while !working_in(&dir).iter().any(|(p, _)| *p == pid)
                && start.elapsed() < std::time::Duration::from_secs(5)
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(working_in(&dir).iter().any(|(p, _)| *p == pid), "a file held open there counts");
            child.kill().unwrap();
            child.wait().unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lineage_goes_up_from_a_process_to_what_started_it() {
        let me = std::process::id();
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        let line = lineage(child.id());
        assert_eq!(line[..2], [child.id(), me]);
        assert_eq!(line[1..], lineage(me));
        assert!(!line.contains(&1), "{line:?}");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// The jobs of a session are those of its terminal, whatever its leader reads: a shell has
    /// its standard input elsewhere while it runs a command given `< file` (dash redirects
    /// itself), as this one has for good.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_jobs_of_a_terminal_are_found_whatever_its_shell_reads() {
        use portable_pty::{CommandBuilder, PtySize};
        let dir = std::env::temp_dir().join(format!("kraal-tty-jobs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("job");
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize { rows: 5, cols: 20, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new("sh");
        // Job control: the background job gets a process group of its own.
        cmd.args(["-c", &format!("set -m; sleep 4254 & echo $! > {}; exec </dev/null; wait", pidfile.display())]);
        let mut shell = pair.slave.spawn_command(cmd).unwrap();
        let leader = shell.process_id().unwrap();
        let reads_elsewhere =
            || std::fs::read_link(format!("/proc/{leader}/fd/0")).is_ok_and(|f| f == Path::new("/dev/null"));
        let start = std::time::Instant::now();
        while !(crate::testing::read_complete_lines(&pidfile).is_some() && reads_elsewhere()) {
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "the shell never got there");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let job: u32 = crate::testing::read_complete_lines(&pidfile).unwrap().trim().parse().unwrap();
        // The job may still be the shell's copy of itself for an instant.
        while argv(job).is_none_or(|argv| argv != ["sleep", "4254"]) {
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "the job never started");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(imp::terminal_of(leader), pair.master.tty_name());
        assert_eq!(background_processes(leader, &[]), [job]);
        assert_eq!(background_jobs(leader, &[]), [["sleep", "4254"]]);
        assert!(background_jobs(leader, &[job]).is_empty(), "a job made of whoever asks is none");
        // SAFETY: plain syscall on a process this test started.
        unsafe { libc::kill(job as libc::pid_t, libc::SIGKILL) };
        shell.kill().unwrap();
        shell.wait().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nul_separated() {
        assert_eq!(nul_strings(b"a\0bc\0\0"), vec!["a", "bc"]);
    }

    #[test]
    fn macos_formats() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/zsh\0\0\0\0zsh\0-l\0HOME=/Users/me\0TERM=xterm\0\0garbage");
        let (argv, env) = parse_procargs2(&buf).unwrap();
        assert_eq!(argv, vec!["zsh", "-l"]);
        assert_eq!(env, vec!["HOME=/Users/me", "TERM=xterm"]);
        let ports = parse_lsof("p123\nf5\nn*:5173\nn127.0.0.1:8000\nn[::1]:9000\n");
        assert_eq!(ports, [5173, 8000, 9000].into_iter().collect());
    }
    #[test]
    fn command_environment_distinguishes_an_unreadable_process() {
        let (command, env) = command_env(std::process::id(), &["PATH"]).unwrap();
        assert_eq!(Some(command), argv(std::process::id()));
        assert!(env.keys().all(|k| k == "PATH"));
        assert!(command_env(u32::MAX, &["HOME"]).is_none());
    }
}
