//! Preparing a step runbook: trust, target, parameters, then the plan to approve (from the
//! picker, or from `kraal runbook run`).

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};

use super::{Draft, PLAN_LIFETIME, Target, first_line, list_choices};
use crate::layout::PaneId;
use crate::protocol::Response;
use crate::runbooks::{self, Body, On, Source};
use crate::server::requests::ReplyHandle;
use crate::server::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind, Prompt, PromptAction};
use crate::server::{ClientId, Server};
use crate::theme::NoticeKind;
use crate::{paths, servers};

impl Server {
    /// Ask for what the draft still needs, in order: trust (a repository runbook, before any of
    /// its commands runs), target, parameters; then show the plan.
    pub(in crate::server) fn advance_draft(&mut self, client: ClientId) -> Mode {
        let Some(d) = self.drafts.get(&client) else { return Mode::Normal };
        if !d.trusted {
            return Mode::confirm(d.trust_question(), ConfirmAction::TrustRunbook);
        }
        if d.targets.is_none() {
            let targets = match d.steps.on {
                On::Local => vec![Target::Local],
                On::Pane => vec![d.pane.map(Target::Pane).unwrap_or(Target::Local)],
                On::Server => return Mode::Picker(self.target_picker(d.pane, &d.runbook.name)),
            };
            self.drafts.get_mut(&client).unwrap().targets = Some(targets);
        }
        let d = &self.drafts[&client];
        let Some(param) = d.steps.params.get(d.values.len()).cloned() else {
            let message = self.plan(d, false);
            let reviewed = self.identities(d);
            let production = self.runbook_production(d);
            if let Some(d) = self.drafts.get_mut(&client) {
                d.reviewed = Some(reviewed);
                d.asked = Some((message.clone(), production));
            }
            return Mode::confirm(message, ConfirmAction::RunRunbook);
        };
        match (&param.choices_command, &d.choices) {
            // List the choices in the background: the command may be slow.
            (Some(cmd), None) if param.choices.is_empty() => {
                let (cmd, cwd, vars) = (cmd.clone(), self.draft_cwd(d), self.draft_vars(d));
                let (draft, asked) = (d.id, d.values.len());
                self.toast(client, NoticeKind::Notice, format!("listing choices for {}…", param.name));
                self.background(
                    move || list_choices(&cmd, cwd.as_deref(), &vars),
                    move |server, listed| {
                        // Only for the question that asked: not a newer draft, not a later one.
                        let current =
                            server.drafts.get(&client).is_some_and(|d| d.id == draft && d.values.len() == asked);
                        if !current {
                            return;
                        }
                        let choices = listed.unwrap_or_else(|e| {
                            // Without its list, the parameter is asked as text.
                            server.toast(client, NoticeKind::Error, format!("{e:#}"));
                            Vec::new()
                        });
                        if let Some(d) = server.drafts.get_mut(&client) {
                            d.choices = Some(choices);
                        }
                        let next = server.advance_draft(client);
                        server.set_mode_if_idle(client, next);
                    },
                );
                Mode::Normal
            }
            _ => self.param_question(client),
        }
    }

    pub(in crate::server) fn confirm_trust(&mut self, client: ClientId, yes: bool) -> Mode {
        if !yes {
            self.drafts.remove(&client);
            return Mode::Normal;
        }
        let Some(d) = self.drafts.get_mut(&client) else { return Mode::Normal };
        if let Err(e) = runbooks::trust(&d.runbook) {
            self.warnings.push(format!("{e:#}"));
        }
        d.trusted = true;
        self.advance_draft(client)
    }

    /// The question for the parameter being asked: a list of choices, or a prompt.
    pub(super) fn param_question(&self, client: ClientId) -> Mode {
        let Some(d) = self.drafts.get(&client) else { return Mode::Normal };
        let Some(p) = d.steps.params.get(d.values.len()) else { return Mode::Normal };
        let vars = self.draft_vars(d);
        let default = p.default.as_deref().map(|v| runbooks::expand_vars(v, &vars)).unwrap_or_default();
        let choices = if p.choices.is_empty() { d.choices.clone().unwrap_or_default() } else { p.choices.clone() };
        if choices.is_empty() {
            // A secret's default is in the field too, masked: Enter keeps it, as on the command
            // line; the title says there is one, since it cannot be read.
            let title = if p.secret && !default.is_empty() {
                format!("{} (Enter keeps the default)", p.prompt())
            } else {
                p.prompt()
            };
            let mut prompt = Prompt::new(&title, &default, PromptAction::RunbookValue);
            prompt.masked = p.secret;
            return Mode::Prompt(prompt);
        }
        let items: Vec<PickItem> = choices
            .into_iter()
            .map(|c| {
                let detail = if c == default { "default" } else { "" };
                PickItem::new(c.clone(), detail, PickAction::RunbookValue(c))
            })
            .collect();
        let mut picker = Picker::new(PickerKind::RunbookParam, &p.prompt(), items);
        if let Some(i) = picker.filtered.iter().position(|&i| picker.items[i].label == default) {
            picker.selected = i;
        }
        Mode::Picker(picker)
    }

    /// Servers and groups to run on, the focused pane's server first.
    pub(super) fn target_picker(&self, pane: Option<PaneId>, name: &str) -> Picker {
        let words = &self.config.production.words;
        let mut items = Vec::new();
        if let Some(s) = pane.and_then(|p| self.pane_server_entry(p)) {
            items.push(PickItem::new(s.name.clone(), "this pane's server", PickAction::RunbookTarget(s.name.clone())));
        }
        for (g, n) in servers::groups(&self.known_servers) {
            let detail = format!("group · {n} server{}", if n == 1 { "" } else { "s" });
            items.push(PickItem::new(format!("@{g}"), detail, PickAction::RunbookTarget(format!("@{g}"))));
        }
        for k in &self.known_servers {
            if items.iter().any(|i| i.label == k.server.name) {
                continue;
            }
            let prod = if crate::context::server_production(&k.server, words) { "PROD · " } else { "" };
            let detail = format!("{prod}{}", k.server.address());
            items.push(PickItem::new(k.server.name.clone(), detail, PickAction::RunbookTarget(k.server.name.clone())));
        }
        Picker::new(PickerKind::RunbookTarget, &format!("Run {name} on…"), items)
    }

    /// `web01`, `user@host` or `@group` as targets.
    pub(in crate::server) fn resolve_targets(&self, spec: &str) -> Result<Vec<Target>> {
        if let Some(group) = spec.strip_prefix('@') {
            let members: Vec<Target> = self
                .known_servers
                .iter()
                .filter(|k| k.server.groups.iter().any(|g| g == group))
                .map(|k| Target::Server(k.server.clone()))
                .collect();
            if members.is_empty() {
                bail!("no server in group {group:?}");
            }
            return Ok(members);
        }
        let s = self.resolve_server(spec).with_context(|| format!("not a server: {spec}"))?;
        Ok(vec![Target::Server(s)])
    }

    pub(in crate::server) fn set_draft_target(&mut self, client: ClientId, spec: &str) -> Mode {
        match self.resolve_targets(spec) {
            Ok(t) => {
                if let Some(d) = self.drafts.get_mut(&client) {
                    d.targets = Some(t);
                }
                self.advance_draft(client)
            }
            Err(e) => {
                self.toast(client, NoticeKind::Error, format!("{e:#}"));
                Mode::Normal
            }
        }
    }

    pub(in crate::server) fn set_draft_value(&mut self, client: ClientId, value: String) -> Mode {
        if let Some(d) = self.drafts.get_mut(&client)
            && let Some(p) = d.steps.params.get(d.values.len())
        {
            let name = p.name.clone();
            d.values.push((name, value));
            d.choices = None;
        }
        self.advance_draft(client)
    }

    /// The focused pane's variables, for parameter defaults and choice commands.
    pub(super) fn draft_vars(&self, d: &Draft) -> Vec<(String, String)> {
        d.pane.map(|p| self.tool_context(p).variables(false)).unwrap_or_default()
    }

    /// Directory of local runs: the runbook's `cwd`, the repository, or the pane's directory.
    pub(super) fn draft_cwd(&self, d: &Draft) -> Option<PathBuf> {
        let pane_cwd =
            d.pane.and_then(|p| self.panes.get(&p)).filter(|p| p.ssh.is_none()).and_then(|p| p.current_cwd());
        match (&d.steps.cwd, &d.runbook.source) {
            (Some(c), Source::Repo(root)) => Some(root.join(paths::expand_tilde(c))),
            (Some(c), Source::User) => {
                let p = paths::expand_tilde(c);
                Some(if p.is_relative() { pane_cwd.unwrap_or_default().join(p) } else { p })
            }
            (None, Source::Repo(root)) => Some(root.clone()),
            (None, Source::User) => pane_cwd,
        }
    }

    pub(in crate::server) fn target_label(&self, t: &Target) -> String {
        match t {
            Target::Local => "this machine".into(),
            Target::Pane(p) => match self.pane_server(*p) {
                Some(s) => s.name,
                None => self.pane_label(*p),
            },
            Target::Server(s) => s.name.clone(),
        }
    }

    pub(in crate::server) fn target_production(
        &self,
        t: &Target,
        pane: Option<PaneId>,
    ) -> Option<crate::server::contexts::Production> {
        match t {
            // Local runs carry the pane's Kubernetes, Terraform and AWS context.
            Target::Local => pane.filter(|p| self.pane_server(*p).is_none()).and_then(|p| self.pane_production(p)),
            Target::Pane(p) => self.pane_production(*p),
            Target::Server(s) => {
                let entry = self.known_servers.iter().find(|k| k.server.name == s.name).map(|k| &k.server).unwrap_or(s);
                crate::context::server_production(entry, &self.config.production.words).then(|| {
                    crate::server::contexts::Production::new(format!("server {}", s.name), format!("server {entry:?}"))
                })
            }
        }
    }

    /// What will run, where and how. `full` lists every line of every step (the CLI); otherwise
    /// the first line of the first steps (the confirmation box).
    pub(in crate::server) fn plan(&self, d: &Draft, full: bool) -> String {
        let s = &d.steps;
        let targets = d.targets.as_deref().unwrap_or(&[]);
        let names: Vec<String> = targets.iter().map(|t| self.target_label(t)).collect();
        let n = names.len();
        let place = if n == 1 {
            names[0].clone()
        } else {
            let mut list = names.iter().take(if full { n } else { 4 }).cloned().collect::<Vec<_>>().join(", ");
            if !full && n > 4 {
                list.push_str(&format!(" and {} more", n - 4));
            }
            format!("{n} servers ({list})")
        };
        let mut how = String::new();
        if n > 1 {
            how = match s.rolling {
                0 => ", all at once".into(),
                1 => ", one at a time".into(),
                k => format!(", {k} at a time"),
            };
            if s.stop_on_failure {
                how.push_str(", stopping at the first failure");
            }
        }
        if let Some(t) = &s.timeout {
            how.push_str(&format!(", at most {t} each"));
        }
        let mut lines = vec![format!("Run {} on {place}{how}?", d.runbook.name)];
        let prod: Vec<String> = targets
            .iter()
            .filter(|t| self.target_production(t, d.pane).is_some())
            .map(|t| self.target_label(t))
            .collect();
        if !prod.is_empty() {
            lines.push(format!("PRODUCTION: {}", prod.join(", ")));
        }
        if let Source::Repo(root) = &d.runbook.source
            && !d.trusted
        {
            lines.push(format!(
                "From the repository {}: new, or changed since you approved it. It runs once this version is approved.",
                paths::shorten_home(root)
            ));
        }
        lines.extend(d.shown_values());
        let steps: Vec<&String> = s.steps.iter().filter(|st| !st.trim().is_empty()).collect();
        for (i, st) in steps.iter().enumerate() {
            if !full && i == 8 {
                lines.push(format!("… and {} more steps", steps.len() - 8));
                break;
            }
            if full {
                for (j, line) in st.lines().enumerate() {
                    lines.push(if j == 0 { format!("{}. {line}", i + 1) } else { format!("   {line}") });
                }
            } else {
                lines.push(format!("{}. {}", i + 1, first_line(st)));
            }
        }
        lines.join("\n")
    }

    /// What identifies each target of a draft: its name, where it runs (server, directory) and
    /// its production reason.
    pub(in crate::server) fn identities(&self, d: &Draft) -> Vec<String> {
        d.targets
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|t| {
                let place = match t {
                    Target::Local => self.draft_cwd(d).map(|c| c.display().to_string()).unwrap_or_default(),
                    // How a server is reached (its jump host's address too) is part of where.
                    Target::Pane(p) => {
                        let ctx = self.tool_context(*p);
                        let reached = ctx.server.as_ref().map(|s| self.connection(s)).unwrap_or_default();
                        format!("{} {reached}", ctx.place())
                    }
                    Target::Server(s) => self.connection(s),
                };
                format!("{} @ {place} · {:?}", self.target_label(t), self.target_production(t, d.pane))
            })
            .collect()
    }

    pub(in crate::server) fn confirm_runbook(&mut self, client: ClientId, yes: bool) {
        let Some(d) = self.drafts.remove(&client) else { return };
        if !yes {
            self.toast(client, NoticeKind::Notice, "runbook cancelled");
            return;
        }
        if let Err(e) = self.launch_runbook(d, &mut super::Asker { client: Some(client), reply: None }) {
            self.toast(client, NoticeKind::Error, format!("{e:#}"));
        }
    }

    /// `kraal runbook run`: resolve everything now (file, targets, parameters) and keep it, with
    /// where each target is; the client `reply` gets the plan's text and a token that runs it,
    /// or refuses if a target is no longer where the plan said. Each value is held to what the
    /// picker offers for its parameter ([`Server::param_question`]): the choices its file gives,
    /// and those a command lists, unless the runbook is a repository's nobody approved yet.
    /// No command of that one runs: the plan says which values wait, and its run holds them to
    /// their lists if it is approved by then ([`Server::run_prepared`]). Asked for by what may be
    /// an `agent` (from an agent's pane, or a pane of another session), a runbook that reaches
    /// production and lists choices with commands is refused: those commands would run before
    /// the user is asked about the run.
    pub(in crate::server) fn prepare_runbook(
        &mut self,
        path: &Path,
        target: Option<&str>,
        params: Vec<(String, String)>,
        pane: Option<PaneId>,
        agent: bool,
        reply: &mut Option<ReplyHandle>,
    ) -> Result<()> {
        let r = runbooks::load(path, runbooks::source_of(path))?;
        let Body::Steps(steps) = &r.body else {
            bail!("{} is a document: open it in kraal with {} and run its steps there", r.name, self.runbooks_key())
        };
        let steps = (**steps).clone();
        let targets = match (target, steps.on) {
            (Some(t), _) => self.resolve_targets(t)?,
            (None, On::Server) => bail!("{} runs on servers: give --on SERVER or --on @group", r.name),
            (None, On::Pane) => vec![pane.map(Target::Pane).unwrap_or(Target::Local)],
            (None, On::Local) => vec![Target::Local],
        };
        for (k, _) in &params {
            if !steps.params.iter().any(|p| &p.name == k) {
                bail!("{} has no parameter {k}", r.name);
            }
        }
        let mut d = Draft::new(r, steps, pane, Some(targets));
        let vars = self.draft_vars(&d);
        let mut missing = Vec::new();
        for p in &d.steps.params {
            match params.iter().find(|(k, _)| k == &p.name) {
                Some((_, v)) => d.values.push((p.name.clone(), v.clone())),
                None => match &p.default {
                    Some(def) => d.values.push((p.name.clone(), runbooks::expand_vars(def, &vars))),
                    None => missing.push(p.name.clone()),
                },
            }
        }
        if !missing.is_empty() {
            bail!("missing parameters: {} (-p NAME=value)", missing.join(", "));
        }
        for (p, (_, value)) in d.steps.params.iter().zip(&d.values) {
            p.accept(value, &p.choices)?;
        }
        if agent
            && !d.listed().is_empty()
            && d.targets.iter().flatten().any(|t| self.target_production(t, pane).is_some())
        {
            bail!(
                "{} lists choices with commands and acts on production: from an agent's pane or another session, those commands would run before you are asked; run it yourself",
                d.runbook.name
            );
        }
        if !d.trusted {
            let why = "its choices are listed once the runbook is approved";
            let waiting = d.listed().iter().map(|(p, ..)| format!("{} is taken as given: {why}", p.name)).collect();
            self.send_plan(reply.take().unwrap(), d, waiting);
            return Ok(());
        }
        self.hold_to_listed(d, reply, move |server, reply, held| match held {
            Ok((d, unchecked)) => server.send_plan(reply, d, unchecked),
            Err(e) => server.answer_runbook(reply, Err(e)),
        })
    }

    /// Hold the values of a draft made for the command line to the choices its parameters'
    /// commands list, off the loop, then call `then` with the draft, or with the value that is
    /// refused. A command that fails or lists nothing constrains nothing, as in the picker,
    /// which then asks for text: `then` gets a line that says so for each that failed.
    fn hold_to_listed(
        &mut self,
        d: Draft,
        reply: &mut Option<ReplyHandle>,
        then: impl FnOnce(&mut Server, ReplyHandle, Result<(Draft, Vec<String>)>) + 'static,
    ) -> Result<()> {
        let listed = d.listed();
        if listed.is_empty() {
            then(self, reply.take().unwrap(), Ok((d, Vec::new())));
            return Ok(());
        }
        let (name, cwd, vars) = (d.runbook.name.clone(), self.draft_cwd(&d), self.draft_vars(&d));
        let started = self.background_owned(
            reply.take().unwrap(),
            move || -> Result<Vec<String>> {
                let mut unchecked = Vec::new();
                for (p, cmd, value) in listed {
                    match list_choices(&cmd, cwd.as_deref(), &vars) {
                        Ok(choices) => p.accept(&value, &choices)?,
                        Err(e) => unchecked.push(format!("{} is taken as given: {e:#}", p.name)),
                    }
                }
                Ok(unchecked)
            },
            move |server, reply, held| then(server, reply, held.map(|unchecked| (d, unchecked))),
        );
        if let Err(returned) = started {
            *reply = Some(returned);
            bail!("cannot list the choices of {name}");
        }
        Ok(())
    }

    /// Keep a draft resolved for the command line, for ten minutes, and send `reply` its plan:
    /// the text, which ends with what was not checked of its values, and the token that runs it.
    fn send_plan(&mut self, reply: ReplyHandle, mut d: Draft, unchecked: Vec<String>) {
        let mut text = self.plan(&d, true);
        for line in unchecked {
            text.push('\n');
            text.push_str(&line);
        }
        d.reviewed = Some(self.identities(&d));
        let token = crate::identity::new_uuid();
        self.plans.retain(|_, (made, _)| made.elapsed() < PLAN_LIFETIME);
        self.plans.insert(token.clone(), (Instant::now(), d));
        self.answer_runbook(reply, Ok(Response::Plan { text, token }));
    }

    /// Answer the `kraal runbook run` that waits for its plan, or for its run to start.
    fn answer_runbook(&mut self, reply: ReplyHandle, answer: Result<Response>) {
        reply.complete(self, answer.map_err(|e| format!("{e:#}")));
    }

    /// Run a plan prepared by [`Server::prepare_runbook`] for the CLI client `reply`, which gets
    /// the run's number once its targets have started (or why they did not). Asking for a plan
    /// to run approves nothing (whoever asks may have shown it to nobody): a repository
    /// runbook runs if the version planned is approved by now.
    pub(in crate::server) fn run_prepared(&mut self, token: &str, reply: &mut Option<ReplyHandle>) -> Result<()> {
        let (made, mut d) = self.plans.remove(token).context("no such plan: review the runbook again")?;
        if made.elapsed() > PLAN_LIFETIME {
            bail!("the plan is more than 10 minutes old: review the runbook again");
        }
        // Until here, `trusted` says whether the runbook was approved when its plan was made:
        // whether the commands that list choices ran for it.
        let listed = d.trusted;
        d.trusted = runbooks::trusted(&d.runbook);
        // One that is still not approved is refused there, before any of its commands runs.
        if listed || !d.trusted {
            let mut asker = super::Asker { client: None, reply: reply.take() };
            let result = self.launch_runbook(d, &mut asker);
            *reply = asker.reply;
            return result;
        }
        // Approved since its plan: the values that waited for it are held to their lists now.
        self.hold_to_listed(d, reply, move |server, reply, held| {
            let mut asker = super::Asker { client: None, reply: Some(reply) };
            let launched = held.and_then(|(d, unchecked)| {
                server.warnings.extend(unchecked.iter().map(|line| format!("{}: {line}", d.runbook.name)));
                server.launch_runbook(d, &mut asker)
            });
            if let Err(e) = launched {
                server.answer_runbook(asker.reply.take().unwrap(), Err(e));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Request;
    use crate::server::requests::{
        RequestOutcome,
        tests::{finish_job, response},
    };
    use crate::server::tests::{client, server};

    #[test]
    fn runbook_plans_reply_once_after_choices_and_keep_start_failure_immediate() {
        for mode in ["plain", "listed", "refused", "start failure", "disconnected"] {
            let (tx, rx) = crate::server::event_channel(32);
            let mut server = server(tx);
            let dir = crate::testing::TempDir::new("reply-plan");
            let mut content = String::from("steps = ['true']\n");
            if mode != "plain" {
                content.push_str("[[param]]\nname = 'VALUE'\ndefault = 'yes'\nchoices_command = 'printf yes'\n");
            }
            let path = dir.write("check.toml", content);
            let (id, _peer) = client(&mut server);
            let receive = server.clients.get_mut(&id).unwrap().out.capture();
            crate::server::jobs::FAIL_START.set(mode == "start failure");
            let params = if mode == "refused" { vec![("VALUE".into(), "no".into())] } else { vec![] };
            let job = crate::server::jobs::JobId(server.next_id);
            let outcome = server.request(id, Request::RunbookPlan { path, target: None, params, pane: None });
            if mode == "start failure" {
                let RequestOutcome::Immediate { reply, result } = outcome else { panic!("start failure") };
                assert!(receive.is_empty());
                reply.complete(&mut server, result);
                assert_eq!(response(&receive).unwrap_err(), "cannot list the choices of check");
            } else {
                assert!(matches!(outcome, RequestOutcome::Deferred));
                if mode != "plain" {
                    assert!(receive.is_empty());
                    if mode == "disconnected" {
                        server.drop_client(id);
                    }
                    finish_job(&mut server, &rx, job);
                }
                if mode == "refused" {
                    assert_eq!(response(&receive).unwrap_err(), "VALUE=no is not one of its choices: yes");
                } else if mode != "disconnected" {
                    let Response::Plan { token, .. } = response(&receive).unwrap() else { panic!("plan") };
                    assert!(server.plans.contains_key(&token));
                } else {
                    assert_eq!(server.plans.len(), 1);
                }
            }
            assert!(server.jobs.is_empty());
            assert!(!server.clients.contains_key(&id));
            assert!(matches!(receive.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
        }
    }
}
