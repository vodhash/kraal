//! The attaching client and the one-shot control connection used by CLI commands.

mod attachments;
mod clipboard_image;
mod laptop;
mod paste;
mod writer;

#[cfg(test)]
pub(crate) use attachments::Controller as AttachmentController;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::protocol::{self, ClientMsg, Request, Response, ServerMsg};
use crate::theme::NoticeKind;
use crate::{paths, process};

/// How long a session may take to have a server that listens, from the moment one is asked for.
const START_LIMIT: Duration = Duration::from_secs(5);

/// How long a server just started is given to say that it listens, or to end because another
/// kraal has the session, when less than that is left of [`START_LIMIT`]: starting a kraal takes
/// a moment, a long one on a busy machine, and one started just before the limit would
/// otherwise be given up on before it could tell why there is no server.
const WORD_LIMIT: Duration = Duration::from_secs(2);

/// Make sure a server runs for `session`, starting one in the background if needed: where it
/// listens. When another kraal has the session (a server started at the same moment, which
/// listens in a moment; one that is going; `kraal forget`), its socket is waited for here, and a
/// server asked for again once nobody has the session, for [`START_LIMIT`] in all (the last
/// server started is heard out: [`WORD_LIMIT`]). A session still held then, with no server that
/// answers here, is not started: its files would be written twice. The error says who has it,
/// where its holder says so.
pub fn ensure_server(session: &str) -> Result<PathBuf> {
    start(session, None)
}

/// [`ensure_server`]. For a restart, each server started inherits `kept`, the descriptor of
/// what keeps the session from `kraal forget` until one of them has it.
fn start(session: &str, kept: Option<std::os::fd::RawFd>) -> Result<PathBuf> {
    paths::private_runtime_dir().context("checking the runtime directory")?;
    let socket = paths::socket_path(session);
    let listens = || paths::connect_now(&socket).is_ok();
    if listens() {
        return Ok(socket);
    }
    crate::persist::inspect(session)?;
    // Said to who starts the server: its own list is shown to who attaches, and none may.
    if let Some(refused) = paths::runtime_dir_refused() {
        eprintln!("kraal: warning: {refused}");
    }
    let log = paths::log_path(session);
    let deadline = Instant::now() + START_LIMIT;
    let mut pause = Duration::from_millis(20);
    // Whether a server started here ended because another kraal has the session.
    let mut held = false;
    loop {
        let logged = std::fs::metadata(&log).map_or(0, |m| m.len());
        let (mut child, ready) = start_server(session, &log, kept)?;
        let heard_by = deadline.max(Instant::now() + WORD_LIMIT);
        if wait_ready(&ready, heard_by.saturating_duration_since(Instant::now())) {
            return Ok(socket);
        }
        // It closed the pipe without a word: it failed, or another kraal has the session.
        let status = loop {
            if let Ok(Some(status)) = child.try_wait() {
                break Some(status);
            }
            if listens() {
                return Ok(socket);
            }
            if Instant::now() >= heard_by {
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        match status {
            Some(status) if status.code() == Some(crate::server::HELD_STATUS) => held = true,
            Some(status) => {
                let why = last_logged(&log, logged).map(|line| format!(": {line}")).unwrap_or_default();
                bail!("the server exited ({status}){why}; see {}", log.display());
            }
            // This one has not said, and those before it found the session held.
            None if held => bail!(held_without_a_server(session, &socket, &log)),
            None => bail!("the server did not start in time; see {}", log.display()),
        }
        // Its server is looked for until the next try, each a little later than the last: many
        // kraal started at once must not keep the one that has the session from listening.
        let next = (Instant::now() + pause).min(deadline);
        while Instant::now() < next {
            if listens() {
                return Ok(socket);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if Instant::now() >= deadline {
            bail!(held_without_a_server(session, &socket, &log));
        }
        pause = (pause * 2).min(Duration::from_millis(320));
    }
}

/// Who has `session`, where its holder says so: what it said of itself is shown, quoted, and
/// nothing else is done with it.
pub fn who_holds(session: &str) -> Option<String> {
    paths::holder(session).map(|holder| said_of(&holder, &paths::runtime_dir()))
}

/// What to say of `session` once another kraal has had it for as long as a start may take
/// without a server of it answering here. What its holder says of itself explains that, and
/// decided nothing: the start was tried again until now whatever it said, since what is read
/// there may be what a holder killed before left.
fn held_without_a_server(session: &str, socket: &Path, log: &Path) -> String {
    if !paths::nobody_listens(socket) {
        return format!("the server of session {session} takes no connection for now ({})", socket.display());
    }
    let here = paths::runtime_dir();
    match paths::holder(session) {
        Some(holder) if !is_here(&holder, &here) => {
            format!("session {session} is not started here: {}", said_of(&holder, &here))
        }
        holder => {
            let who = holder.map_or_else(|| "by another kraal".into(), |holder| said_of(&holder, &here));
            format!("session {session} is held ({who}) and no server of it listens: try again (see {})", log.display())
        }
    }
}

/// Whether `holder` keeps its socket in `here`, this kraal's runtime directory on this machine.
fn is_here(holder: &paths::Holder, here: &Path) -> bool {
    same_directory(&holder.runtime, here) && holder.host == crate::context::hostname().unwrap_or_default()
}

/// Whether two paths are one directory, however each is spelt (`/tmp` is `/private/tmp` on
/// macOS).
fn same_directory(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    if a == b {
        return true;
    }
    // Opened as directories, and without waiting: one of the two is what a holder wrote, which
    // may name anything (a FIFO would never open). Both stay open while they are compared: a
    // directory made once another is gone may be given its number.
    let open = |dir: &Path| {
        let flags = libc::O_DIRECTORY | libc::O_NONBLOCK;
        std::fs::OpenOptions::new().read(true).custom_flags(flags).open(dir).ok()
    };
    let (Some(a), Some(b)) = (open(a), open(b)) else { return false };
    matches!((a.metadata(), b.metadata()), (Ok(a), Ok(b)) if (a.dev(), a.ino()) == (b.dev(), b.ino()))
}

/// What the holder of a session is to a kraal whose runtime directory is `here`, and how to
/// reach it.
fn said_of(holder: &paths::Holder, here: &Path) -> String {
    let paths::Holder { pid, host, runtime } = holder;
    if *host != crate::context::hostname().unwrap_or_default() {
        return format!(
            "it runs on {host:?} (kraal {pid}), which saves in the same directory: use it there, or give this \
             one another name (kraal -s NAME)"
        );
    }
    if same_directory(runtime, here) {
        return format!("kraal {pid} has it");
    }
    let base = runtime.parent().unwrap_or(runtime);
    format!(
        "kraal {pid} runs it with its socket in {runtime:?}, not in {here:?} where this kraal looks: reach it \
         with XDG_RUNTIME_DIR={base:?}"
    )
}

/// The last line a server wrote in `log` after its first `from` bytes: why it did not start.
fn last_logged(log: &Path, from: u64) -> Option<String> {
    use std::io::{Seek, SeekFrom};
    let mut file = std::fs::File::open(log).ok()?;
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut said = String::new();
    file.take(64 << 10).read_to_string(&mut said).ok()?;
    let line = said.lines().rev().find(|line| !line.trim().is_empty())?;
    Some(line.trim().trim_start_matches("kraal: ").to_string())
}

/// Start a server for `session` in the background, its output in `log`: the process, and the
/// pipe it says on that it listens. It inherits `kept`, when a restart starts it.
fn start_server(
    session: &str,
    log: &Path,
    kept: Option<std::os::fd::RawFd>,
) -> Result<(std::process::Child, std::io::PipeReader)> {
    let exe = std::env::current_exe().context("locating the kraal binary")?;
    // The state directory holds what the server logs and saves: only ours.
    if let Some(dir) = log.parent() {
        paths::ensure_private_dir(dir)?;
    }
    let out = paths::open_private_append(log)?;
    // The server writes a byte to this pipe once it listens, instead of being asked again and
    // again whether it does.
    let (ready, ready_end) = std::io::pipe().context("creating a pipe")?;
    let ready_fd = std::os::fd::AsRawFd::as_raw_fd(&ready_end);
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["server", "--session", session])
        .env(READY_FD_VAR, ready_fd.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .current_dir(paths::home_dir().unwrap_or_else(|| "/".into()));
    if let Some(kept) = kept {
        cmd.env(paths::RESTART_FD_VAR, kept.to_string());
    }
    // What this kraal was started with is not the server's to keep: a pipe that whoever started
    // it reads to its end (a script, a build tool, a test) would not end before the session
    // does. Every descriptor open now, but the standard ones, the pipe the server is given and
    // what keeps the session while it is restarted, is closed in the server-to-be before it
    // runs. The one the listing was made through is closed already, and left out: another
    // could have its number by then.
    let left_out: Vec<libc::c_int> = crate::pane::open_descriptors()
        .into_iter()
        // SAFETY: fcntl only looks at the descriptor, and fails on one that is not open.
        .filter(|fd| {
            *fd > 2 && *fd != ready_fd && Some(*fd) != kept && unsafe { libc::fcntl(*fd, libc::F_GETFD) } != -1
        })
        .collect();
    // Detach from the terminal so closing it does not take the server down, and keep what the
    // server is given open across exec (in the child only): the end of the pipe it writes to,
    // and what keeps the session while it is restarted.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            libc::setsid();
            for fd in &left_out {
                libc::close(*fd);
            }
            for fd in [Some(ready_fd), kept].into_iter().flatten() {
                if libc::fcntl(fd, libc::F_SETFD, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let child = cmd.spawn().context("starting the kraal server")?;
    Ok((child, ready))
}

/// The environment variable that gives a new server the descriptor of the pipe it says on that
/// it listens (see [`crate::server::run`]).
pub const READY_FD_VAR: &str = "KRAAL_READY_FD";

/// Wait until the server writes its byte to `ready`: `true` then; `false` when it closed its end
/// without writing (it exited) or `limit` passed.
fn wait_ready(ready: &std::io::PipeReader, limit: Duration) -> bool {
    use std::os::fd::AsRawFd;
    let start = Instant::now();
    loop {
        let left = limit.saturating_sub(start.elapsed());
        if left.is_zero() {
            return false;
        }
        let mut pfd = libc::pollfd { fd: ready.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        match unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(i32::MAX as u128) as i32) } {
            0 => return false,
            n if n < 0 => {
                if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    return false;
                }
            }
            _ => {
                let mut byte = [0u8; 1];
                return matches!((&*ready).read(&mut byte), Ok(1));
            }
        }
    }
}

/// Socket for control commands: `$KRAAL_SOCKET` inside a pane unless a session is named.
pub fn control_socket(session: Option<&str>) -> PathBuf {
    match session {
        Some(s) => paths::socket_path(s),
        None => std::env::var_os("KRAAL_SOCKET").map(PathBuf::from).unwrap_or_else(|| paths::socket_path("main")),
    }
}

/// Send one request and wait for its answer. When the server runs a kraal that cannot do what
/// is asked, the error says so, and how to update it.
pub fn request(socket: &Path, req: Request) -> Result<Response> {
    if socket.starts_with(paths::runtime_dir()) {
        paths::private_runtime_dir().context("checking the runtime directory")?;
    }
    let no_server = || format!("no kraal server at {} (is it running?)", socket.display());
    let stream = UnixStream::connect(socket).with_context(no_server)?;
    request_connected(socket, stream, req)
}

/// This command's line, as a question about it quotes it.
pub(crate) fn command_line() -> String {
    let words: Vec<String> = std::iter::once("kraal".to_string()).chain(std::env::args().skip(1)).collect();
    crate::shell::join(&words)
}

/// Refuse before preparation that would otherwise modify local files.
pub(crate) fn require_protocol(socket: &Path) -> Result<()> {
    let server = ping(socket)?;
    anyhow::ensure!(server.protocol == protocol::PROTOCOL_VERSION, "{}", server.mismatch(None));
    Ok(())
}

/// Use the established connection so a failed mutation is never treated as a stopped session.
pub(crate) fn request_connected(socket: &Path, mut stream: UnixStream, req: Request) -> Result<Response> {
    match exchange(&mut stream, &ClientMsg::request(req))? {
        Ok(answer) => Ok(answer),
        Err(error) => {
            if (legacy_refusal_release(&error).is_some()
                || error.starts_with("bad message:")
                || error.starts_with(protocol::UNSUPPORTED))
                && let Ok(server) = ping(socket)
                && server.protocol != protocol::PROTOCOL_VERSION
            {
                bail!(server.mismatch(None));
            }
            bail!(error)
        }
    }
}

/// Upgrade inspection of a server, including protocol 2 servers.
pub fn upgrade_request_within(socket: &Path, req: Request, limit: Duration) -> Result<Response> {
    let deadline = Instant::now() + limit;
    let version = if matches!(req, Request::Ping) { protocol::upgrade_protocol() } else { ping(socket)?.protocol };
    let mut stream = UnixStream::connect(socket)?;
    exchange_until(&mut stream, &ClientMsg::Request { version, request: req, origin: None }, deadline)?
        .map_err(anyhow::Error::msg)
}

/// [`exchange`], given up on at `deadline`: the server sends one answer.
fn exchange_until(stream: &mut UnixStream, msg: &ClientMsg, deadline: Instant) -> Result<Result<Response, String>> {
    // A socket refuses a zero timeout.
    let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
    stream.set_read_timeout(Some(left))?;
    stream.set_write_timeout(Some(left))?;
    exchange(stream, msg).map_err(|e| {
        let timed_out = e
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut));
        if timed_out { anyhow::anyhow!("the server did not answer in time") } else { e }
    })
}

/// Send `msg` and wait for the server's answer, which is `Err` when it refused.
fn exchange(stream: &mut UnixStream, msg: &ClientMsg) -> Result<Result<Response, String>> {
    protocol::write_json(stream, msg)?;
    loop {
        match protocol::read_frame(stream)? {
            None => bail!("the server closed the connection"),
            Some((protocol::JSON, payload)) => match serde_json::from_slice(&payload)? {
                ServerMsg::Response { result } => return Ok(result),
                ServerMsg::Exit { reason, .. } => bail!("{reason}"),
                ServerMsg::Open { .. }
                | ServerMsg::Event { .. }
                | ServerMsg::Events { .. }
                | ServerMsg::Attached { .. }
                | ServerMsg::ClipboardFocus { .. }
                | ServerMsg::Clipboard { .. }
                | ServerMsg::AttachmentPrepared { .. }
                | ServerMsg::ClipboardImageOffer { .. }
                | ServerMsg::AttachmentOffer { .. }
                | ServerMsg::AttachmentAck { .. }
                | ServerMsg::AttachmentResult { .. }
                | ServerMsg::LaptopCopy { .. }
                | ServerMsg::LaptopList { .. }
                | ServerMsg::LaptopListAck { .. }
                | ServerMsg::Unknown(_) => continue,
            },
            Some(_) => continue,
        }
    }
}

/// What a server says it is, in answer to a ping.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub pid: u32,
    pub session: String,
    /// Its kraal release.
    pub version: String,
    pub protocol: u32,
}

impl ServerInfo {
    fn mismatch(&self, host: Option<&str>) -> String {
        protocol::version_mismatch(protocol::PROTOCOL_VERSION, self.protocol, &self.version, &self.session, host)
    }

    /// Older servers need the 0.4 disk conversion before this binary can restore them.
    pub fn check_restart(&self) -> Result<()> {
        anyhow::ensure!(
            self.protocol > protocol::upgrade_protocol(),
            "not restarting session {}: kraal {} saves state older than 0.4; run kraal 0.4 once first",
            self.session,
            self.version
        );
        Ok(())
    }
}

/// Whether a stopped server saved its layout.
#[derive(Debug, PartialEq)]
pub enum Saved {
    Yes,
    No(String),
    /// A server from another version, stopped by a signal, does not say.
    Unknown,
}

/// Stop the server behind `socket` and wait until it has exited ([`STOP_TIMEOUT`] at most). A
/// server whose upgrade control requests are unknown gets
/// SIGTERM, which every version handles the same way: save the layout, then exit.
pub fn kill_server(socket: &Path) -> Result<Saved> {
    stop_server(socket, false)
}

/// Restart the session's server on this kraal: the server saves the session and stops, telling
/// its attached clients to wait for the next one, which starts from this binary and restores
/// the session. Whether the layout was saved, as for `kraal kill`; a server of this protocol
/// that cannot save it first refuses, and keeps running.
pub fn restart_server(session: &str) -> Result<Saved> {
    // Until the next server has the session: `kraal forget` does nothing in between.
    let kept = keep_from_restart(session)?;
    if let Ok(server) = ping(&paths::socket_path(session)) {
        server.check_restart()?;
    }
    let saved = stop_server(&paths::socket_path(session), true)?;
    match start(session, Some(kept.descriptor())) {
        // A server listens: it has the session.
        Ok(_) => Ok(saved),
        Err(e) => {
            kept.leave_to_the_server();
            Err(e)
        }
    }
}

/// Keep `session` from being restarted by another kraal, for a restart of one's own or for
/// `kraal forget` ([`paths::Restarting`]).
pub fn keep_from_restart(session: &str) -> Result<paths::Restarting> {
    match paths::Restarting::keep(session).context("locking the session")? {
        Some(kept) => Ok(kept),
        None => bail!("another kraal is restarting session {session}: try again"),
    }
}

/// What the server behind `socket` is, given up on quickly when it does not answer.
pub fn ping(socket: &Path) -> Result<ServerInfo> {
    match upgrade_request_within(socket, Request::Ping, Duration::from_secs(2))? {
        Response::Pong { pid, session, version, protocol } => Ok(ServerInfo { pid, session, version, protocol }),
        _ => bail!("unexpected answer from the server"),
    }
}

/// The kraal binary installed where this one was started from. Once an update has replaced the
/// file, Linux names the running one `<path> (deleted)`: the file now at `<path>` is meant.
pub fn installed_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the kraal binary")?;
    if exe.exists() {
        return Ok(exe);
    }
    let name = exe.to_string_lossy();
    match name.strip_suffix(" (deleted)").map(PathBuf::from) {
        Some(path) if path.exists() => Ok(path),
        _ => bail!("{name} is not there any more"),
    }
}

/// Start `kraal -s <session> restart --yes` from `exe` on its own, out of any terminal, pane and
/// session: it outlives the pane or the server that starts it. What it says goes to the
/// session's log.
pub fn restart_detached(exe: &Path, session: &str) -> Result<std::process::Child> {
    let log = paths::log_path(session);
    if let Some(dir) = log.parent() {
        paths::ensure_private_dir(dir)?;
    }
    let out = paths::open_private_append(&log)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["-s", session, "restart", "--yes"])
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .current_dir(paths::home_dir().unwrap_or_else(|| "/".into()));
    for var in ["KRAAL_PANE", "KRAAL_SOCKET", "KRAAL_SESSION", "KRAAL_SESSION_NAME"] {
        cmd.env_remove(var);
    }
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().with_context(|| format!("starting {}", exe.display()))
}

/// How long stopping a server may take, from asking it to its exit: a restart waits up to 10 s
/// for the session to be saved, then the server as long for its state, and as long again for
/// its runs' logs, to reach the disk.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

fn stop_server(socket: &Path, restart: bool) -> Result<Saved> {
    let deadline = Instant::now() + STOP_TIMEOUT;
    // A restart must verify that this binary can restore the server's saved state before
    // asking it to stop, even when an earlier inspection timed out.
    if restart {
        ping(socket)?.check_restart()?;
    }
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("no kraal server at {} (is it running?)", socket.display()))?;
    let (pid, uid) = process::socket_peer(&stream).context("finding the server process")?;
    // SAFETY: getuid cannot fail.
    if uid != unsafe { libc::getuid() } {
        bail!("{} belongs to another user", socket.display());
    }
    let ask = ClientMsg::Request {
        version: protocol::upgrade_protocol(),
        request: Request::KillServer { restart },
        origin: None,
    };
    let answer = exchange_until(&mut stream, &ask, deadline).and_then(|a| a.map_err(anyhow::Error::msg));
    let saved = match answer {
        Ok(Response::Text(why)) => Saved::No(why),
        Ok(_) => Saved::Yes,
        // Never replace a restart refusal with a signal that would lose the session.
        Err(e) if restart => return Err(e),
        Err(_) => {
            // SAFETY: plain signal to the process listening on our own private socket.
            if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ESRCH) {
                    return Err(err).context(format!("stopping the server (pid {pid})"));
                }
            }
            Saved::Unknown
        }
    };
    drop(stream);
    while process::running(pid) {
        if Instant::now() > deadline {
            bail!("the server (pid {pid}) is still running after {} s", STOP_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(saved)
}

/// Stream events matching `filters` to `on_event` until the server goes away or it returns false.
/// `on_ready` runs once the server has started streaming.
pub fn subscribe(
    socket: &Path,
    filters: Vec<String>,
    since: Option<(u64, Option<u64>)>,
    on_ready: impl FnOnce(),
    mut on_event: impl FnMut(&serde_json::Value) -> bool,
) -> Result<()> {
    require_protocol(socket)?;
    let mut on_ready = Some(on_ready);
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("no kraal server at {} (is it running?)", socket.display()))?;
    let request = Request::Subscribe { events: filters, since: since.map(|s| s.0), boot: since.and_then(|s| s.1) };
    protocol::write_json(&mut stream, &ClientMsg::request(request))?;
    loop {
        let Some((kind, payload)) = protocol::read_frame(&mut stream)? else { return Ok(()) };
        if kind != protocol::JSON {
            continue;
        }
        match serde_json::from_slice::<ServerMsg>(&payload)? {
            ServerMsg::Response { result: Err(e) } => bail!(e),
            ServerMsg::Response { result: Ok(_) } => {
                if let Some(f) = on_ready.take() {
                    f();
                }
            }
            ServerMsg::Event { event } => {
                if !on_event(&event) {
                    return Ok(());
                }
            }
            ServerMsg::Events { events } => {
                for event in &events {
                    if !on_event(event) {
                        return Ok(());
                    }
                }
            }
            ServerMsg::Exit { reason, .. } => bail!("{reason}"),
            _ => {}
        }
    }
}

/// Like [`request`] but gives up quickly; used by hooks, which must never slow the agent down.
pub fn request_quick(socket: &Path, req: Request) -> Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    stream.set_read_timeout(Some(Duration::from_millis(1000)))?;
    protocol::write_json(&mut stream, &ClientMsg::request(req))?;
    let _ = protocol::read_frame(&mut stream);
    Ok(())
}

/// The alternate screen, with the cursor hidden.
const SCREEN_ON: &str = "\x1b[?1049h\x1b[?25l";
/// Back to the screen the client found, then the attributes and the cursor as it found them.
const SCREEN_OFF: &str = "\x1b[?1049l\x1b[0m\x1b[0 q\x1b[?25h";
/// The end of a synchronized update, should a frame have left one open.
const SYNC_OFF: &str = "\x1b[?2026l";
// `CSI > 5 u` asks the terminal for unambiguous keys (kitty keyboard protocol), so Shift+Enter,
// Ctrl+Enter or Esc arrive distinct, and for their alternates, so a key with Ctrl or Alt also
// says where it is on the keyboard; typed text still comes as text. Terminals without the
// protocol ignore it. The server raises it for the leader menu by setting this same entry of the
// terminal's stack, which the single pop when the client goes takes away.
const KEYS_ON: &str = "\x1b[?2004h\x1b[?1004h\x1b[>5u";
const KEYS_OFF: &str = "\x1b[<u\x1b[?1004l\x1b[?2004l";
pub(crate) const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
pub(crate) const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

/// A terminal as the client drives it.
trait Tty {
    /// Raw mode, or line mode back.
    fn set_raw(&mut self, on: bool) -> std::io::Result<()>;
    /// All of `bytes`, now.
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()>;
}

/// The terminal kraal was started in.
struct OwnTty;

impl Tty for OwnTty {
    fn set_raw(&mut self, on: bool) -> std::io::Result<()> {
        if on { crossterm::terminal::enable_raw_mode() } else { crossterm::terminal::disable_raw_mode() }
    }

    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let mut out = std::io::stdout().lock();
        out.write_all(bytes)?;
        out.flush()
    }
}

/// What the client has changed on its terminal. Whoever leaves first (the end of `attach`, a
/// signal, a panic) undoes exactly that, once and in order; nothing more is set up or written
/// after. Held while anything is written (see [`terminal`]): a frame goes out whole, and none
/// follows the bytes that give the terminal back.
struct Terminal<T: Tty> {
    tty: T,
    /// Raw mode (off while ssh has the terminal).
    raw: bool,
    /// The alternate screen, with the cursor hidden.
    screen: bool,
    /// The client's entry on the terminal's keyboard stack, bracketed paste and focus reports.
    keys: bool,
    /// The terminal was given back.
    left: bool,
}

impl<T: Tty> Terminal<T> {
    const fn new(tty: T) -> Terminal<T> {
        Terminal { tty, raw: false, screen: false, keys: false, left: false }
    }

    /// Raw mode on, or off for ssh to ask what it has to in the terminal.
    fn set_raw(&mut self, on: bool) -> std::io::Result<()> {
        if self.left || self.raw == on {
            return Ok(());
        }
        self.tty.set_raw(on)?;
        self.raw = on;
        Ok(())
    }

    /// Write `bytes` (a frame, a status line), unless the terminal was given back: whether they
    /// were written.
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<bool> {
        if self.left {
            return Ok(false);
        }
        self.tty.write(bytes)?;
        Ok(true)
    }

    /// Take the screen.
    fn enter(&mut self) -> std::io::Result<()> {
        self.screen |= self.write(SCREEN_ON.as_bytes())?;
        Ok(())
    }

    /// Turn the terminal's input modes on (kitty keys, bracketed paste, focus reports, the mouse
    /// when asked) while a server answers, and off while kraal waits for one. Asked again for
    /// what they are already, they are left alone: the terminal's keyboard stack is popped as
    /// often as it was pushed, and no server has touched the mouse in between.
    fn set_modes(&mut self, on: bool, mouse: bool) {
        if self.keys == on {
            return;
        }
        let mut bytes = if on { KEYS_ON } else { KEYS_OFF }.as_bytes().to_vec();
        // Off whatever the setting was when attaching: the server turns the mouse on or off when
        // its configuration is read again.
        if mouse || !on {
            bytes.extend_from_slice(if on { MOUSE_ON } else { MOUSE_OFF }.as_bytes());
        }
        if self.write(&bytes).unwrap_or(false) {
            self.keys = on;
        }
    }

    /// Give the terminal back as the client found it: line mode first (that cannot wait for the
    /// terminal to read), then the keyboard, the mouse, the screen and the cursor.
    fn leave(&mut self) {
        if std::mem::replace(&mut self.left, true) {
            return;
        }
        if std::mem::take(&mut self.raw) {
            let _ = self.tty.set_raw(false);
        }
        if !std::mem::take(&mut self.screen) {
            return;
        }
        let mut bytes = SYNC_OFF.as_bytes().to_vec();
        if std::mem::take(&mut self.keys) {
            bytes.extend_from_slice(KEYS_OFF.as_bytes());
        }
        // The server may have turned the mouse on since: off, whatever the client set.
        bytes.extend_from_slice(MOUSE_OFF.as_bytes());
        bytes.extend_from_slice(SCREEN_OFF.as_bytes());
        let _ = self.tty.write(&bytes);
    }
}

static TERMINAL: Mutex<Terminal<OwnTty>> = Mutex::new(Terminal::new(OwnTty));

/// The client's terminal, held for the time of one change or one write.
fn terminal() -> std::sync::MutexGuard<'static, Terminal<OwnTty>> {
    TERMINAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Gives the terminal back when `attach` ends, however it ends.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        terminal().leave();
    }
}

/// Whether `keys` hold ctrl+c, as a terminal sends it with or without the kitty protocol, and on
/// a non-Latin layout (the key at the place of `c`).
fn is_ctrl_c(keys: &[u8]) -> bool {
    let mut parser = crate::input::InputParser::new();
    let mut events = parser.feed(keys);
    events.extend(parser.flush());
    let c = crate::input::KeyCode::Char('c');
    events.iter().any(|e| {
        matches!(e, crate::input::InputEvent::Key(k)
            if k.mods.ctrl && !k.mods.alt && (k.code == c || k.by_place().is_some_and(|p| p.code == c)))
    })
}

/// The ssh process of a remote attachment, to stop with the client.
static SSH_PID: AtomicU32 = AtomicU32::new(0);

/// Where to attach: this machine's server, or a server running on another machine over SSH.
#[derive(Debug, Clone)]
pub enum Target {
    Local {
        session: String,
    },
    /// `ssh` arguments up to and including the destination.
    Remote {
        ssh: Vec<String>,
        session: String,
        label: String,
        /// The original `--host` argument, used in recovery commands.
        host: String,
        /// A pane of that session to show on the first connection (an id or a name).
        pane: Option<String>,
        clipboard_native: bool,
    },
}

/// The command line that runs `kraal ARGS` on a server: the `kraal` in its `PATH`, else the one
/// `kraal remote-install` puts in `~/.local/bin`. It goes through sh: the login shell on the
/// server may not be a POSIX one. `args` must be quoted already.
pub fn remote_kraal(args: &str) -> String {
    crate::shell::sh_c(&format!("k=$(command -v kraal || echo \"$HOME/.local/bin/kraal\"); exec \"$k\" {args}"))
}

struct Conn {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    child: Option<Arc<Mutex<std::process::Child>>>,
    /// The server's process, when it runs on this machine.
    server: Option<libc::pid_t>,
    /// The last thing ssh said on stderr (host key, authentication, remote errors).
    ssh_said: Arc<Mutex<String>>,
}

impl Conn {
    /// `what` went wrong, with ssh's last words if it said something: ssh is given a moment to
    /// end and say them (a refused key, an unknown host).
    fn why_lost(&mut self, what: &str) -> String {
        if let Some(child) = &self.child {
            let mut child = child.lock().unwrap();
            let start = Instant::now();
            while matches!(child.try_wait(), Ok(None)) && start.elapsed() < Duration::from_secs(1) {
                std::thread::sleep(Duration::from_millis(20));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let said = self.ssh_said.lock().map(|s| s.trim().to_string()).unwrap_or_default();
        if said.is_empty() { what.to_string() } else { format!("{what}: {said}") }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Some(c) = &self.child {
            let mut c = c.lock().unwrap();
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn connect(target: &Target) -> Result<Conn> {
    match target {
        Target::Local { session } => {
            let socket = ensure_server(session)?;
            let s = UnixStream::connect(&socket)?;
            let server = process::socket_peer(&s).ok().map(|(pid, _)| pid);
            Ok(Conn {
                reader: Box::new(s.try_clone()?),
                writer: Box::new(s),
                child: None,
                server,
                ssh_said: Default::default(),
            })
        }
        Target::Remote { ssh, session, .. } => {
            // The session name is validated (letters, digits, - _ .), so it needs no quoting.
            let remote = remote_kraal(&format!("proxy -s {session}"));
            let mut child = std::process::Command::new("ssh")
                .args(ssh)
                .arg(remote)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .context("starting ssh")?;
            SSH_PID.store(child.id(), Ordering::SeqCst);
            let reader = Box::new(child.stdout.take().context("ssh stdout")?);
            let stdin = child.stdin.take().context("ssh stdin")?;
            // Keep ssh's last error line to explain a lost connection (it would garble the
            // screen if it went to the terminal).
            let ssh_said = match child.stderr.take() {
                Some(stderr) => crate::exec::last_line(stderr),
                None => Default::default(),
            };
            let child = Arc::new(Mutex::new(child));
            let writer = Box::new(writer::TimedWriter::new(stdin, Duration::from_secs(2), Some(child.clone()))?);
            Ok(Conn { reader, writer, child: Some(child), server: None, ssh_said })
        }
    }
}

/// Whether to open a link the server sent. A remote server is less trusted than this machine:
/// only its web links open here, as a `file://` or custom scheme link could run a program.
fn opens(url: &str, remote: bool) -> bool {
    !remote || crate::pane::web_link(url)
}

/// Write a one-line status over the bottom of the screen (used while reconnecting).
fn status_line(kind: NoticeKind, msg: &str) {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let _ = terminal().write(&notice_lines(&[(msg.to_string(), Some(kind))], cols, rows));
}

fn notice_lines(lines: &[(String, Option<NoticeKind>)], cols: u16, rows: u16) -> Vec<u8> {
    // Leave the last column empty: writing there would scroll the screen.
    let width = usize::from(cols).saturating_sub(2);
    let count = lines.len().min(usize::from(rows));
    let mut out = b"\x1b7\x1b[?7l".to_vec();
    for (i, (line, kind)) in lines.iter().take(count).enumerate() {
        let row = usize::from(rows) - count + i + 1;
        out.extend_from_slice(format!("\x1b[{row};1H\x1b[0m").as_bytes());
        if let Some(kind) = kind {
            crate::ansi::foreground(&mut out, kind.colour(crate::theme::current()));
        }
        out.extend_from_slice(format!(" {}\x1b[K\x1b[0m", crate::text::truncate(line, width)).as_bytes());
    }
    out.extend_from_slice(b"\x1b[?7h\x1b8");
    out
}

/// A connection the server answered.
struct Greeted {
    conn: Conn,
    frames: std::io::BufReader<Box<dyn Read + Send>>,
    /// The server's first frame, still to be shown.
    first: (u8, Vec<u8>),
}

/// Connect, say hello, and wait for the server's first answer (returned, still to be shown).
/// Until then, over ssh, ssh has the terminal: it may ask for a password or about a host key,
/// and the terminal is in its usual mode, where ctrl+c ends kraal. `Err` says why it failed.
fn greet(
    target: &Target,
    workspace: &Option<String>,
    laptop: &Option<crate::laptop::Identity>,
) -> Result<Greeted, String> {
    let mut conn = connect(target).map_err(|e| format!("{e:#}"))?;
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    // The workspace asked for is asked again after a reconnection.
    let hello = ClientMsg::Attach {
        version: protocol::PROTOCOL_VERSION,
        cols,
        rows,
        workspace: workspace.clone(),
        remote: matches!(target, Target::Remote { .. }),
        laptop: laptop.clone(),
        pane: match target {
            Target::Remote { pane, .. } => pane.clone(),
            Target::Local { .. } => None,
        },
        // A local client is the kraal installed on the server's machine; a remote one may not be.
        installed: matches!(target, Target::Local { .. }).then(|| protocol::KRAAL_VERSION.to_string()),
    };
    if protocol::write_json(&mut conn.writer, &hello).is_err() {
        return Err(conn.why_lost("could not talk to the server"));
    }
    let mut frames =
        std::io::BufReader::with_capacity(256 * 1024, std::mem::replace(&mut conn.reader, Box::new(std::io::empty())));
    match protocol::read_frame(&mut frames) {
        Ok(Some(mut first)) => {
            if first.0 == protocol::JSON
                && let Ok(ServerMsg::Exit { reason, .. }) = serde_json::from_slice(&first.1)
                && let Some(reason) = attach_refusal(&reason, target)
            {
                first.1 = serde_json::to_vec(&ServerMsg::exit(reason)).expect("exit message");
            }
            Ok(Greeted { conn, frames, first })
        }
        Ok(None) => Err(conn.why_lost("the connection closed")),
        Err(e) => Err(conn.why_lost(&format!("connection error: {e}"))),
    }
}

// Released servers use different refusal messages. Normalize them so a remote attachment
// names a recovery command to run on this machine.
fn attach_refusal(reason: &str, target: &Target) -> Option<String> {
    let (server, release) = refusal_version(reason)?;
    let (session, host) = match target {
        Target::Local { session } => (session, None),
        Target::Remote { session, host, .. } => (session, Some(host.as_str())),
    };
    Some(protocol::version_mismatch(protocol::PROTOCOL_VERSION, server, release, session, host))
}

fn refusal_version(reason: &str) -> Option<(u32, &str)> {
    if let Some(release) = legacy_refusal_release(reason) {
        return Some((protocol::upgrade_protocol(), release));
    }
    let rest = reason.strip_prefix("protocol mismatch: client protocol ")?;
    let (_, rest) = rest.split_once(", server protocol ")?;
    let (server, rest) = rest.split_once(" (kraal ")?;
    let (release, _) = rest.split_once(", session ")?;
    Some((server.parse().ok()?, release))
}

fn legacy_refusal_release(reason: &str) -> Option<&str> {
    if reason.starts_with("protocol mismatch: server v2, client v") {
        return Some("0.2.0");
    }
    let (_, rest) = reason.split_once(" runs kraal ")?;
    let (release, rest) = rest.split_once(", ")?;
    rest.starts_with("too old for this kraal:").then_some(release)
}

/// Whether keyboard reading is paused (ssh is asking something in the terminal). Each pause has
/// a number, and the reader says which pause it has seen: `pause` waits for that very one, so a
/// reader still busy with an earlier read is never taken for a stopped one. A byte written to
/// `wake` after each change wakes the reader to look, so it never wakes up by itself.
struct KeysPause {
    paused: AtomicBool,
    epoch: AtomicU64,
    parked: AtomicU64,
    wake: (std::io::PipeReader, std::io::PipeWriter),
}

impl KeysPause {
    fn new() -> std::io::Result<KeysPause> {
        let (r, w) = std::io::pipe()?;
        for fd in [std::os::fd::AsRawFd::as_raw_fd(&r), std::os::fd::AsRawFd::as_raw_fd(&w)] {
            // Neither end ever blocks: the reader empties it, a full pipe already wakes it.
            // SAFETY: plain fcntl on a descriptor this owns.
            unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
        }
        Ok(KeysPause {
            paused: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            parked: AtomicU64::new(0),
            wake: (r, w),
        })
    }

    /// Stop reading the keyboard; `true` once the reader has stopped, so what is typed next (a
    /// password for ssh) is never read as keys for kraal. The reader only reads what is waiting
    /// and never blocks, so it stops as soon as it is woken; `false` means it did not, and the
    /// terminal must not be handed over.
    fn pause(&self) -> bool {
        // Paused before the number changes: a reader that sees the new number sees the pause.
        self.paused.store(true, Ordering::SeqCst);
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.wake();
        let start = Instant::now();
        while self.parked.load(Ordering::SeqCst) != epoch {
            if start.elapsed() > Duration::from_secs(5) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.wake();
    }

    fn wake(&self) {
        let _ = (&self.wake.1).write(b"w");
    }

    /// Forget the wake-ups seen: the flags they were for are read next.
    fn drain(&self) {
        let mut buf = [0u8; 64];
        while matches!((&self.wake.0).read(&mut buf), Ok(n) if n > 0) {}
    }
}

/// Read `fd` (the keyboard) into `forward` until it ends. Nothing is read while paused: the
/// reader parks before it would read, and it reads only the bytes already waiting, so a read
/// never blocks it past a pause. A true callback result requests one idle callback to resolve
/// a pending escape prefix; otherwise it sleeps until a key comes or the pause changes.
fn read_keys(fd: std::os::unix::io::RawFd, pause: &KeysPause, mut forward: impl FnMut(&[u8]) -> bool) {
    use std::os::fd::AsRawFd;
    let mut buf = [0u8; 16 * 1024];
    let wake = pause.wake.0.as_raw_fd();
    let mut timeout = -1;
    loop {
        if pause.paused.load(Ordering::SeqCst) {
            pause.parked.store(pause.epoch.load(Ordering::SeqCst), Ordering::SeqCst);
            // Until resumed, only the wake-up is listened to.
            let mut pfd = libc::pollfd { fd: wake, events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd.
            unsafe { libc::poll(&mut pfd, 1, -1) };
            pause.drain();
            continue;
        }
        let mut pfd = [
            libc::pollfd { fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake, events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: two valid pollfds.
        let ready = unsafe { libc::poll(pfd.as_mut_ptr(), 2, timeout) };
        if ready == 0 {
            timeout = if forward(&[]) { 50 } else { -1 };
        }
        if ready < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
        if pfd[1].revents != 0 {
            pause.drain();
        }
        if ready <= 0 || pfd[0].revents == 0 || pause.paused.load(Ordering::SeqCst) {
            continue;
        }
        let mut waiting: libc::c_int = 0;
        // SAFETY: FIONREAD writes the number of bytes waiting into `waiting`.
        if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut waiting) } != 0 {
            return;
        }
        if waiting <= 0 {
            // Ready with nothing to read: the end (a closed pipe, a hung-up terminal).
            if pfd[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return;
            }
            continue;
        }
        let want = (waiting as usize).min(buf.len());
        // SAFETY: reading into a buffer we own, at most its length.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), want) };
        match n {
            0 => return,
            n if n < 0 => {
                if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    return;
                }
            }
            n => timeout = if forward(&buf[..n as usize]) { 50 } else { -1 },
        }
    }
}

/// Attach the current terminal to a session. Remote sessions reconnect by themselves when the
/// connection drops; their programs keep running on the server meanwhile. `attached` runs once
/// the session first answered, before the terminal changes.
pub fn attach(mut target: Target, mut workspace: Option<String>, mouse: bool, attached: impl FnOnce()) -> Result<()> {
    if std::env::var_os("KRAAL_PANE").is_some() {
        bail!("already inside kraal (unset KRAAL_PANE to nest anyway)");
    }
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        bail!("kraal needs a terminal");
    }
    // A malformed configuration must not prevent reaching an existing session, nor
    // grant clipboard access whose policy could not be read.
    let clipboard_settings = crate::config::Config::load()
        .map(|cfg| {
            crate::theme::set(crate::theme::Theme::from_config(&cfg.theme).0);
            cfg.clipboard
        })
        .unwrap_or(crate::clipboard::Settings {
            mode: crate::clipboard::Mode::Off,
            max_bytes: crate::clipboard::MAX_BYTES,
        });
    let clipboard_policy = crate::clipboard::Policy {
        settings: clipboard_settings,
        allow_programs: matches!(&target, Target::Remote { clipboard_native: true, .. }),
        intermediate: crate::clipboard::intermediate(),
    };
    // Fail before touching the terminal when the first connection is impossible; ssh asks what
    // it has to ask in the terminal as it is.
    let identity = matches!(&target, Target::Remote { .. }).then(crate::laptop::Identity::new).transpose()?;
    let first = greet(&target, &workspace, &identity).map_err(anyhow::Error::msg)?;
    // So does a session that refuses at once (a kraal of another generation, a name that is no
    // workspace of its own): nothing was attached.
    if let (protocol::JSON, Ok(ServerMsg::Exit { reason, reattach: None })) =
        (first.first.0, serde_json::from_slice(&first.first.1))
    {
        bail!(reason);
    }
    // The pane asked for is shown once: a reconnection leaves the session where it is.
    if let Target::Remote { pane, .. } = &mut target {
        *pane = None;
    }
    attached();

    // The current connection's writer; None while reconnecting.
    let writer: Arc<Mutex<Option<Box<dyn Write + Send>>>> = Arc::new(Mutex::new(None));
    let laptop = laptop::Controller::new(writer.clone(), &target, identity.clone());
    let attachments = attachments::Controller::for_target(writer.clone(), &target, Some(laptop.clone()));
    attachments.set_clipboard_limit(clipboard_policy.settings.max_bytes);

    let clipboard_shutdown = Arc::new(crate::clipboard::Shutdown::default());

    // Being told to go (terminal closed, kill): leave the terminal usable, as a normal exit
    // does (destructors do not run on a signal). Set up before the terminal changes.
    {
        let clipboard_shutdown = clipboard_shutdown.clone();
        let attachments = attachments.clone();
        let laptop = laptop.clone();
        use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
        let mut signals = signal_hook::iterator::Signals::new([SIGTERM, SIGHUP, SIGINT, SIGQUIT])?;
        crate::threads::named("signals").spawn(move || {
            if let Some(sig) = signals.forever().next() {
                clipboard_shutdown.cancel();
                laptop.disconnect();
                attachments.shutdown();
                let pid = SSH_PID.load(Ordering::SeqCst);
                if pid != 0 {
                    // SAFETY: signalling the ssh child this client started.
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
                }
                // After the frame being written, if one is: none is cut short.
                terminal().leave();
                std::process::exit(128 + sig);
            }
        })?;
    }

    terminal().set_raw(true).context("switching the terminal to raw mode")?;
    let guard = TerminalGuard;
    terminal().enter()?;
    terminal().set_modes(true, mouse);

    let quit = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(KeysPause::new().context("creating a pipe")?);

    // Keyboard → server. While disconnected, ctrl+c gives up.
    {
        let writer = writer.clone();
        let quit = quit.clone();
        let attachments = attachments.clone();
        let pause = pause.clone();
        crate::threads::named("stdin").spawn(move || {
            read_keys(0, &pause, |keys| {
                if attachments.input(keys) {
                    return attachments.pending_keys();
                }
                let Ok(mut w) = writer.lock() else { return false };
                match w.as_mut() {
                    Some(w) => {
                        let _ = protocol::write_frame(w, protocol::BYTES, keys);
                    }
                    None if is_ctrl_c(keys) => quit.store(true, Ordering::SeqCst),
                    None => {}
                }
                false
            });
        })?;
    }
    // Window size changes → server.
    {
        let writer = writer.clone();
        let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])?;
        crate::threads::named("winch").spawn(move || {
            for _ in signals.forever() {
                if let Ok((cols, rows)) = crossterm::terminal::size()
                    && let Ok(mut w) = writer.lock()
                    && let Some(w) = w.as_mut()
                {
                    let _ = protocol::write_json(w, &ClientMsg::Resize { cols, rows });
                }
            }
        })?;
    }

    // In a pane of another kraal, attached to a server's session: that kraal is told which
    // session this is, and that session which of its panes this shows, so that the kraal outside
    // knows what is on screen here and can ask for another pane (OSC 7701).
    let nested = match &target {
        Target::Remote { label, session, .. } if std::env::var_os("KRAAL_SOCKET").is_some() => {
            Some(format!("\x1b]7701;attach;{label};{session}\x07"))
        }
        _ => None,
    };
    let mut greeted = Some(first);
    let mut delay = Duration::from_secs(1);
    // What the server said it runs, and the server that said it is restarting.
    let mut server_version = None;
    let mut restarting = None;
    let reason = 'session: loop {
        let session = match greeted.take() {
            Some(s) => Ok(s),
            None => {
                // ssh has the terminal until the server answers (a password, a host key): kraal
                // reads nothing, and ctrl+c (a signal again) ends it. The keyboard modes are off
                // already (see below).
                if pause.pause() {
                    let _ = terminal().set_raw(false);
                    let greeted = greet(&target, &workspace, &identity);
                    let _ = terminal().set_raw(true);
                    if greeted.is_ok() {
                        terminal().set_modes(true, mouse);
                    }
                    pause.resume();
                    greeted
                } else {
                    // ssh would compete with kraal for what is typed: not this time.
                    pause.resume();
                    Err("the keyboard could not be handed to ssh".to_string())
                }
            }
        };
        let lost = match session {
            Ok(Greeted { conn: mut c, mut frames, first }) => {
                let mut to_server = std::mem::replace(&mut c.writer, Box::new(std::io::sink()));
                if let Some(said) = &nested {
                    let _ = protocol::write_frame(&mut to_server, protocol::BYTES, b"\x1b]7701;report\x07");
                    let _ = terminal().write(said.as_bytes());
                }
                *writer.lock().unwrap() = Some(to_server);
                let clipboard_writer = writer.clone();
                let clipboard = crate::clipboard::Worker::start(
                    clipboard_policy.clone(),
                    &clipboard_shutdown,
                    |bytes| terminal().write(bytes).unwrap_or(false),
                    move |id, outcome, native_unavailable, cancelled| {
                        if let Ok(mut w) = clipboard_writer.lock()
                            && !cancelled.load(Ordering::SeqCst)
                            && let Some(w) = w.as_mut()
                        {
                            let _ = protocol::write_json(
                                w,
                                &ClientMsg::ClipboardResult { id, outcome, native_unavailable },
                            );
                        }
                    },
                )?;
                delay = Duration::from_secs(1);
                let mut next = Some(Ok(Some(first)));
                let lost = loop {
                    match next.take().unwrap_or_else(|| protocol::read_frame(&mut frames)) {
                        Ok(Some((protocol::BYTES, bytes))) => {
                            let written = attachments.output(&bytes);
                            match written {
                                Ok(true) => {}
                                Ok(false) => leaving(),
                                Err(_) => break 'session "output closed".to_string(),
                            }
                        }
                        Ok(Some((_, payload))) => match serde_json::from_slice::<ServerMsg>(&payload) {
                            Ok(ServerMsg::Exit { reason, reattach: None }) => break 'session reason,
                            // The next server shows the same workspace.
                            Ok(ServerMsg::Exit { reattach: Some(ws), .. }) => {
                                workspace = Some(ws).filter(|w| !w.is_empty());
                                restarting = Some(c.server);
                                break "the session restarts".to_string();
                            }
                            Ok(ServerMsg::Attached { version, .. }) => {
                                server_version = Some(version);
                                attachments.connected();
                                attachments.reconnect();
                            }
                            Ok(ServerMsg::LaptopCopy { token, message }) => laptop.copy_message(token, message),
                            Ok(ServerMsg::LaptopListAck { pane, side, request }) => {
                                laptop.acknowledge(pane, side, request)
                            }
                            Ok(ServerMsg::LaptopList { pane, side, request, path }) => {
                                laptop.request(pane, side, request, path)
                            }
                            Ok(ServerMsg::Clipboard { id, origin, text }) => {
                                clipboard.submit(id, origin, text);
                            }
                            Ok(msg @ ServerMsg::ClipboardFocus { .. }) => {
                                attachments.message(msg);
                            }
                            Ok(msg @ ServerMsg::ClipboardImageOffer { .. }) => {
                                attachments.message(msg);
                            }
                            Ok(
                                msg @ (ServerMsg::AttachmentPrepared { .. }
                                | ServerMsg::AttachmentOffer { .. }
                                | ServerMsg::AttachmentAck { .. }
                                | ServerMsg::AttachmentResult { .. }),
                            ) => {
                                attachments.message(msg);
                            }
                            Ok(ServerMsg::Open { url }) if opens(&url, matches!(target, Target::Remote { .. })) => {
                                let _ = crate::pane::open_url(&url);
                            }
                            _ => {}
                        },
                        Ok(None) => break c.why_lost("the connection closed"),
                        Err(e) => break c.why_lost(&format!("connection error: {e}")),
                    }
                };
                laptop.disconnect();
                clipboard.cancel();
                attachments.shutdown();
                *writer.lock().unwrap() = None;
                if nested.is_some() {
                    let _ = terminal().write(NOT_ATTACHED);
                }
                lost
            }
            Err(lost) => lost,
        };
        if let (Some(old), Target::Local { session }) = (restarting.take(), &target) {
            terminal().set_modes(false, mouse);
            match wait_restarted(session, old, &quit) {
                Ok(()) => continue 'session,
                Err(why) => break 'session why,
            }
        }
        let Target::Remote { label, .. } = &target else {
            break 'session format!("the server went away ({lost})");
        };
        // Until a server answers again, keys are plain (ctrl+c is ^C, and a signal while ssh
        // has the terminal), and neither the mouse nor focus changes type into a password.
        terminal().set_modes(false, mouse);
        // Remote: wait and try again, until it works or the user gives up.
        let until = Instant::now() + delay;
        while Instant::now() < until {
            if quit.load(Ordering::SeqCst) {
                break 'session format!("gave up reconnecting to {label}");
            }
            let left = until.saturating_duration_since(Instant::now()).as_secs() + 1;
            status_line(
                NoticeKind::Warning,
                &format!("kraal: connection to {label} lost ({lost}). Reconnecting in {left}s… ctrl+c to quit"),
            );
            std::thread::sleep(Duration::from_millis(200));
        }
        status_line(
            NoticeKind::Notice,
            &format!("kraal: reconnecting to {label}… (ssh may ask for a password; ctrl+c to quit)"),
        );
        delay = (delay * 2).min(Duration::from_secs(15));
    };
    laptop.disconnect();
    attachments.shutdown();
    if nested.is_some() {
        let _ = terminal().write(NOT_ATTACHED);
    }
    drop(guard);
    say(&format!("[kraal: {reason}]"));
    if let Target::Local { session } = &target {
        // A session left running on an older kraal is said once more, here.
        let socket = paths::socket_path(session);
        let running = server_version
            .filter(|_| UnixStream::connect(&socket).is_ok())
            .or_else(|| ping(&socket).ok().map(|s| s.version));
        if let Some(older) = running.filter(|v| protocol::newer(protocol::KRAAL_VERSION, v)) {
            say(&format!(
                "kraal {} is installed; session {session} runs {older}: `kraal -s {session} restart` restarts it",
                protocol::KRAAL_VERSION
            ));
        }
    }
    Ok(())
}

/// Say `line` in the terminal the client gave back, in one write: a signal that ends the client
/// meanwhile leaves the line whole or unwritten, never cut.
fn say(line: &str) {
    let _ = std::io::stderr().write_all(format!("{line}\n").as_bytes());
}

/// A signal gave the terminal back while a frame was on its way: its thread ends the process,
/// with the signal's status, and nothing is left to do here.
fn leaving() -> ! {
    loop {
        std::thread::park();
    }
}

/// Wait until the server that is restarting (`old`) has gone and the next one listens, saying
/// so on the bottom line; ctrl+c gives up.
fn wait_restarted(session: &str, old: Option<libc::pid_t>, quit: &AtomicBool) -> Result<(), String> {
    let start = Instant::now();
    loop {
        if quit.load(Ordering::SeqCst) {
            return Err("gave up waiting for the session to restart".into());
        }
        let gone = old.is_none_or(|pid| !process::running(pid));
        if gone && UnixStream::connect(paths::socket_path(session)).is_ok() {
            return Ok(());
        }
        if start.elapsed() > Duration::from_secs(60) {
            let log = paths::log_path(session);
            return Err(format!("the session did not come back after its restart; see {}", log.display()));
        }
        status_line(NoticeKind::Notice, "kraal: the session restarts… ctrl+c to quit");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// What a client attached to a server's session from a pane of another kraal tells that kraal
/// once it is not (the connection dropped, or it ends).
const NOT_ATTACHED: &[u8] = b"\x1b]7701;attach\x07";

/// `kraal proxy`: relay a client's frames between stdin/stdout and the local session socket.
/// Remote attach runs it on the server through ssh.
pub fn proxy(session: &str) -> Result<()> {
    let socket = ensure_server(session)?;
    let stream = UnixStream::connect(&socket)?;
    let mut to_server = stream.try_clone()?;
    let mut from_server = stream;
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut to_server);
        let _ = to_server.shutdown(std::net::Shutdown::Write);
    });
    let mut out = std::io::stdout().lock();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match from_server.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.write_all(&buf[..n]).and_then(|_| out.flush()).is_err() {
                    break;
                }
            }
        }
    }
    drop(up);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn released_attach_refusals_are_recognized_without_rewriting_other_errors() {
        assert_eq!(
            legacy_refusal_release(
                "protocol mismatch: server v2, client v3; run `kraal kill`, then `kraal` again (the layout is kept)"
            ),
            Some("0.2.0")
        );
        assert_eq!(
            legacy_refusal_release(
                "session work runs kraal 0.3.0, too old for this kraal: `kraal -s work restart` restarts it on this one (its layout is kept)"
            ),
            Some("0.3.0")
        );
        assert_eq!(legacy_refusal_release("no such workspace"), None);
        assert_eq!(refusal_version("no such workspace"), None);
        assert_eq!(
            refusal_version(
                "protocol mismatch: client protocol 6, server protocol 3 (kraal 0.4.0, session work); run `kraal -s work restart`"
            ),
            Some((3, "0.4.0")),
        );
        assert_eq!(
            legacy_refusal_release(
                "session work runs kraal 0.4.0, too new for this kraal: update this one (`kraal update`)"
            ),
            None
        );
    }

    #[test]
    fn protocol_three_remote_refusals_keep_the_session_and_name_remote_install() {
        for session in ["main", "work"] {
            let target = Target::Remote {
                ssh: vec!["box.internal".into()],
                session: session.into(),
                label: "box".into(),
                host: "box".into(),
                pane: None,
                clipboard_native: false,
            };
            let reason = format!(
                "protocol mismatch: client protocol 6, server protocol 3 (kraal 0.4.0, session {session}); run `kraal -s {session} restart`"
            );
            let rewritten = attach_refusal(&reason, &target).unwrap();
            let command =
                if session == "main" { "kraal remote-install box" } else { "kraal -s work remote-install box" };
            assert_eq!(
                rewritten,
                format!(
                    "protocol mismatch: client protocol 6, server protocol 3 (kraal 0.4.0, session {session}); run `{command}`"
                )
            );
            assert_eq!(attach_refusal("no such workspace", &target), None);
        }
    }

    #[test]
    fn connection_notices_use_neutral_text_or_blocked_and_restore_the_cursor() {
        for (kind, sgr) in [(NoticeKind::Notice, "37"), (NoticeKind::Warning, "91"), (NoticeKind::Error, "91")] {
            for (cols, rows) in [(80, 24), (124, 36)] {
                let bytes = String::from_utf8(notice_lines(&[("result".into(), Some(kind))], cols, rows)).unwrap();
                assert!(bytes.contains(&format!("\x1b[{rows};1H\x1b[0m\x1b[{sgr}m result\x1b[K\x1b[0m")));
                assert!(bytes.starts_with("\x1b7\x1b[?7l"));
                assert!(bytes.ends_with("\x1b[?7h\x1b8"));
            }
        }
    }

    /// A terminal that keeps what is done to it, in order: `raw`, `line`, or the bytes written.
    #[derive(Default)]
    struct Recorded(Vec<String>);

    impl Tty for Recorded {
        fn set_raw(&mut self, on: bool) -> std::io::Result<()> {
            self.0.push(if on { "raw" } else { "line" }.to_string());
            Ok(())
        }

        fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            self.0.push(String::from_utf8(bytes.to_vec()).unwrap());
            Ok(())
        }
    }

    impl Terminal<Recorded> {
        /// A terminal taken as `attach` takes it.
        fn taken() -> Terminal<Recorded> {
            let mut t = Terminal::new(Recorded::default());
            t.set_raw(true).unwrap();
            t.enter().unwrap();
            t.set_modes(true, true);
            t
        }

        /// How many times `sequence` was written.
        fn wrote(&self, sequence: &str) -> usize {
            self.tty.0.iter().map(|step| step.matches(sequence).count()).sum()
        }
    }

    #[test]
    fn the_terminal_is_given_back_once_and_in_order() {
        let mut t = Terminal::taken();
        assert_eq!(t.tty.0, ["raw", SCREEN_ON, &format!("{KEYS_ON}{MOUSE_ON}")]);
        assert!(t.write(b"frame").unwrap());
        t.tty.0.clear();
        t.leave();
        // Line mode first, then the keyboard, the mouse, the screen and the cursor.
        let bytes = "\x1b[?2026l\x1b[<u\x1b[?1004l\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1049l\x1b[0m\x1b[0 q\x1b[?25h";
        assert_eq!(t.tty.0, ["line", bytes]);
        // Whoever comes second (a signal during a normal exit) finds it done, and nothing more
        // is written or set up.
        t.leave();
        assert!(!t.write(b"a late frame").unwrap());
        t.set_modes(true, true);
        t.set_modes(false, true);
        t.set_raw(true).unwrap();
        t.enter().unwrap();
        assert_eq!(t.tty.0, ["line", bytes]);
    }

    #[test]
    fn the_keyboard_stack_is_popped_as_often_as_it_was_pushed() {
        // A restart wait that is given up: the modes went off for the wait.
        let mut t = Terminal::taken();
        t.set_modes(false, true);
        t.leave();
        assert_eq!((t.wrote("\x1b[>5u"), t.wrote("\x1b[<u"), t.wrote("\x1b[?1049l")), (1, 1, 1));
        // Every failed reconnection to a server turns them off again.
        let mut t = Terminal::taken();
        for _ in 0..3 {
            t.set_modes(false, true);
        }
        t.leave();
        assert_eq!((t.wrote("\x1b[>5u"), t.wrote("\x1b[<u")), (1, 1));
        // A session that came back, then a normal exit.
        let mut t = Terminal::taken();
        t.set_modes(false, true);
        t.set_modes(true, true);
        t.set_modes(true, true);
        t.leave();
        assert_eq!((t.wrote("\x1b[>5u"), t.wrote("\x1b[<u"), t.wrote("\x1b[?1049l")), (2, 2, 1));
        // Without the mouse asked for, it is still turned off on the way out: the server may
        // have turned it on.
        let mut t = Terminal::new(Recorded::default());
        t.enter().unwrap();
        t.set_modes(true, false);
        assert_eq!(t.wrote(MOUSE_ON), 0);
        t.leave();
        assert_eq!(t.wrote(MOUSE_OFF), 1);
    }

    #[test]
    fn a_terminal_left_while_ssh_has_it_stays_in_line_mode() {
        let mut t = Terminal::taken();
        t.set_modes(false, true);
        t.set_raw(false).unwrap();
        t.tty.0.clear();
        // A signal while ssh asks for a password: line mode is on already, and stays.
        t.leave();
        assert_eq!(t.tty.0.len(), 1, "{:?}", t.tty.0);
        assert_eq!(t.wrote("\x1b[<u"), 0);
        t.set_raw(true).unwrap();
        assert_eq!(t.tty.0.len(), 1, "raw mode never comes back: {:?}", t.tty.0);
        // Left before anything was taken: nothing to give back, and nothing is taken after.
        let mut t = Terminal::new(Recorded::default());
        t.leave();
        t.set_raw(true).unwrap();
        t.enter().unwrap();
        assert!(t.tty.0.is_empty(), "{:?}", t.tty.0);
    }

    #[test]
    fn ctrl_c_is_seen_with_or_without_the_kitty_protocol() {
        assert!(is_ctrl_c(b"\x03"));
        assert!(is_ctrl_c(b"\x1b[99;5u"));
        assert!(is_ctrl_c(b"ab\x1b[99;5:1u"));
        assert!(!is_ctrl_c(b"c"));
        assert!(!is_ctrl_c(b"\x1b[99;7u"), "ctrl+alt+c is something else");
        // A Russian layout: the key at the place of `c` types `с`.
        assert!(is_ctrl_c(b"\x1b[1089::99;5u"));
    }

    #[test]
    fn nothing_typed_after_a_pause_is_read() {
        use std::os::unix::io::AsRawFd;
        // A socket pair stands in for the keyboard (closed on exec, unlike a bare pipe: a child
        // another test starts would keep it open).
        let (keyboard_end, mut typing) = std::os::unix::net::UnixStream::pair().unwrap();
        let keyboard = keyboard_end.as_raw_fd();
        let mut write = |bytes: &[u8]| typing.write_all(bytes).unwrap();
        let pause = Arc::new(KeysPause::new().unwrap());
        let got = Arc::new(Mutex::new(Vec::new()));
        let reader = {
            let (pause, got) = (pause.clone(), got.clone());
            std::thread::spawn(move || {
                read_keys(keyboard, &pause, |k| {
                    got.lock().unwrap().extend_from_slice(k);
                    false
                })
            })
        };
        // Keys typed just before a pause are read or left for ssh; nothing typed after it is read.
        let mut left_for_ssh = 0;
        for round in 0..20 {
            write(b"k");
            assert!(pause.pause(), "round {round}: the reader did not stop");
            let before = got.lock().unwrap().len();
            write(b"secret");
            std::thread::sleep(Duration::from_millis(150));
            assert_eq!(got.lock().unwrap().len(), before, "round {round}: read while paused");
            let mut waiting = Vec::new();
            while !waiting.ends_with(b"secret") {
                let mut buf = [0u8; 64];
                // SAFETY: reading the pipe into a buffer we own.
                let n = unsafe { libc::read(keyboard, buf.as_mut_ptr().cast(), buf.len()) };
                assert!(n > 0);
                waiting.extend_from_slice(&buf[..n as usize]);
            }
            left_for_ssh += waiting.len() - b"secret".len();
            pause.resume();
        }
        write(b"after");
        let start = Instant::now();
        while !got.lock().unwrap().ends_with(b"after") {
            assert!(start.elapsed() < Duration::from_secs(5), "{:?}", got.lock().unwrap());
            std::thread::sleep(Duration::from_millis(5));
        }
        let got_keys = got.lock().unwrap().iter().filter(|b| **b == b'k').count();
        assert_eq!(got_keys + left_for_ssh, 20);
        assert!(!got.lock().unwrap().windows(6).any(|w| w == b"secret"));
        // Closing the typing end: the reader sees the end.
        drop(typing);
        reader.join().unwrap();
        drop(keyboard_end);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn the_keyboard_reader_sleeps_until_a_key_or_a_pause() {
        use std::os::unix::io::AsRawFd;
        let (keyboard_end, mut typing) = std::os::unix::net::UnixStream::pair().unwrap();
        let keyboard = keyboard_end.as_raw_fd();
        let pause = Arc::new(KeysPause::new().unwrap());
        let (got_tx, got_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (tid_tx, tid_rx) = crossbeam_channel::bounded(1);
        let reader = {
            let pause = pause.clone();
            std::thread::spawn(move || {
                crate::testing::block_signals();
                // SAFETY: gettid has no preconditions.
                let _ = tid_tx.send(unsafe { libc::gettid() });
                read_keys(keyboard, &pause, |k| {
                    let _ = got_tx.send(k.to_vec());
                    false
                })
            })
        };
        let tid = tid_rx.recv().unwrap();
        let switches = crate::testing::thread_switches(tid);
        // Once it sleeps (it was just started).
        crate::testing::wait_asleep(tid);
        let before = switches();
        crate::testing::sleep_with_signals(tid, Duration::from_millis(700));
        assert_eq!(switches(), before, "the reader woke up while nothing was typed");
        typing.write_all(b"a").unwrap();
        assert_eq!(got_rx.recv_timeout(Duration::from_secs(10)).unwrap(), b"a");
        // A pause is seen at once, without a key to wake the reader; so is the end of it. The time
        // the reader, or this thread asking it, waited for a processor is the machine's, not the
        // reader's.
        // SAFETY: gettid has no preconditions.
        let here = unsafe { libc::gettid() };
        let waited = || crate::testing::waited_for_a_processor(tid) + crate::testing::waited_for_a_processor(here);
        let (start, waited_before) = (Instant::now(), waited());
        assert!(pause.pause());
        let took = start.elapsed().saturating_sub(waited().saturating_sub(waited_before));
        assert!(took < Duration::from_millis(500), "{took:?} of {:?}", start.elapsed());
        // Once it went to sleep (it says it parked just before).
        crate::testing::wait_asleep(tid);
        let parked = switches();
        crate::testing::sleep_with_signals(tid, Duration::from_millis(300));
        assert_eq!(switches(), parked, "a paused reader sleeps too");
        pause.resume();
        typing.write_all(b"b").unwrap();
        assert_eq!(got_rx.recv_timeout(Duration::from_secs(10)).unwrap(), b"b");
        drop(typing);
        reader.join().unwrap();
        drop(keyboard_end);
    }

    #[test]
    fn old_servers_require_the_previous_release_before_restart() {
        let mut server = ServerInfo { pid: 1, session: "main".into(), version: "0.3.0".into(), protocol: 2 };
        assert!(server.check_restart().unwrap_err().to_string().ends_with("run kraal 0.4 once first"));
        for protocol in [3, protocol::PROTOCOL_VERSION] {
            server.protocol = protocol;
            assert!(server.check_restart().is_ok());
        }
    }

    #[test]
    fn delayed_old_servers_are_not_stopped_for_restart() {
        use std::os::unix::net::UnixListener;

        for version in ["0.2.0", "0.3.0"] {
            let session = format!("delayed-{version}");
            let socket = paths::socket_path(&session);
            paths::ensure_private_dir(socket.parent().unwrap()).unwrap();
            let listener = UnixListener::bind(&socket).unwrap();
            let server = std::thread::spawn(move || {
                let mut pings = 0;
                let mut stopped = false;
                while pings < 4 {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream.set_read_timeout(Some(crate::testing::PATIENCE)).unwrap();
                    let (_, payload) = protocol::read_frame(&mut stream).unwrap().unwrap();
                    let ClientMsg::Request { request, .. } = serde_json::from_slice(&payload).unwrap() else {
                        panic!("expected a request");
                    };
                    let result = match request {
                        Request::Ping => {
                            pings += 1;
                            if pings <= 2 {
                                // Both restart preflights time out before the old server recovers.
                                assert!(protocol::read_frame(&mut stream).unwrap().is_none());
                                continue;
                            }
                            Ok(Response::Pong {
                                pid: std::process::id(),
                                session: session.clone(),
                                version: version.into(),
                                protocol: 2,
                            })
                        }
                        Request::KillServer { restart: true } => {
                            stopped = true;
                            // Report the forbidden request without stopping the test process.
                            Err("restart stop request reached the old server".into())
                        }
                        _ => panic!("unexpected request: {request:?}"),
                    };
                    protocol::write_json(&mut stream, &ServerMsg::Response { result }).unwrap();
                }
                stopped
            });
            // The CLI's inspection, followed by restart_server's inspection and final stop check.
            assert!(ping(&socket).is_err());
            let result = restart_server(&format!("delayed-{version}"));
            let recovered = ping(&socket).unwrap();
            let stopped = server.join().unwrap();
            std::fs::remove_file(&socket).unwrap();
            assert_eq!(recovered.version, version);
            assert!(!stopped, "restart sent a stop request to kraal {version}");
            assert!(result.unwrap_err().to_string().ends_with("run kraal 0.4 once first"));
        }
    }

    #[test]
    fn a_new_server_is_waited_for_until_it_says_it_listens() {
        let (ready, mut end) = std::io::pipe().unwrap();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            end.write_all(b"1").unwrap();
        });
        let start = Instant::now();
        assert!(wait_ready(&ready, Duration::from_secs(5)));
        assert!(start.elapsed() < Duration::from_secs(2));
        writer.join().unwrap();
        // A server that goes without a word is not waited for.
        let (ready, end) = std::io::pipe().unwrap();
        drop(end);
        let start = Instant::now();
        assert!(!wait_ready(&ready, Duration::from_secs(5)));
        assert!(start.elapsed() < Duration::from_secs(1));
        // Nor one that says nothing in time.
        let (ready, _end) = std::io::pipe().unwrap();
        assert!(!wait_ready(&ready, Duration::from_millis(50)));
    }

    #[test]
    fn a_remote_server_opens_only_web_links() {
        for url in ["https://example.com", "HTTP://example.com/x"] {
            assert!(opens(url, true), "{url}");
        }
        for url in ["file:///x", "x-custom://y", "/Applications/X.app", " https://example.com", "http:"] {
            assert!(!opens(url, true), "{url}");
            assert!(opens(url, false), "local sessions keep {url}");
        }
    }
}
