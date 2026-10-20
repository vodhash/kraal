//! Production actions asked for from an agent's pane: they wait for the user to allow them in
//! kraal, and each answer is logged (`production.log`) and told to hooks. What the user allows
//! is resolved once, when asked, and is what runs: input for a pane runs as stored; a request
//! runs only if it still reaches what the question showed.

use std::io::{Read, Seek, Write};

use serde_json::{Value, json};

use super::contexts::Production;
use super::requests::{ReplyHandle, RequestOutcome};
use super::ui::{ConfirmAction, Mode};
use super::{ClientId, Dirty, Server};
use crate::layout::PaneId;

/// How long after a question reaches a terminal keys are still taken as typed before it was seen.
const SEEN: std::time::Duration = std::time::Duration::from_millis(300);
use crate::protocol::{self, Origin, Request, Response, ServerMsg};

/// A request held until the user answers, and who asked.
pub(super) struct ProductionAsk {
    id: u32,
    reply: ReplyHandle,
    pane: PaneId,
    agent: String,
    need: Need,
    /// The client asked, and how far the question got there: a key before the client displayed
    /// it, and read it, does not answer it.
    shown: Option<(ClientId, Display)>,
}

/// How far a question got on the client asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Display {
    /// In no frame sent yet.
    Pending,
    /// A frame carried it, then [`ServerMsg::DisplayMark`] with this id, which the client
    /// answers once it wrote that frame to its terminal. A mark answered late for an earlier
    /// display (before a repaint) is not this one.
    Marked(u32),
    /// The client displayed it then.
    At(std::time::Instant),
}

/// What a request would do on production: the command (`exec`, `send`…), what it runs or
/// types, the production servers or panes it reaches, everything it reaches as resolved (ssh
/// arguments, production identities: compared, not shown), and what Allow does.
struct Need {
    kind: String,
    plan: String,
    servers: Vec<String>,
    reach: Vec<String>,
    act: Act,
}

enum Act {
    /// The command line acts itself once answered (`kraal exec`, `cp`, `rm`, `ssh`).
    Reply,
    /// Input for a pane, as it was when asked, for the production it was asked about.
    Input { pane: PaneId, production: Production, input: Input },
    /// Handled as it came, once it still reaches what was shown.
    Request(Request),
}

enum Input {
    Text(String),
    Keys(Vec<String>),
    Paste(String),
}

/// Where the command that sent a request runs.
enum Place {
    Pane(PaneId),
    /// In a pane of another kraal session, or one kraal cannot place.
    Elsewhere,
    /// Outside kraal.
    Outside,
}

pub(super) fn production_log() -> std::path::PathBuf {
    crate::paths::state_dir().join("production.log")
}

impl Server {
    /// Hold a request that acts on production from an agent's pane until the user answers;
    /// anything else goes on as it came. From an agent, a request kraal cannot resolve is
    /// refused: never let through unasked.
    // Like `RequestOutcome`, this lives only for dispatch; boxing would allocate for each request.
    #[allow(clippy::result_large_err)]
    pub(super) fn production_gate(
        &mut self,
        reply: ReplyHandle,
        req: Request,
    ) -> Result<(ReplyHandle, Request), RequestOutcome> {
        let covered = match &req {
            Request::ApproveProduction { .. } | Request::RunbookRun { .. } => true,
            Request::SendText { production, .. }
            | Request::SendKeys { production, .. }
            | Request::BufferPaste { production, .. }
            | Request::EditRemote { production, .. } => *production,
            Request::OpenSsh { command, .. } => !command.is_empty(),
            _ => false,
        };
        if !covered {
            return Ok((reply, req));
        }
        let place = self.caller_place(reply.client());
        let agent = match place {
            Place::Pane(pane) => self.pane_agent(pane).map(|agent| (pane, agent)),
            Place::Elsewhere | Place::Outside => None,
        };
        if agent.is_none() && !matches!(place, Place::Elsewhere) {
            // Yours: not asked, but on record.
            if let Ok(Some(need)) = self.production_need(&req) {
                let pane = match place {
                    Place::Pane(pane) => Some(pane),
                    Place::Elsewhere | Place::Outside => None,
                };
                self.record_yours(pane, &need.kind, &need.plan, need.servers, "yours");
            }
            return Ok((reply, req));
        }
        let need = match self.production_need(&req) {
            Ok(Some(need)) => need,
            Ok(None) => return Ok((reply, req)),
            Err(e) => return Err(RequestOutcome::Immediate { reply, result: Err(format!("{e}: nothing was done")) }),
        };
        if let Request::RunbookRun { token } = &req
            && let Some((_, d)) = self.plans.get(token)
            && !d.listed().is_empty()
        {
            let error = format!(
                "{} lists choices with commands and acts on production: from an agent's pane, those commands would run before you are asked; run it yourself: nothing was done",
                d.runbook_name()
            );
            return Err(RequestOutcome::Immediate { reply, result: Err(error) });
        }
        // Only the session of the agent's pane knows the agent, and asks about it.
        let Some((pane, agent)) = agent else {
            let session = &self.session;
            let error = format!(
                "this acts on production in session {session} from a pane of another kraal session: run it from a pane of {session}, or yourself; nothing was done"
            );
            return Err(RequestOutcome::Immediate { reply, result: Err(error) });
        };
        let ask = ProductionAsk { id: self.alloc_id(), reply, pane, agent, need, shown: None };
        if !self.clients.values().any(|c| c.attached) {
            let error = format!(
                "{} asked to act on production and no kraal is attached to allow it: attach kraal and run it again; nothing was done",
                super::account_ui::agent_name(&ask.agent)
            );
            self.refused(&ask, "refused: nobody attached");
            return Err(RequestOutcome::Immediate { reply: ask.reply, result: Err(error) });
        }
        self.production_asks.push(ask);
        self.show_production_ask();
        Err(RequestOutcome::Deferred)
    }

    /// What `req` would do on production, when it would: resolved now, as Allow would run it.
    fn production_need(&self, req: &Request) -> Result<Option<Need>, String> {
        let input = |pane: &str, kind: &str, plan: String, input: Input| -> Result<Option<Need>, String> {
            let p = self.resolve(pane)?;
            let Some(production) = self.pane_production(p) else { return Ok(None) };
            let servers = vec![format!("{} ({production})", self.pane_label(p))];
            let act = Act::Input { pane: p, production, input };
            Ok(Some(Need { kind: kind.into(), plan, servers, reach: Vec::new(), act }))
        };
        let server = |s: &crate::servers::Server| format!("{} ({})", s.name, s.destination());
        match req {
            Request::ApproveProduction { kind, plan, servers } => Ok(Some(Need {
                kind: kind.clone(),
                plan: plan.clone(),
                servers: servers.clone(),
                reach: Vec::new(),
                act: Act::Reply,
            })),
            // Without --production, they are refused on production: nothing to ask.
            Request::SendText { pane, text, production: true } => {
                input(pane, "send", text.clone(), Input::Text(text.clone()))
            }
            Request::SendKeys { pane, keys, production: true } => {
                input(pane, "keys", keys.join(" "), Input::Keys(keys.clone()))
            }
            Request::BufferPaste { buffer, pane, production: true } => {
                let text = self.buffers.get(buffer.as_deref())?.text.clone();
                input(pane, "buffer paste", text.clone(), Input::Paste(text))
            }
            Request::EditRemote { target, production: true, sudo, .. } => {
                let endpoint =
                    crate::transfer::parse_endpoint(target, &self.known_servers).map_err(|e| format!("{e:#}"))?;
                let crate::transfer::Endpoint::Remote { server: s, .. } = endpoint else { return Ok(None) };
                if self.server_production(&s).is_none() {
                    return Ok(None);
                }
                let sudo = if *sudo { " --sudo" } else { "" };
                Ok(Some(Need {
                    kind: "edit".into(),
                    plan: format!("kraal edit{sudo} {target}"),
                    servers: vec![server(&s)],
                    reach: vec![self.connection(&s)],
                    act: Act::Request(req.clone()),
                }))
            }
            Request::OpenSsh { target, group, command, .. } if !command.is_empty() => {
                let reached = if *group {
                    let members = self.group_members(super::ssh::ssh_group(target));
                    if members.is_empty() {
                        return Err(format!("no server in group {:?}", super::ssh::ssh_group(target)));
                    }
                    members
                } else {
                    vec![self.resolve_server(target).ok_or_else(|| format!("not a server: {target}"))?]
                };
                let servers: Vec<String> =
                    reached.iter().filter(|s| self.server_production(s).is_some()).map(server).collect();
                let line = super::ssh::remote_command(command);
                Ok((!servers.is_empty()).then(|| Need {
                    kind: "ssh".into(),
                    plan: format!("{target}: {line}"),
                    servers,
                    reach: reached.iter().map(|s| self.connection(s)).collect(),
                    act: Act::Request(req.clone()),
                }))
            }
            Request::RunbookRun { token } => {
                let (_, d) = self.plans.get(token).ok_or("no such plan: review the runbook again")?;
                let servers = self.runbook_production(d);
                Ok((!servers.is_empty()).then(|| Need {
                    kind: "runbook".into(),
                    plan: self.plan(d, true),
                    servers,
                    // Checked again at launch, once the run is on record ([`Server::identities`]).
                    reach: self.identities(d),
                    act: Act::Request(req.clone()),
                }))
            }
            _ => Ok(None),
        }
    }

    /// The production targets of a runbook's plan, each with why it is production.
    pub(in crate::server) fn runbook_production(&self, d: &super::runbooks::Draft) -> Vec<String> {
        d.targets
            .iter()
            .flatten()
            .filter_map(|t| self.target_production(t, d.pane).map(|p| format!("{} ({})", self.target_label(t), p.why)))
            .collect()
    }

    /// Whether the caller may be an agent that is not asked here: one runs in its pane, or it
    /// runs in a pane of another session (or one kraal cannot place).
    pub(in crate::server) fn caller_maybe_agent(&self, client: ClientId) -> bool {
        match self.caller_place(client) {
            Place::Pane(pane) => self.pane_agent(pane).is_some(),
            Place::Elsewhere => true,
            Place::Outside => false,
        }
    }

    fn pane_agent(&self, pane: PaneId) -> Option<String> {
        let p = self.panes.get(&pane)?;
        p.agent
            .kind
            .clone()
            .or_else(|| p.foreground_argv().as_deref().and_then(crate::agent::detect_agent).map(str::to_string))
    }

    /// Where the caller runs: in the pane among whose processes it is; else in the pane both its
    /// environment (what a process its shell left behind keeps) and that of the nearest process
    /// it runs under name, if they name one of this session (they cannot be changed by one
    /// command); elsewhere in kraal if either names something else; outside kraal if neither
    /// names anything.
    fn caller_place(&self, client: ClientId) -> Place {
        let Some(c) = self.clients.get(&client) else { return Place::Outside };
        let lineage = c.peer.map(crate::process::lineage).unwrap_or_default();
        if let Some(pane) = self.panes.iter().find(|(_, p)| p.pid().is_some_and(|pid| lineage.contains(&pid))) {
            return Place::Pane(*pane.0);
        }
        let above = c.peer.and_then(crate::process::ppid).map(crate::process::kraal_env).unwrap_or_default();
        let above = (!above.is_empty()).then(|| Origin {
            session: above.get("KRAAL_SESSION").cloned().unwrap_or_default(),
            pane: above.get("KRAAL_PANE").cloned().unwrap_or_default(),
        });
        let named: Vec<&Origin> = c.origin.iter().chain(above.as_ref()).collect();
        let here = |o: &&Origin| {
            let pane = o.pane.parse::<PaneId>().ok().filter(|p| self.panes.contains_key(p))?;
            (o.session == self.session).then_some(pane)
        };
        match named.iter().map(here).collect::<Option<Vec<PaneId>>>() {
            None => Place::Elsewhere,
            Some(panes) => match panes.first() {
                None => Place::Outside,
                Some(first) if panes.iter().all(|p| p == first) => Place::Pane(*first),
                Some(_) => Place::Elsewhere,
            },
        }
    }

    /// What a server is reached with, as compared before an allowed action runs: its ssh
    /// arguments and destination, and its production identity.
    pub(in crate::server) fn connection(&self, s: &crate::servers::Server) -> String {
        let mut args = crate::servers::ssh_args(s, &self.known_servers, self.config.servers.multiplex);
        args.push(s.destination());
        format!("{} {:?}", crate::shell::join(&args), self.server_production(s))
    }

    /// Whether a key from client `id` comes before the user could see the question about action
    /// `ask`: the client has not said it displayed it (a client that reads slowly or far away, a
    /// key queued with the request or held then replayed), or just did.
    pub(super) fn ask_not_drawn(&self, id: ClientId, ask: u32) -> bool {
        self.production_asks.iter().any(|a| {
            a.id == ask
                && a.shown.is_some_and(|(c, d)| c == id && !matches!(d, Display::At(at) if at.elapsed() >= SEEN))
        })
    }

    /// A frame went to client `id`: the first since a question showed there carries it, and a
    /// mark follows it for the client to answer once that frame is on its terminal.
    pub(super) fn frame_sent(&mut self, id: ClientId) {
        let Some(at) = self.production_asks.iter().position(|a| a.shown == Some((id, Display::Pending))) else {
            return;
        };
        self.production_marks = self.production_marks.wrapping_add(1);
        let number = self.production_marks;
        let mut mark = Vec::new();
        let _ = protocol::write_json(&mut mark, &ServerMsg::DisplayMark { id: number });
        let Some(c) = self.clients.get_mut(&id) else { return };
        if c.out.try_send(mark).is_ok() {
            self.production_asks[at].shown = Some((id, Display::Marked(number)));
        } else {
            // A client behind: a whole frame carries the question again, then its mark.
            c.encoder.invalidate();
            c.dirty = Dirty::Yes;
            self.next_render = Some(std::time::Instant::now() + super::FRAME);
        }
    }

    /// Client `id` asks for a whole frame: its screen was covered (a local question of the
    /// client closed), so a question there counts as displayed only once that frame is.
    pub(super) fn production_repaint(&mut self, id: ClientId) {
        for (client, display) in self.production_asks.iter_mut().filter_map(|a| a.shown.as_mut()) {
            if *client == id {
                *display = Display::Pending;
            }
        }
    }

    /// Client `id` displayed what came before mark `mark`.
    pub(super) fn production_displayed(&mut self, id: ClientId, mark: u32) {
        if let Some(a) = self.production_asks.iter_mut().find(|a| a.shown == Some((id, Display::Marked(mark)))) {
            a.shown = Some((id, Display::At(std::time::Instant::now())));
        }
    }

    /// Ask about the first action waiting, unless a question about one shows already: on a
    /// client that shows its pane, else on the one used last, among those with no other question
    /// or form and whose input no transfer holds. With no client attached, nobody can answer:
    /// every action waiting is refused.
    pub(super) fn show_production_ask(&mut self) {
        if self.production_asks.is_empty() {
            return;
        }
        if !self.clients.values().any(|c| c.attached) {
            for ask in std::mem::take(&mut self.production_asks) {
                self.refused(&ask, "refused: nobody attached");
                ask.reply.complete(self, Err("nobody is attached to kraal to allow it: nothing was done".into()));
            }
            return;
        }
        let asking = |c: &super::Client| matches!(&c.mode, Mode::Confirm(q) if matches!(q.action, ConfirmAction::ApproveProduction(_)));
        if self.clients.values().any(asking) {
            return;
        }
        let pane = self.production_asks[0].pane;
        let Some(id) = self
            .clients
            .iter()
            .filter(|(_, c)| {
                c.attached
                    && matches!(c.mode, Mode::Normal)
                    && c.attachments.pending.is_none()
                    && c.attachments.blocked.is_none()
            })
            .max_by_key(|(id, c)| (self.pane_inner_rect(**id, pane).is_some(), c.last_input))
            .map(|(id, _)| *id)
        else {
            return;
        };
        self.production_asks[0].shown = Some((id, Display::Pending));
        let first = &self.production_asks[0];
        let quoted: Vec<String> = first.need.plan.lines().map(|l| format!(" {l}")).collect();
        let message = format!(
            "Allow {} in {} to act on production?\n{}\nProduction: {}\nNothing is done until you allow it.",
            super::account_ui::agent_name(&first.agent),
            self.pane_label(pane),
            quoted.join("\n"),
            first.need.servers.join(", ")
        );
        let Mode::Confirm(mut q) = Mode::confirm(message, ConfirmAction::ApproveProduction(first.id)) else {
            unreachable!()
        };
        q.title = Some(format!("Production · kraal {}", first.need.kind));
        let c = self.clients.get_mut(&id).unwrap();
        c.mode = Mode::Confirm(q);
        c.dirty = Dirty::Yes;
        self.mark_dirty_client(id);
    }

    /// The user's answer about action `id`: allowed, it runs as it was shown, once it is on
    /// record; refused, or no longer what was shown, nothing runs.
    pub(super) fn answer_production_ask(&mut self, id: u32, yes: bool) {
        let Some(at) = self.production_asks.iter().position(|a| a.id == id) else { return };
        let ask = self.production_asks.remove(at);
        if !yes {
            self.refused(&ask, "refused");
            ask.reply.complete(self, Err("refused in kraal: nothing was done".into()));
            return;
        }
        if let Err(changed) = self.still_holds(&ask.need) {
            self.refused(&ask, "refused: changed while asked");
            ask.reply.complete(self, Err(format!("{changed} since you were asked: nothing was done")));
            return;
        }
        if let Err(e) = self.log_production(&ask, "allowed") {
            let log = production_log();
            self.warnings.push(format!("{}: {e}", log.display()));
            ask.reply.complete(
                self,
                Err(format!("could not record the approval in {}: {e}; nothing was done", log.display())),
            );
            return;
        }
        let fields = self.production_fields(&ask, "allowed");
        self.emit("production.approved", fields);
        let ProductionAsk { reply, need, .. } = ask;
        let result = match need.act {
            Act::Reply => Ok(Response::Ok),
            Act::Input { pane, input, .. } => self.allow_input(pane, true).and_then(|()| match input {
                Input::Text(text) => self.send_text(pane, text),
                Input::Keys(keys) => self.send_keys(pane, &keys),
                Input::Paste(text) => self.paste_text(pane, &text),
            }),
            Act::Request(req) => match self.answer(reply, req) {
                RequestOutcome::Immediate { reply, result } => {
                    reply.complete(self, result);
                    return;
                }
                RequestOutcome::Deferred | RequestOutcome::Streaming => return,
            },
        };
        reply.complete(self, result);
    }

    /// Whether what `need` acts on is still what the question showed.
    fn still_holds(&self, need: &Need) -> Result<(), &'static str> {
        match &need.act {
            Act::Reply => Ok(()),
            Act::Input { pane, production, .. } => {
                if self.pane_production(*pane).as_ref() == Some(production) {
                    Ok(())
                } else {
                    Err("the pane changed")
                }
            }
            Act::Request(req) => match self.production_need(req) {
                Ok(Some(now)) if (&now.plan, &now.servers, &now.reach) == (&need.plan, &need.servers, &need.reach) => {
                    Ok(())
                }
                _ => Err("what it acts on changed"),
            },
        }
    }

    /// A client went away: the actions it asked for are not answered (their question goes),
    /// and one it showed is asked again elsewhere.
    pub(super) fn production_client_gone(&mut self, id: ClientId) {
        let gone: Vec<u32> = self.production_asks.iter().filter(|a| a.reply.client() == id).map(|a| a.id).collect();
        self.production_asks.retain(|a| a.reply.client() != id);
        for c in self.clients.values_mut() {
            if matches!(&c.mode, Mode::Confirm(q) if matches!(q.action, ConfirmAction::ApproveProduction(a) if gone.contains(&a)))
            {
                c.mode = Mode::Normal;
                c.dirty = Dirty::Yes;
            }
        }
        self.show_production_ask();
    }

    /// A refusal on record, and told to hooks. Nothing runs either way: a log that cannot be
    /// written is only reported.
    fn refused(&mut self, ask: &ProductionAsk, decision: &str) {
        if let Err(e) = self.log_production(ask, decision) {
            self.warnings.push(format!("{}: {e}", production_log().display()));
        }
        let fields = self.production_fields(ask, decision);
        self.emit("production.refused", fields);
    }

    fn production_fields(&self, ask: &ProductionAsk, decision: &str) -> Value {
        let mut fields = self.pane_fields(ask.pane);
        fields.insert("agent".into(), json!(ask.agent));
        fields.insert("kind".into(), json!(ask.need.kind));
        fields.insert("plan".into(), json!(ask.need.plan));
        fields.insert("servers".into(), json!(ask.need.servers));
        fields.insert("decision".into(), json!(decision));
        Value::Object(fields)
    }

    /// Your own action on production, or your answer to kraal's question about one (`allowed`,
    /// `refused`; `yours` when kraal does not ask), on record. A line that cannot be written is
    /// only reported: what you do yourself goes on.
    pub(in crate::server) fn record_yours(
        &mut self,
        pane: Option<PaneId>,
        kind: &str,
        plan: &str,
        servers: Vec<String>,
        decision: &str,
    ) {
        let mut fields = pane.map(|p| self.pane_fields(p)).unwrap_or_default();
        fields.insert("agent".into(), Value::Null);
        fields.insert("kind".into(), json!(kind));
        fields.insert("plan".into(), json!(plan));
        fields.insert("servers".into(), json!(servers));
        fields.insert("decision".into(), json!(decision));
        if let Err(e) = self.append_production_log(fields) {
            self.warnings.push(format!("{}: {e}", production_log().display()));
        }
    }

    /// Append the answer about `ask` to the production log.
    fn log_production(&self, ask: &ProductionAsk, decision: &str) -> std::io::Result<()> {
        let Value::Object(fields) = self.production_fields(ask, decision) else { unreachable!() };
        self.append_production_log(fields)
    }

    /// Append `fields`, with when and the session, to the production log: a whole line under
    /// the lock that sessions writing the same file share.
    fn append_production_log(&self, fields: serde_json::Map<String, Value>) -> std::io::Result<()> {
        let mut line = serde_json::Map::new();
        line.insert("time".into(), json!(super::events::now_ms()));
        line.insert("session".into(), json!(self.session));
        line.extend(fields);
        let mut text = format!("{}\n", Value::Object(line));
        let log = production_log();
        if let Some(dir) = log.parent() {
            crate::paths::ensure_private_dir(dir)?;
        }
        crate::paths::locked(&log, || {
            let mut f = super::events::open_log(&log)?;
            // A line a failed write cut short is ended first: this one starts on its own.
            if f.metadata()?.len() > 0 && last_byte(&log)? != Some(b'\n') {
                text.insert(0, '\n');
            }
            f.write_all(text.as_bytes())
        })?
    }
}

/// The last byte of the file at `path`, not followed if it is a link.
fn last_byte(path: &std::path::Path) -> std::io::Result<Option<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?;
    if f.metadata()?.len() == 0 {
        return Ok(None);
    }
    f.seek(std::io::SeekFrom::End(-1))?;
    let mut byte = [0u8];
    f.read_exact(&mut byte)?;
    Ok(Some(byte[0]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Origin, Placement};
    use crate::server::requests::tests::{finish_job, response};
    use crate::server::tests::{client, server};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    struct Setup {
        server: Server,
        rx: crate::server::queue::EventReceiver,
        ui: ClientId,
        _ui_peer: UnixStream,
        agent: PaneId,
        shell: PaneId,
        /// A production pane whose program writes what it reads to `got`.
        prod: PaneId,
        dir: crate::testing::TempDir,
    }

    /// An attached client, a pane where Claude runs, a pane without an agent, a production pane
    /// and a production server in the group "prod".
    fn setup() -> Setup {
        let (tx, rx) = crate::server::event_channel(4096);
        let mut server = server(tx);
        let (ui, ui_peer) = client(&mut server);
        let ws = server.clients[&ui].workspace;
        let dir = crate::testing::TempDir::new("production-asks");
        let got = crate::shell::quote(&dir.0.join("got").display().to_string());
        let mut spawn = |command: Vec<String>| {
            let spec = crate::pane::PaneSpec { command, ..Default::default() };
            server.spawn_pane_in(ws, None, spec, Placement::Tab, None).unwrap()
        };
        let agent = spawn(vec!["cat".into()]);
        let shell = spawn(vec!["cat".into()]);
        let prod = spawn(vec!["sh".into(), "-c".into(), format!("stty -echo; exec cat > {got}")]);
        server.panes.get_mut(&agent).unwrap().agent.kind = Some("claude".into());
        server.panes.get_mut(&prod).unwrap().ssh =
            Some(crate::servers::Server { name: "prod".into(), host: "prod".into(), ..Default::default() });
        let web = crate::servers::Server {
            name: "web-prod-1".into(),
            host: "web-prod-1".into(),
            groups: vec!["prod".into()],
            ..Default::default()
        };
        server.known_servers = vec![crate::servers::Known { server: web, source: crate::servers::Source::Inventory }];
        Setup { server, rx, ui, _ui_peer: ui_peer, agent, shell, prod, dir }
    }

    type Caller = (ClientId, UnixStream, crossbeam_channel::Receiver<Vec<u8>>);

    /// A command-line client whose process runs in `pane`, and what it is answered.
    fn caller(server: &mut Server, pane: PaneId) -> Caller {
        let pid = server.panes[&pane].pid();
        from(server, pid, None)
    }

    /// A command-line client of process `peer`, run from `origin` as its environment says.
    fn from(server: &mut Server, peer: Option<u32>, origin: Option<Origin>) -> Caller {
        let (id, stream) = client(server);
        let c = server.clients.get_mut(&id).unwrap();
        c.attached = false;
        c.peer = peer;
        c.origin = origin;
        let receive = c.out.capture();
        (id, stream, receive)
    }

    fn exec(marker: &str) -> Request {
        Request::ApproveProduction {
            kind: "exec".into(),
            plan: format!("kraal exec @web -- {marker}"),
            servers: vec!["web-prod-1".into()],
        }
    }

    fn asking(server: &Server, id: ClientId) -> Option<u32> {
        match &server.clients[&id].mode {
            Mode::Confirm(q) => match q.action {
                ConfirmAction::ApproveProduction(ask) => Some(ask),
                _ => None,
            },
            _ => None,
        }
    }

    fn question(server: &Server, id: ClientId) -> String {
        let Mode::Confirm(q) = &server.clients[&id].mode else { panic!("no question") };
        q.message.clone()
    }

    /// The user answers the question shown on `ui`.
    fn answer(s: &mut Setup, yes: bool) {
        let ask = asking(&s.server, s.ui).expect("the user is asked");
        s.server.answer_production_ask(ask, yes);
        s.server.clients.get_mut(&s.ui).unwrap().mode = Mode::Normal;
    }

    fn logged(marker: &str) -> Value {
        let log = std::fs::read_to_string(production_log()).unwrap();
        let line = log.lines().rev().find(|l| l.contains(marker)).expect("the answer is logged");
        serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"))
    }

    fn immediate(server: &mut Server, outcome: RequestOutcome) -> Result<Response, String> {
        let RequestOutcome::Immediate { reply, result } = outcome else { panic!("an immediate answer") };
        reply.complete(server, result.clone());
        result
    }

    /// A frame goes to `ui`, and its client says it displayed the question, as a client does.
    fn displayed(s: &mut Setup) {
        s.server.render_clients();
        asking(&s.server, s.ui).expect("the user is asked");
        let mark = s
            .server
            .production_asks
            .iter()
            .find_map(|a| match a.shown {
                Some((c, Display::Marked(mark))) if c == s.ui => Some(mark),
                _ => None,
            })
            .expect("the frame showing the question was marked");
        let ack = serde_json::to_vec(&crate::protocol::ClientMsg::Displayed { id: mark }).unwrap();
        s.server.client_frame(s.ui, protocol::JSON, ack);
    }

    fn typed(s: &Setup) -> bool {
        s.server.panes[&s.prod].agent.typed
    }

    /// What the production pane's program read, once it holds `text`.
    fn got(s: &Setup, text: &str) -> String {
        let deadline = Instant::now() + crate::testing::PATIENCE;
        loop {
            let got = std::fs::read_to_string(s.dir.0.join("got")).unwrap_or_default();
            if got.contains(text) || Instant::now() > deadline {
                return got;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_production_action_outside_an_agents_pane_goes_on_at_once() {
        let mut s = setup();
        let (shell, _peer, _receive) = caller(&mut s.server, s.shell);
        let outcome = s.server.request(shell, exec("outside-agent"));
        assert!(matches!(immediate(&mut s.server, outcome), Ok(Response::Ok)));
        assert_eq!(asking(&s.server, s.ui), None);
        // Yours, on record all the same.
        let line = logged("outside-agent");
        assert_eq!((line["decision"].as_str(), line["agent"].is_null()), (Some("yours"), true));
        assert_eq!((line["kind"].as_str(), &line["pane"]), (Some("exec"), &json!(s.shell)));
        assert_eq!(line["servers"], json!(["web-prod-1"]));
    }

    #[test]
    fn an_agents_production_action_waits_for_the_users_answer() {
        let mut s = setup();
        for (yes, marker) in [(true, "agent-allowed"), (false, "agent-refused")] {
            let (id, _peer, receive) = caller(&mut s.server, s.agent);
            assert!(matches!(s.server.request(id, exec(marker)), RequestOutcome::Deferred));
            let message = question(&s.server, s.ui);
            assert!(message.contains(&format!(" kraal exec @web -- {marker}")) && message.contains("web-prod-1"));
            assert!(receive.is_empty(), "nothing is answered before the user");
            answer(&mut s, yes);
            if yes {
                assert!(matches!(response(&receive), Ok(Response::Ok)));
            } else {
                assert_eq!(response(&receive).unwrap_err(), "refused in kraal: nothing was done");
            }
            let line = logged(marker);
            assert_eq!(line["decision"], if yes { "allowed" } else { "refused" });
            assert_eq!((line["agent"].as_str(), line["kind"].as_str()), (Some("claude"), Some("exec")));
            assert_eq!(line["pane"], json!(s.agent));
            assert_eq!(line["servers"], json!(["web-prod-1"]));
        }
    }

    /// Nothing is typed before the answer nor after a refusal; once allowed, what was asked about
    /// is typed, even when the buffer changed meanwhile.
    #[test]
    fn typing_waits_for_the_answer_and_types_what_was_asked_about() {
        let mut s = setup();
        let (id, _peer, receive) = caller(&mut s.server, s.agent);
        let send = Request::SendText { pane: s.prod.to_string(), text: "refused-text\n".into(), production: true };
        assert!(matches!(s.server.request(id, send), RequestOutcome::Deferred));
        assert!(!typed(&s));
        answer(&mut s, false);
        assert_eq!(response(&receive).unwrap_err(), "refused in kraal: nothing was done");
        assert!(!typed(&s));
        let set = |s: &mut Setup, text: &str| {
            let (human, _peer, _receive) = caller(&mut s.server, s.shell);
            let outcome = s.server.request(human, Request::BufferSet { name: None, text: text.into() });
            immediate(&mut s.server, outcome).unwrap();
        };
        set(&mut s, "pasted-first\n");
        let (id, _peer, receive) = caller(&mut s.server, s.agent);
        let paste = Request::BufferPaste { buffer: None, pane: s.prod.to_string(), production: true };
        assert!(matches!(s.server.request(id, paste), RequestOutcome::Deferred));
        assert!(question(&s.server, s.ui).contains(" pasted-first"));
        set(&mut s, "pasted-second\n");
        answer(&mut s, true);
        assert!(matches!(response(&receive), Ok(Response::Ok)));
        let got = got(&s, "pasted-first");
        assert!(got.contains("pasted-first") && !got.contains("pasted-second") && !got.contains("refused"), "{got:?}");
        assert_eq!(logged("pasted-first")["kind"], "buffer paste");
    }

    #[test]
    fn a_pane_that_changed_while_asked_gets_nothing() {
        let mut s = setup();
        let (id, _peer, receive) = caller(&mut s.server, s.agent);
        let send = Request::SendText { pane: s.prod.to_string(), text: "changed-pane\n".into(), production: true };
        assert!(matches!(s.server.request(id, send), RequestOutcome::Deferred));
        s.server.panes.get_mut(&s.prod).unwrap().ssh =
            Some(crate::servers::Server { name: "prod-2".into(), host: "prod-2".into(), ..Default::default() });
        answer(&mut s, true);
        assert_eq!(response(&receive).unwrap_err(), "the pane changed since you were asked: nothing was done");
        assert!(!typed(&s));
        assert_eq!(logged("changed-pane")["decision"], "refused: changed while asked");
    }

    /// `kraal ssh --group @prod -- …` is asked about with the line the servers run, and a group
    /// kraal cannot find is refused, not let through.
    #[test]
    fn an_agents_ssh_to_a_group_is_asked_about_as_it_runs() {
        let mut s = setup();
        let ssh = |target: &str| Request::OpenSsh {
            target: target.into(),
            group: true,
            placement: Placement::Tab,
            near: None,
            command: vec!["echo".into(), "'ok;".into(), "touch".into(), "x'".into()],
        };
        let (id, _peer, receive) = caller(&mut s.server, s.agent);
        assert!(matches!(s.server.request(id, ssh("@prod")), RequestOutcome::Deferred));
        let message = question(&s.server, s.ui);
        assert!(message.contains(" @prod: echo 'ok; touch x'"), "{message}");
        assert!(message.contains("web-prod-1 (web-prod-1)"), "{message}");
        answer(&mut s, false);
        assert!(response(&receive).is_err());
        let (id, _peer, _receive) = caller(&mut s.server, s.agent);
        let outcome = s.server.request(id, ssh("@nowhere"));
        assert_eq!(immediate(&mut s.server, outcome).unwrap_err(), "no server in group \"nowhere\": nothing was done");
    }

    /// From an agent's pane, a runbook on production that lists choices with commands is refused,
    /// for its plan and for the run of a plan a person made: those commands would run before the
    /// user is asked.
    #[test]
    fn an_agents_production_runbook_with_choice_commands_is_refused() {
        let mut s = setup();
        let book = "on = 'pane'\nsteps = ['true']\n[[param]]\nname = 'VALUE'\ndefault = 'yes'\nchoices_command = 'printf yes'\n";
        let path = s.dir.write("deploy.toml", book);
        let plan = |s: &Setup| Request::RunbookPlan {
            path: path.clone(),
            target: None,
            params: vec![],
            pane: Some(s.prod.to_string()),
        };
        let (id, _peer, _receive) = caller(&mut s.server, s.agent);
        let outcome = s.server.request(id, plan(&s));
        assert!(immediate(&mut s.server, outcome).unwrap_err().contains("lists choices with commands"));
        assert!(s.server.jobs.is_empty(), "no command listed choices");
        let (human, _peer, receive) = caller(&mut s.server, s.shell);
        let job = crate::server::jobs::JobId(s.server.next_id);
        assert!(matches!(s.server.request(human, plan(&s)), RequestOutcome::Deferred));
        finish_job(&mut s.server, &s.rx, job);
        let Response::Plan { token, .. } = response(&receive).unwrap() else { panic!("a plan") };
        let (id, _peer, _receive) = caller(&mut s.server, s.agent);
        let outcome = s.server.request(id, Request::RunbookRun { token });
        assert!(immediate(&mut s.server, outcome).unwrap_err().contains("lists choices with commands"));
        // From a pane of another session too, before any of those commands runs.
        let jobs = s.server.jobs.len();
        let elsewhere = Origin { session: "other".into(), pane: "1".into() };
        let (id, _peer, _receive) = from(&mut s.server, None, Some(elsewhere));
        let outcome = s.server.request(id, plan(&s));
        assert!(immediate(&mut s.server, outcome).unwrap_err().contains("lists choices with commands"));
        assert_eq!(s.server.jobs.len(), jobs, "no command listed choices");
    }

    /// A runbook target's jump host that moved while the run was asked about: nothing starts.
    #[test]
    fn a_runbook_whose_jump_host_changed_while_asked_is_refused() {
        let mut s = setup();
        let bastion = crate::servers::Server { name: "bastion".into(), host: "10.0.0.1".into(), ..Default::default() };
        s.server
            .known_servers
            .push(crate::servers::Known { server: bastion, source: crate::servers::Source::Inventory });
        s.server.known_servers[0].server.jump = Some("bastion".into());
        let path = s.dir.write("deploy.toml", "on = 'server'\nsteps = ['true']\n");
        let (id, _peer, receive) = caller(&mut s.server, s.agent);
        let plan = Request::RunbookPlan { path, target: Some("web-prod-1".into()), params: vec![], pane: None };
        let plan = match s.server.request(id, plan) {
            RequestOutcome::Deferred => response(&receive),
            outcome => immediate(&mut s.server, outcome),
        };
        let Response::Plan { token, .. } = plan.unwrap() else { panic!("a plan") };
        let (id, _peer, receive) = caller(&mut s.server, s.agent);
        assert!(matches!(s.server.request(id, Request::RunbookRun { token }), RequestOutcome::Deferred));
        s.server.known_servers[1].server.host = "10.0.0.2".into();
        answer(&mut s, true);
        assert_eq!(response(&receive).unwrap_err(), "what it acts on changed since you were asked: nothing was done");
    }

    /// An agent's runbook on production without such commands is asked about, then runs.
    #[test]
    fn an_agents_production_runbook_waits_for_the_answer() {
        let mut s = setup();
        let path = s.dir.write("deploy.toml", "on = 'pane'\nsteps = ['true']\n");
        let (id, _peer, receive) = caller(&mut s.server, s.agent);
        let plan = Request::RunbookPlan { path, target: None, params: vec![], pane: Some(s.prod.to_string()) };
        let plan = match s.server.request(id, plan) {
            RequestOutcome::Deferred => response(&receive),
            outcome => immediate(&mut s.server, outcome),
        };
        let Response::Plan { token, .. } = plan.unwrap() else { panic!("a plan") };
        let (id, _peer, receive) = caller(&mut s.server, s.agent);
        assert!(matches!(s.server.request(id, Request::RunbookRun { token }), RequestOutcome::Deferred));
        assert!(question(&s.server, s.ui).contains("prod"));
        answer(&mut s, true);
        // Allowed, then started: this test server has no run log writer.
        assert_eq!(response(&receive).unwrap_err(), "the run log writer is gone: nothing was started");
    }

    /// An allowed edit whose work cannot start still answers its caller.
    #[test]
    fn an_allowed_edit_still_answers() {
        let mut s = setup();
        let (id, _peer, receive) = caller(&mut s.server, s.agent);
        let edit = Request::EditRemote {
            target: "web-prod-1:/etc/app.conf".into(),
            line: None,
            near: None,
            production: true,
            sudo: false,
        };
        assert!(matches!(s.server.request(id, edit), RequestOutcome::Deferred));
        assert!(question(&s.server, s.ui).contains(" kraal edit web-prod-1:/etc/app.conf"));
        crate::server::jobs::FAIL_START.set(true);
        answer(&mut s, true);
        crate::server::jobs::FAIL_START.set(false);
        assert!(response(&receive).is_err());
    }

    #[test]
    fn with_nobody_attached_an_agents_action_is_refused_at_once() {
        let mut s = setup();
        s.server.clients.get_mut(&s.ui).unwrap().attached = false;
        let (id, _peer, _receive) = caller(&mut s.server, s.agent);
        let outcome = s.server.request(id, exec("nobody-attached"));
        assert!(immediate(&mut s.server, outcome).unwrap_err().contains("nothing was done"));
        assert_eq!(logged("nobody-attached")["decision"], "refused: nobody attached");
    }

    #[test]
    fn a_question_nobody_answers_still_gets_its_caller_an_answer() {
        let mut s = setup();
        // The client asked leaves: with nobody else attached, the action is refused.
        let (id, _peer, receive) = caller(&mut s.server, s.agent);
        assert!(matches!(s.server.request(id, exec("client-left")), RequestOutcome::Deferred));
        s.server.drop_client(s.ui);
        assert!(response(&receive).unwrap_err().contains("nothing was done"));
        assert!(s.server.production_asks.is_empty());
        // The caller leaves: its question goes, and the next one is asked.
        let mut s = setup();
        let (first, _p1, _r1) = caller(&mut s.server, s.agent);
        let (second, _p2, _r2) = caller(&mut s.server, s.agent);
        assert!(matches!(s.server.request(first, exec("caller-left")), RequestOutcome::Deferred));
        assert!(matches!(s.server.request(second, exec("next")), RequestOutcome::Deferred));
        let asked = asking(&s.server, s.ui).unwrap();
        s.server.drop_client(first);
        let next = asking(&s.server, s.ui).expect("the next question shows");
        assert_ne!(asked, next);
        assert_eq!(s.server.production_asks.len(), 1);
    }

    /// A client whose input a transfer holds is not asked; it is once the transfer ends. Among
    /// those free, the one that shows the agent's pane is asked, before one used later.
    #[test]
    fn the_question_goes_to_a_client_free_to_answer_that_shows_the_agent() {
        let mut s = setup();
        s.server.clients.get_mut(&s.ui).unwrap().attachments.blocked = Some("transfer".into());
        let (id, _peer, _receive) = caller(&mut s.server, s.agent);
        assert!(matches!(s.server.request(id, exec("reserved")), RequestOutcome::Deferred));
        assert_eq!(asking(&s.server, s.ui), None);
        s.server.clients.get_mut(&s.ui).unwrap().attachments.blocked = None;
        s.server.show_production_ask();
        assert!(asking(&s.server, s.ui).is_some());
        answer(&mut s, false);
        let (other, _other_peer) = client(&mut s.server);
        s.server.reveal(s.ui, s.agent);
        s.server.clients.get_mut(&other).unwrap().last_input = Instant::now() + Duration::from_secs(1);
        let (id, _peer, _receive) = caller(&mut s.server, s.agent);
        assert!(matches!(s.server.request(id, exec("shown")), RequestOutcome::Deferred));
        assert!(asking(&s.server, s.ui).is_some() && asking(&s.server, other).is_none());
    }

    /// A command its shell left behind (its parents gone) still counts as the agent's by the pane
    /// its environment names; a pane of another session names nothing here.
    #[test]
    fn a_command_counts_as_the_agents_by_the_pane_it_names() {
        let mut s = setup();
        let here = Some(Origin { session: s.server.session.clone(), pane: s.agent.to_string() });
        let (id, _peer, _receive) = from(&mut s.server, None, here);
        assert!(matches!(s.server.request(id, exec("declared")), RequestOutcome::Deferred));
        answer(&mut s, false);
        let elsewhere = Some(Origin { session: "another".into(), pane: s.agent.to_string() });
        let (id, _peer, _receive) = from(&mut s.server, None, elsewhere);
        let outcome = s.server.request(id, exec("elsewhere"));
        assert!(immediate(&mut s.server, outcome).unwrap_err().contains("from a pane of another kraal session"));
    }

    /// A command that dropped the kraal variables still counts by the process it runs under: as
    /// the agent's when that one names the agent's pane here, as another session's otherwise.
    #[test]
    fn a_command_that_dropped_its_variables_counts_by_the_process_it_runs_under() {
        use std::io::BufRead;
        let mut s = setup();
        for (session, here) in [(s.server.session.clone(), true), ("another".to_string(), false)] {
            let mut sh = std::process::Command::new("sh")
                .args(["-c", "env -u KRAAL_SESSION -u KRAAL_PANE sleep 30 & echo $!; wait"])
                .env("KRAAL_SESSION", &session)
                .env("KRAAL_PANE", s.agent.to_string())
                .env_remove("KRAAL_SOCKET")
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(sh.stdout.take().unwrap()).read_line(&mut line).unwrap();
            let sleep: u32 = line.trim().parse().unwrap();
            let (id, _peer, _receive) = from(&mut s.server, Some(sleep), None);
            let outcome = s.server.request(id, exec(&format!("dropped-{here}")));
            if here {
                assert!(matches!(outcome, RequestOutcome::Deferred), "asked about as the agent's");
                answer(&mut s, false);
            } else {
                assert!(immediate(&mut s.server, outcome).unwrap_err().contains("another kraal session"));
            }
            // SAFETY: kill sends a signal to the process this test started.
            unsafe { libc::kill(sleep as i32, libc::SIGKILL) };
            sh.wait().unwrap();
        }
    }

    /// What an allowed action reaches is compared with what was asked about, beyond the names
    /// shown: a server's port, a group's members, a pane's server.
    #[test]
    fn an_action_whose_reach_changed_while_asked_is_refused() {
        let ssh = Request::OpenSsh {
            target: "@prod".into(),
            group: true,
            placement: Placement::Tab,
            near: None,
            command: vec!["uptime".into()],
        };
        let changes: [fn(&mut Setup); 2] = [
            |s| s.server.known_servers[0].server.port = Some(2222),
            |s| {
                let mut other = s.server.known_servers[0].server.clone();
                other.name = "web-prod-2".into();
                s.server
                    .known_servers
                    .push(crate::servers::Known { server: other, source: crate::servers::Source::Inventory });
            },
        ];
        for change in changes {
            let mut s = setup();
            let (id, _peer, receive) = caller(&mut s.server, s.agent);
            assert!(matches!(s.server.request(id, ssh.clone()), RequestOutcome::Deferred));
            change(&mut s);
            answer(&mut s, true);
            assert_eq!(
                response(&receive).unwrap_err(),
                "what it acts on changed since you were asked: nothing was done"
            );
        }
        // The same server name on another host is another server.
        let mut s = setup();
        let (id, _peer, receive) = caller(&mut s.server, s.agent);
        let send = Request::SendText { pane: s.prod.to_string(), text: "moved\n".into(), production: true };
        assert!(matches!(s.server.request(id, send), RequestOutcome::Deferred));
        s.server.panes.get_mut(&s.prod).unwrap().ssh =
            Some(crate::servers::Server { name: "prod".into(), host: "prod-b".into(), ..Default::default() });
        answer(&mut s, true);
        assert_eq!(response(&receive).unwrap_err(), "the pane changed since you were asked: nothing was done");
        assert!(!typed(&s));
    }

    /// Keys that come before a frame showed the question do not answer it (held while a transfer
    /// ran and replayed, arriving with the key that freed the client, or queued with the request
    /// before the next frame); later ones do.
    #[test]
    fn keys_typed_before_the_question_showed_do_not_answer_it() {
        let mut s = setup();
        s.server.clients.get_mut(&s.ui).unwrap().attachments.blocked = Some("transfer".into());
        let (id, _peer, receive) = caller(&mut s.server, s.agent);
        assert!(matches!(s.server.request(id, exec("early-keys")), RequestOutcome::Deferred));
        assert_eq!(asking(&s.server, s.ui), None);
        s.server.clients.get_mut(&s.ui).unwrap().attachments.blocked = None;
        s.server.client_input(s.ui, b"ay");
        assert!(asking(&s.server, s.ui).is_some(), "the question shows, unanswered");
        s.server.client_input(s.ui, b"y");
        assert!(receive.is_empty(), "no frame showed the question yet");
        displayed(&mut s);
        std::thread::sleep(SEEN);
        s.server.client_input(s.ui, b"y");
        assert!(matches!(response(&receive), Ok(Response::Ok)));
        // A request and a key both handled before the next frame.
        let (id, _peer, receive) = caller(&mut s.server, s.agent);
        assert!(matches!(s.server.request(id, exec("queued-key")), RequestOutcome::Deferred));
        s.server.client_input(s.ui, b"y");
        assert!(asking(&s.server, s.ui).is_some() && receive.is_empty());
        displayed(&mut s);
        std::thread::sleep(SEEN);
        s.server.client_input(s.ui, b"y");
        assert!(matches!(response(&receive), Ok(Response::Ok)));
    }

    /// A question shows once its client says it displayed it, which a client far away (`attach
    /// --host`) or reading slowly says late: keys before that, and in the moment after, do not
    /// answer it; later ones do.
    #[test]
    fn keys_before_the_client_displayed_the_question_do_not_answer_it() {
        let mut s = setup();
        let held = s.server.clients.get_mut(&s.ui).unwrap().out.capture();
        let (id, _peer, receive) = caller(&mut s.server, s.agent);
        assert!(matches!(s.server.request(id, exec("far-client")), RequestOutcome::Deferred));
        let ask = asking(&s.server, s.ui).unwrap();
        s.server.render_clients();
        // The frame showing the question, then the mark the client answers.
        let sent: Vec<u8> = held.try_iter().flatten().collect();
        let mut frames = std::io::Cursor::new(sent);
        let mut marks = Vec::new();
        while let Some((kind, payload)) = protocol::read_frame(&mut frames).unwrap() {
            if kind == protocol::JSON
                && let Ok(ServerMsg::DisplayMark { id }) = serde_json::from_slice(&payload)
            {
                marks.push(id);
            }
        }
        let [mark] = marks[..] else { panic!("one mark after the frame: {marks:?}") };
        assert_eq!(asking(&s.server, s.ui), Some(ask));
        std::thread::sleep(SEEN);
        s.server.client_input(s.ui, b"y");
        assert!(asking(&s.server, s.ui).is_some() && receive.is_empty(), "not displayed: no answer");
        // Another mark, or this one from another client, displays nothing here.
        let other = serde_json::to_vec(&crate::protocol::ClientMsg::Displayed { id: mark + 1 }).unwrap();
        s.server.client_frame(s.ui, protocol::JSON, other);
        let ack = serde_json::to_vec(&crate::protocol::ClientMsg::Displayed { id: mark }).unwrap();
        s.server.client_frame(id, protocol::JSON, ack.clone());
        s.server.client_input(s.ui, b"y");
        assert!(receive.is_empty(), "nothing displayed it on this client");
        s.server.client_frame(s.ui, protocol::JSON, ack);
        s.server.client_input(s.ui, b"y");
        assert!(receive.is_empty(), "just displayed: not read yet");
        std::thread::sleep(SEEN);
        s.server.client_input(s.ui, b"y");
        assert!(matches!(response(&receive), Ok(Response::Ok)));
    }

    /// A local question of the client covered the production question (laptop files over
    /// `attach --host`): its repaint makes the question count as displayed only once the client
    /// says again that it drew it.
    #[test]
    fn a_question_covered_on_the_client_is_displayed_again_after_its_repaint() {
        let mut s = setup();
        let held = s.server.clients.get_mut(&s.ui).unwrap().out.capture();
        let marks = |held: &crossbeam_channel::Receiver<Vec<u8>>| {
            let mut frames = std::io::Cursor::new(held.try_iter().flatten().collect::<Vec<u8>>());
            let mut marks = Vec::new();
            while let Some((kind, payload)) = protocol::read_frame(&mut frames).unwrap() {
                if kind == protocol::JSON
                    && let Ok(ServerMsg::DisplayMark { id }) = serde_json::from_slice(&payload)
                {
                    marks.push(id);
                }
            }
            marks
        };
        let (id, _peer, receive) = caller(&mut s.server, s.agent);
        assert!(matches!(s.server.request(id, exec("covered")), RequestOutcome::Deferred));
        assert!(asking(&s.server, s.ui).is_some());
        s.server.render_clients();
        let [first] = marks(&held)[..] else { panic!("one mark after the frame") };
        let ack = |mark| serde_json::to_vec(&crate::protocol::ClientMsg::Displayed { id: mark }).unwrap();
        // The client's own question covered it until now: the first mark is still on its way.
        let repaint = serde_json::to_vec(&crate::protocol::ClientMsg::Repaint).unwrap();
        s.server.client_frame(s.ui, protocol::JSON, repaint);
        s.server.client_input(s.ui, b"y");
        assert!(asking(&s.server, s.ui).is_some() && receive.is_empty(), "covered: no answer");
        s.server.render_clients();
        let [again] = marks(&held)[..] else { panic!("the repainted frame carries a mark") };
        assert_ne!(again, first, "a mark of its own");
        // The first mark answered late, once the local question closed, displays nothing.
        s.server.client_frame(s.ui, protocol::JSON, ack(first));
        std::thread::sleep(SEEN);
        s.server.client_input(s.ui, b"y");
        assert!(receive.is_empty(), "an earlier display's mark: no answer");
        s.server.client_frame(s.ui, protocol::JSON, ack(again));
        std::thread::sleep(SEEN);
        s.server.client_input(s.ui, b"y");
        assert!(matches!(response(&receive), Ok(Response::Ok)));
    }

    /// An approval that cannot be put on record runs nothing.
    #[test]
    fn an_approval_that_cannot_be_recorded_runs_nothing() {
        if !crate::testing::in_own_process("server::approvals::tests::an_approval_that_cannot_be_recorded_runs_nothing")
        {
            return;
        }
        let mut s = setup();
        std::fs::create_dir_all(production_log()).unwrap();
        let (id, _peer, receive) = caller(&mut s.server, s.agent);
        let send = Request::SendText { pane: s.prod.to_string(), text: "unrecorded\n".into(), production: true };
        assert!(matches!(s.server.request(id, send), RequestOutcome::Deferred));
        answer(&mut s, true);
        assert!(response(&receive).unwrap_err().contains("could not record the approval"));
        assert!(!typed(&s));
    }

    /// A line a failed write cut short is ended before the next one.
    #[test]
    fn a_line_cut_short_is_ended_before_the_next() {
        if !crate::testing::in_own_process("server::approvals::tests::a_line_cut_short_is_ended_before_the_next") {
            return;
        }
        let mut s = setup();
        std::fs::create_dir_all(production_log().parent().unwrap()).unwrap();
        std::fs::write(production_log(), "{\"cut").unwrap();
        let (id, _peer, _receive) = caller(&mut s.server, s.agent);
        assert!(matches!(s.server.request(id, exec("after-cut")), RequestOutcome::Deferred));
        answer(&mut s, false);
        let log = std::fs::read_to_string(production_log()).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines[0], "{\"cut");
        assert!(serde_json::from_str::<Value>(lines[1]).is_ok_and(|v| v["decision"] == "refused"), "{log}");
    }
}
