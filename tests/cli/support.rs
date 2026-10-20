use crate::common::{Env, write_script};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// How long a test waits for what it expects to show before it fails. Far longer than it takes:
/// a machine busy with other work (a CI runner, the suite run beside other suites) is slower by
/// tens of seconds where programs are started and talk to each other, and a wait that ends when
/// its condition holds costs nothing when it does.
pub(super) const PATIENCE: Duration = Duration::from_secs(60);

impl Env {
    pub(super) fn workspace(&self, name: &str, kdl: &str) {
        std::fs::write(self.root.join("config/kraal/workspaces").join(format!("{name}.kdl")), kdl).unwrap();
    }

    pub(super) fn wait_for_text(&self, pane: &str, text: &str) -> String {
        self.wait_for(pane, text, |screen| screen)
    }

    /// Wait for `text` below the line marking what a restart restored, and return that part.
    pub(super) fn wait_for_text_since_restore(&self, pane: &str, text: &str) -> String {
        self.wait_for(pane, text, since_restore)
    }

    fn wait_for(&self, pane: &str, text: &str, part: impl Fn(&str) -> &str) -> String {
        let start = Instant::now();
        loop {
            let out = self.try_run(&["read", "-p", pane]);
            let screen = if out.status.success() {
                String::from_utf8(out.stdout).unwrap()
            } else {
                // A pane that a command has just asked for may not exist yet.
                let err = String::from_utf8_lossy(&out.stderr);
                assert!(err.contains("no pane named"), "kraal read -p {pane:?} failed: {err}");
                String::new()
            };
            let seen = part(&screen);
            // A narrow pane wraps long lines: the text may run over two rows.
            if seen.contains(text) || seen.lines().collect::<String>().contains(text) {
                return seen.to_string();
            }
            assert!(start.elapsed() < PATIENCE, "timed out waiting for {text:?} in:\n{screen}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The pid of this test's server: a `server --session <name>` process whose environment has
    /// this test's runtime directory.
    #[cfg(not(target_os = "linux"))]
    pub(super) fn server_pid(&self) -> Option<i32> {
        let want = format!("XDG_RUNTIME_DIR={}", self.root.join("run").display());
        let found = Command::new("pgrep").args(["-f", &format!("server --session {}$", self.session)]).output().ok()?;
        String::from_utf8_lossy(&found.stdout).lines().filter_map(|l| l.trim().parse::<i32>().ok()).find(|pid| {
            // `ps -E` shows the environment after the command.
            Command::new("ps")
                .args(["-E", "-ww", "-o", "command=", "-p", &pid.to_string()])
                .output()
                .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&want))
        })
    }

    /// The pid of this test's server, from /proc.
    #[cfg(target_os = "linux")]
    pub(super) fn server_pid(&self) -> Option<i32> {
        let want = format!("XDG_RUNTIME_DIR={}", self.root.join("run").display());
        std::fs::read_dir("/proc").ok()?.filter_map(|e| e.ok()).find_map(|e| {
            let pid: i32 = e.file_name().to_str()?.parse().ok()?;
            let cmdline = std::fs::read(e.path().join("cmdline")).ok()?;
            let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).filter(|a| !a.is_empty()).collect();
            let tail = [&b"server"[..], b"--session", self.session.as_bytes()];
            if args.len() < 3 || args[args.len() - 3..] != tail {
                return None;
            }
            let environ = std::fs::read(e.path().join("environ")).ok()?;
            environ.split(|b| *b == 0).any(|v| v == want.as_bytes()).then_some(pid)
        })
    }

    /// Start a long-running command; its output lines arrive on the receiver (read with a
    /// deadline, so a stalled stream fails the test instead of hanging it).
    pub(super) fn follow(&self, args: &[&str]) -> (std::process::Child, std::sync::mpsc::Receiver<String>) {
        use std::io::BufRead;
        let mut child = self.cmd(args).stdout(std::process::Stdio::piped()).spawn().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        (child, rx)
    }

    /// A client attached in a terminal (`script`, from util-linux, provides one), and what is
    /// typed into it.
    #[cfg(target_os = "linux")]
    pub(super) fn client(&self) -> std::process::Child {
        let attach = self.cmd(&["attach"]);
        let line = std::iter::once(attach.get_program())
            .chain(attach.get_args())
            .map(|a| format!("'{}'", a.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" ");
        let mut c = Command::new("script");
        // The terminal `script` makes has no size until it is given one.
        c.args(["-q", "-f", "-c", &format!("stty cols 100 rows 30; exec {line}"), "/dev/null"]);
        for (k, v) in attach.get_envs() {
            match v {
                Some(v) => c.env(k, v),
                None => c.env_remove(k),
            };
        }
        c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
        let mut child = c.spawn().expect("script (util-linux)");
        let drawn = client_output(child.stdout.take().unwrap());
        if let Err(error) = drawn.recv_timeout(PATIENCE) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the attached client never drew a frame: {error}");
        }
        child
    }

    pub(super) fn state_file(&self) -> PathBuf {
        self.root.join("state/kraal/sessions-v2").join(format!("{}.json", self.session))
    }

    pub(super) fn legacy_state_file(&self) -> PathBuf {
        self.root.join("state/kraal/sessions").join(format!("{}.json", self.session))
    }

    /// Start the session by opening workspace `name`, with a server that notes how it composes
    /// each frame (this build does, where `KRAAL_TEST_FRAMES` says): what reads the notes so far,
    /// a frame each, `<client> output` for one made from what one pane printed alone and
    /// `<client> whole` for any other.
    pub(super) fn open_noting_frames(&self, name: &str) -> impl Fn() -> Vec<String> {
        let notes = self.root.join("frames");
        let out = self.cmd(&["open", name]).env("KRAAL_TEST_FRAMES", &notes).output().unwrap();
        assert!(out.status.success(), "kraal open {name}: {}", String::from_utf8_lossy(&out.stderr));
        move || std::fs::read_to_string(&notes).unwrap_or_default().lines().map(String::from).collect()
    }
}

#[cfg(target_os = "linux")]
fn client_output(mut output: impl std::io::Read + Send + 'static) -> crossbeam_channel::Receiver<()> {
    let (drawn, receive) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let mut drawn = Some(drawn);
        let mut pending = Vec::new();
        let mut started = false;
        let mut buf = [0u8; 8192];
        while let Ok(n @ 1..) = output.read(&mut buf) {
            // A complete synchronized update proves attachment; terminal setup alone does not.
            // Keep draining after it, or the client's terminal could fill and block later input.
            if drawn.is_some() {
                pending.extend_from_slice(&buf[..n]);
                for sequence in pending.windows(8) {
                    if sequence == b"\x1b[?2026h" {
                        started = true;
                    } else if started && sequence == b"\x1b[?2026l" {
                        let _ = drawn.take().unwrap().send(());
                        break;
                    }
                }
                pending.drain(..pending.len().saturating_sub(7));
            }
        }
    });
    receive
}

#[cfg(target_os = "linux")]
#[test]
fn client_readiness_requires_a_complete_frame_and_keeps_draining() {
    struct Output {
        chunks: crossbeam_channel::Receiver<Vec<u8>>,
        reading: crossbeam_channel::Sender<()>,
    }
    impl std::io::Read for Output {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reading.send(()).unwrap();
            let Ok(chunk) = self.chunks.recv_timeout(PATIENCE) else { return Ok(0) };
            buf[..chunk.len()].copy_from_slice(&chunk);
            Ok(chunk.len())
        }
    }
    let (send, chunks) = crossbeam_channel::unbounded();
    let (reading, read) = crossbeam_channel::unbounded();
    let drawn = client_output(Output { chunks, reading });
    read.recv_timeout(PATIENCE).unwrap();
    send.send(b"terminal setup\x1b[?2026l\x1b[?20".to_vec()).unwrap();
    read.recv_timeout(PATIENCE).unwrap();
    assert!(drawn.is_empty(), "a partial frame is not attachment");
    send.send(b"26hframe contents\x1b[?20".to_vec()).unwrap();
    read.recv_timeout(PATIENCE).unwrap();
    assert!(drawn.is_empty(), "the frame has not ended");
    send.send(b"26l".to_vec()).unwrap();
    read.recv_timeout(PATIENCE).unwrap();
    drawn.recv_timeout(PATIENCE).unwrap();
    send.send(b"another frame".to_vec()).unwrap();
    read.recv_timeout(PATIENCE).unwrap();
    assert!(matches!(drawn.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
}

#[cfg(target_os = "linux")]
#[test]
fn client_output_ending_before_a_frame_does_not_report_attachment() {
    for bytes in [b"".as_slice(), b"setup\x1b[?2026l", b"\x1b[?2026hpartial frame"] {
        let drawn = client_output(std::io::Cursor::new(bytes));
        assert_eq!(drawn.recv_timeout(PATIENCE), Err(crossbeam_channel::RecvTimeoutError::Disconnected));
    }
}

/// A client attached through the session's socket, as kraal's own client is: it sends what a
/// terminal would, and keeps what the server draws.
pub(super) struct RawClient {
    pub(super) stream: std::os::unix::net::UnixStream,
    pub(super) screen: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    /// The messages the server sent, as JSON.
    pub(super) messages: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Its terminal's columns and rows.
    size: (usize, usize),
    trace: Option<std::cell::RefCell<Vec<ClientStep>>>,
}

struct ClientStep {
    at: Instant,
    drawn: usize,
    size: (usize, usize),
    action: ClientAction,
}

enum ClientAction {
    Input(Vec<u8>),
    Observed(String),
}

impl RawClient {
    /// Keep input and screen offsets cheaply; replay them only if the test fails.
    pub(super) fn traced(mut self) -> Self {
        self.trace = Some(Default::default());
        self.trace_step(ClientAction::Observed("attached".into()));
        self
    }

    fn trace_step(&self, action: ClientAction) {
        let drawn = self.screen.lock().unwrap().len();
        self.trace_step_at(drawn, action);
    }

    fn trace_step_at(&self, drawn: usize, action: ClientAction) {
        if let Some(trace) = &self.trace {
            trace.borrow_mut().push(ClientStep { at: Instant::now(), drawn, size: self.size, action });
        }
    }

    fn print_trace(&self) {
        let Some(trace) = &self.trace else { return };
        let trace = trace.borrow();
        let bytes = self.screen.lock().unwrap().clone();
        let show = |drawn, size: (usize, usize)| {
            let rows = text_after(&bytes[..drawn], size.0, size.1);
            for (y, row) in rows.iter().enumerate() {
                eprintln!("{:2} {row}", y + 1);
            }
            rows
        };
        eprintln!("Client trace (mouse coordinates and screen rows are 1-based):");
        for (i, step) in trace.iter().enumerate() {
            eprintln!(
                "Step {i}, +{:?}, {}x{}, {} bytes drawn",
                step.at - trace[0].at,
                step.size.0,
                step.size.1,
                step.drawn
            );
            match &step.action {
                ClientAction::Observed(what) => {
                    eprintln!("Observed: {what}");
                    show(step.drawn, step.size);
                }
                ClientAction::Input(input) => {
                    eprintln!("Before input: {}", input.escape_ascii());
                    let rows = show(step.drawn, step.size);
                    // SGR mouse reports, including both the press and release of a click.
                    for report in input.split(|b| *b == 0x1b).filter_map(|s| s.strip_prefix(b"[<")) {
                        if let Some(end) = report.iter().position(|b| *b == b'M' || *b == b'm') {
                            let fields: Vec<usize> = String::from_utf8_lossy(&report[..end])
                                .split(';')
                                .filter_map(|s| s.parse().ok())
                                .collect();
                            if let [button, col, row] = fields.as_slice() {
                                let text = row.checked_sub(1).and_then(|y| rows.get(y));
                                eprintln!(
                                    "Mouse {} button={button} column={col} row={row}, row text: {text:?}",
                                    if report[end] == b'M' { "press" } else { "release" }
                                );
                            }
                        }
                    }
                    // The next observation/input marks how far the test let this input run.
                    let next = trace.get(i + 1);
                    eprintln!("After input (at next step, or failure):");
                    show(next.map_or(bytes.len(), |s| s.drawn), next.map_or(self.size, |s| s.size));
                }
            }
        }
        eprintln!("Screen at failure:");
        show(bytes.len(), self.size);
    }

    pub(super) fn attach(env: &Env, workspace: &str) -> RawClient {
        RawClient::attach_installed(env, workspace, None)
    }

    /// Attach, saying which kraal is installed (a local client says its own).
    pub(super) fn attach_installed(env: &Env, workspace: &str, installed: Option<&str>) -> RawClient {
        RawClient::attach_as(env, Some(workspace), (120, 40), installed)
    }

    /// Attach from a terminal of `size` (columns, rows), asking for a workspace or, as `kraal`
    /// alone does, for none.
    pub(super) fn attach_sized(env: &Env, workspace: Option<&str>, size: (usize, usize)) -> RawClient {
        RawClient::attach_as(env, workspace, size, None)
    }

    fn attach_as(env: &Env, workspace: Option<&str>, size: (usize, usize), installed: Option<&str>) -> RawClient {
        let socket = env.root.join("run/kraal").join(format!("{}.sock", env.session));
        RawClient::attach_at(&socket, workspace, size, installed)
    }

    /// Attach to the server that listens at `socket`.
    pub(super) fn attach_at(
        socket: &Path,
        workspace: Option<&str>,
        size: (usize, usize),
        installed: Option<&str>,
    ) -> RawClient {
        use std::io::Read;
        let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
        let installed = installed.map(|v| format!(r#","installed":"{v}""#)).unwrap_or_default();
        let (cols, rows) = size;
        let workspace = serde_json::json!(workspace);
        let hello = format!(
            r#"{{"type":"attach","version":6,"cols":{cols},"rows":{rows},"workspace":{workspace},"remote":false{installed}}}"#
        );
        raw_frame(&mut stream, 0, hello.as_bytes());
        let screen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut reader, kept, said) = (stream.try_clone().unwrap(), screen.clone(), messages.clone());
        std::thread::spawn(move || {
            let mut header = [0u8; 5];
            while reader.read_exact(&mut header).is_ok() {
                let mut payload = vec![0u8; u32::from_le_bytes(header[..4].try_into().unwrap()) as usize];
                if reader.read_exact(&mut payload).is_err() {
                    break;
                }
                match header[4] {
                    1 => kept.lock().unwrap().extend_from_slice(&payload),
                    _ => said.lock().unwrap().push(String::from_utf8_lossy(&payload).into_owned()),
                }
            }
        });
        RawClient { stream, screen, messages, size, trace: None }
    }

    /// Its terminal takes another size: the server draws the whole screen again.
    pub(super) fn resize(&mut self, size: (usize, usize)) {
        self.size = size;
        self.trace_step(ClientAction::Observed(format!("resize to {}x{}", size.0, size.1)));
        let (cols, rows) = size;
        raw_frame(&mut self.stream, 0, format!(r#"{{"type":"resize","cols":{cols},"rows":{rows}}}"#).as_bytes());
    }

    /// Wait until the server has sent a message containing `text`.
    pub(super) fn wait_for_message(&self, text: &str) {
        let start = Instant::now();
        while !self.messages.lock().unwrap().iter().any(|m| m.contains(text)) {
            assert!(start.elapsed() < PATIENCE, "{text:?} never sent: {:?}", self.messages.lock().unwrap());
            std::thread::sleep(Duration::from_millis(50));
        }
        self.trace_step(ClientAction::Observed(format!("message {text:?}")));
    }

    /// Bytes as the terminal sends them, one read each.
    pub(super) fn send(&mut self, bytes: &[u8]) {
        if self.trace.is_some() {
            self.trace_step(ClientAction::Input(bytes.to_vec()));
        }
        raw_frame(&mut self.stream, 1, bytes);
    }

    /// A bracketed paste, sent as a terminal does: in reads of 64 KiB.
    pub(super) fn paste(&mut self, data: &[u8]) {
        let all = [&b"\x1b[200~"[..], data, b"\x1b[201~"].concat();
        for chunk in all.chunks(64 << 10) {
            self.send(chunk);
        }
    }

    /// Wait until the server has drawn `text` on the client's screen.
    pub(super) fn wait_for_screen(&self, text: &str) {
        let start = Instant::now();
        while !String::from_utf8_lossy(&self.screen.lock().unwrap()).contains(text) {
            assert!(start.elapsed() < PATIENCE, "{text:?} never shown");
            std::thread::sleep(Duration::from_millis(50));
        }
        self.trace_step(ClientAction::Observed(format!("screen contains {text:?}")));
    }
}

impl Drop for RawClient {
    fn drop(&mut self) {
        // The thread that keeps what the server sends holds the connection too: closed for both,
        // so that the server sees this client go when the test lets go of it.
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if std::thread::panicking() {
            self.print_trace();
        }
    }
}

pub(super) fn raw_frame(w: &mut impl std::io::Write, kind: u8, payload: &[u8]) {
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.push(kind);
    frame.extend_from_slice(payload);
    w.write_all(&frame).unwrap();
}

/// A client attached in a terminal the test holds (a pty): what the client writes to it is kept
/// byte for byte, and the terminal's line mode can be looked at.
#[cfg(unix)]
pub(super) struct TerminalClient {
    master: Box<dyn portable_pty::MasterPty + Send>,
    keys: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    pub(super) written: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    reader: std::thread::JoinHandle<()>,
}

#[cfg(unix)]
impl TerminalClient {
    /// Attach to `env`'s session, and wait until it is drawn.
    #[cfg(target_os = "linux")]
    pub(super) fn attach(env: &Env) -> TerminalClient {
        Self::start(env.cmd(&["attach"]))
    }

    pub(super) fn start(attach: Command) -> TerminalClient {
        let size = portable_pty::PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 };
        let pair = portable_pty::native_pty_system().openpty(size).unwrap();
        let mut cmd = portable_pty::CommandBuilder::new(attach.get_program());
        cmd.args(attach.get_args());
        for (k, v) in attach.get_envs() {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            }
        }
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let written = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut from_client, kept) = (pair.master.try_clone_reader().unwrap(), written.clone());
        // Until the client has gone: the terminal then has no other end.
        let reader = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 64 * 1024];
            while let Ok(n @ 1..) = from_client.read(&mut buf) {
                kept.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        let client =
            TerminalClient { keys: pair.master.take_writer().unwrap(), master: pair.master, child, written, reader };
        client.wait_for_frames(1);
        client
    }

    /// Wait until the client has written `frames` frames to its terminal (each ends a
    /// synchronized update).
    pub(super) fn wait_for_frames(&self, frames: usize) {
        let start = Instant::now();
        loop {
            let written = self.written.lock().unwrap().clone();
            if written.windows(8).filter(|w| *w == b"\x1b[?2026l").count() >= frames {
                return;
            }
            let shown = String::from_utf8_lossy(&written).replace('\x1b', "^[");
            assert!(start.elapsed() < PATIENCE, "{frames} frame(s) never written: {shown}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait until the client has written `text` to its terminal.
    pub(super) fn wait_for(&self, text: &str) {
        let start = Instant::now();
        loop {
            let written = String::from_utf8_lossy(&self.written.lock().unwrap()).into_owned();
            if written.contains(text) {
                return;
            }
            assert!(start.elapsed() < PATIENCE, "{text:?} never written: {written:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn type_keys(&mut self, keys: &[u8]) {
        self.keys.write_all(keys).unwrap();
        self.keys.flush().unwrap();
    }

    #[cfg(target_os = "linux")]
    pub(super) fn signal(&self, signal: libc::c_int) {
        // SAFETY: plain syscall to the client this test started (ended already, it is still ours
        // to wait for).
        unsafe { libc::kill(self.child.process_id().unwrap() as libc::pid_t, signal) };
    }

    /// Whether the terminal is in line mode, as a shell has it.
    pub(super) fn line_mode(&self) -> bool {
        // SAFETY: tcgetattr fills the zeroed struct, for a descriptor the test holds.
        let mode = unsafe {
            let mut mode: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(self.master.as_raw_fd().unwrap(), &mut mode), 0);
            mode
        };
        mode.c_lflag & (libc::ICANON | libc::ECHO) == libc::ICANON | libc::ECHO
    }

    /// Wait for the client to end, and check that it gave its terminal back, `what` saying how
    /// it left: its exit status, and what it said once the terminal was the shell's again.
    ///
    /// Given back means: line mode; the keyboard stack popped as often as the client pushed it;
    /// the alternate screen left once; in order (the keyboard, the mouse, the screen, the
    /// cursor); and nothing but plain text after that.
    pub(super) fn left(mut self, what: &str) -> (u32, String) {
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(start.elapsed() < Duration::from_secs(15), "{what}: the client is still there");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(self.line_mode(), "{what}: the terminal is not in line mode");
        // The terminal has no other end once the client has gone: all it wrote is read. Only
        // then may the test's own end go, which says so in the terminal (a new line it echoes).
        while !self.reader.is_finished() {
            assert!(start.elapsed() < Duration::from_secs(15), "{what}: the terminal is still held");
            std::thread::sleep(Duration::from_millis(1));
        }
        let written = self.written.lock().unwrap().clone();
        let shown = String::from_utf8_lossy(&written).replace('\x1b', "^[");
        let count = |sequence: &str| written.windows(sequence.len()).filter(|w| *w == sequence.as_bytes()).count();
        let last = |sequence: &str| {
            let at = written.windows(sequence.len()).rposition(|w| w == sequence.as_bytes());
            at.unwrap_or_else(|| panic!("{what}: {sequence:?} never written: {shown}"))
        };
        let (pushed, popped, screens) = (count("\x1b[>5u"), count("\x1b[<u"), count("\x1b[?1049l"));
        assert_eq!((popped, screens), (pushed, 1), "{what}: pushed {pushed}, popped {popped}: {shown}");
        let order = [last("\x1b[<u"), last("\x1b[?1000l"), last("\x1b[?1049l"), last("\x1b[?25h")];
        assert!(order.is_sorted(), "{what}: given back out of order {order:?}: {shown}");
        let said = String::from_utf8_lossy(&written[order[3] + "\x1b[?25h".len()..]).into_owned();
        assert!(!said.contains('\x1b'), "{what}: written after the terminal was given back: {shown}");
        (status.exit_code(), said)
    }
}

#[cfg(unix)]
impl Drop for TerminalClient {
    fn drop(&mut self) {
        if self.child.try_wait().is_ok_and(|status| status.is_none()) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Wait until the pane named `name` is seen as production.
pub(super) fn wait_production(env: &Env, name: &str) {
    let start = Instant::now();
    loop {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let pane = panes.as_array().unwrap().iter().find(|p| p["name"] == name).cloned().unwrap();
        if pane["production"].is_string() {
            return;
        }
        assert!(start.elapsed() < PATIENCE, "never seen as production: {pane}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// What a pane shows below the line marking the text a restart restored (all of it without one):
/// what runs in the pane now wrote that.
fn since_restore(screen: &str) -> &str {
    match screen.rfind("kraal: restored from") {
        Some(at) => screen[at..].split_once('\n').map_or("", |(_, rest)| rest),
        None => screen,
    }
}

pub(super) fn wait_gone(path: &Path) {
    let start = Instant::now();
    while path.exists() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// How `child` ended and what it printed, once it has: still running after `limit`, it is
/// killed and the test fails.
pub(super) fn finish(mut child: std::process::Child, limit: Duration) -> Output {
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("still running after {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

/// A process this test stopped (SIGSTOP), continued when the test ends, however it ends.
pub(super) struct Stopped(libc::pid_t);

impl Stopped {
    pub(super) fn stop(pid: libc::pid_t) -> Stopped {
        // SAFETY: plain syscall to a process this test started.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
        Stopped(pid)
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        // SAFETY: plain syscall to a process this test started.
        unsafe { libc::kill(self.0, libc::SIGCONT) };
    }
}

/// The session's answer to `request`, sent through its socket as written: what a kraal of
/// another release, or any other program, can send.
pub(super) fn raw_request(env: &Env, request: serde_json::Value) -> serde_json::Value {
    use std::io::Read;
    let socket = env.root.join("run/kraal").join(format!("{}.sock", env.session));
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let message = serde_json::json!({ "type": "request", "version": 6, "request": request }).to_string();
    raw_frame(&mut stream, 0, message.as_bytes());
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).unwrap();
    let mut payload = vec![0u8; u32::from_le_bytes(header[..4].try_into().unwrap()) as usize];
    stream.read_exact(&mut payload).unwrap();
    serde_json::from_slice::<serde_json::Value>(&payload).unwrap()["result"].take()
}

/// A step as a question quotes it to be approved: under its number, each line as it is written.
pub(super) fn numbered(number: usize, step: &str) -> Vec<String> {
    let label = format!("  {number}. ");
    let under = " ".repeat(label.len());
    step.lines().enumerate().map(|(i, line)| format!("{}{line}", if i == 0 { &label } else { &under })).collect()
}

/// Lines as [`RawClient::read_question`] gives them: a terminal's row does not tell the spaces
/// that end a line from the room after it.
pub(super) fn without_trailing_spaces(lines: Vec<String>) -> Vec<String> {
    lines.iter().map(|line| line.trim_end().to_string()).collect()
}

/// A terminal of `cols × rows` after `bytes`.
pub(super) fn emulate(
    bytes: &[u8],
    cols: usize,
    rows: usize,
) -> alacritty_terminal::Term<alacritty_terminal::event::VoidListener> {
    use alacritty_terminal::term::{Config, Term, test::TermSize};
    let mut term = Term::new(Config::default(), &TermSize::new(cols, rows), alacritty_terminal::event::VoidListener);
    let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
    parser.advance(&mut term, bytes);
    term
}

/// The text of each row a terminal of `cols × rows` shows after `bytes`.
pub(super) fn text_after(bytes: &[u8], cols: usize, rows: usize) -> Vec<String> {
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::cell::Flags;
    let term = emulate(bytes, cols, rows);
    let grid = term.grid();
    (0..rows)
        .map(|y| {
            let row = &grid[Line(y as i32)];
            (0..cols)
                .map(|x| &row[Column(x)])
                .filter(|c| !c.flags.contains(Flags::WIDE_CHAR_SPACER))
                .map(|c| c.c)
                .collect()
        })
        .collect()
}

/// What a terminal of `cols × rows` shows after `bytes`: each row's cells (text, colours,
/// attributes, link), the cursor (its place, its shape, whether it blinks) and the modes the
/// terminal is in (the cursor shown, what it reports of the mouse and of the keys...).
pub(super) fn terminal_after(bytes: &[u8], cols: usize, rows: usize) -> Vec<String> {
    use alacritty_terminal::index::{Column, Line};
    let term = emulate(bytes, cols, rows);
    let grid = term.grid();
    let mut out: Vec<String> = (0..rows)
        .map(|y| {
            let row = &grid[Line(y as i32)];
            (0..cols)
                .map(|x| {
                    let c = &row[Column(x)];
                    let link = c.hyperlink().map(|h| h.uri().to_string());
                    let text: String =
                        std::iter::once(c.c).chain(c.zerowidth().into_iter().flatten().copied()).collect();
                    format!("{text:?} {:?} {:?} {:?} {:?} {link:?}", c.fg, c.bg, c.flags, c.underline_color())
                })
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .collect();
    let cursor = grid.cursor.point;
    out.push(format!("cursor {} {} {:?}", cursor.line.0, cursor.column.0, term.cursor_style()));
    out.push(format!("modes {:?}", term.mode()));
    out
}

impl RawClient {
    /// The text of each row its terminal shows.
    pub(super) fn rows(&self) -> Vec<String> {
        let bytes = self.screen.lock().unwrap().clone();
        text_after(&bytes, self.size.0, self.size.1)
    }

    /// The text of each row its terminal showed after the first frame drawn for it.
    pub(super) fn first_frame(&self) -> Vec<String> {
        let end = "\x1b[?2026l";
        self.wait_for_screen(end);
        let screen = self.screen.lock().unwrap();
        let drawn = screen.windows(end.len()).position(|w| w == end.as_bytes()).unwrap() + end.len();
        text_after(&screen[..drawn], self.size.0, self.size.1)
    }

    /// Wait until its terminal shows `text` on a row; the rows then. Unlike
    /// [`RawClient::wait_for_screen`], this finds text drawn over cells that already had some of
    /// it, which a frame leaves out.
    pub(super) fn wait_for_row(&self, text: &str) -> Vec<String> {
        self.wait_for_rows(text, |rows| rows.iter().any(|r| r.contains(text)))
    }

    /// Close the overlay identified by `text` before sending more keys: Escape followed by
    /// another key before its timeout is Alt+that key, even in separate reads.
    pub(super) fn close_overlay(&mut self, text: &str) {
        self.wait_for_row(text);
        self.send(b"\x1b");
        self.wait_for_rows(&format!("{text} closed"), |rows| !rows.iter().any(|r| r.contains(text)));
    }

    /// Wait until the rows its terminal shows are `what` says (`done`); the rows then.
    pub(super) fn wait_for_rows(&self, what: &str, done: impl Fn(&[String]) -> bool) -> Vec<String> {
        let start = Instant::now();
        loop {
            let bytes = self.screen.lock().unwrap().clone();
            let rows = text_after(&bytes, self.size.0, self.size.1);
            if done(&rows) {
                self.trace_step_at(bytes.len(), ClientAction::Observed(what.into()));
                return rows;
            }
            assert!(start.elapsed() < PATIENCE, "never {what}:\n{}", rows.join("\n"));
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The message of the question its terminal shows, read as someone would: from the top to
    /// the end, pressing `key` while the box says there is more below. Each line is as written,
    /// a line the box cut over several rows (marked in its margin) put back together.
    pub(super) fn read_question(&mut self, key: &[u8]) -> Vec<String> {
        // The rows still below, when the message is longer than the box.
        let below = |rows: &[String]| {
            let said = rows.iter().find(|r| r.contains("more line") || r.contains("↑ top"))?;
            let count = said.split("↓ ").nth(1).and_then(|rest| rest.split(' ').next());
            Some(count.map_or(0, |n| n.parse::<usize>().unwrap()))
        };
        let mut rows: Vec<(bool, Vec<char>)> = Vec::new();
        let mut before = None;
        let mut first = None;
        loop {
            // The footer is drawn last: the box above it is whole.
            let screen = self.wait_for_rows("the question, further down", |rows| {
                rows.iter().any(|r| r.contains("Enter Cancel") && r.contains("Esc Cancel"))
                    && Some(below(rows)) != before
            });
            let left_to_read = below(&screen);
            // The message may itself say "Cancel and save...": the button row comes last,
            // before the footer that also names Cancel.
            let footer = screen.iter().rposition(|r| r.contains("Enter Cancel") && r.contains("Esc Cancel")).unwrap();
            let buttons = screen[..footer].iter().rposition(|r| r.contains(" Cancel ")).unwrap();
            // The Cancel button is two columns inside the box, after the margin.
            let left = screen[buttons].split("Cancel").next().unwrap().chars().count() - 3;
            let border = screen[buttons].chars().nth(left).unwrap();
            let right = left + 1 + screen[buttons].chars().skip(left + 1).position(|c| c == border).unwrap();
            let top =
                screen[..buttons].iter().rposition(|row| matches!(row.chars().nth(left), Some('╭' | '+'))).unwrap();
            // Above the buttons: an empty row and, when there is more to read, the row saying so.
            let body = &screen[top + 1..buttons - 1 - usize::from(left_to_read.is_some())];
            let at = *first.get_or_insert(left_to_read.unwrap_or(0)) - left_to_read.unwrap_or(0);
            rows.truncate(at);
            for row in body {
                let cells: Vec<char> = row.chars().collect();
                rows.push((cells[left + 1] == '↪', cells[left + 2..right - 1].to_vec()));
            }
            if left_to_read.unwrap_or(0) == 0 {
                break;
            }
            before = Some(left_to_read);
            self.send(key);
        }
        let mut lines: Vec<Vec<char>> = Vec::new();
        for (continued, text) in rows {
            match lines.last_mut().filter(|_| continued) {
                Some(line) => {
                    let indent = line.iter().take_while(|c| **c == ' ').count().min(text.len() / 2);
                    line.extend(&text[indent..]);
                }
                None => lines.push(text),
            }
        }
        lines.iter().map(|line| line.iter().collect::<String>().trim_end().to_string()).collect()
    }

    /// Wait until nothing more is drawn for a while; how much was drawn.
    pub(super) fn settle(&self) -> usize {
        let mut seen = self.screen.lock().unwrap().len();
        let start = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(300));
            let (now, framed) = {
                let screen = self.screen.lock().unwrap();
                (screen.len(), screen.windows(8).any(|w| w == b"\x1b[?2026l"))
            };
            // A new connection can be quiet before its first frame arrives.
            if now == seen && framed {
                return now;
            }
            seen = now;
            assert!(start.elapsed() < PATIENCE, "the screen never settles");
        }
    }
}

/// The script of a pane that says its size each time its terminal is resized, as a program that
/// draws itself again does.
pub(super) fn says_its_size(name: &str) -> String {
    format!("trap 'echo {name} is $(stty size)' WINCH; echo {name} ready; while :; do sleep 1 & wait $!; done")
}

pub(super) fn account_switch_agent(env: &Env, agent: &str, mode: &str) -> String {
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join(agent),
        r#"#!/bin/sh
home="${CLAUDE_CONFIG_DIR:-${CODEX_HOME:-}}"
case "$*" in
    'auth status'|'login status')
        test -d "$home" || exit 1
        echo check >> "$home/checks"
        while test -f "$home/hold-check"; do sleep 0.05; done
        echo checked >> "$home/checked"
        exit "$(cat "$home/status" 2>/dev/null || echo 1)"
        ;;
    'auth login'|'login') echo login >> "$home/logins"; echo SIGNING-IN; exit 1 ;;
    app-server*) exit 0 ;;
esac
echo "$$" >> "$home/starts"
while test -f "$home/hold-start"; do sleep 0.05; done
test -f "$home/fail-start" && exit 23
if [ "$1" = --session-id ]; then
    mkdir -p "$home/projects/work"
    printf '{"message":1}\n' > "$home/projects/work/$2.jsonl"
fi
if [ -n "$CODEX_HOME" ]; then
    session="conversation-$(basename "$home")"
    [ "$1" = resume ] && session="$2"
    mkdir -p "$home/sessions/2026/10/13"
    printf '{"type":"session_meta","payload":{"id":"%s"}}\n' "$session" > "$home/sessions/2026/10/13/rollout-today-$session.jsonl"
fi
echo "READY home=$home args=$*"
while IFS= read -r line; do
    if [ -n "$CODEX_HOME" ]; then
        "$KRAAL_TEST_BIN" hook codex "{\"type\":\"agent-turn-complete\",\"thread-id\":\"$session\"}"
    fi
    printf 'reply:%s\n' "$line"
done
"#,
    );
    let mut config = format!("[limits]\nmode = {mode:?}\nthreshold = 90\n");
    for name in ["alpha", "beta"] {
        let home = env.root.join(name);
        std::fs::create_dir_all(home.join("projects/work")).unwrap();
        std::fs::write(home.join("status"), "0").unwrap();
        config.push_str(&format!(
            "[[account]]\nid = \"test-{agent}-{name}\"\nagent = {agent:?}\nname = {name:?}\nhome = {:?}\n",
            home.to_str().unwrap()
        ));
    }
    std::fs::write(env.root.join("config/kraal/accounts.toml"), config).unwrap();
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let binary = env.cmd(&[]).get_program().to_os_string();
    let out = env.cmd(&["open", "w"]).env("PATH", &path).env("KRAAL_TEST_BIN", binary).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let socket = env.root.join(format!("run/kraal/{}.sock", env.session));
    let out = env
        .cmd(&["agent", agent, "--account", "alpha", "--tab"])
        .env("PATH", &path)
        .env("KRAAL_SOCKET", socket)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let pane = String::from_utf8(out.stdout).unwrap().trim().to_string();
    env.wait_for_text(&pane, "READY");
    if agent == "codex" {
        env.run(&["send", "-p", &pane, "first turn\r"]);
        env.wait_for_text(&pane, "reply:first turn");
    }
    pane
}

/// Native login and status commands controlled by files in the isolated account home.
pub(super) fn login_agent(env: &Env, agent: &str) -> (PathBuf, String) {
    let bin = env.root.join("bin");
    let home = env.root.join("account");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    write_script(
        &bin.join(agent),
        r#"#!/bin/sh
home="${CLAUDE_CONFIG_DIR:-${CODEX_HOME:-}}"
case "$*" in
    'auth status'|'login status')
        test -d "$home" || exit 1
        code=$(cat "$home/status" 2>/dev/null) || code=1
        echo check >> "$home/checks"
        while test -f "$home/hold-check"; do sleep 0.05; done
        echo checked >> "$home/checks"
        printf 'PRIVATE-LOGIN-OUTPUT\n' >&2
        exit "$code"
        ;;
    'auth login'|'login')
        echo login >> "$home/logins"
        printf 'PRIVATE-LOGIN-OUTPUT\n\033]0;PRIVATE-LOGIN-OUTPUT\007\033]777;notify;PRIVATE-LOGIN-OUTPUT;secret\007'
        while test ! -f "$home/finish-login"; do sleep 0.05; done
        exit "$(cat "$home/finish-login")"
        ;;
    *) exit 1 ;;
esac
"#,
    );
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nid = 'test-{agent}-work'\nname = \"work\"\nagent = {agent:?}\nhome = {:?}\n",
            home.to_str().unwrap()
        ),
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    (home, path)
}

pub(super) fn existing_sign_in_agent(env: &Env, agent: &str) -> String {
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join(agent),
        r#"#!/bin/sh
case "$0" in
    *claude) home="${CLAUDE_CONFIG_DIR:-$HOME/.claude}" ;;
    *codex) home="${CODEX_HOME:-$HOME/.codex}" ;;
esac
case "$*" in
    'auth status'|'login status')
        echo "$home" >> "$HOME/status-checks"
        test -f "$home/status" || exit 1
        while test -f "$home/hold-check"; do sleep 0.05; done
        echo checked >> "$home/check-finished"
        test ! -f "$home/public-status" || cat "$home/public-status"
        exit "$(cat "$home/status")"
        ;;
    'auth login'|'login') echo unexpected >> "$HOME/logins"; exit 2 ;;
    app-server*) exit 1 ;;
esac
echo "$$" >> "$home/starts"
if [ "$1" = --session-id ]; then
    mkdir -p "$home/projects/work"
    printf '{"message":1}\n' > "$home/projects/work/$2.jsonl"
fi
echo "READY home=$home"
while IFS= read -r line; do printf 'reply:%s\n' "$line"; done
"#,
    );
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default())
}

/// A fake ssh, first in the PATH returned: it runs the command "on the server", that is here, as
/// the kraal of `remote` (with its own directories); "down.lan" does not answer. Every call is
/// logged in the file returned.
#[cfg(unix)]
pub(super) fn fake_ssh(local: &Env, remote: &Env) -> (PathBuf, String) {
    let bin = local.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_kraal"), bin.join("kraal")).unwrap();
    let log = local.root.join("ssh.log");
    write_script(
        &bin.join("ssh"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log}\nfor a; do cmd=$a; done\n\
             case \"$*\" in *down.lan*) echo 'ssh: connect to host down.lan port 22: Connection refused' >&2; exit 255;; esac\n\
             unset KRAAL KRAAL_SOCKET KRAAL_SESSION KRAAL_PANE\n\
             export HOME={r}/home XDG_RUNTIME_DIR={r}/run XDG_STATE_HOME={r}/state XDG_CONFIG_HOME={r}/config\n\
             exec sh -c \"$cmd\"\n",
            log = log.display(),
            r = remote.root.display()
        ),
    );
    (log, format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()))
}

/// Copy the program `from` to `to`. A child process copies it, as one writes a script
/// ([`write_script`]): a program this process wrote cannot be run ("text file busy") while a
/// child that another test started at that moment still holds its file, on its way to running
/// what it was started for.
pub(super) fn copy_program(from: &str, to: &Path) {
    let copied = Command::new("cp").arg(from).arg(to).status().unwrap();
    assert!(copied.success(), "copying {from} to {}", to.display());
}

/// A release as GitHub serves it, made from this build: `kraal-<target>.tar.gz` and its
/// checksum, in `dir`.
pub(super) fn fake_release(dir: &Path) -> String {
    fake_release_of(dir, None)
}

/// A release, made from this build; with `version`, its kraal says it is that version and is
/// this build otherwise.
pub(super) fn fake_release_of(dir: &Path, version: Option<&str>) -> String {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        _ => "unknown-linux-musl",
    };
    let target = format!("{}-{os}", std::env::consts::ARCH);
    let inner = dir.join(format!("kraal-{target}"));
    std::fs::create_dir_all(&inner).unwrap();
    match version {
        Some(v) => write_script(
            &inner.join("kraal"),
            &format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo 'kraal {v}'; exit 0; }}\nexec '{}' \"$@\"\n",
                env!("CARGO_BIN_EXE_kraal")
            ),
        ),
        None => copy_program(env!("CARGO_BIN_EXE_kraal"), &inner.join("kraal")),
    }
    let asset = format!("kraal-{target}.tar.gz");
    let tar = Command::new("tar")
        .arg("-czf")
        .arg(dir.join(&asset))
        .arg("-C")
        .arg(dir)
        .arg(format!("kraal-{target}"))
        .status()
        .unwrap();
    assert!(tar.success());
    let sum = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "cd '{}' && (sha256sum {asset} 2>/dev/null || shasum -a 256 {asset}) > {asset}.sha256",
            dir.display()
        ))
        .status()
        .unwrap();
    assert!(sum.success());
    asset
}

/// Run `cmd` in a terminal (`script`, from util-linux), typing `input` into it; what the
/// terminal showed.
#[cfg(target_os = "linux")]
pub(super) fn in_terminal(cmd: Command, input: &[u8]) -> String {
    use std::io::{Read, Write};
    let line = std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|a| format!("'{}'", a.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ");
    let mut c = Command::new("script");
    c.args(["-q", "-f", "-c", &line, "/dev/null"]);
    if let Some(dir) = cmd.get_current_dir() {
        c.current_dir(dir);
    }
    for (k, v) in cmd.get_envs() {
        match v {
            Some(v) => c.env(k, v),
            None => c.env_remove(k),
        };
    }
    c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    let mut child = c.spawn().expect("script (util-linux)");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input).unwrap();
    let mut out = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut shown = Vec::new();
        let _ = out.read_to_end(&mut shown);
        shown
    });
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < Duration::from_secs(30), "{line} is still running");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(stdin);
    String::from_utf8_lossy(&reader.join().unwrap()).into_owned()
}

/// A throwaway sshd on 127.0.0.1 with its own host and user keys, and an `ssh` that uses only
/// them (never ~/.ssh): a server reached for real, where the user who logs in has a home of the
/// test's own. `None` where no sshd can run here.
#[cfg(target_os = "linux")]
pub(super) struct Sshd {
    child: std::process::Child,
    /// Where it listens: a port of its own, so that no test waits for the server of another
    /// one, in this run of the tests or in another on this machine.
    pub(super) port: u16,
    /// Where that `ssh` is, to put first in PATH.
    pub(super) bin: PathBuf,
}

#[cfg(target_os = "linux")]
impl Sshd {
    pub(super) fn start(dir: &Path) -> Option<Sshd> {
        let sshd = ["/usr/sbin/sshd", "/usr/bin/sshd"].into_iter().map(PathBuf::from).find(|p| p.exists())?;
        let ssh =
            ["/usr/bin/ssh", "/bin/ssh", "/usr/local/bin/ssh"].into_iter().map(PathBuf::from).find(|p| p.exists())?;
        std::fs::create_dir_all(dir.join("home")).unwrap();
        for key in ["host_key", "user_key"] {
            let made =
                Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(dir.join(key)).status();
            if !made.is_ok_and(|s| s.success()) {
                return None;
            }
        }
        std::fs::copy(dir.join("user_key.pub"), dir.join("authorized_keys")).unwrap();
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
        let d = dir.display();
        let host_key = std::fs::read_to_string(dir.join("host_key.pub")).unwrap();
        std::fs::write(
            dir.join("ssh_config"),
            format!(
                "Host *\n  UserKnownHostsFile {d}/known_hosts\n  GlobalKnownHostsFile /dev/null\n  \
                 StrictHostKeyChecking yes\n  IdentityFile {d}/user_key\n  IdentitiesOnly yes\n  \
                 IdentityAgent none\n  LogLevel ERROR\n"
            ),
        )
        .unwrap();
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // kraal gives a server a few seconds to answer. This one is on this machine, as slow as
        // the tests beside it make it, and that limit is not what its tests are about: its `ssh`
        // waits as long as they do (of an option given twice, ssh takes the first).
        write_script(
            &bin.join("ssh"),
            &format!(
                "#!/bin/sh\nexec '{}' -F '{d}/ssh_config' -o ConnectTimeout={} \"$@\"\n",
                ssh.display(),
                PATIENCE.as_secs()
            ),
        );
        let log = dir.join("sshd.log");
        loop {
            // A port the system says is free. Another program may take it before sshd does:
            // sshd says so, and another one is asked for.
            let port = std::net::TcpListener::bind(("127.0.0.1", 0)).ok()?.local_addr().ok()?.port();
            std::fs::write(
                dir.join("sshd_config"),
                format!(
                    "Port {port}\nListenAddress 127.0.0.1\nHostKey {d}/host_key\nPidFile {d}/sshd.pid\n\
                     AuthorizedKeysFile {d}/authorized_keys\nStrictModes no\nUsePAM no\nPasswordAuthentication no\n\
                     KbdInteractiveAuthentication no\nPubkeyAuthentication yes\nAllowUsers {}\n\
                     SetEnv HOME={d}/home\nPermitUserRC no\n",
                    user.trim()
                ),
            )
            .unwrap();
            std::fs::write(dir.join("known_hosts"), format!("[127.0.0.1]:{port} {host_key}")).unwrap();
            let _ = std::fs::remove_file(&log);
            let config = dir.join("sshd_config");
            let child = Command::new(&sshd).arg("-D").arg("-f").arg(config).arg("-E").arg(&log).spawn().ok()?;
            let mut sshd = Sshd { child, port, bin: bin.clone() };
            let start = Instant::now();
            let said = || std::fs::read_to_string(&log).unwrap_or_default();
            loop {
                if said().contains("Server listening") {
                    return Some(sshd);
                }
                // What it said last is read once it has ended.
                let ended = sshd.child.try_wait().ok().flatten().is_some();
                if ended && said().contains("Address already in use") {
                    break;
                }
                if ended || start.elapsed() > PATIENCE {
                    eprintln!("sshd did not start: {}", said());
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for Sshd {
    fn drop(&mut self) {
        // One that could not start has ended, and its pid may be another program's by now.
        if let Ok(None) = self.child.try_wait() {
            // SAFETY: the pid of the child this test started, not reaped yet.
            unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
            let _ = self.child.wait();
        }
    }
}

/// `kraal buffer set` with `text` on its standard input.
pub(super) fn set_buffer(env: &Env, args: &[&str], text: &str) -> Output {
    use std::io::Write;
    let mut child = env
        .cmd(&[&["buffer", "set"], args].concat())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

pub(super) fn buffer_list(env: &Env) -> Vec<serde_json::Value> {
    let list: serde_json::Value = serde_json::from_str(&env.run(&["buffer", "list", "--json"])).unwrap();
    list.as_array().unwrap().clone()
}

/// A session, a real sshd as the server `box`, and a local editor told what
/// to do, for the tests of editing a server's file. The commands kraal runs there find a `sudo`
/// that takes the password `pass word` (and runs them as this user), and wait 2 seconds once
/// the file `slow` exists.
#[cfg(target_os = "linux")]
pub(super) struct EditBench {
    pub(super) env: Env,
    _sshd: Sshd,
    pub(super) path: String,
    editor: PathBuf,
    pub(super) srv: PathBuf,
}

#[cfg(target_os = "linux")]
impl EditBench {
    pub(super) fn new(name: &str) -> Option<EditBench> {
        let env = Env::new(name);
        let sshd = Sshd::start(&env.root.join("sshd"))?;
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
        std::fs::write(
            env.root.join("config/kraal/servers.toml"),
            format!(
                "[[server]]\nname = \"box\"\nhost = \"127.0.0.1\"\nport = {}\nuser = \"{}\"\n",
                sshd.port,
                user.trim()
            ),
        )
        .unwrap();
        let cfg = env.root.join("config/kraal/config.toml");
        let mut text = std::fs::read_to_string(&cfg).unwrap();
        let downloads = env.root.join("downloads");
        text.push_str(&format!("\n[servers]\nmultiplex = false\ndownload_dir = \"{}\"\n", downloads.display()));
        std::fs::write(&cfg, text).unwrap();
        let root = env.root.display().to_string();
        let fake = env.root.join("server-bin");
        std::fs::create_dir_all(&fake).unwrap();
        write_script(
            &fake.join("sudo"),
            "#!/bin/sh\nn=\nwhile [ $# -gt 0 ]; do case $1 in -n) n=1 ;; -p) shift ;; --) shift; break ;; -*) ;; *) break ;; esac; shift; done\n\
             if [ -n \"$n\" ]; then echo 'sudo: a password is required' >&2; exit 1; fi\n\
             IFS= read -r pw\nif [ \"$pw\" != 'pass word' ]; then echo 'Sorry, try again.' >&2; exit 1; fi\nexec \"$@\"\n",
        );
        // In front of the test's ssh: what runs there finds that sudo first.
        let bin = env.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(
            &bin.join("ssh"),
            &format!(
                "#!/bin/sh\nif [ -e '{root}/slow' ]; then rm -f '{root}/slow'; sleep 2; fi\nn=$#; i=0\nfor a; do\n  i=$((i + 1))\n  \
                 if [ \"$i\" -eq \"$n\" ]; then set -- \"$@\" \"env PATH={}:/usr/local/bin:/usr/bin:/bin $a\"; else set -- \"$@\" \"$a\"; fi\n\
                 done\nshift \"$n\"\nexec '{}/ssh' \"$@\"\n",
                fake.display(),
                sshd.bin.display()
            ),
        );
        let editor = env.root.join("editor");
        write_script(
            &editor,
            &format!(
                "#!/bin/sh\nfor f; do :; done\nprintf '%s' \"$f\" > '{root}/editing.tmp' && mv '{root}/editing.tmp' '{root}/editing'\n\
                 n=0\nwhile :; do\n  if [ -e '{root}/step'$n ]; then . '{root}/step'$n; n=$((n + 1)); fi\n  \
                 if [ -e '{root}/quit' ]; then rm -f '{root}/quit' '{root}/editing' '{root}/step'*; exit 0; fi\n  sleep 0.05\ndone\n"
            ),
        );
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let srv = env.root.join("srv");
        std::fs::create_dir_all(&srv).unwrap();
        env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
        let bench = EditBench { env, _sshd: sshd, path, editor, srv };
        let out = bench.kraal(&["open", "w"]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        Some(bench)
    }

    pub(super) fn kraal(&self, args: &[&str]) -> Command {
        let mut c = self.env.cmd(args);
        c.env("PATH", &self.path).env("EDITOR", &self.editor).env_remove("VISUAL");
        c
    }

    pub(super) fn wait_until(&self, what: &str, ok: &dyn Fn() -> bool) {
        let start = Instant::now();
        while !ok() {
            assert!(start.elapsed() < PATIENCE, "timed out waiting: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The editor's copy, once the editor runs.
    pub(super) fn editing(&self) -> PathBuf {
        let editing = self.env.root.join("editing");
        self.wait_until("the editor starts", &|| editing.exists());
        PathBuf::from(read(&editing))
    }

    /// Have the editor run `script` (`$f` is the file) as its step `n`.
    pub(super) fn step(&self, n: u32, script: &str) {
        let tmp = self.env.root.join("step.tmp");
        std::fs::write(&tmp, script).unwrap();
        std::fs::rename(&tmp, self.env.root.join(format!("step{n}"))).unwrap();
    }

    /// End the editor: its private copy goes.
    pub(super) fn quit(&self, local: &Path) {
        std::fs::write(self.env.root.join("quit"), "").unwrap();
        let dir = local.parent().unwrap().to_path_buf();
        self.wait_until("the private copy is removed", &|| !dir.exists());
    }
}

#[cfg(target_os = "linux")]
pub(super) fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

pub(super) fn click_account_text(client: &mut RawClient, text: &str) {
    let clickable = |row: &str| !row.contains("Enter ") && !row.contains('⏎');
    let rows = client.wait_for_rows(&format!("clickable {text}"), |rows| {
        rows.iter().any(|row| clickable(row) && row.contains(text))
    });
    let (x, y) = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| clickable(r))
        .filter_map(|(y, r)| r.rfind(text).map(|at| (r[..at].chars().count(), y)))
        .max()
        .unwrap();
    client.send(format!("\x1b[<0;{};{}M\x1b[<0;{};{}m", x + 1, y + 1, x + 1, y + 1).as_bytes());
}
