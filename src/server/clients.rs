//! Clients: connections, their frames, attaching and detaching.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{SendTimeoutError, TrySendError};

use super::requests::{ReplyHandle, RequestOutcome};
use super::{Client, ClientId, Dirty, Event, Mode, Printed, Server};
use crate::ansi::Encoder;
use crate::input::InputParser;
use crate::pane::PaneSpec;
use crate::protocol::{self, ClientMsg, ServerMsg};

/// Clients connected at once (attached terminals, CLI commands, event streams).
const MAX_CLIENTS: usize = 256;
const FIRST_MESSAGE_TIMEOUT: Duration = Duration::from_secs(10);

impl Server {
    pub(super) fn new_connection(&mut self, stream: UnixStream) {
        self.new_connection_with_timeout(stream, Some(FIRST_MESSAGE_TIMEOUT));
    }

    pub(super) fn new_connection_with_timeout(&mut self, stream: UnixStream, first_message: Option<Duration>) {
        // Every connection costs two threads: a runaway script must not open thousands.
        if self.clients.len() + self.subscribers.len() + self.waits.len() >= MAX_CLIENTS {
            let mut stream = stream;
            let refused = ServerMsg::Response { result: Err(format!("too many connections ({MAX_CLIENTS})")) };
            let _ = protocol::write_json(&mut stream, &refused);
            return;
        }
        let id = self.alloc_id();
        let peer = crate::process::socket_peer(&stream).ok().and_then(|(pid, _)| u32::try_from(pid).ok());
        let Ok(read_half) = stream.try_clone() else { return };
        // One that connects without saying anything is let go.
        let _ = read_half.set_read_timeout(first_message);
        // Without a thread to read or write it, the connection is closed (dropped).
        let Ok(out) = Outbox::start(stream, id) else { return };
        let tx = self.tx.clone();
        let reading = crate::threads::named(format!("client-{id}-read")).spawn(move || {
            let mut r = std::io::BufReader::with_capacity(64 * 1024, read_half);
            let mut first = true;
            while let Ok(Some((kind, payload))) = protocol::read_frame(&mut r) {
                if std::mem::take(&mut first) {
                    let _ = r.get_ref().set_read_timeout(None);
                }
                if tx.send(Event::ClientFrame(id, kind, payload)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Event::ClientGone(id));
        });
        if reading.is_err() {
            return;
        }
        let ws = self.workspaces.first().map(|w| w.id).unwrap_or(0);
        self.clients.insert(
            id,
            Client {
                id,
                out,
                attached: false,
                clipboard: Default::default(),
                attachments: Default::default(),
                cols: 80,
                rows: 24,
                workspace: ws,
                parser: InputParser::new(),
                pending_since: None,
                mode: Mode::Normal,
                last_account_agent: None,
                adding_account: None,
                adding_account_back: None,
                encoder: Encoder::new(),
                frame: Default::default(),
                memo: Default::default(),
                sidebar: None,
                sidebar_hover: None,
                dirty: Dirty::No,
                printed: Printed::None,
                last_input: Instant::now(),
                terminal_focused: true,
                pointer_motion: false,
                keys_full: None,
                keys_by_place: false,
                told_shown: None,
                paste: None,
                drag: None,
                toast: None,
                toast_action: None,
                notice: None,
                last_click: None,
                welcome: false,
                remote: false,
                laptop: None,
                peer,
                origin: None,
            },
        );
    }

    pub(super) fn client_frame(&mut self, id: ClientId, kind: u8, payload: Vec<u8>) {
        if kind == protocol::BYTES {
            if !self.attachment_input(id, &payload) {
                self.client_input(id, &payload);
            }
            return;
        }
        let msg: ClientMsg = match serde_json::from_slice(&payload) {
            Ok(m) => m,
            Err(e) => {
                self.send_msg(id, &ServerMsg::Response { result: Err(format!("bad message: {e}")) });
                return;
            }
        };
        match msg {
            ClientMsg::LaptopCopy { token, message } => self.laptop_copy_message(id, token, message),
            ClientMsg::Attach { version, cols, rows, workspace, remote, laptop, pane, installed } => {
                self.cancel_laptop_client(id);
                if version != protocol::PROTOCOL_VERSION {
                    self.send_msg(id, &ServerMsg::exit(self.version_mismatch(version)));
                    self.clients.remove(&id);
                    return;
                }
                if let Some(c) = self.clients.get_mut(&id) {
                    c.attachments.pending = None;
                    c.attachments.blocked = None;
                    c.remote = remote;
                    c.laptop = laptop.filter(|l| {
                        remote
                            && crate::attachments::token_valid(&l.id)
                            && !l.host.is_empty()
                            && l.host.len() <= 255
                            && !l.host.chars().any(char::is_control)
                    });
                    c.attachments.focus = None;
                }
                let (cols, rows) = clamp_size(cols, rows);
                self.attach(id, cols, rows, workspace);
                if let Some(laptop) = self.clients.get(&id).and_then(|c| c.laptop.clone()) {
                    for p in self.panes.values_mut() {
                        if let crate::pane::Content::Files(f) = &mut p.content {
                            f.reconnect_laptop(&laptop.id, &laptop.host, id);
                        }
                    }
                }
                if let Some(installed) = installed {
                    self.version_notice(id, &installed);
                }
                // A pane asked for (an agent chosen from another kraal's sidebar) is shown.
                if let Some(p) = pane {
                    match self.resolve(&p) {
                        Ok(p) => self.reveal(id, p),
                        Err(e) => self.warnings.push(e),
                    }
                }
            }
            ClientMsg::LaptopListChunk { pane, side, request, data, done } => {
                if self.clients.get(&id).is_some_and(|c| c.attached && c.laptop.is_some()) {
                    if let Some(crate::pane::Content::Files(f)) = self.panes.get_mut(&pane).map(|p| &mut p.content) {
                        f.laptop_chunk(id, side, request, data, done);
                    }
                    self.send_msg(id, &ServerMsg::LaptopListAck { pane, side, request });
                    self.mark_dirty();
                }
            }
            ClientMsg::LaptopCaseSensitive { pane, side, request, case_sensitive } => {
                if self.clients.get(&id).is_some_and(|c| c.attached && c.laptop.is_some())
                    && let Some(crate::pane::Content::Files(f)) = self.panes.get_mut(&pane).map(|p| &mut p.content)
                {
                    f.laptop_case_sensitive(id, side, request, case_sensitive);
                }
            }
            ClientMsg::LaptopListed { pane, side, request, result } => {
                if self.clients.get(&id).is_some_and(|c| c.attached && c.laptop.is_some()) {
                    if let Some(crate::pane::Content::Files(f)) = self.panes.get_mut(&pane).map(|p| &mut p.content) {
                        f.laptop_result(id, side, request, result);
                    }
                    self.mark_dirty();
                }
            }
            ClientMsg::Resize { cols, rows } => {
                let (cols, rows) = clamp_size(cols, rows);
                if let Some(c) = self.clients.get_mut(&id) {
                    c.cols = cols;
                    c.rows = rows;
                    c.encoder.invalidate();
                }
                self.primary = Some(id);
                self.probe_cell_size(id);
                self.relayout();
            }
            ClientMsg::Repaint => {
                if let Some(c) = self.clients.get_mut(&id).filter(|c| c.attached) {
                    c.encoder.invalidate();
                    c.dirty = Dirty::Yes;
                    self.render_now();
                }
            }
            ClientMsg::ClipboardResult { id: copy, outcome, native_unavailable } => {
                self.clipboard_result(id, copy, outcome, native_unavailable);
            }
            ClientMsg::ClipboardPaste { pane, key, text, token } => self.clipboard_paste(id, pane, key, text, token),
            ClientMsg::PasteStart => self.reserve_paste(id),
            msg @ (ClientMsg::AttachmentRequest { .. }
            | ClientMsg::AttachmentText { .. }
            | ClientMsg::AttachmentStart { .. }
            | ClientMsg::AttachmentChunk { .. }
            | ClientMsg::AttachmentFinish { .. }
            | ClientMsg::AttachmentCancel { .. }
            | ClientMsg::AttachmentStatus { .. }
            | ClientMsg::AttachmentDismiss { .. }
            | ClientMsg::AttachmentLocal { .. }) => {
                self.attachment_message(id, msg);
                // A transfer that ended gives the client's input back: a question may wait for it.
                self.show_production_ask();
            }
            ClientMsg::Detach => self.detach(id, "detached"),
            ClientMsg::Request { version, request, origin } => {
                if let Some(c) = self.clients.get_mut(&id) {
                    c.origin = origin;
                }
                let result = if version != protocol::PROTOCOL_VERSION
                    && !(version == protocol::upgrade_protocol()
                        && matches!(
                            request,
                            crate::protocol::Request::Ping | crate::protocol::Request::KillServer { .. }
                        )) {
                    RequestOutcome::Immediate {
                        reply: ReplyHandle::new(id),
                        result: Err(self.version_mismatch(version)),
                    }
                } else {
                    self.request(id, request)
                };
                if let RequestOutcome::Immediate { reply, result } = result {
                    reply.complete(self, result);
                }
            }
            // From a newer client: an attached one goes on without it, a connection that came
            // to ask something is told so.
            ClientMsg::Unknown(msg) => {
                if !self.clients.get(&id).is_some_and(|c| c.attached) {
                    self.send_msg(id, &ServerMsg::Response { result: Err(protocol::unsupported(&msg)) });
                    self.clients.remove(&id);
                }
            }
        }
    }

    /// What a client of another protocol generation is told. `kraal restart` and `kraal kill`
    /// stop a server of any version.
    pub(super) fn version_mismatch(&self, version: u32) -> String {
        protocol::version_mismatch(version, protocol::PROTOCOL_VERSION, protocol::KRAAL_VERSION, &self.session, None)
    }

    pub(super) fn attach(&mut self, id: ClientId, cols: u16, rows: u16, workspace: Option<String>) {
        if let Some(c) = self.clients.get_mut(&id) {
            c.cols = cols;
            c.rows = rows;
        }
        // While no client is attached, the layout is for a terminal nobody has. The workspace
        // this client opens is laid out for its own at once, each pane's emulator made for the
        // place it keeps, and not for that layout first and for this client's after.
        if self.primary.is_none()
            && let Some(c) = self.clients.get(&id)
        {
            self.layout_area = self.main_area(c);
        }
        let mut target = None;
        if let Some(name) = workspace {
            match self.open_workspace(&name) {
                Ok(ws) => target = Some(ws),
                // A server's name where a workspace's goes (`kraal attach fpa@anvil`): this
                // session is not what was meant, and the answer says how to reach that one.
                Err(e) if crate::servers::names_server(&self.known_servers, &name) => {
                    self.send_msg(id, &ServerMsg::exit(format!("{e:#}")));
                    return;
                }
                Err(e) => self.warnings.push(format!("{e:#}")),
            }
        }
        if self.workspaces.is_empty() && self.deleted_workspaces.is_empty() && self.removed_sources.is_empty() {
            let ws = self.new_workspace("main".into(), None);
            if let Err(e) = self.spawn_pane_in(ws, None, PaneSpec::default(), crate::protocol::Placement::Tab, None) {
                self.send_msg(id, &ServerMsg::exit(format!("cannot start a shell: {e:#}")));
                return;
            }
        }
        let ws = target.unwrap_or_else(|| {
            self.workspaces.iter().find(|w| !w.closed()).or(self.workspaces.first()).map_or(0, |w| w.id)
        });
        let mode = self.workspaces.iter().all(|w| w.closed()).then(|| Mode::Picker(self.workspace_picker(ws)));
        let welcome = !crate::paths::state_dir().join("welcomed").exists();
        // Before anything is drawn: what this server is.
        self.send_msg(
            id,
            &ServerMsg::Attached { version: protocol::KRAAL_VERSION.into(), protocol: protocol::PROTOCOL_VERSION },
        );
        if let Some(c) = self.clients.get_mut(&id) {
            c.attached = true;
            c.workspace = ws;
            if let Some(mode) = mode {
                c.mode = mode;
            }
            c.dirty = Dirty::Yes;
            c.welcome = welcome;
            // Ask the outer terminal for its colours so panes querying them get real values.
            let _ = c.out.try_send(frame_bytes(b"\x1b]10;?\x07\x1b]11;?\x07"));
            // The mouse as the server's configuration says, which may have been read again since
            // the client read its own (a reconnection keeps the client's first reading).
            let mouse = if self.config.mouse { crate::client::MOUSE_ON } else { crate::client::MOUSE_OFF };
            let _ = c.out.try_send(frame_bytes(mouse.as_bytes()));
        }
        self.probe_graphics(id);
        self.primary = Some(id);
        self.relayout();
        self.mark_seen_focused(id);
        self.git_soon_for(id);
        self.show_warnings();
        self.show_production_ask();
        self.mark_dirty();
        // The first frame goes at once: nothing else is coming to batch it with.
        self.next_render = Some(Instant::now());
    }

    pub fn detach(&mut self, id: ClientId, reason: &str) {
        self.graphics_farewell(id);
        self.send_msg(id, &ServerMsg::exit(reason));
        self.drop_client(id);
    }

    pub(super) fn drop_client(&mut self, id: ClientId) {
        self.cancel_laptop_client(id);
        self.clients.remove(&id);
        // A `kraal wait` or `kraal events` whose caller went away is dropped with it.
        self.waits.retain(|w| w.reply.client() != id);
        self.subscribers.retain(|s| s.client != id);
        // What the client was in the middle of goes with it.
        self.drafts.remove(&id);
        self.md_sessions.remove(&id);
        self.production_client_gone(id);
        let mut files_changed = false;
        for p in self.panes.values_mut() {
            if let crate::pane::Content::Files(f) = &mut p.content {
                files_changed |= f.forget_viewer(id);
            }
        }
        if files_changed {
            self.mark_dirty();
        }
        if self.primary == Some(id) {
            self.primary = self.clients.values().filter(|c| c.attached).max_by_key(|c| c.last_input).map(|c| c.id);
            self.relayout();
        }
        self.graphics_client_gone(id);
    }

    pub fn send_msg(&self, id: ClientId, msg: &ServerMsg) {
        if let Some(c) = self.clients.get(&id) {
            let mut buf = Vec::new();
            if let Err(e) = protocol::write_json(&mut buf, msg) {
                // Too large to send: the client must not wait for an answer that never comes.
                buf.clear();
                let answer = ServerMsg::Response { result: Err(format!("the answer could not be sent: {e}")) };
                let _ = protocol::write_json(&mut buf, &answer);
            }
            let _ = c.out.send_timeout(buf, Duration::from_millis(200));
        }
    }
}

/// A client's terminal size, within what kraal draws (a bogus size must not allocate gigabytes).
fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(1, 1000), rows.clamp(1, 500))
}

/// Terminal bytes in a protocol frame.
pub fn frame_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 5);
    let _ = protocol::write_frame(&mut out, protocol::BYTES, bytes);
    out
}

/// Where what goes to a client is sent. Written to its socket at once, from the server's
/// thread, as far as the socket takes it without waiting and when nothing waits before it;
/// the rest is queued, in order, for the client's writer thread.
pub struct Outbox {
    #[cfg(test)]
    captured_writer: Option<crossbeam_channel::Sender<Vec<u8>>>,
    queue: crossbeam_channel::Sender<Vec<u8>>,
    /// Messages queued and not written yet.
    queued: Arc<AtomicUsize>,
    /// Messages taken so far: the number of the last one.
    sent: AtomicU64,
    written: Arc<Mutex<Written>>,
    socket: Arc<UnixStream>,
    /// What the writer thread writes after everything queued, before it closes the connection
    /// ([`Outbox::end_with`]).
    last: Arc<OnceLock<Vec<u8>>>,
}

/// Messages an outbox queues for its writer thread: a client further behind takes no more.
const QUEUE: usize = 64;

/// How far a client's messages are written to its socket, in order: their count, and when the
/// one watched ([`Outbox::watch`]) was.
#[derive(Default)]
struct Written {
    count: u64,
    watched: u64,
    at: Option<Instant>,
}

impl Written {
    fn one(&mut self) {
        self.count += 1;
        if self.count == self.watched {
            self.at = Some(Instant::now());
        }
    }
}

fn wrote(written: &Mutex<Written>) {
    if let Ok(mut w) = written.lock() {
        w.one();
    }
}

impl Outbox {
    #[cfg(test)]
    pub(super) fn capture(&mut self) -> crossbeam_channel::Receiver<Vec<u8>> {
        let (send, receive) = crossbeam_channel::bounded(4);
        // Closing the original queue would shut down the socket and report ClientGone.
        let writer = std::mem::replace(&mut self.queue, send);
        self.captured_writer.get_or_insert(writer);
        // Keep handler tests on the queue, independent of the socket writer's scheduling.
        self.queued.store(1, Ordering::Release);
        receive
    }

    /// The outbox of a client connected on `stream`, and its writer thread.
    fn start(stream: UnixStream, id: ClientId) -> std::io::Result<Outbox> {
        let (tx, rx) = crossbeam_channel::bounded::<Vec<u8>>(QUEUE);
        // A client that stops reading is let go rather than holding its writer forever.
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
        let out = Outbox {
            #[cfg(test)]
            captured_writer: None,
            queue: tx,
            queued: Arc::new(AtomicUsize::new(0)),
            sent: AtomicU64::new(0),
            written: Arc::default(),
            socket: Arc::new(stream),
            last: Arc::default(),
        };
        let (queued, progress, socket, last) =
            (out.queued.clone(), out.written.clone(), out.socket.clone(), out.last.clone());
        crate::threads::named(format!("client-{id}-write")).spawn(move || {
            let mut w = &*socket;
            let written = rx.iter().all(|data| {
                let written = w.write_all(&data).and_then(|_| w.flush()).is_ok();
                if written {
                    // What the queue holds is one message each: whole, or what the server could
                    // not write of it.
                    wrote(&progress);
                    // Once nothing is left, the server writes again itself.
                    queued.fetch_sub(1, Ordering::AcqRel);
                }
                written
            });
            // The outbox is gone and what it queued is written: now what it left to say last.
            if written && let Some(last) = last.get() {
                let _ = w.write_all(last);
            }
            let _ = socket.shutdown(std::net::Shutdown::Both);
        })?;
        Ok(out)
    }

    /// Let the client take as long as it needs to read what waits for it: for a connection that
    /// only listens (an event stream), which is closed for how far behind it is, never for how
    /// long it takes. Cut after a while without reading, it could not be told why its stream ends.
    pub fn wait_for_reader(&self) {
        let _ = self.socket.set_write_timeout(None);
    }

    /// Close the connection once the client has read what is queued, then `last`: what a client
    /// that takes no more (its queue is full) is told before it goes.
    pub fn end_with(self, last: Vec<u8>) {
        let _ = self.last.set(last);
    }

    pub fn try_send(&self, data: Vec<u8>) -> Result<(), TrySendError<Vec<u8>>> {
        let Some(rest) = self.write_now(data) else {
            self.taken(true);
            return Ok(());
        };
        self.queued.fetch_add(1, Ordering::AcqRel);
        self.queue.try_send(rest).inspect(|_| self.taken(false)).inspect_err(|_| {
            self.queued.fetch_sub(1, Ordering::AcqRel);
        })
    }

    pub fn send_timeout(&self, data: Vec<u8>, wait: Duration) -> Result<(), SendTimeoutError<Vec<u8>>> {
        let Some(rest) = self.write_now(data) else {
            self.taken(true);
            return Ok(());
        };
        self.queued.fetch_add(1, Ordering::AcqRel);
        self.queue.send_timeout(rest, wait).inspect(|_| self.taken(false)).inspect_err(|_| {
            self.queued.fetch_sub(1, Ordering::AcqRel);
        })
    }

    fn taken(&self, written: bool) {
        self.sent.fetch_add(1, Ordering::AcqRel);
        if written {
            wrote(&self.written);
        }
    }

    /// The number of the last message taken.
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Acquire)
    }

    /// Note when message `seq` is written to the socket (now if it already is).
    pub fn watch(&self, seq: u64) {
        if let Ok(mut w) = self.written.lock() {
            w.watched = seq;
            w.at = (w.count >= seq).then(Instant::now);
        }
    }

    /// When the watched message `seq` was written to the socket, once it was.
    pub fn written_at(&self, seq: u64) -> Option<Instant> {
        self.written.lock().ok().filter(|w| w.watched == seq).and_then(|w| w.at)
    }

    /// The writer thread wrote one more message: for tests that hold what it would write.
    #[cfg(test)]
    pub(super) fn wrote_one(&self) {
        wrote(&self.written);
    }

    /// Messages waiting for the writer thread.
    pub fn len(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }

    /// Write `data` to the socket as far as it takes it now, unless something waits for the
    /// writer thread: what is left to queue. A message cut there goes on through the queue,
    /// which is empty then and has room for it.
    #[cfg(target_os = "linux")]
    fn write_now(&self, data: Vec<u8>) -> Option<Vec<u8>> {
        use std::os::fd::AsRawFd;
        if self.len() > 0 {
            return Some(data);
        }
        let flags = libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL;
        let mut done = 0;
        while done < data.len() {
            let rest = &data[done..];
            // SAFETY: sends from a live slice on a socket the outbox holds.
            let n = unsafe { libc::send(self.socket.as_raw_fd(), rest.as_ptr().cast(), rest.len(), flags) };
            match n {
                n if n > 0 => done += n as usize,
                n if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
                _ => break,
            }
        }
        match done {
            _ if done == data.len() => None,
            0 => Some(data),
            _ => Some(data[done..].to_vec()),
        }
    }

    /// Everything goes through the writer thread: macOS does not let a send be told not to wait
    /// (`MSG_DONTWAIT` waits all the same, up to the socket's timeout), and the socket itself
    /// must wait for that thread and the reader.
    #[cfg(not(target_os = "linux"))]
    fn write_now(&self, data: Vec<u8>) -> Option<Vec<u8>> {
        Some(data)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    /// Wait until `out` has nothing queued.
    fn drained(out: &Outbox) {
        let start = Instant::now();
        while out.len() > 0 {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A connection, the server's end first, with a small send buffer, as macOS gives a socket:
    /// messages are cut often.
    fn small_pair() -> (UnixStream, UnixStream) {
        use std::os::fd::AsRawFd;
        let (server_end, client_end) = UnixStream::pair().unwrap();
        let size: libc::c_int = 8 * 1024;
        // SAFETY: sets an int option on a socket the test holds.
        let set = unsafe {
            libc::setsockopt(
                server_end.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(set, 0);
        (server_end, client_end)
    }

    fn outgoing(server: &mut Server, id: ClientId) -> crossbeam_channel::Receiver<Vec<u8>> {
        server.clients.get_mut(&id).unwrap().out.capture()
    }

    #[test]
    fn fixture_clients_capture_replies_without_a_first_message_timeout() {
        use super::super::tests::{client, server};
        let (tx, rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        let (id, _peer) = client(&mut server);
        let captured = outgoing(&mut server, id);
        // No first frame is sent. In particular, ClientGone must not discard a delayed reply.
        assert!(matches!(rx.wait(FIRST_MESSAGE_TIMEOUT + Duration::from_secs(1)), super::super::queue::Wait::Timeout));
        server.send_msg(id, &ServerMsg::Response { result: Ok(crate::protocol::Response::Ok) });
        assert!(matches!(super::super::requests::tests::response(&captured), Ok(crate::protocol::Response::Ok)));
    }

    #[test]
    fn capturing_replies_keeps_the_original_writer_until_the_outbox_is_dropped() {
        let (queue, original) = crossbeam_channel::bounded(1);
        let (socket, _peer) = UnixStream::pair().unwrap();
        let mut out = Outbox {
            captured_writer: None,
            queue,
            queued: Arc::new(AtomicUsize::new(0)),
            sent: AtomicU64::new(0),
            written: Arc::default(),
            socket: Arc::new(socket),
            last: Arc::default(),
        };
        let captured = out.capture();
        assert_eq!(original.try_recv(), Err(crossbeam_channel::TryRecvError::Empty));
        out.try_send(b"reply".to_vec()).unwrap();
        assert_eq!(captured.try_recv().unwrap(), b"reply");
        assert_eq!(original.try_recv(), Err(crossbeam_channel::TryRecvError::Empty));
        drop(out);
        assert_eq!(original.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected));
        assert_eq!(captured.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected));
    }

    #[test]
    fn connection_limit_counts_commands_waits_and_streams() {
        use crate::protocol::{Request, Response};
        use crate::server::tests::{client, server};
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        let (command, _command_peer) = client(&mut server);
        assert!(matches!(server.request(command, Request::KillServer { restart: false }), RequestOutcome::Deferred));
        let (wait, _wait_peer) = client(&mut server);
        let Response::Pane(pane) = server
            .request(0, Request::OpenFiles { left: "/missing".into(), right: "/missing".into(), near: None })
            .immediate()
            .unwrap()
        else {
            panic!("files pane")
        };
        assert!(matches!(
            server.request(
                wait,
                Request::Wait { pane: pane.id.to_string(), until: vec!["exit".into()], timeout_ms: None }
            ),
            RequestOutcome::Deferred
        ));
        let (stream, _stream_peer) = client(&mut server);
        assert!(matches!(
            server.request(stream, Request::Subscribe { events: vec![], since: None, boot: None }),
            RequestOutcome::Streaming
        ));
        assert_eq!((server.clients.len(), server.waits.len(), server.subscribers.len()), (1, 1, 1));
        // Streams awaiting removal still count until the next subscriber cleanup.
        server.subscribers.extend((3..MAX_CLIENTS).map(|n| super::super::events::Subscriber {
            client: 10_000 + n as u32,
            out: None,
            filters: vec![],
        }));
        let (local, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let next = server.next_id;
        server.new_connection(local);
        let (_, payload) = protocol::read_frame(&mut peer).unwrap().unwrap();
        assert!(
            matches!(serde_json::from_slice(&payload).unwrap(), ServerMsg::Response { result: Err(e) } if e == "too many connections (256)")
        );
        assert_eq!(server.next_id, next);
        server.drop_client(wait);
        let (local, _peer) = UnixStream::pair().unwrap();
        server.new_connection(local);
        assert!(server.clients.contains_key(&next));
        assert_eq!(server.next_id, next + 1);
    }

    #[test]
    fn only_remote_attachments_advertise_a_laptop() {
        use super::super::tests::{client, server};
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        for remote in [false, true] {
            let (id, _peer) = client(&mut server);
            let message = ClientMsg::Attach {
                version: protocol::PROTOCOL_VERSION,
                cols: 80,
                rows: 24,
                workspace: None,
                remote,
                pane: None,
                installed: None,
                laptop: Some(crate::laptop::Identity {
                    id: "abc123".into(),
                    host: "travel".into(),
                    os: "macos".into(),
                }),
            };
            server.client_frame(id, protocol::JSON, serde_json::to_vec(&message).unwrap());
            assert_eq!(server.clients[&id].laptop.is_some(), remote);
        }
    }

    #[test]
    fn mismatched_attach_and_mutations_leave_the_session_unchanged() {
        use super::super::tests::{client, server};
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        for version in [2, protocol::PROTOCOL_VERSION + 1] {
            for message in [
                serde_json::json!({"type":"attach", "version":version, "cols":80, "rows":24, "workspace":"unwanted"}),
                serde_json::json!({"type":"request", "version":version, "request":{"cmd":"create_workspace", "name":"unwanted", "cwd":"/tmp"}}),
            ] {
                let (id, _peer) = client(&mut server);
                let receive = outgoing(&mut server, id);
                server.clients.get_mut(&id).unwrap().attached = false;
                let before = server.workspaces.len();
                server.client_frame(id, protocol::JSON, serde_json::to_vec(&message).unwrap());
                let frame = receive.recv_timeout(Duration::from_secs(2)).unwrap();
                let (kind, payload) = protocol::read_frame(&mut frame.as_slice()).unwrap().unwrap();
                assert_eq!(kind, protocol::JSON);
                let reason = match serde_json::from_slice(&payload).unwrap() {
                    ServerMsg::Exit { reason, .. } | ServerMsg::Response { result: Err(reason) } => reason,
                    other => panic!("{other:?}"),
                };
                assert_eq!(reason, server.version_mismatch(version));
                assert!(!server.clients.contains_key(&id));
                assert_eq!(server.workspaces.len(), before);
            }
        }
    }

    #[test]
    fn upgrade_ping_uses_the_released_envelope_and_reports_the_current_protocol() {
        use super::super::tests::{client, server};
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        let (id, _peer) = client(&mut server);
        let receive = outgoing(&mut server, id);
        server.client_frame(id, protocol::JSON, br#"{"type":"request","version":2,"request":{"cmd":"ping"}}"#.to_vec());
        let frame = receive.recv_timeout(Duration::from_secs(2)).unwrap();
        let (_, payload) = protocol::read_frame(&mut frame.as_slice()).unwrap().unwrap();
        assert!(matches!(
            serde_json::from_slice(&payload).unwrap(),
            ServerMsg::Response {
                result: Ok(crate::protocol::Response::Pong { protocol: protocol::PROTOCOL_VERSION, .. })
            }
        ));
    }

    #[test]
    fn upgrade_kill_keeps_its_reply_until_the_final_save() {
        use super::super::tests::{client, server};
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        let saved = super::super::persistence::tests::writer(&mut server, vec![Ok(())]);
        let (id, _peer) = client(&mut server);
        let receive = outgoing(&mut server, id);
        server.client_frame(
            id,
            protocol::JSON,
            br#"{"type":"request","version":2,"request":{"cmd":"kill_server"}}"#.to_vec(),
        );
        assert!(receive.is_empty());
        assert!(saved.lock().unwrap().is_empty());
        assert_eq!(server.kill_waiters.len(), 1);
        server.shutdown();
        assert_eq!(saved.lock().unwrap().len(), 1);
        assert!(matches!(super::super::requests::tests::response(&receive), Ok(crate::protocol::Response::Ok)));
        assert!(matches!(receive.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
    }

    #[test]
    fn what_a_client_does_not_read_yet_waits_for_it_in_order() {
        let (server_end, mut client_end) = small_pair();
        let out = Outbox::start(server_end, 1).unwrap();
        // Small enough to go at once (through the writer thread where a send cannot be told not
        // to wait).
        out.try_send(b"first".to_vec()).unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(out.len(), 0);
        let mut first = [0u8; 5];
        client_end.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"first");
        drained(&out);
        // Far more than the socket holds: cut where it is full, the rest queued, nothing waits.
        let messages: Vec<Vec<u8>> = (0..40u8).map(|i| vec![b'a' + i % 26; 64 * 1024 + usize::from(i)]).collect();
        let start = Instant::now();
        for m in &messages {
            out.try_send(m.clone()).unwrap();
        }
        assert!(start.elapsed() < Duration::from_millis(500), "{:?}", start.elapsed());
        assert!(out.len() > 0);
        let want: Vec<u8> = messages.concat();
        let mut got = vec![0u8; want.len()];
        client_end.read_exact(&mut got).unwrap();
        assert!(got == want, "every byte, in order");
        drained(&out);
        out.try_send(b"last".to_vec()).unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(out.len(), 0, "written at once again");
        drop(out);
        let mut rest = Vec::new();
        client_end.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"last", "and the connection closed after it");
    }

    #[test]
    fn a_client_that_takes_no_more_reads_what_was_queued_then_the_last_words() {
        let (server_end, mut client_end) = small_pair();
        let out = Outbox::start(server_end, 1).unwrap();
        // It reads nothing: its socket fills, then its queue, and it is sent no more.
        let mut taken: Vec<Vec<u8>> = Vec::new();
        loop {
            let message = vec![b'a' + (taken.len() % 26) as u8; 1000 + taken.len()];
            if out.try_send(message.clone()).is_err() {
                break;
            }
            taken.push(message);
            assert!(taken.len() < 10 * QUEUE, "never full");
        }
        assert!(taken.len() >= QUEUE, "{}", taken.len());
        out.end_with(b"the end".to_vec());
        let mut got = Vec::new();
        client_end.read_to_end(&mut got).unwrap();
        taken.push(b"the end".to_vec());
        assert!(got == taken.concat(), "every message taken, in order, then the last words, then the end");
    }

    #[test]
    fn a_reader_waited_for_takes_the_time_it_needs() {
        let (server_end, mut client_end) = small_pair();
        let out = Outbox::start(server_end, 1).unwrap();
        // A client has ten seconds to take what is written to it: far less here, and this one
        // takes longer.
        out.socket.set_write_timeout(Some(Duration::from_millis(20))).unwrap();
        out.wait_for_reader();
        let message = vec![b'x'; 256 * 1024];
        out.try_send(message.clone()).unwrap();
        out.end_with(b"the end".to_vec());
        std::thread::sleep(Duration::from_millis(200));
        let mut got = Vec::new();
        client_end.read_to_end(&mut got).unwrap();
        assert!(got == [message, b"the end".to_vec()].concat(), "cut before it had read everything");
    }
}
