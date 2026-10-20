//! Wire protocol between the `kraal` client/CLI and the session server.
//!
//! Every message is a frame: `u32` little-endian payload length, one kind byte, then the payload.
//! Kind [`JSON`] carries a serialized [`ClientMsg`] or [`ServerMsg`]; kind [`BYTES`] carries raw
//! terminal bytes (keyboard input from the client, screen updates from the server). A frame of
//! another kind is skipped.
//!
//! Peers must use the same protocol version. Only the upgrade control requests retain
//! protocol 2 to inspect, save and stop 0.2.0 and 0.3.0 sessions.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent::AgentState;

pub const PROTOCOL_VERSION: u32 = 6;
pub const KRAAL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Kraal 0.2.0 and 0.3.0 used protocol 2 and omitted it from Pong.
pub const fn upgrade_protocol() -> u32 {
    2
}

pub fn version_mismatch(client: u32, server: u32, release: &str, session: &str, host: Option<&str>) -> String {
    let fix = if client < server {
        "update this machine with `kraal update`".to_string()
    } else if let Some(host) = host {
        let host = crate::shell::quote(host);
        if session == "main" {
            format!("run `kraal remote-install {host}`")
        } else {
            format!("run `kraal -s {session} remote-install {host}`")
        }
    } else {
        format!("run `kraal -s {session} restart`")
    };
    format!(
        "protocol mismatch: client protocol {client}, server protocol {server} (kraal {release}, session {session}); {fix}"
    )
}

/// How a server begins its answer to a request or message it does not know or cannot read.
/// Servers older than this prefix answer `bad message: …`.
pub const UNSUPPORTED: &str = "unsupported request";

/// The answer to `msg` (a request or client message, as read), which this server does not know.
pub fn unsupported(msg: &serde_json::Value) -> String {
    let name = msg.get("cmd").or_else(|| msg.get("type")).and_then(|n| n.as_str()).unwrap_or("?");
    format!("{UNSUPPORTED} `{name}`: kraal {KRAAL_VERSION} does not know it")
}

/// Whether release `a` (`0.2.1`, `v0.3.0-rc.1`) comes after release `b`, as semver orders them:
/// a pre-release comes before its release. A version that cannot be read is never newer.
pub fn newer(a: &str, b: &str) -> bool {
    use std::cmp::Ordering;
    fn parse(v: &str) -> Option<([u64; 3], Vec<&str>)> {
        let v = v.trim();
        let v = v.strip_prefix('v').unwrap_or(v);
        let v = v.split('+').next().unwrap_or(v);
        let (core, pre) = match v.split_once('-') {
            Some((core, pre)) => (core, pre.split('.').collect()),
            None => (v, Vec::new()),
        };
        let mut nums = [0u64; 3];
        let parts: Vec<&str> = core.split('.').collect();
        if parts.len() > 3 {
            return None;
        }
        for (n, p) in nums.iter_mut().zip(parts) {
            *n = p.parse().ok()?;
        }
        Some((nums, pre))
    }
    let (Some((a_nums, a_pre)), Some((b_nums, b_pre))) = (parse(a), parse(b)) else { return false };
    let pre = || {
        if a_pre.is_empty() || b_pre.is_empty() {
            // The release comes after its pre-releases.
            return b_pre.len().min(1).cmp(&a_pre.len().min(1));
        }
        for (x, y) in a_pre.iter().zip(&b_pre) {
            let order = match (x.parse::<u64>(), y.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => x.cmp(y),
            };
            if order != Ordering::Equal {
                return order;
            }
        }
        a_pre.len().cmp(&b_pre.len())
    };
    a_nums.cmp(&b_nums).then_with(pre) == Ordering::Greater
}

pub const JSON: u8 = 0;
pub const BYTES: u8 = 1;

const MAX_FRAME: usize = 64 << 20;
pub const LAPTOP_CHUNK: usize = 4096;
pub const LAPTOP_LIST_LIMIT: usize = 64 << 20;

pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    // The other side would refuse it: say so here instead of sending it.
    if payload.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
    }
    let mut header = [0u8; 5];
    header[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    header[4] = kind;
    // A small frame (a key) goes in one write: the reader is not woken by a header alone.
    if payload.len() <= 4096 {
        let mut frame = Vec::with_capacity(header.len() + payload.len());
        frame.extend_from_slice(&header);
        frame.extend_from_slice(payload);
        w.write_all(&frame)?;
    } else {
        w.write_all(&header)?;
        w.write_all(payload)?;
    }
    w.flush()
}

pub fn write_json<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let payload = serde_json::to_vec(msg).map_err(io::Error::other)?;
    write_frame(w, JSON, &payload)
}

/// Read one frame. `Ok(None)` on a clean EOF between frames.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    match r.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some((header[4], payload)))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    LaptopCopy {
        token: String,
        message: crate::laptop::copy::FromClient,
    },
    /// Start an interactive session; raw input frames follow.
    Attach {
        version: u32,
        cols: u16,
        rows: u16,
        /// Workspace to show first (opened from its definition if needed).
        workspace: Option<String>,
        /// The client runs on another machine than the server (attached over ssh).
        #[serde(default)]
        remote: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        laptop: Option<crate::laptop::Identity>,
        /// A pane to show first (an id or a name), in place of the workspace.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
        /// The release of the kraal installed on the server's machine, as the client knows it
        /// (a local client is that binary): the server says when it is newer than its own.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        installed: Option<String>,
    },
    LaptopListChunk {
        pane: crate::layout::PaneId,
        side: crate::files::Side,
        request: u64,
        data: Vec<u8>,
        done: bool,
    },
    LaptopCaseSensitive {
        pane: crate::layout::PaneId,
        side: crate::files::Side,
        request: u64,
        case_sensitive: bool,
    },
    LaptopListed {
        pane: crate::layout::PaneId,
        side: crate::files::Side,
        request: u64,
        result: crate::laptop::Listing,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Restore terminal content without taking layout control.
    Repaint,
    /// The frames before [`ServerMsg::DisplayMark`] `id` are on the terminal.
    Displayed {
        id: u32,
    },
    Detach,
    /// Reserve the destination while the client classifies a buffered paste.
    PasteStart,
    /// A client clipboard read bound to the agent that received the shortcut.
    ClipboardPaste {
        pane: crate::layout::PaneId,
        key: Vec<u8>,
        text: Vec<u8>,
        token: Option<String>,
    },
    AttachmentRequest {
        token: String,
        /// Paste content, or the original shortcut key when `image` and `pane` are set.
        text: Vec<u8>,
        #[serde(default)]
        image: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<crate::layout::PaneId>,
    },
    AttachmentText {
        token: String,
        text: Vec<u8>,
    },
    AttachmentStart {
        token: String,
        files: Vec<crate::attachments::FileSpec>,
    },
    AttachmentChunk {
        token: String,
        file: usize,
        offset: u64,
        data: String,
    },
    AttachmentFinish {
        token: String,
    },
    AttachmentCancel {
        token: String,
    },
    AttachmentStatus {
        token: String,
    },
    AttachmentDismiss {
        token: String,
    },
    AttachmentLocal {
        token: String,
        paths: Vec<String>,
    },
    ClipboardResult {
        id: u64,
        outcome: ClipboardOutcome,
        #[serde(default)]
        native_unavailable: bool,
    },
    /// One control request; the server answers with [`ServerMsg::Response`] and closes.
    Request {
        version: u32,
        request: Request,
        /// The pane the asking command runs in, as its environment says.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<Origin>,
    },
    /// A message from a newer client, or one this kraal cannot read (only ever read).
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

impl ClientMsg {
    /// `request`, as this kraal sends it: from the pane it runs in, if any.
    pub fn request(request: Request) -> ClientMsg {
        let origin = match (std::env::var("KRAAL_SESSION"), std::env::var("KRAAL_PANE")) {
            (Ok(session), Ok(pane)) => Some(Origin { session, pane }),
            _ => None,
        };
        ClientMsg::Request { version: PROTOCOL_VERSION, request, origin }
    }
}

/// A kraal pane, named by its session and its id (`KRAAL_SESSION`, `KRAAL_PANE`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    pub session: String,
    pub pane: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    LaptopCopy {
        token: String,
        message: crate::laptop::copy::ToClient,
    },
    LaptopListAck {
        pane: crate::layout::PaneId,
        side: crate::files::Side,
        request: u64,
    },
    LaptopList {
        pane: crate::layout::PaneId,
        side: crate::files::Side,
        request: u64,
        path: String,
    },
    /// The interactive session ended (detach, server shutdown, …).
    Exit {
        reason: String,
        /// The server restarts (`kraal restart`): the workspace the client showed, to attach to
        /// again once the next server listens.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reattach: Option<String>,
    },
    /// The first answer to an accepted attach.
    Attached {
        version: String,
        protocol: u32,
    },
    Response {
        result: Result<Response, String>,
    },
    ClipboardFocus {
        /// The focused pane, only when the client mode allows a clipboard shortcut.
        pane: Option<crate::layout::PaneId>,
        agent: bool,
    },
    /// The frames before it show a question, and `id` is new for each time they do: answer
    /// [`ClientMsg::Displayed`] once they are on the terminal.
    DisplayMark {
        id: u32,
    },
    AttachmentPrepared {
        token: String,
        target: String,
        limits: crate::attachments::Settings,
        broadcast: bool,
    },
    ClipboardImageOffer {
        token: String,
        target: String,
        limits: crate::attachments::Settings,
    },
    AttachmentOffer {
        token: String,
        target: String,
        limits: crate::attachments::Settings,
    },
    AttachmentAck {
        token: String,
        received: u64,
    },
    AttachmentResult {
        #[serde(default)]
        kind: crate::theme::NoticeKind,
        token: String,
        message: String,
        #[serde(default)]
        pasted: bool,
    },
    Clipboard {
        id: u64,
        origin: ClipboardOrigin,
        text: String,
    },
    /// Open a URL on the client's machine (Ctrl+click on a link).
    Open {
        url: String,
    },
    /// Something happened in the session (to `kraal events`).
    Event {
        event: serde_json::Value,
    },
    /// Events that happened before the subscription (`--since`), several to a frame.
    Events {
        events: Vec<serde_json::Value>,
    },
    /// A message from a newer server, or one this kraal cannot read (only ever read): skipped.
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

impl ServerMsg {
    /// The end of an interactive session that does not come back by itself.
    pub fn exit(reason: impl Into<String>) -> ServerMsg {
        ServerMsg::Exit { reason: reason.into(), reattach: None }
    }
}

/// Unknown origins never gain permission to write the native clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardOrigin {
    User,
    Program,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardOutcome {
    Native,
    NativeStarted,
    Terminal,
    Disabled,
    TooLarge,
    Busy,
    Failed,
    #[serde(other)]
    Unknown,
}

impl ClipboardOutcome {
    pub fn message(self) -> &'static str {
        match self {
            Self::Native => "copied to laptop clipboard",
            Self::NativeStarted => "copy handed to native clipboard owner",
            Self::Terminal => "copy sent to terminal via OSC 52",
            Self::Disabled => "external copy disabled; use paste buffers",
            Self::TooLarge => "copy too large; raise clipboard.max_bytes and attach again",
            Self::Busy => "copy busy: try again",
            Self::Failed | Self::Unknown => "copy not completed: try again",
        }
    }
}

/// Where a new pane goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    Right,
    Down,
    Floating,
    /// A new tab in the target workspace.
    Tab,
    /// Split the target pane along its longer side. Also a placement this kraal does not know.
    #[default]
    #[serde(other)]
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Ping,
    ListPanes,
    /// The panes running an agent, then the agents of the servers' sessions watched (those
    /// have `session`).
    ListAgents,
    ListWorkspaces,
    ListDeletedWorkspaces,
    CreateWorkspace {
        name: String,
        cwd: PathBuf,
    },
    RenameWorkspace {
        workspace: String,
        name: String,
    },
    CloseWorkspace {
        workspace: String,
        expected: Option<String>,
    },
    DeleteWorkspace {
        workspace: String,
        expected: Option<String>,
    },
    RestoreWorkspace {
        workspace: String,
        name: Option<String>,
    },
    OpenWorkspace {
        name: String,
    },
    /// `pane` refs are an id (`"12"`), a pane name, or `"current"` (the caller's `$KRAAL_PANE`).
    NewPane {
        target: Option<String>,
        command: Vec<String>,
        cwd: Option<PathBuf>,
        name: Option<String>,
        placement: Placement,
        preview: Option<PathBuf>,
        focus: bool,
        /// Close the pane when its program exits (default: keep it, showing the exit code).
        #[serde(default)]
        close_on_exit: bool,
    },
    ClosePane {
        pane: String,
    },
    FocusPane {
        pane: String,
    },
    RenamePane {
        pane: String,
        name: String,
    },
    /// Replace a pane's program with `command`: tmux's `respawn-pane -k`, with which Claude's
    /// agent teams start a teammate in the pane they split (see `kraal agent --teams`).
    RespawnPane {
        pane: String,
        command: Vec<String>,
    },
    /// Type into a pane. A production pane is refused unless `production` says the caller
    /// knows (scripts and agents must not reach production by accident).
    SendText {
        pane: String,
        text: String,
        #[serde(default)]
        production: bool,
    },
    SendKeys {
        pane: String,
        keys: Vec<String>,
        #[serde(default)]
        production: bool,
    },
    ReadPane {
        pane: String,
        /// Number of trailing lines, scrollback included; `None` = visible screen.
        lines: Option<usize>,
    },
    /// Block until the pane matches one of `until` (`working`, `blocked`, `done`, `idle`,
    /// `exit`, `quiet`) or the timeout expires.
    Wait {
        pane: String,
        until: Vec<String>,
        timeout_ms: Option<u64>,
    },
    /// Sent by agent hooks running inside a pane.
    AgentReport {
        pane: String,
        agent: String,
        /// A state this kraal does not know (from a newer hook) reads as none: it changes
        /// nothing, where `unknown` would end the agent's session.
        #[serde(default, deserialize_with = "known_state")]
        state: Option<AgentState>,
        session_id: Option<String>,
        message: Option<String>,
        /// The hook is the agent taking a prompt (the user's, sent): a draft went with it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        prompt: bool,
    },
    Notify {
        title: String,
        body: String,
    },
    /// Prepare a step runbook: the full plan to review, and a token to run exactly that plan
    /// (the file as read now, the targets and parameters resolved now). `params` are
    /// `NAME=value`; the others take their default. `pane` gives the context.
    RunbookPlan {
        path: PathBuf,
        target: Option<String>,
        params: Vec<(String, String)>,
        pane: Option<String>,
    },
    /// Run the plan a [`Request::RunbookPlan`] prepared; a token works once, for 10 minutes.
    RunbookRun {
        token: String,
    },
    /// Stream events (`pane.exited`, `agent.*`…; all when empty) until the client goes away,
    /// after those numbered above `since` that the server still has.
    Subscribe {
        events: Vec<String>,
        #[serde(default)]
        since: Option<u64>,
        /// The `boot` of the server the cursor `since` comes from.
        #[serde(default)]
        boot: Option<u64>,
    },
    /// Open an SSH pane to a server (name or `user@host[:port]`), or a whole group in a grid.
    OpenSsh {
        target: String,
        group: bool,
        placement: Placement,
        /// Pane to split (its workspace receives the new pane).
        near: Option<String>,
        /// A remote command to run instead of a shell (e.g. `journalctl -fu nginx`).
        #[serde(default)]
        command: Vec<String>,
    },
    /// Start an agent (claude, codex…) in a new pane, on `account` or the one the rules pick.
    OpenAgent {
        agent: String,
        account: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        placement: Placement,
        near: Option<String>,
        cwd: Option<PathBuf>,
        /// Given to the agent after kraal's own arguments.
        #[serde(default)]
        args: Vec<String>,
        /// Claude Code's agent teams open their teammates as kraal panes (`crate::tmux`).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        teams: bool,
    },
    /// Forward `remote` (`[host:]port` as seen from the server) to a local port, and keep it up.
    TunnelAdd {
        server: String,
        remote: String,
        local: Option<u16>,
    },
    /// Close a tunnel by its local port.
    TunnelRemove {
        key: String,
    },
    TunnelList,
    /// Open a two-panel file manager on two locations (`/path` or `server:path`).
    OpenFiles {
        left: String,
        right: String,
        near: Option<String>,
    },
    /// Edit a server's file (`server:path`) in the local editor, in a floating pane; each save
    /// goes back to the server. A production server is refused unless `production` says the
    /// caller knows. With `sudo`, as root through sudo there (a pane asks for the password).
    /// Answered once the editor is open.
    EditRemote {
        target: String,
        line: Option<u32>,
        near: Option<String>,
        #[serde(default)]
        production: bool,
        #[serde(default)]
        sudo: bool,
    },
    ImportWorkspace {
        path: PathBuf,
        name: Option<String>,
        expected: Option<crate::paths::FileRevision>,
        #[serde(default)]
        accounts: Vec<(String, String)>,
    },
    ExportWorkspace {
        workspace: Option<String>,
        #[serde(default)]
        identity: Option<String>,
        path: PathBuf,
        expected: Option<crate::paths::FileRevision>,
    },
    /// Publish a batch import while the server holds workspace ownership stable.
    PublishWorkspaceFile {
        path: PathBuf,
        source: String,
        expected: crate::paths::FileRevision,
    },
    /// The paste buffers, newest first.
    BufferList,
    /// A buffer's text: by number (0 is the newest numbered buffer) or name; the newest without.
    BufferShow {
        buffer: Option<String>,
    },
    /// Keep `text` in the buffer `name`, or in a new numbered buffer without one.
    BufferSet {
        name: Option<String>,
        text: String,
    },
    /// Paste a buffer (the newest without one) into a pane as a terminal pastes: bracketed when
    /// the program asked for it. A production pane is refused unless `production` says the
    /// caller knows.
    BufferPaste {
        buffer: Option<String>,
        pane: String,
        #[serde(default)]
        production: bool,
    },
    /// Delete a buffer (the newest without one), or all of them.
    BufferDelete {
        buffer: Option<String>,
        #[serde(default)]
        all: bool,
    },
    /// A command about to act on production servers from the caller's side (`kraal exec`, `cp`,
    /// `rm`, `ssh`), or on another session's: answered at once, unless it runs in an agent's
    /// pane, where the user is asked first. Refused with an error that says nothing was done.
    ApproveProduction {
        kind: String,
        plan: String,
        servers: Vec<String>,
    },
    /// Read the configuration file again; answered with its warnings, one per line.
    ReloadConfig,
    /// Save the session and stop. With `restart`, attached clients wait for the next server
    /// (older servers read it as a plain stop).
    KillServer {
        #[serde(default)]
        restart: bool,
    },
    /// A request from a newer client, or one this kraal cannot read (only ever read): refused
    /// with [`UNSUPPORTED`].
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

/// A reported agent state, `None` for one this kraal does not know.
fn known_state<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<AgentState>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(v.as_ref().and_then(|v| v.as_str()).and_then(AgentState::parse))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Pong {
        pid: u32,
        session: String,
        version: String,
        #[serde(default = "upgrade_protocol")]
        protocol: u32,
    },
    Pane(Box<PaneInfo>),
    Panes(Vec<PaneInfo>),
    Workspaces(Vec<WorkspaceInfo>),
    Text(String),
    /// A runbook's plan, and the token that runs it.
    Plan {
        text: String,
        token: String,
    },
    Path(PathBuf),
    WorkspacePreview {
        workspace: String,
        revision: String,
        question: Option<String>,
    },
    ImportPreview(crate::workspace::ImportPreview),
    ExportPreview {
        identity: String,
        path: PathBuf,
        revision: crate::paths::FileRevision,
        question: Option<String>,
    },
    Tunnels(Vec<crate::server::net::TunnelInfo>),
    Buffers(Vec<BufferInfo>),
    /// An answer from a newer server, or one this kraal cannot read (only ever read).
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

/// A paste buffer, as `kraal buffer list` shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferInfo {
    /// What `kraal buffer` commands take: the number of a numbered buffer (0 is the newest), or
    /// the name of a named one.
    pub buffer: String,
    pub named: bool,
    pub bytes: usize,
    pub lines: usize,
    /// When it was copied or set, in seconds since the epoch.
    pub at: i64,
    /// Its text on one line, cut short, control characters made visible.
    pub preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: u32,
    pub name: Option<String>,
    pub title: String,
    pub workspace: String,
    pub tab: String,
    pub kind: String,
    pub command: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    pub state: AgentState,
    pub exit_code: Option<i32>,
    /// The command waits for Enter (restored session, start_suspended, or it could not start).
    #[serde(default)]
    pub suspended: bool,
    /// Kitty keyboard protocol flags the program enabled (0 = legacy keys).
    #[serde(default)]
    pub keyboard: u8,
    /// The server the pane is connected to.
    #[serde(default)]
    pub server: Option<String>,
    /// For an agent of a kraal session on a server (`server`): that session. `id` is then its
    /// pane's id there, and `kind` is `remote`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Why the pane is production (`server web-prod-1`), if it is.
    #[serde(default)]
    pub production: Option<String>,
    /// Kubernetes context, Terraform workspace and AWS profile labels (`tf:prod`).
    #[serde(default)]
    pub contexts: Vec<String>,
    pub focused: bool,
    pub floating: bool,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub open: bool,
    pub definition: Option<PathBuf>,
    pub tabs: usize,
    pub panes: usize,
    pub working: usize,
    pub blocked: usize,
    pub done: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_removed_save_request_is_not_an_export_alias() {
        let request: Request =
            serde_json::from_str(r#"{"cmd":"save_workspace","workspace":"w","name":"copy"}"#).unwrap();
        let Request::Unknown(value) = request else { panic!("legacy save was accepted") };
        assert!(unsupported(&value).contains("`save_workspace`"));
    }

    #[test]
    fn mismatches_name_both_protocols_and_one_fix() {
        for (client, server, host, fix) in [
            (3, 2, None, "run `kraal -s work restart`"),
            (3, 2, Some("box"), "run `kraal -s work remote-install box`"),
            (2, 3, None, "update this machine with `kraal update`"),
            (2, 3, Some("box"), "update this machine with `kraal update`"),
        ] {
            assert_eq!(
                version_mismatch(client, server, "0.4.0", "work", host),
                format!(
                    "protocol mismatch: client protocol {client}, server protocol {server} (kraal 0.4.0, session work); {fix}"
                )
            );
        }
    }

    #[test]
    fn workspace_lifecycle_requests_default_to_preview() {
        for json in [
            r#"{"cmd":"list_deleted_workspaces"}"#,
            r#"{"cmd":"create_workspace","name":"api","cwd":"/tmp"}"#,
            r#"{"cmd":"rename_workspace","workspace":"api","name":"backend"}"#,
            r#"{"cmd":"close_workspace","workspace":"api"}"#,
            r#"{"cmd":"delete_workspace","workspace":"api"}"#,
            r#"{"cmd":"restore_workspace","workspace":"api"}"#,
        ] {
            let request: Request = serde_json::from_str(json).unwrap();
            let copy: Request = serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
            assert_eq!(serde_json::to_value(copy).unwrap(), serde_json::to_value(&request).unwrap());
            match request {
                Request::CloseWorkspace { expected, .. } | Request::DeleteWorkspace { expected, .. } => {
                    assert!(expected.is_none())
                }
                _ => {}
            }
        }
    }

    #[test]
    fn releases_compare_as_semver() {
        assert!(newer("0.2.1", "0.2.0"));
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("v1.0.0", "0.99.0"));
        assert!(!newer("0.2.0", "0.2.0"));
        assert!(!newer("0.2.0", "0.2.1"));
        assert!(newer("0.3.0", "0.3.0-rc.1"));
        assert!(!newer("0.3.0-rc.1", "0.3.0"));
        assert!(newer("0.3.0-rc.10", "0.3.0-rc.9"));
        assert!(newer("0.3.0-rc.1", "0.3.0-beta.2"));
        assert!(newer("0.3.0-rc.1.1", "0.3.0-rc.1"));
        assert!(newer("0.2.1+build.5", "0.2.0"));
        assert!(newer("0.3", "0.2.9"));
        assert!(!newer("garbage", "0.2.0") && !newer("0.2.0", "garbage") && !newer("", ""));
        assert!(!newer(KRAAL_VERSION, KRAAL_VERSION));
    }

    /// What a server of the oldest supported release (0.2.0) sends, as it sends it.
    #[test]
    fn answers_of_the_oldest_supported_release_read() {
        let pong = r#"{"type":"response","result":{"Ok":{"kind":"pong","data":{"pid":7,"session":"main","version":"0.2.0"}}}}"#;
        let Ok(ServerMsg::Response { result: Ok(Response::Pong { pid: 7, protocol: 2, .. }) }) =
            serde_json::from_str::<ServerMsg>(pong)
        else {
            panic!("{pong}")
        };
        let pane = r#"{"id":3,"name":null,"title":"sh","workspace":"w","tab":"t","kind":"shell","command":[],"cwd":null,"agent":"claude","state":"working","exit_code":null,"focused":true,"floating":false,"cols":80,"rows":24}"#;
        let panes = format!(r#"{{"type":"response","result":{{"Ok":{{"kind":"panes","data":[{pane}]}}}}}}"#);
        let Ok(ServerMsg::Response { result: Ok(Response::Panes(list)) }) = serde_json::from_str::<ServerMsg>(&panes)
        else {
            panic!("{panes}")
        };
        assert_eq!((list[0].id, list[0].state, list[0].session.as_deref()), (3, AgentState::Working, None));
        let refused = r#"{"type":"response","result":{"Err":"bad message: unknown variant `buffer_list`, expected one of `ping`"}}"#;
        let Ok(ServerMsg::Response { result: Err(e) }) = serde_json::from_str::<ServerMsg>(refused) else {
            panic!("{refused}")
        };
        assert!(e.starts_with("bad message:"));
        // And what such a client sends.
        let request = r#"{"type":"request","version":2,"request":{"cmd":"list_panes"}}"#;
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(request),
            Ok(ClientMsg::Request { request: Request::ListPanes, .. })
        ));
    }

    #[test]
    fn what_this_kraal_does_not_know_is_refused_by_name() {
        let from_newer = r#"{"type":"request","version":3,"request":{"cmd":"teleport","to":"mars"}}"#;
        let Ok(ClientMsg::Request { request: Request::Unknown(request), .. }) =
            serde_json::from_str::<ClientMsg>(from_newer)
        else {
            panic!("{from_newer}")
        };
        let answer = unsupported(&request);
        assert!(answer.starts_with(UNSUPPORTED) && answer.contains("`teleport`"), "{answer}");
        let Ok(ClientMsg::Unknown(msg)) = serde_json::from_str::<ClientMsg>(r#"{"type":"zoom","factor":2}"#) else {
            panic!()
        };
        assert!(unsupported(&msg).contains("`zoom`"));
    }

    #[test]
    fn a_hook_state_this_kraal_does_not_know_changes_nothing() {
        let report = |state: &str| {
            let json = format!(r#"{{"cmd":"agent_report","pane":"1","agent":"claude","state":{state}}}"#);
            match serde_json::from_str::<Request>(&json) {
                Ok(Request::AgentReport { state, .. }) => state,
                other => panic!("{json}: {other:?}"),
            }
        };
        assert_eq!(report(r#""compacting""#), None);
        assert_eq!(report("null"), None);
        assert_eq!(report(r#""unknown""#), Some(AgentState::Unknown), "the agent's session ended");
        assert_eq!(report(r#""blocked""#), Some(AgentState::Blocked));
        // In an answer, it is shown as unknown.
        assert_eq!(serde_json::from_str::<AgentState>(r#""compacting""#).unwrap(), AgentState::Unknown);
        assert_eq!(serde_json::to_string(&AgentState::Unknown).unwrap(), r#""unknown""#);
    }

    /// One message of each kind, both ways.
    fn samples() -> (Vec<ClientMsg>, Vec<ServerMsg>) {
        let s = |v: &str| v.to_string();
        let p = |v: &str| PathBuf::from(v);
        let requests = vec![
            Request::Ping,
            Request::ListPanes,
            Request::ListAgents,
            Request::ListWorkspaces,
            Request::OpenWorkspace { name: s("w") },
            Request::NewPane {
                target: Some(s("1")),
                command: vec![s("sh")],
                cwd: Some(p("/tmp")),
                name: Some(s("n")),
                placement: Placement::Right,
                preview: Some(p("/tmp/a.md")),
                focus: true,
                close_on_exit: true,
            },
            Request::ClosePane { pane: s("1") },
            Request::FocusPane { pane: s("1") },
            Request::RenamePane { pane: s("1"), name: s("n") },
            Request::RespawnPane { pane: s("1"), command: vec![s("sh"), s("-c"), s("claude")] },
            Request::SendText { pane: s("1"), text: s("hi"), production: true },
            Request::SendKeys { pane: s("1"), keys: vec![s("ctrl+c")], production: false },
            Request::ReadPane { pane: s("1"), lines: Some(5) },
            Request::Wait { pane: s("1"), until: vec![s("done")], timeout_ms: Some(10) },
            Request::AgentReport {
                pane: s("1"),
                agent: s("claude"),
                state: Some(AgentState::Working),
                session_id: Some(s("x")),
                message: Some(s("m")),
                prompt: true,
            },
            Request::Notify { title: s("t"), body: s("b") },
            Request::RunbookPlan {
                path: p("/r.toml"),
                target: Some(s("web")),
                params: vec![(s("A"), s("1"))],
                pane: Some(s("1")),
            },
            Request::RunbookRun { token: s("t") },
            Request::Subscribe { events: vec![s("pane.*")], since: Some(3), boot: Some(4) },
            Request::OpenSsh {
                target: s("web"),
                group: false,
                placement: Placement::Tab,
                near: Some(s("1")),
                command: vec![s("top")],
            },
            Request::OpenAgent {
                agent: s("claude"),
                account: Some(s("a")),
                account_id: None,
                placement: Placement::Floating,
                near: None,
                cwd: Some(p("/")),
                args: vec![s("-c")],
                teams: true,
            },
            Request::TunnelAdd { server: s("web"), remote: s("80"), local: Some(8080) },
            Request::TunnelRemove { key: s("8080") },
            Request::TunnelList,
            Request::OpenFiles { left: s("/"), right: s("web:/"), near: None },
            Request::EditRemote { target: s("web:/a"), line: Some(3), near: None, production: true, sudo: true },
            Request::BufferList,
            Request::BufferShow { buffer: Some(s("0")) },
            Request::BufferSet { name: Some(s("n")), text: s("t") },
            Request::BufferPaste { buffer: None, pane: s("1"), production: false },
            Request::BufferDelete { buffer: Some(s("n")), all: false },
            Request::ApproveProduction {
                kind: s("exec"),
                plan: s("kraal exec web -- uptime"),
                servers: vec![s("web")],
            },
            Request::ReloadConfig,
            Request::KillServer { restart: true },
        ];
        let mut client: Vec<ClientMsg> = requests.into_iter().map(ClientMsg::request).collect();
        client.extend([
            ClientMsg::Attach {
                version: PROTOCOL_VERSION,
                cols: 80,
                rows: 24,
                workspace: Some(s("w")),
                remote: true,
                laptop: Some(crate::laptop::Identity { id: s("abc123"), host: s("travel"), os: s("macos") }),
                pane: Some(s("3")),
                installed: Some(s("0.2.0")),
            },
            ClientMsg::LaptopListChunk {
                pane: 3,
                side: crate::files::Side::Left,
                request: 7,
                data: b"{}".to_vec(),
                done: true,
            },
            ClientMsg::LaptopListed {
                pane: 3,
                side: crate::files::Side::Left,
                request: 7,
                result: Ok((s("~"), Vec::new())),
            },
            ClientMsg::LaptopListed {
                pane: 3,
                side: crate::files::Side::Right,
                request: 8,
                result: Err(s("refused: nothing was listed")),
            },
            ClientMsg::Resize { cols: 80, rows: 24 },
            ClientMsg::Repaint,
            ClientMsg::Displayed { id: 4 },
            ClientMsg::Detach,
            ClientMsg::PasteStart,
            ClientMsg::ClipboardPaste { pane: 7, key: vec![0x16], text: b"text".to_vec(), token: None },
            ClientMsg::ClipboardPaste { pane: 7, key: vec![0x16], text: b"/tmp/a".to_vec(), token: Some(s("ab")) },
            ClientMsg::AttachmentRequest { token: s("ab"), text: b"/tmp/a".to_vec(), image: false, pane: None },
            ClientMsg::AttachmentRequest { token: s("cd"), text: Vec::new(), image: true, pane: Some(7) },
            ClientMsg::AttachmentText { token: s("ab"), text: b"/tmp/a".to_vec() },
            ClientMsg::AttachmentStart {
                token: s("ab"),
                files: vec![crate::attachments::FileSpec { name: s("notes.txt"), size: 3 }],
            },
            ClientMsg::AttachmentChunk { token: s("ab"), file: 0, offset: 0, data: s("YWJj") },
            ClientMsg::AttachmentFinish { token: s("ab") },
            ClientMsg::AttachmentCancel { token: s("ab") },
            ClientMsg::AttachmentStatus { token: s("ab") },
            ClientMsg::AttachmentDismiss { token: s("ab") },
            ClientMsg::AttachmentLocal { token: s("ab"), paths: vec![s("/tmp/notes.txt")] },
            ClientMsg::ClipboardResult { id: 1, outcome: ClipboardOutcome::Terminal, native_unavailable: true },
        ]);
        let pane = PaneInfo {
            account: None,
            account_id: None,
            id: 1,
            name: Some(s("n")),
            title: s("t"),
            workspace: s("w"),
            tab: s("t"),
            kind: s("shell"),
            command: vec![s("sh")],
            cwd: Some(p("/")),
            agent: Some(s("claude")),
            state: AgentState::Blocked,
            exit_code: Some(1),
            suspended: true,
            keyboard: 1,
            server: Some(s("web")),
            session: Some(s("main")),
            production: Some(s("server web")),
            contexts: vec![s("tf:prod")],
            focused: true,
            floating: false,
            cols: 80,
            rows: 24,
        };
        let tunnel = crate::server::net::TunnelInfo {
            id: 1,
            spec: crate::servers::TunnelSpec {
                server: s("web"),
                via: Some(crate::servers::Server { name: s("web"), host: s("10.0.0.1"), ..Default::default() }),
                local: 8080,
                remote_host: s("localhost"),
                remote_port: 80,
            },
            state: crate::server::net::TunnelState::Connecting,
            error: Some(s("e")),
        };
        let answers = vec![
            Response::Ok,
            Response::Pong { pid: 1, session: s("main"), version: s(KRAAL_VERSION), protocol: PROTOCOL_VERSION },
            Response::Pane(Box::new(pane.clone())),
            Response::Panes(vec![pane]),
            Response::Workspaces(vec![WorkspaceInfo {
                id: Some(s("workspace-id")),
                name: s("w"),
                open: true,
                definition: Some(p("/w.kdl")),
                tabs: 1,
                panes: 2,
                working: 1,
                blocked: 0,
                done: 0,
            }]),
            Response::Text(s("t")),
            Response::Plan { text: s("t"), token: s("k") },
            Response::Path(p("/p")),
            Response::Tunnels(vec![tunnel]),
            Response::Buffers(vec![BufferInfo {
                buffer: s("0"),
                named: false,
                bytes: 2,
                lines: 1,
                at: 5,
                preview: s("hi"),
            }]),
        ];
        let mut server: Vec<ServerMsg> = answers.into_iter().map(|a| ServerMsg::Response { result: Ok(a) }).collect();
        server.extend([
            ServerMsg::Response { result: Err(s("no")) },
            ServerMsg::Exit { reason: s("bye"), reattach: Some(s("w")) },
            ServerMsg::LaptopListAck { pane: 3, side: crate::files::Side::Left, request: 7 },
            ServerMsg::LaptopList { pane: 3, side: crate::files::Side::Left, request: 7, path: s("~") },
            ServerMsg::Attached { version: s(KRAAL_VERSION), protocol: PROTOCOL_VERSION },
            ServerMsg::Open { url: s("https://example.com") },
            ServerMsg::ClipboardFocus { pane: Some(7), agent: true },
            ServerMsg::ClipboardFocus { pane: None, agent: false },
            ServerMsg::DisplayMark { id: 4 },
            ServerMsg::Clipboard { id: 1, origin: ClipboardOrigin::User, text: s("hello") },
            ServerMsg::AttachmentPrepared {
                token: s("ab"),
                target: s("main"),
                limits: Default::default(),
                broadcast: true,
            },
            ServerMsg::ClipboardImageOffer { token: s("ab"), target: s("main"), limits: Default::default() },
            ServerMsg::AttachmentOffer { token: s("ab"), target: s("main"), limits: Default::default() },
            ServerMsg::AttachmentAck { token: s("ab"), received: 3 },
            ServerMsg::AttachmentResult {
                kind: crate::theme::NoticeKind::Notice,
                pasted: true,
                token: s("ab"),
                message: s("Files received."),
            },
            ServerMsg::Event { event: serde_json::json!({"v": 1, "type": "pane.opened"}) },
            ServerMsg::Events { events: vec![serde_json::json!({"v": 1, "type": "pane.closed"})] },
        ]);
        use crate::laptop::copy::{Entry, FromClient, Kind, Packet, Spec, ToClient};
        client.extend([
            ClientMsg::LaptopCaseSensitive {
                pane: 3,
                side: crate::files::Side::Right,
                request: 7,
                case_sensitive: false,
            },
            ClientMsg::LaptopCopy { token: s("abc"), message: FromClient::Open { version: PROTOCOL_VERSION } },
            ClientMsg::LaptopCopy { token: s("abc"), message: FromClient::Ack(3) },
            ClientMsg::LaptopCopy { token: s("abc"), message: FromClient::Finish(Ok(())) },
            ClientMsg::LaptopCopy { token: s("abc"), message: FromClient::Finish(Err(s("cancelled"))) },
        ]);
        server.extend([
            ServerMsg::LaptopCopy {
                token: s("abc"),
                message: ToClient::Ready(Spec {
                    operation: crate::laptop::copy::Operation::Copy,
                    to_laptop: false,
                    remote: None,
                    sources: vec![s("~/report")],
                    destination: p("/destination"),
                    allow: vec![s("report")],
                }),
            },
            ServerMsg::LaptopCopy { token: s("abc"), message: ToClient::Start { sources: vec![s("~/report")] } },
            ServerMsg::LaptopCopy {
                token: s("abc"),
                message: ToClient::Write(Spec {
                    operation: crate::laptop::copy::Operation::Copy,
                    to_laptop: true,
                    remote: Some(crate::files::Remote {
                        server: s("web"),
                        ssh_args: vec![s("web.lan")],
                        path: s("/server/report"),
                    }),
                    sources: vec![s("/server/report")],
                    destination: p("~/Downloads"),
                    allow: vec![],
                }),
            },
            ServerMsg::LaptopCopy { token: s("abc"), message: ToClient::Installed },
            ServerMsg::LaptopCopy { token: s("abc"), message: ToClient::Delete { sources: vec![s("~/report")] } },
            ServerMsg::LaptopCopy {
                token: s("abc"),
                message: ToClient::Move(Spec {
                    operation: crate::laptop::copy::Operation::Move,
                    to_laptop: false,
                    remote: None,
                    sources: vec![s("~/report")],
                    destination: p("/destination"),
                    allow: vec![],
                }),
            },
            ServerMsg::LaptopCopy { token: s("abc"), message: ToClient::Progress },
            ServerMsg::LaptopCopy { token: s("abc"), message: ToClient::Ack(3) },
            ServerMsg::LaptopCopy { token: s("abc"), message: ToClient::Finish(Ok(())) },
            ServerMsg::LaptopCopy { token: s("abc"), message: ToClient::Finish(Err(s("connection lost"))) },
        ]);
        for packet in [
            Packet::Begin { bytes: u64::MAX, entries: 100_000 },
            Packet::Entry(Entry {
                path: s("folder/report"),
                kind: Kind::File,
                size: 3,
                mode: 0o644,
                seconds: -1,
                nanos: 123,
            }),
            Packet::Entry(Entry {
                path: s("folder"),
                kind: Kind::Directory,
                size: 0,
                mode: 0o755,
                seconds: 0,
                nanos: 0,
            }),
            Packet::Entry(Entry {
                path: s("link"),
                kind: Kind::Link(s("folder/report")),
                size: 0,
                mode: 0o777,
                seconds: 0,
                nanos: 0,
            }),
            Packet::Chunk(s("YWJj")),
            Packet::End,
            Packet::Done,
        ] {
            client.push(ClientMsg::LaptopCopy {
                token: s("abc"),
                message: FromClient::Data { sequence: 1, packet: packet.clone() },
            });
            server.push(ServerMsg::LaptopCopy { token: s("abc"), message: ToClient::Data { sequence: 1, packet } });
        }
        (client, server)
    }

    #[test]
    fn messages_round_trip() {
        let (clients, servers) = samples();
        for msg in clients {
            let value = serde_json::to_value(msg).unwrap();
            let read: ClientMsg = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(read).unwrap(), value);
        }
        for msg in servers {
            let value = serde_json::to_value(msg).unwrap();
            let read: ServerMsg = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(read).unwrap(), value);
        }
    }

    /// Frames and messages as a peer may send them, any of them: read bounded, never a panic.
    #[test]
    fn fuzz_protocol() {
        crate::fuzz::run("protocol", 300, |rng| {
            let mut wire = Vec::new();
            for _ in 0..rng.range(1, 6) {
                let payload: Vec<u8> = match rng.below(4) {
                    0 => serde_json::to_vec(&ClientMsg::Attach {
                        version: PROTOCOL_VERSION,
                        cols: rng.number() as u16,
                        rows: rng.number() as u16,
                        workspace: None,
                        remote: rng.one_in(2),
                        laptop: None,
                        pane: None,
                        installed: None,
                    })
                    .unwrap(),
                    1 => br#"{"type":"exit","reason":"x"}"#.to_vec(),
                    2 => br#"{"cmd":"kill_server"}"#.to_vec(),
                    _ => {
                        let n = rng.size(64);
                        rng.bytes(n)
                    }
                };
                write_frame(&mut wire, rng.below(3) as u8, &payload).unwrap();
            }
            if rng.one_in(2) {
                rng.mutate(&mut wire, &[&[0xff, 0xff, 0xff, 0x7f], &[0, 0, 0, 0], b"{", b"\""]);
            }
            let mut r = std::io::Cursor::new(wire);
            while let Ok(Some((_, payload))) = read_frame(&mut r) {
                assert!(payload.len() <= MAX_FRAME);
                let _ = serde_json::from_slice::<ClientMsg>(&payload);
                let _ = serde_json::from_slice::<ServerMsg>(&payload);
                let _ = serde_json::from_slice::<Request>(&payload);
            }
            let v = String::from_utf8_lossy(&rng.text(20)).into_owned();
            let _ = newer(&v, KRAAL_VERSION) || newer(KRAAL_VERSION, &v);
        });
    }
}
