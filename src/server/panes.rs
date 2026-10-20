//! Panes and workspaces coming and going: spawning, exiting, closing.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::ui::{ConfirmAction, Mode};
use super::{ClientId, FloatRect, Floating, Server, SyncScope, Tab, Workspace};
use crate::layout::{Axis, Node, PaneId};
use crate::pane::{Pane, PaneSpec, ProcessHold};
use crate::protocol;
use crate::theme::NoticeKind;
/// Keep the terminal's session and the agent's children identifiable after its shell exits.
pub(super) struct ProgramStop {
    process: Option<crate::pane::ProcessHold>,
    members: Vec<(u32, u64)>,
    sessions: Vec<crate::process::SessionOwner>,
    pub(super) since: Instant,
}

impl ProgramStop {
    fn recover(intent: &crate::persist::CloseIntent) -> Self {
        let mut stop = Self {
            process: None,
            members: intent.local_members(),
            sessions: intent.local_sessions(),
            since: Instant::now(),
        };
        stop.refresh();
        stop
    }

    pub(super) fn new(pane: &crate::pane::Pane) -> Option<Self> {
        let process = pane.process_hold()?;
        let sessions = process.session_owner().into_iter().collect();
        let mut stop = Self { process: Some(process), members: Vec::new(), sessions, since: Instant::now() };
        stop.refresh();
        Some(stop)
    }

    fn refresh(&mut self) {
        self.members.retain(|&(pid, started)| crate::process::still_running(pid, started));
        let mut pids = self.process.as_ref().map(|p| p.session_members()).unwrap_or_default();
        if self.process.is_none() {
            pids.extend(self.sessions.iter().flat_map(|s| s.members()));
        }
        pids.extend(self.members.iter().map(|&(pid, _)| pid));
        // Remember children before signalling their parents, including ones in another session.
        let roots = pids.clone();
        for pid in roots {
            pids.extend(crate::process::descendants(pid));
        }
        pids.sort_unstable();
        pids.dedup();
        for pid in pids {
            if !self.members.iter().any(|&(seen, _)| seen == pid)
                && let Some(started) = crate::process::start_time(pid)
            {
                self.members.push((pid, started));
            }
        }
    }

    pub(super) fn signal(&self, signal: i32) {
        if let Some(process) = &self.process {
            process.signal(signal);
        }
        for &(pid, started) in &self.members {
            if crate::process::still_running(pid, started) {
                // SAFETY: this member of our stopped job still has the identity we retained.
                unsafe { libc::kill(pid as libc::pid_t, signal) };
            }
        }
    }

    pub(super) fn finished(&mut self) -> bool {
        self.refresh();
        if !self.process.as_ref().is_some_and(|p| p.group_running())
            && self.members.iter().all(|&(pid, started)| !crate::process::still_running(pid, started))
        {
            return true;
        }
        let elapsed = self.since.elapsed();
        if elapsed >= Duration::from_secs(2) {
            self.signal(libc::SIGKILL);
        } else if elapsed >= Duration::from_secs(1) {
            self.signal(libc::SIGTERM);
        }
        false
    }
}

impl Server {
    /// An open destination for new panes: the preferred workspace, the primary client's, or
    /// the first open one. A prepared close owns its layout until it has finished.
    pub(super) fn available_workspace(&self, preferred: Option<u32>) -> Option<u32> {
        preferred
            .into_iter()
            .chain(self.primary.and_then(|p| self.clients.get(&p)).map(|c| c.workspace))
            .chain(self.workspaces.iter().map(|w| w.id))
            .find(|ws| self.ws_index(*ws).is_some_and(|wi| self.workspaces[wi].kept.is_none()))
    }

    pub fn new_workspace(&mut self, name: String, definition: Option<PathBuf>) -> u32 {
        let id = self.new_workspace_record(name, definition);
        let name = self.workspaces.last().map(|w| w.name.clone()).unwrap_or_default();
        self.emit("workspace.opened", serde_json::json!({ "workspace": name }));
        id
    }

    // A retained closed layout must not announce Open or run its hooks before the user opens it.
    pub(crate) fn new_workspace_record(&mut self, name: String, definition: Option<PathBuf>) -> u32 {
        let id = self.alloc_id();
        let mut name = name;
        let mut n = 2;
        let base = name.clone();
        while self.workspaces.iter().any(|w| w.name == name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        let mut ws = Workspace {
            id,
            stable_id: crate::identity::new_uuid(),
            name,
            tabs: Vec::new(),
            kept: None,
            close_error: None,
            delete_pending: false,
            active: 0,
            definition: None,
            definition_inode: None,
            definition_source: None,
            source_reviewed: None,
            layout_undo: None,
            source_status: None,
            cwd: None,
            sync: SyncScope::Off,
            marked: Default::default(),
            sync_approved: Default::default(),
        };
        ws.set_definition(definition.as_deref());
        self.workspaces.push(ws);
        self.persist_dirty = true;
        id
    }

    /// Spawn a pane process with a provisional size; call [`Server::relayout`] afterwards.
    pub fn spawn_pane(&mut self, name: Option<String>, spec: PaneSpec) -> Result<PaneId> {
        let mut spec = spec;
        self.accounts.bind_pane(&mut spec);
        spec.prepare_agent_clipboard();
        self.add_worktree_env(&mut spec);
        let id = self.alloc_id();
        let inner = self.inner(self.layout_area);
        if let Some([left, right]) = spec.files.clone() {
            let fm = self.file_manager(&left, &right);
            self.panes.insert(id, Pane::files(id, name, spec, fm));
            self.persist_dirty = true;
            return Ok(id);
        }
        let ssh = self.ssh_target_of(&spec);
        let mut pane = match Pane::spawn(id, name.clone(), spec.clone(), inner.width, inner.height, &self.env, &self.tx)
        {
            Ok(p) => p,
            // A command (or a shell with input to type, an agent to resume) that cannot start
            // still gets its pane, so the layout stays whole and Enter can retry once the
            // problem is fixed.
            Err(e) if !spec.command.is_empty() || spec.startup_input.is_some() => {
                if let Some(login) = &mut spec.login {
                    login.state = super::agents::login_start_failed(&spec.command);
                    login.lock = None;
                    return self.spawn_pane_suspended(name, spec, "sign-in could not start");
                }
                let reason = format!("{e:#}");
                let reason = reason.lines().next().unwrap_or("cannot start").to_string();
                return self.spawn_pane_suspended(name, spec, &format!("cannot start: {reason}"));
            }
            Err(e) => return Err(e),
        };
        pane.ssh = ssh;
        self.panes.insert(id, pane);
        self.persist_dirty = true;
        Ok(id)
    }

    /// A command pane that does not run its command yet: it shows `reason`, and Enter starts it.
    pub fn spawn_pane_suspended(&mut self, name: Option<String>, spec: PaneSpec, reason: &str) -> Result<PaneId> {
        let mut spec = spec;
        self.accounts.bind_pane(&mut spec);
        spec.prepare_agent_clipboard();
        self.add_worktree_env(&mut spec);
        let id = self.alloc_id();
        let inner = self.inner(self.layout_area);
        let mut pane =
            Pane::spawn(id, name, placeholder(&spec, reason), inner.width, inner.height, &self.env, &self.tx)?;
        pane.ssh = self.ssh_target_of(&spec);
        pane.spec = spec;
        pane.suspended = true;
        self.panes.insert(id, pane);
        self.persist_dirty = true;
        Ok(id)
    }

    /// Run `f` with the programs of the panes it sets up held, then start them (see
    /// `crate::pane::batch`): the panes of a workspace are all set up first, instead of the
    /// server competing for the processors with the programs it started already.
    pub(super) fn with_batch<T>(&mut self, f: impl FnOnce(&mut Server) -> T) -> T {
        let batch = crate::pane::batch();
        let result = f(self);
        drop(batch);
        // The outermost batch starts them all.
        if !crate::pane::batching() {
            self.launch_pending();
        }
        result
    }

    /// Start the programs of the panes set up during a batch, in the order the panes were made,
    /// off this thread (see `crate::pane::launcher`).
    fn launch_pending(&mut self) {
        let mut ids: Vec<PaneId> = self.panes.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let Some(pending) = self.panes.get_mut(&id).and_then(Pane::take_pending) else { continue };
            // Without the launcher thread, here.
            if let Err(e) = self.launches.send(pending)
                && let crate::pane::Started::Failed(id, process, why) = e.into_inner().start()
            {
                // Before queued events, but focus still belongs to the enclosing handler's boundary.
                self.pane_failed(id, &process, &why);
            }
        }
    }

    /// The program of pane `id` (`process`), started after the pane was set up, could not start
    /// (`why`). Unless the pane is gone or runs another program since, it waits for Enter,
    /// saying why, as one whose program could not start when it was made does.
    pub(super) fn pane_failed(&mut self, id: PaneId, process: &ProcessHold, why: &str) {
        let Some(pane) = self.panes.get_mut(&id) else { return };
        if !pane.process_hold().is_some_and(|p| p.is(process)) {
            return;
        }
        if self.switches.get(&id).is_some_and(|s| {
            matches!(s.stage, super::agents::SwitchStage::Starting | super::agents::SwitchStage::Restoring)
        }) {
            pane.exit_code = Some(None);
            self.switch_start_failed(id, why);
            return;
        }
        if let Some(login) = &mut pane.spec.login {
            login.state = super::agents::login_start_failed(&pane.spec.command);
            login.lock = None;
            pane.exit_code = Some(None);
            self.mark_dirty();
            return;
        }
        let reason = format!("cannot start: {}", why.lines().next().unwrap_or("cannot start"));
        // A prelude (a worktree's setup) that could not start: Enter starts the command.
        pane.preparing = false;
        let spec = pane.spec.clone();
        if let Err(e) = self.suspend_pane(id, spec, &reason) {
            self.warnings.push(format!("{reason}; {e:#}"));
        }
        self.mark_dirty();
    }

    /// Make pane `id` wait for Enter to run `spec`, saying why (`reason`).
    pub fn suspend_pane(&mut self, id: PaneId, spec: PaneSpec, reason: &str) -> Result<()> {
        let ssh = self.ssh_target_of(&spec);
        let pane = self.panes.get_mut(&id).context("no such pane")?;
        pane.spec = placeholder(&spec, reason);
        let result = pane.respawn(&self.env, &self.tx);
        pane.ssh = ssh;
        pane.spec = spec;
        pane.suspended = true;
        if result.is_err() {
            pane.exit_code = Some(None);
        }
        self.persist_dirty = true;
        result
    }

    /// Run Claude teammate `command` in pane `id`, which `lead` split (with `cat` waiting there):
    /// with the variables kraal gave the lead's account, and the pane kept when it fails.
    pub(super) fn start_teammate(&mut self, id: PaneId, lead: PaneId, command: Vec<String>) -> Result<(), String> {
        let lead_label = self.pane_label(lead);
        let lead = &self.panes.get(&lead).ok_or("no such pane")?.spec;
        if !crate::tmux::is_lead(&lead.env) {
            return Err(format!("{lead_label} is not a Claude started with kraal agent --teams"));
        }
        let (env, env_remove) = (crate::tmux::teammate_env(&lead.env), lead.env_remove.clone());
        let label = self.pane_label(id);
        let edited = self.pane_edits.contains_key(&id);
        let pane = self.panes.get_mut(&id).ok_or("no such pane")?;
        // Only a pane running a local command: never a shell, a server, an agent kraal started or a
        // file kraal edits, which this would end.
        if pane.term().is_none()
            || pane.ssh.is_some()
            || pane.spec.command.is_empty()
            || pane.spec.account.is_some()
            || edited
        {
            return Err(format!("{label} is not a pane split for a teammate"));
        }
        pane.kill();
        let cwd = pane.spec.cwd.clone();
        pane.spec = PaneSpec {
            command,
            env,
            env_remove,
            cwd,
            close_on_exit: true,
            keep_on_failure: true,
            ..Default::default()
        };
        let result = pane.respawn(&self.env, &self.tx).map_err(|e| format!("{e:#}"));
        self.persist_dirty = true;
        self.mark_dirty();
        result
    }

    /// A pane in workspace `ws` that runs `prelude` (a new worktree's setup) before its own
    /// command, `spec`: the command starts in its place once the prelude succeeds, and waits for
    /// Enter when it fails.
    pub fn spawn_pane_after(
        &mut self,
        ws: u32,
        prelude: PaneSpec,
        spec: PaneSpec,
        placement: crate::protocol::Placement,
        name: Option<String>,
    ) -> Result<PaneId> {
        let id = self.spawn_pane_in(ws, None, prelude, placement, name)?;
        let mut spec = spec;
        self.accounts.bind_pane(&mut spec);
        spec.prepare_agent_clipboard();
        self.add_worktree_env(&mut spec);
        if let Some(p) = self.panes.get_mut(&id) {
            // A prelude that could not start waits for Enter: Enter starts the command instead.
            p.preparing = !p.suspended;
            p.spec = spec;
        }
        Ok(id)
    }

    /// Spawn a pane and place it in workspace `ws` relative to `target` (default: the focused pane).
    pub fn spawn_pane_in(
        &mut self,
        ws: u32,
        target: Option<PaneId>,
        spec: PaneSpec,
        placement: crate::protocol::Placement,
        name: Option<String>,
    ) -> Result<PaneId> {
        use crate::protocol::Placement;
        let wi = self.ws_index(ws).context("no such workspace")?;
        anyhow::ensure!(self.workspaces[wi].kept.is_none(), "Open the workspace before adding a pane.");
        let mut spec = spec;
        if spec.cwd.is_none() {
            let from =
                target.or_else(|| self.workspaces[wi].tabs.get(self.workspaces[wi].active).and_then(|t| t.focus));
            spec.cwd = from
                .and_then(|p| self.panes.get(&p))
                .and_then(|p| p.current_cwd())
                .or_else(|| self.workspaces[wi].cwd.clone());
        }
        let id = self.spawn_pane(name, spec)?;
        let area = self.layout_area;
        let ws = &mut self.workspaces[wi];
        let new_tab = ws.tabs.is_empty() || placement == Placement::Tab;
        if new_tab {
            let tab_id = self.next_id;
            self.next_id += 1;
            ws.tabs.push(Tab::new(tab_id, String::new()));
            ws.active = ws.tabs.len() - 1;
        }
        // A new tab is where the pane goes; otherwise the target's tab, else the active one.
        let found = if new_tab { None } else { target.and_then(|t| ws.tabs.iter().position(|tab| tab.contains(t))) };
        let tab = match found {
            Some(ti) => {
                ws.active = ti;
                &mut ws.tabs[ti]
            }
            None => ws.tab_mut().expect("a tab was just created"),
        };
        let target = target.filter(|t| tab.contains(*t)).or(tab.focus);
        if placement == Placement::Floating {
            let n = tab.floating.len() as f32;
            let off = (n * 0.03).min(0.15);
            tab.floating.push(Floating { pane: id, rect: FloatRect { x: 0.15 + off, y: 0.12 + off, w: 0.7, h: 0.7 } });
            tab.show_floating = true;
        } else {
            match (&mut tab.root, target.filter(|t| !tab.floating.iter().any(|f| f.pane == *t))) {
                (None, _) => tab.root = Some(Node::Leaf(id)),
                (Some(root), Some(t)) => {
                    let axis = match placement {
                        Placement::Right => Axis::Row,
                        Placement::Down => Axis::Column,
                        _ => {
                            let rects = root.layout(area);
                            let r = rects.get(&root.holder(t)).copied().unwrap_or(area);
                            // Cells are about twice as tall as wide.
                            if r.width >= r.height * 2 { Axis::Row } else { Axis::Column }
                        }
                    };
                    // A new pane next to a stacked one goes in its stack; a split by hand
                    // splits the stack's place.
                    let split = matches!(placement, Placement::Right | Placement::Down);
                    if split || !root.join(t, id) {
                        root.split(t, axis, id);
                    }
                    if split {
                        tab.preset = None;
                    }
                }
                (Some(root), None) => {
                    let first = root.leaves()[0];
                    root.split(first, Axis::Row, id);
                }
            }
            // The arrangement the tab keeps takes the new pane in.
            if let Some(preset) = tab.preset {
                tab.focus = Some(id);
                tab.arrange(preset);
            }
            tab.zoom = None;
        }
        tab.set_focus(id);
        self.relayout();
        Ok(id)
    }

    /// The process of pane `id` ended, and its last output was read (see `finish_exits`).
    pub(super) fn pane_exited(&mut self, id: PaneId, pid: Option<u32>, code: Option<i32>) {
        let Some(pane) = self.panes.get_mut(&id) else { return };
        if pid.is_some() && pane.pid() != pid {
            return; // an earlier process of a respawned pane
        }
        if self.pane_edits.contains_key(&id) {
            pane.exit_code = Some(code);
            self.check_pane_edits();
            self.mark_dirty();
            return;
        }
        if pane.spec.login.is_some() {
            self.login_exited(id, code);
            return;
        }
        if !pane.suspended
            && self.switches.get(&id).is_some_and(|s| {
                matches!(
                    s.stage,
                    super::agents::SwitchStage::Starting
                        | super::agents::SwitchStage::Restoring
                        | super::agents::SwitchStage::Recovering
                        | super::agents::SwitchStage::RecoveryStopping
                )
            })
        {
            pane.exit_code = Some(code);
            self.switch_start_failed(id, &format!("the agent exited before starting ({code:?})"));
            self.mark_dirty();
            return;
        }
        if self.switches.get(&id).is_some_and(|s| s.stage == super::agents::SwitchStage::Stopping) {
            // The last source output is read before its replacement starts.
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.exit_code = Some(code);
                let session = pane.agent.session_id.take();
                pane.agent = crate::agent::AgentTracker { session_id: session, ..Default::default() };
            }
            self.finish_switch(id);
            self.mark_dirty();
            return;
        }
        let Some(pane) = self.panes.get_mut(&id) else { return };
        if pane.preparing {
            pane.preparing = false;
            let result = if code == Some(0) {
                pane.respawn(&self.env, &self.tx)
            } else {
                let spec = pane.spec.clone();
                self.suspend_pane(id, spec, "the worktree's setup did not succeed; Enter starts this anyway")
            };
            if let Err(e) = result {
                self.warnings.push(format!("{e:#}"));
            }
            self.mark_dirty();
            self.relayout();
            return;
        }
        let Some(pane) = self.panes.get_mut(&id) else { return };
        if let Some(spec) = pane.restart_with.take() {
            pane.spec = spec;
            if let Err(e) = pane.respawn(&self.env, &self.tx) {
                self.warnings.push(format!("{e:#}"));
            }
            self.limit_alerts.remove(&id);
            self.relayout();
            return;
        }
        // A suspended pane's placeholder ending is not its command ending.
        if !pane.suspended {
            self.announce_pane(id);
            let Some(pane) = self.panes.get(&id) else { return };
            let command = pane.spec.command.clone();
            let fields = serde_json::json!({ "code": code, "command": command });
            self.emit_pane("pane.exited", id, fields.clone());
            if code.is_some_and(|c| c != 0) {
                self.emit_pane("pane.failed", id, fields);
            }
        }
        let Some(pane) = self.panes.get_mut(&id) else { return };
        // Quitting with ctrl+c (130) is a normal end for a tool, not a failure to show.
        let failed = !matches!(code, Some(0) | Some(130) | None);
        let keep = pane.suspended
            || (!pane.spec.command.is_empty() && (!pane.spec.close_on_exit || (pane.spec.keep_on_failure && failed)));
        if keep {
            pane.exit_code = Some(code);
            // The conversation's id outlives the process: restarting resumes it.
            let session = pane.agent.session_id.take();
            pane.agent = crate::agent::AgentTracker { session_id: session, ..Default::default() };
            let job = pane.spec.close_on_success;
            if code == Some(0) && job {
                pane.close_at = Some(Instant::now() + Duration::from_secs(3));
            }
            if job {
                self.refresh_file_managers();
            }
            self.mark_dirty();
            self.check_waits();
            self.runbook_pane_exited(id, code);
            self.show_ssh_failure(id);
        } else {
            self.close_pane(id);
        }
    }

    /// Close a pane and remove its empty tab, keeping the workspace.
    pub fn close_pane(&mut self, id: PaneId) {
        self.cancel_laptop_pane(id);
        self.abandon_added_login(id);
        if !self.panes.contains_key(&id) {
            return;
        }
        self.announce_pane(id);
        let fields = self.pane_fields(id);
        self.end_pane_states(id, &fields);
        let Some(mut pane) = self.panes.remove(&id) else { return };
        pane.kill();
        self.panes_closed = true;
        if let Some((wi, ti)) = self.workspace_of(id) {
            let ws = &mut self.workspaces[wi];
            ws.marked.remove(&id);
            if ws.sync == SyncScope::Marked && ws.marked.is_empty() {
                ws.sync = SyncScope::Off;
            }
            if !ws.tabs[ti].remove(id) {
                ws.tabs.remove(ti);
                // The tab shown stays shown when an earlier one goes; when it is the one that
                // goes, the next one (or the last) is shown.
                if ti < ws.active || ws.active >= ws.tabs.len() {
                    ws.active = ws.active.saturating_sub(1);
                }
            }
        }
        self.persist_dirty = true;
        self.emit("pane.closed", serde_json::Value::Object(fields));
        self.runbook_pane_closed(id);
        self.check_waits();
        self.relayout();
    }

    pub(super) fn close_workspace(&mut self, client: ClientId, ws: u32) -> Mode {
        match self.keep_and_close_workspace(ws) {
            Ok(()) => {
                self.toast(client, NoticeKind::Notice, "Workspace closed. Enter opens it again.");
                let mut picker = self.workspace_picker(self.clients.get(&client).map_or(0, |c| c.workspace));
                picker.select(|item| item.action.workspace() == Some(ws));
                Mode::Picker(picker)
            }
            Err(message) => {
                if let Some(wi) = self.ws_index(ws) {
                    self.workspaces[wi].close_error = Some(message.clone());
                }
                self.refresh_workspace_pickers();
                self.mark_dirty();
                Mode::confirm(message, ConfirmAction::RetryCloseWorkspace(ws))
            }
        }
    }

    pub(super) fn keep_and_close_workspace(&mut self, ws: u32) -> Result<(), String> {
        self.keep_and_close_workspace_checked(ws, |_| Ok(()))
    }

    pub(super) fn keep_and_close_workspace_checked(
        &mut self,
        ws: u32,
        check: impl FnOnce(&Self) -> Result<(), String>,
    ) -> Result<(), String> {
        let wi = self.ws_index(ws).ok_or("This workspace is no longer available.")?;
        if self.workspaces[wi].closed() {
            self.save_now().map_err(|e| format!("Programs stopped. Couldn't save the final state.\n{e}"))?;
            check(self)?;
            self.finish_workspace_close(ws);
            return Ok(());
        }
        let panes = self.workspaces[wi].panes();
        if panes
            .iter()
            .filter_map(|p| self.panes.get(p))
            .any(|p| p.is_terminal() && p.pid().is_none() && p.exit_code.is_none())
        {
            return Err("A program is still starting. Nothing was stopped. Retry Close once it starts.".into());
        }
        let mut stops: Vec<_> = panes.iter().filter_map(|p| self.panes.get(p).and_then(ProgramStop::new)).collect();
        if let Some(intent) = self.workspaces[wi].kept.as_ref().and_then(|s| s.close_pending.as_ref()) {
            stops.push(ProgramStop::recover(intent));
        }
        let retry = self.workspaces[wi].kept.is_some();
        if !retry {
            self.save_scrollback(true);
            let mut saved = crate::server::persistence::snapshot(self).workspaces.remove(wi);
            saved.close_pending = Some(crate::persist::CloseIntent::new(
                stops.iter().flat_map(|s| s.members.iter().copied()).collect(),
                stops.iter().flat_map(|s| s.sessions.iter().cloned()).collect(),
            ));
            self.workspaces[wi].kept = Some(saved);
        }
        if let Err(e) = self.save_now() {
            if !retry {
                self.workspaces[wi].kept = None;
                // A timed-out writer may still publish the intent. Queue its cancellation after it.
                self.persist();
            }
            return Err(format!(
                "Couldn't save the workspace.\n{}\n{e}",
                if retry { "The workspace is still open. Retry to finish closing." } else { "Nothing was stopped." }
            ));
        }
        if let Err(e) = check(self) {
            if !retry {
                self.workspaces[wi].kept = None;
                // The durable close intent must not survive a cancelled review.
                self.persist();
            }
            return Err(e);
        }
        for &id in &panes {
            self.switches.finish(id);
            self.pane_edits.remove(&id);
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.restart_with = None;
                pane.preparing = false;
                pane.kill();
            }
        }
        for stop in &mut stops {
            stop.since = Instant::now();
            stop.signal(libc::SIGHUP);
        }
        let start = Instant::now();
        loop {
            let remaining = stops.iter_mut().filter_map(|s| (!s.finished()).then_some(())).count();
            if remaining == 0 {
                break;
            }
            if start.elapsed() >= Duration::from_secs(3) {
                return Err("Couldn't stop all programs.\nThe workspace is still open. Retry to finish closing.".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        for pane in panes {
            self.close_pane(pane);
        }
        let saved = self.workspaces[wi].kept.as_mut().expect("closing keeps its layout");
        saved.closed = true;
        saved.close_pending = None;
        self.workspaces[wi].tabs.clear();
        self.workspaces[wi].active = 0;
        self.workspaces[wi].sync = SyncScope::Off;
        self.workspaces[wi].marked.clear();
        self.workspaces[wi].sync_approved.clear();
        self.save_now().map_err(|e| format!("Programs stopped. Couldn't save the final state.\n{e}"))?;
        self.finish_workspace_close(ws);
        Ok(())
    }

    fn finish_workspace_close(&mut self, ws: u32) {
        let Some(wi) = self.ws_index(ws) else { return };
        if self.workspaces[wi].delete_pending {
            return;
        }
        self.workspaces[wi].close_error = None;
        self.emit("workspace.closed", serde_json::json!({ "workspace": self.workspaces[wi].name }));
        let fallback = self.workspaces.iter().find(|w| !w.closed() && w.id != ws).map(|w| w.id);
        let clients: Vec<_> = self.clients.values().filter(|c| c.workspace == ws).map(|c| c.id).collect();
        for id in clients {
            let mode = fallback.is_none().then(|| Mode::Picker(self.workspace_picker(ws)));
            if let Some(c) = self.clients.get_mut(&id) {
                c.workspace = fallback.unwrap_or(ws);
                if let Some(mode) = mode {
                    c.mode = mode;
                }
            }
        }
        self.refresh_workspace_pickers();
        self.mark_dirty();
    }

    pub(super) fn reopen_workspace(&mut self, ws: u32) -> Result<()> {
        let wi = self.ws_index(ws).context("no such workspace")?;
        anyhow::ensure!(!self.workspaces[wi].delete_pending, "Deletion is not finished. Retry Delete in Workspaces.");
        let Some(saved) = self.workspaces[wi].kept.clone() else { return Ok(()) };
        if saved.closed && self.workspaces[wi].close_error.is_some() {
            self.keep_and_close_workspace(ws).map_err(anyhow::Error::msg)?;
        }
        anyhow::ensure!(saved.closed, "The workspace has not finished closing. Retry Close first.");
        let before = self.panes.keys().copied().collect();
        let workspaces = self.workspaces.iter().map(|w| w.id).collect::<Vec<_>>();
        let result = self.with_batch(|server| {
            let result = if saved.unopened {
                server.starting_workspace = Some(saved.name.clone());
                let result = saved
                    .pending_layout
                    .as_deref()
                    .or(saved.definition_source.as_deref())
                    .context("the saved source is missing")
                    .and_then(|s| crate::workspace::parse_workspace(&saved.name, s))
                    .and_then(|d| server.populate_definition(wi, &d));
                server.starting_workspace = None;
                result
            } else {
                crate::server::persistence::restore_workspace_tabs(server, wi, &saved)
            };
            if result.is_err() {
                server.rollback(&before, &workspaces);
                server.workspaces[wi].tabs.clear();
            }
            result
        });
        if let Err(e) = result {
            return Err(e).context("Couldn't open the workspace. Its layout is kept. Fix the cause and retry Open");
        }
        self.workspaces[wi].kept = None;
        self.workspaces[wi].close_error = None;
        self.emit("workspace.opened", serde_json::json!({ "workspace": self.workspaces[wi].name }));
        self.refresh_workspace_pickers();
        self.mark_persist();
        self.relayout();
        Ok(())
    }

    pub fn pane_label(&self, pane: PaneId) -> String {
        let name = self.panes.get(&pane).map(|p| p.display_name()).unwrap_or_default();
        match self.workspace_of(pane) {
            Some((wi, _)) => format!("{} › {name}", self.workspaces[wi].name),
            None => name,
        }
    }

    pub fn pane_info(&self, id: PaneId) -> protocol::PaneInfo {
        let p = &self.panes[&id];
        let (ws, tab, floating, focused) = match self.workspace_of(id) {
            Some((wi, ti)) => {
                let w = &self.workspaces[wi];
                let t = &w.tabs[ti];
                (w.name.clone(), self.tab_label(t), t.is_floating(id), t.focus == Some(id) && w.active == ti)
            }
            None => (String::new(), String::new(), false, false),
        };
        protocol::PaneInfo {
            account: p.spec.account.clone(),
            account_id: p.spec.account_id.clone(),
            id,
            name: p.name.clone(),
            title: p.title.clone(),
            workspace: ws,
            tab,
            kind: match &p.content {
                crate::pane::Content::Preview(pv) => format!("preview:{:?}", pv.kind()).to_lowercase(),
                crate::pane::Content::Files(_) => "files".into(),
                crate::pane::Content::Term(_) => "terminal".into(),
            },
            command: p.spec.command.clone(),
            cwd: p.current_cwd(),
            agent: p.agent.kind.clone(),
            state: p.agent.state,
            exit_code: if p.suspended { None } else { p.exit_code.map(|c| c.unwrap_or(-1)) },
            suspended: p.suspended,
            keyboard: p.input_modes().kitty_flags,
            server: p.ssh.as_ref().map(|s| s.name.clone()),
            session: None,
            production: self.pane_production_shown(id),
            contexts: self.context_labels(id).into_iter().map(|(l, _)| l).collect(),
            focused,
            floating,
            cols: p.cols,
            rows: p.rows,
        }
    }

    /// A tab's name: the one it was given, else what its focused pane shows.
    pub fn tab_label(&self, tab: &Tab) -> String {
        if !tab.name.is_empty() {
            return tab.name.clone();
        }
        tab.focus
            .or_else(|| tab.panes().first().copied())
            .and_then(|p| self.panes.get(&p))
            .map(|p| p.display_name())
            .unwrap_or_else(|| "shell".into())
    }
}

/// What a suspended pane shows until Enter runs `spec`: why it waits, and the command.
fn placeholder(spec: &PaneSpec, reason: &str) -> PaneSpec {
    let command = spec.command.join(" ");
    let line = crate::shell::quote(spec.startup_input.as_deref().map(str::trim_end).unwrap_or(&command));
    PaneSpec {
        command: vec![
            "sh".into(),
            "-c".into(),
            format!(
                "printf '\\033[2m%s\\n\\033[0m$ %s\\n' {} {line}",
                crate::shell::quote(&format!("kraal: {reason}"))
            ),
        ],
        cwd: spec.cwd.clone().filter(|c| c.is_dir()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::Event;

    #[test]
    fn close_recovery_stops_children_forked_after_preparation_and_their_parent_exits() {
        let dir = std::env::temp_dir().join(format!("kraal-close-survivor-{}", crate::identity::new_uuid()));
        std::fs::create_dir(&dir).unwrap();
        let child_file = dir.join("child");
        let ready = dir.join("ready");
        let script = dir.join("survivor");
        crate::testing::write_script(
            &script,
            &format!("#!/bin/sh\ntrap '' HUP TERM\necho $$ > {}\nexec sleep 60\n", child_file.display()),
        );
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = super::super::tests::server(tx);
        let ws = server.new_workspace("closing".into(), None);
        let command = format!(
            "trap 'trap \"\" HUP TERM; sh {} </dev/null >/dev/null 2>&1 & exit 0' HUP; echo ready > {}; while :; do sleep 0.01; done",
            script.display(),
            ready.display()
        );
        let pane = server
            .spawn_pane_in(
                ws,
                None,
                PaneSpec { command: vec!["sh".into(), "-c".into(), command], ..Default::default() },
                protocol::Placement::Tab,
                None,
            )
            .unwrap();
        let wait = |done: &dyn Fn() -> bool| {
            let start = Instant::now();
            while !done() {
                assert!(start.elapsed() < crate::testing::PATIENCE, "the process did not reach the expected state");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait(&|| crate::testing::read_complete_lines(&ready).is_some());
        let parent = server.panes[&pane].pid().unwrap();
        let stop = ProgramStop::new(&server.panes[&pane]).unwrap();
        let mut other = stop.sessions[0].clone();
        other.started = other.started.wrapping_add(1);
        assert!(other.members().is_empty(), "a reused leader's pid is not our session");
        let intent = crate::persist::CloseIntent::new(stop.members.clone(), stop.sessions.clone());
        let prepared = serde_json::to_vec(&intent).unwrap();
        assert!(!child_file.exists(), "the survivor is not in the preparation's process inventory");
        stop.signal(libc::SIGHUP);
        let read_child = || crate::testing::read_complete_lines(&child_file).and_then(|s| s.trim().parse::<u32>().ok());
        wait(&|| read_child().is_some() && !crate::process::running(parent as libc::pid_t));
        let child = read_child().unwrap();
        let started = crate::process::start_time(child).unwrap();
        server.close_pane(pane);
        drop(stop);
        wait(&|| crate::process::start_time(parent).is_none());
        assert!(crate::process::still_running(child, started), "the new child survived its parent and SIGHUP");
        other.started = intent.local_sessions()[0].started;
        other.token.push_str("different");
        assert!(other.members().is_empty(), "an orphaned session needs its ownership token");
        let restored = serde_json::from_slice(&prepared).unwrap();
        let mut recovered = ProgramStop::recover(&restored);
        let found = recovered.members.contains(&(child, started));
        if !found {
            // SAFETY: clean up only this test's survivor when discovery regresses.
            unsafe { libc::kill(child as libc::pid_t, libc::SIGKILL) };
        }
        assert!(found, "recovery must find the new child through its retained session ownership");
        recovered.since = Instant::now() - Duration::from_secs(3);
        let start = Instant::now();
        while !recovered.finished() {
            assert!(start.elapsed() < crate::testing::PATIENCE);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!crate::process::still_running(child, started), "Closed requires actual survivor termination");
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn broken() -> PaneSpec {
        let dir = std::env::temp_dir().join(format!("kraal-fallback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("{:?}", std::thread::current().id()));
        crate::testing::write_script(&file, "#!/no/such/interpreter\n");
        PaneSpec { command: vec![file.to_string_lossy().into_owned()], ..Default::default() }
    }

    #[test]
    fn synchronous_launch_failure_does_not_wait_for_its_full_queue() {
        for capacity in [1, 4096] {
            let (tx, rx) = super::super::queue::channel(capacity);
            let producer = tx.clone();
            std::thread::spawn(move || {
                for i in 0..capacity {
                    assert!(producer.send(Event::Warning(i.to_string())).is_ok());
                }
            })
            .join()
            .unwrap();
            let mut server = super::super::tests::server(tx);
            let spec = broken();
            let file = spec.command[0].clone();
            let (done_tx, done_rx) = super::super::queue::channel(1);
            let job = super::super::jobs::JobId(server.alloc_id());
            let delivery = server.jobs.register(job, done_tx, move |server, ()| {
                server.new_workspace("empty".into(), None);
                server.focus_reported.insert(999);
                server.with_batch(|server| {
                    let ws = server.new_workspace("kept".into(), None);
                    server.spawn_pane_in(ws, None, spec, protocol::Placement::Tab, None).unwrap();
                });
                assert!(server.panes.values().all(|p| p.suspended));
                assert!(server.workspaces.iter().any(|w| w.name == "empty"));
                assert!(server.focus_reported.contains(&999));
                assert!(server.warnings.is_empty(), "queued work has not run");
            });
            // The server queue is full; keep this completion separate until the handler runs.
            std::thread::spawn(move || delivery.complete(Ok(()))).join().unwrap();
            server.handle_and_follow(done_rx.recv_timeout(crate::testing::PATIENCE).unwrap());
            assert_eq!(server.workspaces.len(), 2, "empty workspaces are kept");
            assert!(server.focus_reported.is_empty(), "focus at the handler boundary");
            for i in 0..capacity {
                assert!(matches!(rx.try_recv(), Ok(Event::Warning(w)) if w == i.to_string()));
            }
            std::fs::remove_file(file).unwrap();
        }
    }

    #[test]
    fn synchronous_launch_failure_survives_receiver_shutdown() {
        let (tx, rx) = super::super::queue::channel(1);
        drop(rx);
        let mut server = super::super::tests::server(tx);
        let spec = broken();
        let file = spec.command[0].clone();
        server.with_batch(|server| {
            server.spawn_pane(None, spec).unwrap();
        });
        assert!(server.panes.values().all(|p| p.suspended));
        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn a_returned_failure_owns_the_failed_process_and_cannot_suspend_its_replacement() {
        let (tx, _rx) = super::super::queue::channel(1);
        let mut server = super::super::tests::server(tx);
        let batch = crate::pane::batch();
        let spec = broken();
        let file = spec.command[0].clone();
        let id = server.spawn_pane(None, spec).unwrap();
        let hold = server.panes[&id].process_hold().unwrap();
        let pending = server.panes.get_mut(&id).unwrap().take_pending().unwrap();
        let crate::pane::Started::Failed(failed_id, process, why) = pending.start() else { panic!("failure") };
        assert_eq!(failed_id, id);
        assert!(process.is(&hold));
        assert!(why.contains("No such file or directory"), "{why}");
        let pane = server.panes.get_mut(&id).unwrap();
        pane.spec.command = vec!["true".into()];
        pane.respawn(&server.env, &server.tx).unwrap();
        assert!(!pane.process_hold().unwrap().is(&process));
        server.pane_failed(id, &process, &why);
        assert!(!server.panes[&id].suspended);
        let pending = server.panes.get_mut(&id).unwrap().take_pending().unwrap();
        server.panes.remove(&id);
        assert!(matches!(pending.start(), crate::pane::Started::Cancelled));
        server.pane_failed(id, &process, &why);
        assert!(server.panes.is_empty());
        drop(batch);
        std::fs::remove_file(file).unwrap();
    }
}
