//! Events: what happens in the session, streamed to `kraal events` and run through the
//! `[hooks]` of the config.
//!
//! Every event carries `seq`, increasing within a server's life, and `boot`, which changes when
//! the server restarts. The last events are kept, so a subscriber that was dropped (too slow,
//! which it is told) or reconnects can ask for what it missed with `since`.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use super::{ClientId, Server};
use crate::layout::PaneId;
use crate::protocol::{self, ServerMsg};

/// Every event type, for checking filters and hooks.
pub const TYPES: &[&str] = &[
    "workspace.opened",
    "workspace.closed",
    "pane.opened",
    "pane.exited",
    "pane.failed",
    "pane.closed",
    "agent.working",
    "agent.blocked",
    "agent.done",
    "agent.idle",
    "ssh.started",
    "ssh.ended",
    "production.entered",
    "production.left",
    "production.approved",
    "production.refused",
    "notification",
    "tunnel.up",
    "tunnel.down",
    "port.opened",
    "port.closed",
    "account.limit",
    "account.changed",
    "runbook.started",
    "runbook.finished",
    "command.finished",
];

/// Events kept for subscribers that ask for what they missed.
const KEPT: usize = 1024;

/// Whether `pattern` (`pane.exited`, `agent.*`, `*`) selects events of type `ty`.
pub fn matches(pattern: &str, ty: &str) -> bool {
    if pattern == "*" || pattern == ty {
        return true;
    }
    pattern.strip_suffix(".*").is_some_and(|p| ty.strip_prefix(p).is_some_and(|rest| rest.starts_with('.')))
}

/// Patterns that select no event: typos.
pub fn unknown_patterns<'a>(patterns: impl Iterator<Item = &'a String>) -> Vec<String> {
    patterns.filter(|p| !TYPES.iter().any(|t| matches(p, t))).cloned().collect()
}

fn selected(filters: &[String], ty: &str) -> bool {
    filters.is_empty() || filters.iter().any(|f| matches(f, ty))
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub struct Subscriber {
    pub client: ClientId,
    /// Where its events go; `None` once it was dropped for being too slow. It stays listed, and
    /// counted as a connection, until it has read that and its connection ends.
    pub out: Option<super::clients::Outbox>,
    pub filters: Vec<String>,
}

/// Numbering of events and the last ones, for replay.
pub struct EventLog {
    boot: u64,
    seq: u64,
    pub(super) kept: VecDeque<Map<String, Value>>,
}

impl Default for EventLog {
    fn default() -> Self {
        EventLog { boot: now_ms(), seq: 0, kept: VecDeque::new() }
    }
}

impl EventLog {
    /// Number an event and keep it.
    fn record(&mut self, ty: &str, fields: Value) -> Map<String, Value> {
        self.seq += 1;
        let mut event = Map::new();
        event.insert("v".into(), json!(1));
        event.insert("seq".into(), json!(self.seq));
        event.insert("boot".into(), json!(self.boot));
        event.insert("type".into(), json!(ty));
        event.insert("time".into(), json!(now_ms()));
        if let Value::Object(f) = fields {
            event.extend(f);
        }
        if self.kept.len() == KEPT {
            self.kept.pop_front();
        }
        self.kept.push_back(event.clone());
        event
    }

    /// Kept events after `since`, and whether some in between were already forgotten.
    fn after(&self, since: u64) -> (Vec<&Map<String, Value>>, bool) {
        let oldest = self.kept.front().and_then(|e| e["seq"].as_u64()).unwrap_or(self.seq + 1);
        let lost = since.saturating_add(1) < oldest && since < self.seq;
        let events = self.kept.iter().filter(|e| e["seq"].as_u64().is_some_and(|s| s > since)).collect();
        (events, lost)
    }
}

/// `[hooks]` of the config, flattened: `agent.blocked = "…"` and `[hooks.agent] blocked = "…"`
/// are the same. Returns warnings for values that are not commands.
pub fn read_hooks(table: &toml::Table) -> (Vec<(String, String)>, Vec<String>) {
    fn walk(prefix: &str, t: &toml::Table, out: &mut Vec<(String, String)>, warnings: &mut Vec<String>) {
        for (k, v) in t {
            let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            match v {
                toml::Value::String(cmd) => out.push((key, cmd.clone())),
                toml::Value::Table(t) => walk(&key, t, out, warnings),
                _ => warnings.push(format!("hooks.{key}: expected a command line")),
            }
        }
    }
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    walk("", table, &mut out, &mut warnings);
    for p in unknown_patterns(out.iter().map(|(p, _)| p)) {
        warnings.push(format!("hooks.{p}: no such event (see `kraal events --list`)"));
    }
    (out, warnings)
}

/// Hook processes, launched in the background and reaped by the tick.
#[derive(Default)]
pub struct Hooks {
    pub list: Vec<(String, String)>,
    running: Vec<Running>,
    recent: VecDeque<Instant>,
    warned: Option<Instant>,
}

/// A hook process, the readers of its output, and where that output goes.
struct Running {
    child: Child,
    started: Instant,
    output: Option<crate::exec::Drains>,
    log: PathBuf,
}

impl Running {
    /// Write what it printed to the log, off the event loop (a background descendant may hold
    /// its pipes open a little while).
    fn log_output(&mut self) {
        let (Some(drains), log) = (self.output.take(), self.log.clone()) else { return };
        std::thread::spawn(move || {
            let (out, err, _) = drains.finish(Duration::from_millis(500));
            if (!out.is_empty() || !err.is_empty())
                && let Ok(mut f) = open_log(&log)
            {
                let _ = f.write_all(&out);
                let _ = f.write_all(&err);
            }
        });
    }
}

/// More launches than this per second are dropped (a hook that triggers its own event)...
const MAX_PER_SECOND: usize = 20;
/// ...and so are launches beyond this many hooks running at once.
const MAX_RUNNING: usize = 32;
const MAX_RUNTIME: Duration = Duration::from_secs(600);
/// Output kept in hooks.log per hook and stream.
const OUTPUT_LIMIT: usize = 64 << 10;

impl Hooks {
    pub fn new(list: Vec<(String, String)>) -> Hooks {
        Hooks { list, ..Default::default() }
    }

    /// Forget finished hooks; stop their process groups when they run too long. A hook runs
    /// until its group has no live members: what it left in the background still counts.
    pub fn reap(&mut self) {
        self.running.retain_mut(|h| {
            // Keep the leader unreaped until the last possible signal, so its group id stays ours.
            let done = crate::exec::has_exited(&h.child) && !crate::exec::group_alive(&h.child);
            if !done && h.started.elapsed() <= MAX_RUNTIME {
                return true;
            }
            if done {
                let _ = h.child.wait();
            } else {
                crate::exec::kill_group(&mut h.child);
            }
            h.log_output();
            false
        });
    }

    /// Whether hooks run (they are looked at every tick, to be stopped in time).
    pub fn busy(&self) -> bool {
        !self.running.is_empty()
    }

    /// kraal is stopping: so do the hooks.
    pub fn stop_all(&mut self) {
        for h in &mut self.running {
            crate::exec::kill_group(&mut h.child);
            h.log_output();
        }
        self.running.clear();
    }

    /// Whether another hook may start now; says why not otherwise.
    fn admit(&mut self) -> Result<(), &'static str> {
        let now = Instant::now();
        while self.recent.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1)) {
            self.recent.pop_front();
        }
        if self.recent.len() >= MAX_PER_SECOND {
            return Err("too many events at once");
        }
        if self.running.len() >= MAX_RUNNING {
            return Err("too many hooks still running");
        }
        self.recent.push_back(now);
        Ok(())
    }
}

/// `KRAAL_<FIELD>` variables of an event's scalar fields (lists are joined with spaces).
pub fn hook_env(event: &Map<String, Value>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (k, v) in event {
        let value = match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => if *b { "1" } else { "" }.to_string(),
            Value::Null => String::new(),
            Value::Array(a) => {
                a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or(x.to_string())).collect::<Vec<_>>().join(" ")
            }
            Value::Object(_) => continue,
        };
        let name = match k.as_str() {
            "type" => "EVENT".to_string(),
            other => other.to_ascii_uppercase(),
        };
        out.push((format!("KRAAL_{name}"), value));
    }
    out
}

fn hooks_log() -> PathBuf {
    crate::paths::state_dir().join("hooks.log")
}

/// The hooks log, for appending. Commands and what they print may hold secrets (a webhook
/// token): the file is only ours, in a directory only ours, and never a link placed there.
pub(super) fn open_log(log: &Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = log.parent() {
        crate::paths::ensure_private_dir(dir)?;
    }
    crate::paths::open_private_append(log)
}

fn rotate(log: &Path) {
    if std::fs::metadata(log).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = std::fs::rename(log, log.with_extension("log.1"));
    }
}

/// What a subscriber dropped for being too slow reads last, after the events it was sent: a line
/// for what reads its output, with the cursor to come back with (`since`, `boot`), then the end
/// of its stream and why. Every release reads both: `kraal events` prints the first and fails
/// with the second.
fn dropped_frames(boot: u64, since: u64) -> Vec<u8> {
    let event = json!({ "v": 1, "type": "events.dropped", "boot": boot, "since": since, "reason": "too slow" });
    let reason = format!(
        "too slow to follow the events, so kraal dropped this reader: `kraal events --since {since} --boot {boot}` goes on from there"
    );
    let mut frames = Vec::new();
    for msg in [ServerMsg::Event { event }, ServerMsg::exit(reason)] {
        let _ = protocol::write_json(&mut frames, &msg);
    }
    frames
}

/// A frame of replayed events.
fn events_frame(events: Vec<Value>) -> Result<Vec<u8>, String> {
    let mut frame = Vec::new();
    protocol::write_json(&mut frame, &ServerMsg::Events { events })
        .map_err(|e| format!("could not replay the events: {e}"))?;
    Ok(frame)
}

impl Server {
    /// Something happened: number it, tell subscribers and run the matching hooks.
    pub fn emit(&mut self, ty: &str, fields: Value) {
        let event = self.events.record(ty, fields);
        if !self.subscribers.is_empty() {
            let mut frame = Vec::new();
            if protocol::write_json(&mut frame, &ServerMsg::Event { event: Value::Object(event.clone()) }).is_ok() {
                // A subscriber too slow to keep up (its queue is full) is dropped rather than
                // slowing the session. It is told, after what it was sent, and can come back
                // with `since` for what it missed: this event and those after it.
                let (boot, since) = (self.events.boot, self.events.seq - 1);
                for s in self.subscribers.iter_mut().filter(|s| selected(&s.filters, ty)) {
                    if let Some(out) = s.out.take_if(|out| out.try_send(frame.clone()).is_err()) {
                        out.end_with(dropped_frames(boot, since));
                    }
                }
            }
        }
        let commands: Vec<String> =
            self.hooks.list.iter().filter(|(p, _)| matches(p, ty)).map(|(_, c)| c.clone()).collect();
        for cmd in commands {
            self.run_hook(&cmd, &event);
        }
    }

    fn run_hook(&mut self, cmd: &str, event: &Map<String, Value>) {
        if let Err(why) = self.hooks.admit() {
            if self.hooks.warned.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
                self.hooks.warned = Some(Instant::now());
                self.warnings.push(format!("hooks: {why}, some were skipped"));
            }
            return;
        }
        let log = hooks_log();
        rotate(&log);
        if let Ok(mut f) = open_log(&log) {
            let ty = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let _ = writeln!(f, "--- {} {ty}: {cmd}", now_ms());
        }
        let mut command = self.hook_command(cmd, event);
        match crate::exec::own_group(&mut command).spawn() {
            Ok(mut child) => {
                let output = Some(crate::exec::Drains::start(&mut child, OUTPUT_LIMIT));
                self.hooks.running.push(Running { child, started: Instant::now(), output, log });
            }
            Err(e) => self.warnings.push(format!("hook {cmd:?}: {e}")),
        }
    }

    /// `sh -c cmd` with the event's fields over this session's name (a remote agent's event
    /// names its own session), and this server's socket last: `kraal` in a hook reaches it.
    fn hook_command(&self, cmd: &str, event: &Map<String, Value>) -> Command {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(cmd)
            .env("KRAAL_SESSION", &self.env.session)
            .envs(hook_env(event))
            .env("KRAAL_EVENT_JSON", Value::Object(event.clone()).to_string())
            .env("KRAAL_SOCKET", &self.env.socket)
            .env("KRAAL_HOOK", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Fields that describe a pane in events.
    pub fn pane_fields(&self, id: PaneId) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("pane".into(), json!(id));
        if let Some(p) = self.panes.get(&id) {
            m.insert("name".into(), json!(p.display_name()));
        }
        if let Some((wi, _)) = self.workspace_of(id) {
            m.insert("workspace".into(), json!(self.workspaces[wi].name));
        }
        m
    }

    pub fn emit_pane(&mut self, ty: &str, id: PaneId, extra: Value) {
        let mut m = self.pane_fields(id);
        if let Value::Object(e) = extra {
            m.extend(e);
        }
        self.emit(ty, Value::Object(m));
    }

    /// Say `pane.opened` once for a pane: at the first tick after it has its place in a
    /// workspace, or before anything else is said about it.
    pub(super) fn announce_pane(&mut self, id: PaneId) {
        if !self.panes.contains_key(&id) || !self.announced.insert(id) {
            return;
        }
        let Some(p) = self.panes.get(&id) else { return };
        let command = p.spec.command.clone();
        let cwd = p.spec.cwd.as_ref().map(|c| c.display().to_string());
        let kind = match &p.content {
            crate::pane::Content::Term(_) => "terminal",
            crate::pane::Content::Preview(_) => "preview",
            crate::pane::Content::Files(_) => "files",
        };
        self.emit_pane("pane.opened", id, json!({ "command": command, "cwd": cwd, "kind": kind }));
    }

    /// Start streaming events to a `kraal events` client, after replaying those after `since`.
    pub(super) fn subscribe(
        &mut self,
        reply: super::requests::ReplyHandle,
        filters: Vec<String>,
        since: Option<u64>,
        boot: Option<u64>,
    ) -> super::requests::RequestOutcome {
        use super::requests::RequestOutcome;
        match self.start_subscription(reply.client(), filters, since, boot) {
            Ok(()) => RequestOutcome::Streaming,
            Err(e) => RequestOutcome::Immediate { reply, result: Err(e) },
        }
    }

    fn start_subscription(
        &mut self,
        client: ClientId,
        filters: Vec<String>,
        since: Option<u64>,
        boot: Option<u64>,
    ) -> Result<(), String> {
        let unknown = unknown_patterns(filters.iter());
        if !unknown.is_empty() {
            return Err(format!("no such event: {} (see `kraal events --list`)", unknown.join(", ")));
        }
        if !self.clients.contains_key(&client) {
            return Err("no client".into());
        }
        let mut frames = Vec::new();
        let mut ok = Vec::new();
        if protocol::write_json(&mut ok, &ServerMsg::Response { result: Ok(protocol::Response::Ok) }).is_ok() {
            frames.push(ok);
        }
        // A cursor from before a restart means nothing now: everything kept is new to it.
        let restarted = boot.is_some_and(|b| b != self.events.boot);
        let since = if restarted { Some(0) } else { since };
        if let Some(since) = since {
            let (missed, lost) = self.events.after(since);
            let mut replay = Vec::new();
            if lost || restarted {
                let reason = if restarted { "kraal restarted" } else { "too old" };
                replay.push(json!({ "v": 1, "type": "events.lost", "boot": self.events.boot, "since": since, "reason": reason }));
            }
            replay.extend(
                missed
                    .into_iter()
                    .filter(|e| selected(&filters, e["type"].as_str().unwrap_or("")))
                    .map(|e| Value::Object(e.clone())),
            );
            // A few frames of at most a few MiB each for all of it (the replay buffer holds 1024
            // events), so it fits the client's queue whatever its length.
            const BATCH: usize = 4 << 20;
            let mut batch: Vec<Value> = Vec::new();
            let mut size = 0;
            for e in replay {
                let len = serde_json::to_vec(&e).map(|v| v.len()).unwrap_or(0);
                if size + len > BATCH && !batch.is_empty() {
                    frames.push(events_frame(std::mem::take(&mut batch))?);
                    size = 0;
                }
                size += len;
                batch.push(e);
            }
            if !batch.is_empty() {
                frames.push(events_frame(batch)?);
            }
        }
        // From here on the connection is a subscriber, no longer a client.
        let Some(c) = self.clients.remove(&client) else { return Err("no client".into()) };
        // Never wait on a client; a fresh connection has room for these few frames.
        for f in frames {
            if c.out.try_send(f).is_err() {
                return Err("the event stream could not start: try again".into());
            }
        }
        c.out.wait_for_reader();
        self.subscribers.push(Subscriber { client, out: Some(c.out), filters });
        Ok(())
    }

    /// Changes noticed by polling: new panes, SSH sessions and production status.
    pub(super) fn diff_pane_states(&mut self) {
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        for id in ids {
            self.announce_pane(id);
            // A pane keeps its server once its ssh has ended (a drop: Enter connects again).
            let ssh = self
                .panes
                .get(&id)
                .filter(|p| p.exit_code.is_none())
                .and_then(|p| p.ssh.as_ref().map(|s| s.name.clone()));
            let prod = self.pane_production_shown(id);
            let (old_ssh, old_prod) = self.pane_states.get(&id).cloned().unwrap_or_default();
            if ssh != old_ssh {
                if let Some(s) = &old_ssh {
                    self.emit_pane("ssh.ended", id, json!({ "server": s }));
                }
                if let Some(s) = &ssh {
                    let host = self.pane_server_entry(id).map(|e| e.destination()).unwrap_or_default();
                    let fields = json!({ "server": s, "host": host, "production": prod.is_some() });
                    self.emit_pane("ssh.started", id, fields);
                }
            }
            if prod != old_prod {
                match &prod {
                    Some(reason) => {
                        self.emit_pane("production.entered", id, json!({ "reason": reason, "previous": old_prod }))
                    }
                    None => self.emit_pane("production.left", id, json!({ "previous": old_prod })),
                }
            }
            self.pane_states.insert(id, (ssh, prod));
        }
        let panes = &self.panes;
        self.pane_states.retain(|id, _| panes.contains_key(id));
        self.announced.retain(|id| panes.contains_key(id));
    }

    /// A pane closes (described by `fields`): the SSH connection and the production said of it
    /// end with it, so that what keeps count of either from the events is told.
    pub(super) fn end_pane_states(&mut self, id: PaneId, fields: &Map<String, Value>) {
        let Some((ssh, prod)) = self.pane_states.remove(&id) else { return };
        if let Some(server) = ssh {
            let mut m = fields.clone();
            m.insert("server".into(), json!(server));
            self.emit("ssh.ended", Value::Object(m));
        }
        if let Some(previous) = prod {
            let mut m = fields.clone();
            m.insert("previous".into(), json!(previous));
            self.emit("production.left", Value::Object(m));
        }
    }
}

/// Remember what changed between two port lists.
pub fn port_changes(
    before: &std::collections::BTreeMap<u16, PaneId>,
    after: &std::collections::BTreeMap<u16, PaneId>,
) -> (Vec<(u16, PaneId)>, Vec<u16>) {
    let opened = after.iter().filter(|(p, _)| !before.contains_key(p)).map(|(p, id)| (*p, *id)).collect();
    let closed = before.keys().filter(|p| !after.contains_key(p)).copied().collect();
    (opened, closed)
}

pub type PaneStates = HashMap<PaneId, (Option<String>, Option<String>)>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriptions_transfer_the_connection_and_keep_replay_before_live_events() {
        use crate::protocol::{Request, Response};
        use crate::server::requests::{RequestOutcome, tests::response};
        use crate::server::tests::{client, server};
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = server(tx);
        server.emit("pane.opened", json!({"pane": 17}));
        let (id, _peer) = client(&mut server);
        let receive = server.clients.get_mut(&id).unwrap().out.capture();
        assert!(matches!(
            server.request(id, Request::Subscribe { events: vec!["pane.*".into()], since: Some(0), boot: None }),
            RequestOutcome::Streaming
        ));
        assert!(!server.clients.contains_key(&id));
        assert_eq!(server.subscribers.len(), 1);
        assert!(matches!(response(&receive), Ok(Response::Ok)));
        let frame = receive.recv_timeout(crate::testing::PATIENCE).unwrap();
        let (_, payload) = protocol::read_frame(&mut frame.as_slice()).unwrap().unwrap();
        let replay: ServerMsg = serde_json::from_slice(&payload).unwrap();
        assert!(matches!(replay, ServerMsg::Events { events } if events.len() == 1 && events[0]["pane"] == 17));
        server.emit("pane.closed", json!({"pane": 17}));
        let frame = receive.recv_timeout(crate::testing::PATIENCE).unwrap();
        let (_, payload) = protocol::read_frame(&mut frame.as_slice()).unwrap().unwrap();
        assert!(
            matches!(serde_json::from_slice(&payload).unwrap(), ServerMsg::Event { event } if event["type"] == "pane.closed")
        );
        server.drop_client(id);
        assert!(server.subscribers.is_empty());
        server.emit("pane.opened", json!({"pane": 18}));
        assert!(matches!(receive.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
    }

    #[test]
    fn invalid_subscriptions_keep_an_immediate_error_and_no_stream() {
        use crate::protocol::Request;
        use crate::server::requests::{RequestOutcome, tests::response};
        use crate::server::tests::{client, server};
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = server(tx);
        let (id, _peer) = client(&mut server);
        let receive = server.clients.get_mut(&id).unwrap().out.capture();
        let RequestOutcome::Immediate { reply, result } =
            server.request(id, Request::Subscribe { events: vec!["unknown".into()], since: None, boot: None })
        else {
            panic!("invalid subscription")
        };
        assert!(receive.is_empty());
        assert!(server.subscribers.is_empty());
        reply.complete(&mut server, result);
        assert_eq!(response(&receive).unwrap_err(), "no such event: unknown (see `kraal events --list`)");
        assert!(matches!(receive.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
    }

    #[test]
    fn duplicate_and_closed_restore_emit_open_only_when_opened_successfully() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let source = server.new_workspace("source".into(), None);
        let seq = server.events.seq;
        super::super::persistence::tests::writer(&mut server, vec![Err("disk full".into()), Ok(())]);
        assert!(server.duplicate_workspace(source, "copy").is_err());
        assert_eq!(server.events.seq, seq);
        let copy = server.duplicate_workspace(source, "copy").unwrap();
        assert_eq!(server.events.seq, seq);
        let snapshot = crate::server::persistence::snapshot(&server);
        server.reopen_workspace(copy).unwrap();
        assert_eq!(server.events.seq, seq + 1);
        assert_eq!(server.events.kept.back().unwrap()["type"], "workspace.opened");
        assert_eq!(server.events.kept.back().unwrap()["workspace"], "copy");
        server.reopen_workspace(copy).unwrap();
        assert_eq!(server.events.seq, seq + 1);
        let (tx, _rx) = super::super::queue::channel(32);
        let mut restarted = super::super::tests::server(tx);
        crate::server::persistence::apply(&mut restarted, snapshot).unwrap();
        assert!(restarted.events.kept.iter().all(|e| e["workspace"] != "copy"));
    }

    #[test]
    fn patterns() {
        assert!(matches("agent.*", "agent.blocked"));
        assert!(!matches("agent.*", "agents.x"));
        assert!(matches("*", "pane.exited"));
        assert!(matches("pane.exited", "pane.exited"));
        assert!(!matches("pane", "pane.exited"));
        let bad = unknown_patterns(["agent.*".to_string(), "agent.blokced".to_string()].iter());
        assert_eq!(bad, vec!["agent.blokced"]);
    }

    #[test]
    fn hooks_flatten_and_env() {
        let t: toml::Table =
            toml::from_str("\"pane.failed\" = \"a\"\n[agent]\nblocked = \"b\"\n[ssh]\nstarted = 3\n").unwrap();
        let (hooks, warnings) = read_hooks(&t);
        assert_eq!(hooks, vec![("agent.blocked".into(), "b".into()), ("pane.failed".into(), "a".into())]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let ev = json!({"type": "pane.exited", "pane": 3, "command": ["make", "test"], "production": false});
        let env = hook_env(ev.as_object().unwrap());
        assert!(env.contains(&("KRAAL_EVENT".into(), "pane.exited".into())));
        assert!(env.contains(&("KRAAL_PANE".into(), "3".into())));
        assert!(env.contains(&("KRAAL_COMMAND".into(), "make test".into())));
        assert!(env.contains(&("KRAAL_PRODUCTION".into(), String::new())));
    }

    #[test]
    fn a_remote_event_keeps_its_session_and_the_local_socket() {
        let (tx, _rx) = crate::server::event_channel(32);
        let server = crate::server::tests::server(tx);
        let ev = json!({"type": "agent.blocked", "server": "db", "session": "remote", "socket": "/elsewhere"});
        let command = server.hook_command("true", ev.as_object().unwrap());
        let env =
            |name: &str| command.get_envs().find(|(k, _)| **k == *name).and_then(|(_, v)| v.map(|v| v.to_owned()));
        assert_eq!(env("KRAAL_SESSION"), Some("remote".into()));
        assert_eq!(env("KRAAL_SOCKET"), Some(server.env.socket.clone().into_os_string()));
    }

    #[test]
    fn numbering_and_replay() {
        let mut log = EventLog::default();
        for i in 0..(KEPT + 10) {
            log.record("pane.opened", json!({ "pane": i }));
        }
        let (events, lost) = log.after(5);
        assert!(lost, "events 6..10 were forgotten");
        assert_eq!(events.len(), KEPT);
        assert_eq!(events[0]["seq"], 11);
        let (events, lost) = log.after(log.seq - 2);
        assert!(!lost);
        assert_eq!(events.len(), 2);
        let (events, lost) = log.after(log.seq);
        assert!(events.is_empty() && !lost);
    }

    #[test]
    fn hook_logs_are_private() {
        let dir = std::env::temp_dir().join(format!("kraal-hooklog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("hooks.log");
        std::fs::write(&log, "old\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
            let _ = open_log(&log).unwrap();
            assert_eq!(std::fs::metadata(&log).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
            std::fs::remove_file(&log).unwrap();
            std::os::unix::fs::symlink(dir.join("elsewhere"), &log).unwrap();
            assert!(open_log(&log).is_err(), "a link is not followed");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hook_limits() {
        let mut h = Hooks::new(Vec::new());
        for _ in 0..MAX_PER_SECOND {
            assert!(h.admit().is_ok());
        }
        assert_eq!(h.admit(), Err("too many events at once"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn hooks_keep_work_forked_during_the_group_scan() {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        use std::rc::Rc;

        let (input, release_leader) = std::io::pipe().unwrap();
        let child =
            crate::exec::own_group(Command::new("sh").args(["-c", "read release"])).stdin(input).spawn().unwrap();
        let pid = child.id() as libc::pid_t;
        let (input, release_member) = std::io::pipe().unwrap();
        let mut member = Command::new("sh")
            .args(["-c", "exec 3<&0; read fork; sh -c 'read release' <&3 & echo $!; exit 0"])
            .process_group(pid)
            .stdin(input)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = std::io::BufReader::new(member.stdout.take().unwrap());
        let member = Rc::new(member);
        let mut hooks = Hooks::default();
        hooks.running.push(Running { child, started: Instant::now(), output: None, log: PathBuf::new() });
        drop(release_leader);
        let start = Instant::now();
        while !crate::exec::has_exited(&hooks.running[0].child) && start.elapsed() < crate::testing::PATIENCE {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(crate::exec::has_exited(&hooks.running[0].child));
        let parent = member.clone();
        let mut fork = release_member.try_clone().unwrap();
        crate::process::AFTER_GROUP_ENUMERATION.with(|after| {
            *after.borrow_mut() = Some(Box::new(move || {
                // The enumerated parent forks a reader absent from that list, then becomes a zombie.
                writeln!(fork, "fork").unwrap();
                let start = Instant::now();
                while !crate::exec::has_exited(&parent) && start.elapsed() < crate::testing::PATIENCE {
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(crate::exec::has_exited(&parent));
            }));
        });
        hooks.reap();
        let busy = hooks.busy();
        let unreaped = hooks.running.first().is_some_and(|h| crate::exec::has_exited(&h.child));
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        let successor = line.trim().parse().unwrap();
        let successor_running = crate::process::running(successor);
        // SAFETY: getpgid only reads the process table.
        let successor_group = unsafe { libc::getpgid(successor) };
        hooks.stop_all();
        // Keep the reader blocked to check shutdown's signal. On regression, release it so
        // even a hook that was forgotten cannot leave this test's background work running.
        let release_member = busy.then_some(release_member);
        Rc::try_unwrap(member).unwrap().wait().unwrap();
        let start = Instant::now();
        while crate::process::running(successor) && start.elapsed() < crate::testing::PATIENCE {
            std::thread::sleep(Duration::from_millis(10));
        }
        let cleaned_up = !crate::process::running(successor);
        drop(release_member);
        assert!(cleaned_up, "the successor was cleaned up");
        assert!(successor_running, "the successor is still waiting in the hook's group");
        assert_eq!(successor_group, pid);
        assert!(busy, "a fork during the scan must keep the hook supervised");
        assert!(unreaped, "the leader must still reserve its pid");
    }

    #[test]
    fn hooks_keep_the_leader_until_background_work_ends_or_is_stopped() {
        for ending in ["finished", "timeout", "shutdown"] {
            let (input, release) = std::io::pipe().unwrap();
            let mut release = Some(release);
            let child =
                crate::exec::own_group(Command::new("sh").args(["-c", "exec 3<&0; sh -c 'read release' <&3 & exit 3"]))
                    .stdin(input)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
            let pid = child.id() as libc::pid_t;
            let mut hooks = Hooks::default();
            hooks.running.push(Running { child, started: Instant::now(), output: None, log: PathBuf::new() });
            let start = Instant::now();
            while !crate::exec::has_exited(&hooks.running[0].child) && start.elapsed() < crate::testing::PATIENCE {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(crate::exec::has_exited(&hooks.running[0].child));
            // The shell has exited, but its background reader cannot exit before we close the pipe.
            hooks.reap();
            assert!(hooks.busy(), "{ending}: background work still counts");
            let unreaped = crate::exec::has_exited(&hooks.running[0].child);
            match ending {
                "finished" => {
                    drop(release.take());
                    let start = Instant::now();
                    while crate::process::group_running(pid) && start.elapsed() < crate::testing::PATIENCE {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    hooks.reap();
                }
                "timeout" => {
                    hooks.running[0].started = Instant::now() - MAX_RUNTIME - Duration::from_secs(1);
                    hooks.reap();
                }
                _ => {
                    hooks.stop_all();
                }
            }
            let busy = hooks.busy();
            hooks.stop_all();
            assert!(unreaped, "{ending}: the leader must reserve its pid until the last possible signal");
            assert!(!busy, "{ending}: supervision ended");
            // SAFETY: this is the test's child; WNOHANG never blocks.
            assert_eq!(unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) }, -1);
            assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
            let start = Instant::now();
            while crate::process::group_running(pid) && start.elapsed() < crate::testing::PATIENCE {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(!crate::process::group_running(pid), "{ending}: no background work remains");
        }
    }
}
