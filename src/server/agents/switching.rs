use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::super::attention;
use super::super::panes::ProgramStop;
use super::super::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind};
use super::super::{ClientId, Server};
use super::choices::cannot_switch_mode;
use super::{AMBIGUOUS, AccountSwitch, AccountSwitches, SwitchStage};
use crate::account_status::{self, Status};
use crate::accounts::{self, Account};
use crate::agent::AgentState;
use crate::layout::PaneId;
use crate::pane::PaneSpec;
use crate::theme::NoticeKind;

pub(super) fn switch_timing(hooked: bool, state: AgentState, exited: bool) -> SwitchStage {
    if exited || hooked && matches!(state, AgentState::Idle | AgentState::Done) {
        SwitchStage::Stopping
    } else if hooked && matches!(state, AgentState::Working | AgentState::Blocked) {
        SwitchStage::Waiting
    } else {
        SwitchStage::Confirm
    }
}

/// Whether a conversation can move between accounts of this agent (they share its storage).
fn conversations_move(agent: &str) -> bool {
    crate::agent_registry::find(agent).is_some_and(|a| a.cross_account_resume)
}

#[derive(Clone, PartialEq, Eq)]
struct ConversationCheck {
    from: Account,
    to: Account,
    session: Option<String>,
    undo: bool,
    undo_session: Option<String>,
}

impl ConversationCheck {
    fn verify(&self) -> Result<()> {
        if conversations_move(&self.from.agent) {
            let id = self.session.as_deref().context("could not verify the Claude conversation id")?;
            accounts::check_carry(&self.from, &self.to, id)?;
        } else {
            if let Some(id) = self.session.as_deref() {
                accounts::check_resume(&self.from, id)?;
            } else {
                anyhow::ensure!(self.undo, "conversation id is unavailable; finish a turn and try again");
            }
            if self.undo {
                accounts::check_resume(
                    &self.to,
                    self.undo_session.as_deref().context("the original conversation id is unavailable")?,
                )?;
            }
        }
        Ok(())
    }
}

enum StartProgress {
    Pending,
    Started,
    Stop(SwitchStage),
    Failed,
}

impl AccountSwitches {
    pub(in crate::server) fn get(&self, pane: &PaneId) -> Option<&AccountSwitch> {
        self.pending.get(pane)
    }

    pub(in crate::server) fn contains(&self, pane: PaneId) -> bool {
        self.pending.contains_key(&pane)
    }

    pub(in crate::server) fn finish(&mut self, pane: PaneId) -> Option<AccountSwitch> {
        self.pending.remove(&pane)
    }

    pub(in crate::server) fn retain_panes(&mut self, exists: impl Fn(PaneId) -> bool) {
        self.pending.retain(|&pane, _| exists(pane));
    }

    pub(in crate::server) fn blocks_restart(&self, pane: PaneId) -> bool {
        self.get(&pane).is_some_and(|s| matches!(s.stage, SwitchStage::Transferring | SwitchStage::RecoveryChecking))
    }

    pub(in crate::server) fn busy(&self) -> bool {
        self.pending.values().any(|s| {
            matches!(
                s.stage,
                SwitchStage::Stopping
                    | SwitchStage::Transferring
                    | SwitchStage::RecoveryChecking
                    | SwitchStage::Starting
                    | SwitchStage::Restoring
                    | SwitchStage::Recovering
                    | SwitchStage::RecoveryStopping
            )
        })
    }

    fn in_stages<'a>(&'a self, stages: &'a [SwitchStage]) -> impl Iterator<Item = (PaneId, SwitchStage)> + 'a {
        self.pending.iter().filter_map(|(&id, s)| stages.contains(&s.stage).then_some((id, s.stage)))
    }

    fn start_progress(&mut self, pane: &crate::pane::Pane) -> StartProgress {
        let s = self.pending.get_mut(&pane.id).unwrap();
        let elapsed = s.started.get_or_insert_with(Instant::now).elapsed();
        let expected = if s.stage == SwitchStage::Restoring { &s.from.agent } else { &s.to.agent };
        let running = pane.pid().is_some_and(|pid| crate::process::running(pid as libc::pid_t));
        if running
            && pane.has_printed()
            && pane.agent.kind.as_ref() == Some(expected)
            && elapsed >= Duration::from_secs(1)
        {
            StartProgress::Started
        } else if elapsed >= Duration::from_secs(10) {
            if running {
                // Wait for the last output and exit before another process takes this pane.
                StartProgress::Stop(if s.stage == SwitchStage::Restoring {
                    SwitchStage::RecoveryStopping
                } else {
                    SwitchStage::Recovering
                })
            } else {
                StartProgress::Failed
            }
        } else {
            StartProgress::Pending
        }
    }

    /// Whether a pending change already goes to `to`'s home: no Claude conversation moves into a
    /// home where an agent runs, so a second change there would fail after stopping its source.
    pub(in crate::server) fn reserved(&self, to: &Account) -> bool {
        conversations_move(&to.agent) && self.pending.values().any(|s| s.to.same_home(to))
    }

    fn reserve(&mut self, pane: PaneId, switch: AccountSwitch) {
        self.pending.insert(pane, switch);
    }

    fn check(&mut self, pane: PaneId) -> Option<(u32, Account)> {
        let switch = self.pending.get_mut(&pane)?;
        switch.stage = SwitchStage::Checking;
        Some((switch.token, switch.to.clone()))
    }

    fn checked(&self, pane: PaneId, token: u32) -> Option<&AccountSwitch> {
        self.pending.get(&pane).filter(|s| s.token == token && s.stage == SwitchStage::Checking)
    }

    fn cancel(&mut self, pane: PaneId, token: u32) -> bool {
        if self.pending.get(&pane).is_some_and(|s| {
            s.token == token
                && matches!(
                    s.stage,
                    SwitchStage::Checking | SwitchStage::Verifying | SwitchStage::Waiting | SwitchStage::Confirm
                )
        }) {
            self.pending.remove(&pane);
            true
        } else {
            false
        }
    }

    fn confirm(&mut self, pane: PaneId, token: u32, client: ClientId) -> bool {
        let Some(switch) = self.pending.get_mut(&pane).filter(|s| s.token == token && s.stage == SwitchStage::Confirm)
        else {
            return false;
        };
        switch.client = Some(client);
        true
    }

    fn prepared(&mut self, pane: PaneId, session: Option<String>, cwd: Option<std::path::PathBuf>, stage: SwitchStage) {
        let switch = self.pending.get_mut(&pane).unwrap();
        switch.original.session = session.clone();
        if switch.observed.is_none() {
            switch.original.cwd = cwd.or_else(|| switch.original.cwd.clone());
        }
        switch.session = session;
        switch.stage = stage;
    }

    fn stop(&mut self, pane: PaneId, stage: SwitchStage, stop: Option<ProgramStop>) {
        let switch = self.pending.get_mut(&pane).unwrap();
        switch.stage = stage;
        switch.stopping = stop;
        if let Some(stop) = &switch.stopping {
            stop.signal(libc::SIGHUP);
        }
    }

    fn stopped(&mut self, pane: PaneId, exited: bool) -> bool {
        let Some(stop) = self.pending.get_mut(&pane).and_then(|s| s.stopping.as_mut()) else { return true };
        stop.finished() && exited
    }

    fn spawned(&mut self, pane: PaneId) {
        let switch = self.pending.get_mut(&pane).unwrap();
        switch.started = Some(Instant::now());
        switch.stopping = None;
    }

    fn starting(&mut self, pane: PaneId) {
        self.pending.get_mut(&pane).unwrap().stage = SwitchStage::Starting;
    }

    fn restore(&mut self, pane: PaneId) {
        self.pending.get_mut(&pane).unwrap().stage = SwitchStage::Restoring;
    }
}

impl Server {
    pub(in crate::server) fn undo_account_change(&mut self, pane: PaneId, client: ClientId) -> Result<()> {
        let saved =
            self.panes.get(&pane).and_then(|p| p.spec.account_undo.as_deref()).context("no account change to undo")?;
        let to = self.spec_account(saved).context("the previous account was removed; open Change account")?;
        self.begin_switch(pane, &to, Some(client), false, true)
    }

    /// Reserve the pane before the asynchronous check, so attached clients share one change.
    pub(in crate::server) fn switch_account(
        &mut self,
        pane: PaneId,
        to: &Account,
        client: Option<ClientId>,
        ask: bool,
    ) -> Result<()> {
        self.begin_switch(pane, to, client, ask, false)
    }

    pub(super) fn begin_switch(
        &mut self,
        pane: PaneId,
        to: &Account,
        client: Option<ClientId>,
        ask: bool,
        undo: bool,
    ) -> Result<()> {
        let p = self.panes.get(&pane).context("no such pane")?;
        let observed = if p.spec.account.is_none() && p.spec.account_id.is_none() {
            Some(self.observed_agent(pane)?.context("Can't switch this process. Open with another account.")?)
        } else {
            None
        };
        let from = observed
            .as_ref()
            .map(|a| a.account.clone())
            .or_else(|| self.pane_account(pane))
            .context("Can't switch this process. Open with another account.")?;
        anyhow::ensure!(from.agent == to.agent, "{} cannot continue on a {} account", from.agent, to.agent);
        if from.id == to.id || from.same_home(to) {
            return Ok(());
        }
        anyhow::ensure!(!self.switches.contains(pane), "an account change is already pending for this pane");
        // A closed pane's change reserves nothing, though the periodic check has not dropped it yet.
        self.switches.retain_panes(|p| self.panes.contains_key(&p));
        anyhow::ensure!(!self.switches.reserved(to), "another pane is already changing to {}", accounts::label(to));
        anyhow::ensure!(!self.pane_edits.contains_key(&pane), "a pane edit is already being applied");
        let kind = accounts::agent_kind(&to.agent).context("unknown agent")?;
        let mut original = match &observed {
            Some(a) => PaneSpec {
                command: a.command.clone(),
                cwd: Some(a.cwd.clone()),
                session: Some(a.session.clone()),
                account: Some(accounts::key(&from.agent, &from.name)),
                account_id: Some(from.id.clone()),
                env: vec![(kind.home_env.into(), from.home.clone()), ("PATH".into(), a.path.clone())],
                ..Default::default()
            },
            None => p.spec.clone(),
        };
        original.account = Some(accounts::key(&from.agent, &from.name));
        original.account_id = Some(from.id.clone());
        accounts::resumable_args(kind, &accounts::launch_args(kind, &original.command))
            .context("a one-shot run (-p, exec) is not a conversation to continue")?;
        let process = p.process_hold();
        let token = self.alloc_id();
        self.switches.reserve(
            pane,
            AccountSwitch {
                token,
                from,
                to: to.clone(),
                session: None,
                process,
                client,
                ask,
                original,
                observed,
                undo,
                started: None,
                stopping: None,
                stage: SwitchStage::Checking,
            },
        );
        if let Some(client) = client {
            self.toast(client, NoticeKind::Notice, format!("Checking sign-in for {}", accounts::label(to)));
        }
        self.check_switch(pane, false);
        Ok(())
    }

    fn check_switch(&mut self, pane: PaneId, approved: bool) {
        let Some((token, to)) = self.switches.check(pane) else { return };
        if !self.background(
            move || account_status::check(&to),
            move |server, status| server.switch_checked(pane, token, status, approved),
        ) {
            self.switch_checked(pane, token, Status::CouldNotCheck, approved);
        }
        self.refresh_switch_views(pane);
        self.mark_dirty();
    }

    pub(super) fn switch_checked(&mut self, pane: PaneId, token: u32, status: Status, approved: bool) {
        let Some(switch) = self.switches.checked(pane, token) else {
            return;
        };
        if !self.panes.contains_key(&pane) {
            self.switches.finish(pane);
            self.refresh_switch_views(pane);
            return;
        }
        if status != Status::SignedIn {
            let message =
                format!("{}: {}.\nThe current agent keeps running.", accounts::label(&switch.to), status.label());
            self.switch_failed(pane, &message, status == Status::NotSignedIn);
        } else if let Err(e) = self.prepare_switch(pane, approved) {
            self.switch_failed(pane, &format!("{e:#}. The current pane is unchanged."), false);
        }
    }

    pub(super) fn switch_failed(&mut self, pane: PaneId, reason: &str, sign_in: bool) {
        let Some(switch) = self.switches.finish(pane) else { return };
        self.refresh_switch_views(pane);
        let unsupported = switch
            .observed
            .as_ref()
            .is_some_and(|before| self.observed_agent(pane).is_ok_and(|now| now.as_ref() != Some(before)));
        let client = switch.client.or(self.primary);
        if let Some(c) = client.and_then(|id| self.clients.get_mut(&id)).filter(|c| matches!(c.mode, Mode::Normal)) {
            let action = if sign_in {
                ConfirmAction::SignInAccount(switch.to)
            } else {
                ConfirmAction::RetryAccountSwitch { pane, account: switch.to, undo: switch.undo }
            };
            c.mode = if unsupported {
                cannot_switch_mode(pane)
            } else {
                Mode::confirm(format!("Could not change account\n{reason}"), action)
            };
            self.mark_dirty();
        } else {
            self.notify(
                NoticeKind::Error,
                "Could not change account",
                &format!("{reason} Open Change account to try again."),
                Some(pane),
            );
        }
    }

    fn switch_conversation(&self, pane: PaneId) -> Result<ConversationCheck> {
        let switch = self.switches.get(&pane).context("no pending account change")?;
        let p = self.panes.get(&pane).context("the pane was closed")?;
        anyhow::ensure!(self.switch_process_current(pane), "the pane's process changed; choose an account again");
        if let Some(observed) = &switch.observed {
            anyhow::ensure!(
                self.observed_agent(pane)?.as_ref() == Some(observed),
                "Can't switch this process. Open with another account."
            );
        }
        let from = switch
            .observed
            .as_ref()
            .map(|a| a.account.clone())
            .or_else(|| self.pane_account(pane))
            .context("the pane's account was removed")?;
        anyhow::ensure!(
            from.id == switch.from.id && from.same_home(&switch.from),
            "the pane's account changed; choose an account again"
        );
        anyhow::ensure!(
            self.switch_source_current(&switch.from),
            "the source account changed; choose an account again"
        );
        let to = self.accounts.by_id(&switch.to.id).context("the target account was removed")?;
        anyhow::ensure!(to.same_home(&switch.to), "the target account changed; choose an account again");
        if switch.client.is_none() {
            anyhow::ensure!(self.restarts_by_itself(pane), "this command now needs confirmation before restarting");
            anyhow::ensure!(
                !to.sign_in_required
                    && self.usage.peak(&to, accounts::now()).is_some_and(|p| p < self.accounts.threshold(&to) as f64),
                "the target account no longer has verified usage below its threshold"
            );
        }
        let session = if switch.observed.is_some() {
            switch.original.session.clone()
        } else {
            p.agent.session_id.clone().or_else(|| switch.original.session.clone())
        };
        Ok(ConversationCheck {
            from,
            to,
            session,
            undo: switch.undo,
            undo_session: switch.original.account_undo.as_ref().and_then(|s| s.session.clone()),
        })
    }

    pub(super) fn prepare_switch(&mut self, pane: PaneId, approved: bool) -> Result<()> {
        let check = self.switch_conversation(pane)?;
        let token = self.switches.get(&pane).unwrap().token;
        self.switches.pending.get_mut(&pane).unwrap().stage = SwitchStage::Verifying;
        let work = check.clone();
        anyhow::ensure!(
            self.background(
                move || work.verify(),
                move |server, result| server.conversation_checked(pane, token, check, approved, result),
            ),
            "could not start the conversation check"
        );
        self.refresh_switch_views(pane);
        self.mark_dirty();
        Ok(())
    }

    fn conversation_checked(
        &mut self,
        pane: PaneId,
        token: u32,
        check: ConversationCheck,
        approved: bool,
        result: Result<()>,
    ) {
        if !self.switches.get(&pane).is_some_and(|s| s.token == token && s.stage == SwitchStage::Verifying) {
            return;
        }
        let result = result.and_then(|()| {
            anyhow::ensure!(
                self.switch_conversation(pane)? == check,
                "the conversation or accounts changed; choose an account again"
            );
            self.prepare_verified_switch(pane, approved, check.session)
        });
        if let Err(e) = result {
            self.switch_failed(pane, &format!("{e:#}. The current pane is unchanged."), false);
        }
    }

    fn prepare_verified_switch(&mut self, pane: PaneId, approved: bool, session: Option<String>) -> Result<()> {
        let switch = self.switches.get(&pane).context("no pending account change")?;
        let p = self.panes.get(&pane).context("the pane was closed")?;
        let to = switch.to.clone();
        let hooked = p.agent.hooked
            && switch.observed.as_ref().is_none_or(|a| p.agent.session_process == Some((a.pid, a.started)));
        let timing = switch_timing(hooked, p.agent.state, p.exit_code.is_some());
        // An automatic change asks instead once someone has used the pane, even during the checks.
        let attended = switch.client.is_none() && !self.unattended(pane);
        let stage = if approved {
            SwitchStage::Stopping
        } else if switch.ask || (attended || !conversations_move(&to.agent)) && timing == SwitchStage::Stopping {
            SwitchStage::Confirm
        } else {
            timing
        };
        let exited = p.exit_code.is_some();
        let client = switch.client;
        let cwd = switch.observed.is_none().then(|| p.current_cwd()).flatten();
        self.switches.prepared(pane, session, cwd, stage);
        match stage {
            SwitchStage::Waiting => {
                self.notify(
                    NoticeKind::Notice,
                    "Switch after this turn",
                    &format!("{}; open Change account to cancel", accounts::label(&to)),
                    Some(pane),
                );
            }
            SwitchStage::Confirm => {
                if let Some(client) = client.or(self.primary) {
                    let mode = self.pending_switch_mode(pane);
                    if let Some(c) = self.clients.get_mut(&client).filter(|c| matches!(c.mode, Mode::Normal)) {
                        c.mode = mode;
                    } else {
                        self.notify(
                            NoticeKind::Warning,
                            "Stop and switch?",
                            "Open Change account to confirm or cancel",
                            Some(pane),
                        );
                    }
                } else {
                    self.switches.finish(pane);
                    self.notify(
                        NoticeKind::Warning,
                        "Account change needs confirmation",
                        "Open Change account to try again",
                        Some(pane),
                    );
                }
            }
            SwitchStage::Stopping => {
                self.stop_switch(pane, SwitchStage::Stopping);
                if exited {
                    self.finish_switch(pane);
                }
            }
            SwitchStage::Checking
            | SwitchStage::Verifying
            | SwitchStage::Transferring
            | SwitchStage::RecoveryChecking
            | SwitchStage::Starting
            | SwitchStage::Restoring
            | SwitchStage::Recovering
            | SwitchStage::RecoveryStopping => {
                unreachable!()
            }
        }
        self.refresh_switch_views(pane);
        self.mark_dirty();
        Ok(())
    }

    fn refresh_switch_views(&mut self, pane: PaneId) {
        let clients: Vec<ClientId> = self
            .clients
            .iter()
            .filter_map(|(&id, c)| {
                let pending = match &c.mode {
                    Mode::Confirm(q) => matches!(q.action, ConfirmAction::StopAndSwitch { pane: p, .. } if p == pane),
                    Mode::Picker(p) => p
                        .items
                        .iter()
                        .any(|i| matches!(i.action, PickAction::CancelSwitch { pane: p, .. } if p == pane)),
                    _ => false,
                };
                pending.then_some(id)
            })
            .collect();
        for id in clients {
            let mode = self.pending_switch_mode(pane);
            self.clients.get_mut(&id).unwrap().mode = mode;
        }
    }

    pub(in crate::server) fn pending_switch_mode(&self, pane: PaneId) -> Mode {
        let Some(switch) = self.switches.get(&pane) else { return Mode::Normal };
        if matches!(
            switch.stage,
            SwitchStage::Stopping
                | SwitchStage::Transferring
                | SwitchStage::RecoveryChecking
                | SwitchStage::Starting
                | SwitchStage::Restoring
                | SwitchStage::Recovering
                | SwitchStage::RecoveryStopping
        ) {
            return Mode::Normal;
        }
        if switch.stage == SwitchStage::Confirm {
            let effect = if conversations_move(&switch.to.agent) || switch.undo {
                "Continue conversation"
            } else {
                "New conversation"
            };
            let reason = if switch.ask {
                format!("{} reached its usage threshold.\n", accounts::label(&switch.from))
            } else {
                String::new()
            };
            return Mode::confirm(
                format!(
                    "Stop and switch?\n{reason}{}: {effect} on {}. This stops the current agent.",
                    self.pane_label(pane),
                    accounts::label(&switch.to)
                ),
                ConfirmAction::StopAndSwitch { pane, token: switch.token },
            );
        }
        let detail = match switch.stage {
            SwitchStage::Waiting => "Switch after this turn",
            SwitchStage::Checking => "Checking sign-in",
            SwitchStage::Verifying => "Checking conversation",
            SwitchStage::Stopping
            | SwitchStage::Transferring
            | SwitchStage::RecoveryChecking
            | SwitchStage::Confirm
            | SwitchStage::Starting
            | SwitchStage::Restoring
            | SwitchStage::Recovering
            | SwitchStage::RecoveryStopping => unreachable!(),
        };
        Mode::Picker(Picker::new(
            PickerKind::Agent,
            "Change account",
            vec![PickItem {
                label: "Cancel account change".into(),
                detail: format!("{detail} · {}", accounts::label(&switch.to)),
                action: PickAction::CancelSwitch { pane, token: switch.token },
                state: None,
                dim: false,
            }],
        ))
    }

    pub(in crate::server) fn cancel_switch(&mut self, pane: PaneId, token: u32) {
        if self.switches.cancel(pane, token) {
            self.refresh_switch_views(pane);
            self.mark_dirty();
        }
    }

    pub(in crate::server) fn confirm_switch(&mut self, pane: PaneId, token: u32, client: ClientId, yes: bool) {
        if !self.switches.get(&pane).is_some_and(|s| s.token == token && s.stage == SwitchStage::Confirm) {
            return;
        }
        if yes {
            self.switches.confirm(pane, token, client);
            self.check_switch(pane, true);
        } else {
            self.cancel_switch(pane, token);
        }
    }

    pub(in crate::server) fn check_switches(&mut self) {
        self.switches.retain_panes(|p| self.panes.contains_key(&p));
        let ready: Vec<PaneId> = self
            .switches
            .in_stages(&[SwitchStage::Waiting])
            .filter_map(|(id, _)| {
                let p = self.panes.get(&id)?;
                (switch_timing(p.agent.hooked, p.agent.state, p.exit_code.is_some()) != SwitchStage::Waiting)
                    .then_some(id)
            })
            .collect();
        for pane in ready {
            self.check_switch(pane, false);
        }
        let stopping: Vec<_> = self
            .switches
            .in_stages(&[SwitchStage::Stopping, SwitchStage::Recovering, SwitchStage::RecoveryStopping])
            .collect();
        for (id, stage) in stopping {
            if stage == SwitchStage::Stopping {
                self.finish_switch(id);
            } else {
                self.switch_start_failed(id, "the agent did not start");
            }
        }
        let starting: Vec<_> = self.switches.in_stages(&[SwitchStage::Starting, SwitchStage::Restoring]).collect();
        for (id, _) in starting {
            let Some(p) = self.panes.get(&id).filter(|p| !p.suspended) else { continue };
            match self.switches.start_progress(p) {
                StartProgress::Pending => {}
                StartProgress::Started => self.switch_started(id),
                StartProgress::Stop(stage) => self.stop_switch(id, stage),
                StartProgress::Failed => self.switch_start_failed(id, "the agent did not start"),
            }
        }
    }

    fn stop_switch(&mut self, pane: PaneId, stage: SwitchStage) {
        self.switches.stop(pane, stage, ProgramStop::new(&self.panes[&pane]));
    }

    fn switch_stopped(&mut self, pane: PaneId) -> bool {
        self.switches.stopped(pane, self.panes.get(&pane).is_none_or(|p| p.exit_code.is_some()))
    }

    /// Keep the source until the replacement reports a usable start, including an early exit.
    pub(in crate::server) fn finish_switch(&mut self, pane: PaneId) {
        if !self.switch_stopped(pane) {
            return;
        }
        let Some(switch) = self.switches.get(&pane).filter(|s| s.stage == SwitchStage::Stopping) else { return };
        let (token, from, to, session) = (switch.token, switch.from.clone(), switch.to.clone(), switch.session.clone());
        if let Err(e) = self.switch_accounts_current(pane) {
            self.switch_start_failed(pane, &format!("{e:#}"));
            return;
        }
        self.switches.pending.get_mut(&pane).unwrap().stage = SwitchStage::Transferring;
        if !self.background(
            move || session.as_deref().map(|id| accounts::carry_conversation(&from, &to, id)).transpose(),
            move |server, result| server.conversation_carried(pane, token, result),
        ) {
            self.conversation_carried(pane, token, Err(anyhow::anyhow!("could not start the conversation transfer")));
        }
    }

    fn switch_source_current(&self, from: &Account) -> bool {
        self.accounts
            .by_home(&from.agent, &crate::paths::expand_tilde(&from.home))
            .is_some_and(|a| a.id == from.id && a.agent == from.agent)
    }

    fn switch_process_current(&self, pane: PaneId) -> bool {
        let Some(switch) = self.switches.get(&pane) else { return false };
        let Some(p) = self.panes.get(&pane) else { return false };
        match (&switch.process, p.process_hold()) {
            (Some(before), Some(now)) => before.is(&now),
            (None, None) => true,
            _ => false,
        }
    }

    fn switch_accounts_current(&self, pane: PaneId) -> Result<()> {
        let switch = self.switches.get(&pane).context("no pending account change")?;
        anyhow::ensure!(self.panes.contains_key(&pane), "the pane was closed");
        anyhow::ensure!(
            self.switch_source_current(&switch.from),
            "the source account changed during the account switch"
        );
        anyhow::ensure!(
            self.accounts.by_id(&switch.to.id).is_some_and(|a| a.agent == switch.to.agent && a.same_home(&switch.to)),
            "the target account changed during the account switch"
        );
        Ok(())
    }

    fn conversation_carried(&mut self, pane: PaneId, token: u32, result: Result<Option<bool>>) {
        let Some(switch) =
            self.switches.get(&pane).filter(|s| s.token == token && s.stage == SwitchStage::Transferring)
        else {
            return;
        };
        if !self.switch_process_current(pane) {
            self.switches.finish(pane);
            self.refresh_switch_views(pane);
            return;
        }
        let (to, session, original, undo) =
            (switch.to.clone(), switch.session.clone(), switch.original.clone(), switch.undo);
        let result = (|| -> Result<()> {
            self.switch_accounts_current(pane)?;
            let carried = result?;
            let kind = accounts::agent_kind(&to.agent).context("unknown agent")?;
            let resume = if undo && to.agent == "codex" {
                original.account_undo.as_ref().and_then(|s| s.session.clone())
            } else {
                session.filter(|_| carried == Some(true))
            };
            let source = if undo && to.agent == "codex" {
                original.account_undo.as_deref().context("no account change to undo")?
            } else {
                &original
            };
            let (args, exact) = accounts::resumable_args(kind, &accounts::launch_args(kind, &source.command))
                .context("a one-shot run is not a conversation to continue")?;
            let mut spec = PaneSpec {
                worktree: original.worktree.clone(),
                close_on_exit: original.close_on_exit,
                ..self.agent_spec(&to, source.cwd.clone(), resume.as_deref(), &args)?
            };
            if let Some(program) = source.command.first().filter(|p| std::path::Path::new(p).is_absolute()) {
                spec.command[0] = program.clone();
            }
            spec.env.extend(source.env.iter().filter(|(k, _)| k == "PATH").cloned());
            crate::worktree_setup::carry_env(&original.env, &mut spec.env);
            crate::tmux::carry_env(&original.env, &mut spec.env);
            self.switches.starting(pane);
            if !undo {
                let mut previous = original.clone();
                previous.account_undo = None;
                spec.account_undo = Some(Box::new(previous));
            }
            if !exact {
                self.suspend_pane(pane, spec, AMBIGUOUS)?;
                let p = self.panes.get_mut(&pane).unwrap();
                p.agent.session_id = p.spec.session.clone();
                return Ok(());
            }
            self.start_switch_spec(pane, spec)
        })();
        if let Err(e) = result {
            self.notify(NoticeKind::Error, "Could not change account", &format!("{e:#}"), Some(pane));
            self.switch_start_failed(pane, &format!("{e:#}"));
        }
        self.refresh_switch_views(pane);
        self.mark_persist();
        self.relayout();
    }

    pub(super) fn start_switch_spec(&mut self, pane: PaneId, spec: PaneSpec) -> Result<()> {
        let p = self.panes.get_mut(&pane).context("the pane was closed")?;
        p.spec = spec;
        p.respawn(&self.env, &self.tx)?;
        // A new Codex conversation must not inherit the source conversation's hook identity.
        p.agent.session_id = p.spec.session.clone();
        self.switches.pending.get_mut(&pane).unwrap().process = p.process_hold();
        self.switches.spawned(pane);
        Ok(())
    }

    pub(in crate::server) fn switch_start_failed(&mut self, pane: PaneId, reason: &str) {
        if let Some(switch) = self.switches.get(&pane)
            && switch.stopping.is_none()
        {
            let stage = if matches!(switch.stage, SwitchStage::Restoring | SwitchStage::RecoveryStopping) {
                SwitchStage::RecoveryStopping
            } else {
                SwitchStage::Recovering
            };
            self.stop_switch(pane, stage);
        }
        if !self.switch_stopped(pane) {
            return;
        }
        let Some(switch) = self.switches.get(&pane) else { return };
        if matches!(switch.stage, SwitchStage::Restoring | SwitchStage::RecoveryStopping) {
            self.check_switch_recovery(pane, reason, false);
            return;
        }
        self.check_switch_recovery(pane, reason, true);
    }

    fn check_switch_recovery(&mut self, pane: PaneId, reason: &str, retry: bool) {
        let Some(switch) = self.switches.pending.get_mut(&pane) else { return };
        switch.stage = SwitchStage::RecoveryChecking;
        // Recovery can follow a failed destination or source launch, so reserve that process.
        switch.process = self.panes.get(&pane).and_then(|p| p.process_hold());
        let (token, from, original) = (switch.token, switch.from.clone(), switch.original.clone());
        let accounts = self.accounts.clone();
        let reason = reason.to_string();
        let failure = reason.clone();
        let fallback = original.clone();
        #[cfg(test)]
        let pending = RECOVERY_LOOKUP.with(|gate| gate.borrow_mut().take());
        if !self.background(
            move || {
                #[cfg(test)]
                if let Some(pending) = pending {
                    pending.recv().unwrap();
                }
                let (spec, exact) = super::launch::resume_spec(&accounts, &original, original.session.as_deref());
                let saved = original.session.as_deref().is_some_and(|s| accounts::check_resume(&from, s).is_ok());
                (spec, exact, saved)
            },
            move |server, result| server.switch_recovery_checked(pane, token, &reason, retry, result),
        ) {
            self.switch_recovery_checked(pane, token, &failure, false, (fallback, false, false));
        }
    }

    fn switch_recovery_checked(
        &mut self,
        pane: PaneId,
        token: u32,
        reason: &str,
        retry: bool,
        (spec, exact, saved): (PaneSpec, bool, bool),
    ) {
        if !self.switches.get(&pane).is_some_and(|s| s.token == token && s.stage == SwitchStage::RecoveryChecking) {
            return;
        }
        if !self.switch_process_current(pane) {
            self.switches.finish(pane);
            self.refresh_switch_views(pane);
            return;
        }
        let current = self.switch_source_current(&self.switches.get(&pane).unwrap().from);
        if retry && exact && current {
            self.switches.restore(pane);
            if let Err(e) = self.start_switch_spec(pane, spec) {
                self.check_switch_recovery(pane, &format!("{e:#}"), false);
            }
            self.mark_persist();
            self.relayout();
            return;
        }
        let saved = saved && current;
        let switch = self.switches.finish(pane).unwrap();
        let message = if saved {
            "Couldn't resume. Your conversation is saved. Enter retries on the previous account."
        } else {
            "Couldn't resume. The previous command and pane output are available. Enter retries."
        };
        if let Some(p) = self.panes.get_mut(&pane) {
            p.spec = if current { spec } else { switch.original };
            p.agent = crate::agent::AgentTracker { session_id: p.spec.session.clone(), ..Default::default() };
            p.exit_code = Some(None);
        }
        self.notify(NoticeKind::Error, "Could not change account", &format!("{message} {reason}"), Some(pane));
        self.refresh_switch_views(pane);
        self.mark_persist();
    }

    pub(super) fn switch_started(&mut self, pane: PaneId) {
        let Some(switch) = self.switches.finish(pane) else { return };
        if switch.stage == SwitchStage::Restoring {
            self.notify(
                NoticeKind::Error,
                "Could not change account",
                &format!("Resumed on {}. Open Change account to try again.", switch.from.name),
                Some(pane),
            );
        } else {
            self.limit_alerts.remove(&pane);
            self.limit_events.remove(&pane);
            self.screen_limits.remove(&pane);
            let mut original = switch.original;
            original.account_undo = None;
            if let Some(p) = self.panes.get_mut(&pane) {
                p.spec.account_undo = (!switch.undo).then(|| Box::new(original));
            }
            let fields = serde_json::json!({
                "agent": switch.to.agent,
                "account": switch.to.name,
                "account_id": switch.to.id,
                "from": switch.from.name,
                "from_id": switch.from.id,
                "undo": switch.undo,
            });
            self.emit_pane("account.changed", pane, fields);
            let message = format!("Using {}", switch.to.name);
            if switch.undo {
                self.notify(NoticeKind::Notice, &message, "Account change undone", Some(pane));
            } else {
                self.notify_action(NoticeKind::Notice, &message, pane, attention::ToastAction::UndoAccount);
            }
        }
        self.refresh_switch_views(pane);
        self.mark_persist();
        self.mark_dirty();
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static RECOVERY_LOOKUP: std::cell::RefCell<Option<std::sync::mpsc::Receiver<()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(stage: SwitchStage) -> AccountSwitch {
        AccountSwitch {
            token: 7,
            from: Account::new("claude", "from", "/unused/from".into(), None, false),
            to: Account::new("claude", "to", "/unused/to".into(), None, false),
            session: None,
            process: None,
            client: None,
            ask: false,
            original: PaneSpec::default(),
            undo: false,
            observed: None,
            started: None,
            stopping: None,
            stage,
        }
    }

    #[test]
    fn cancellation_and_completion_belong_to_the_reserved_change() {
        let mut switches = AccountSwitches::default();
        for stage in [
            SwitchStage::Checking,
            SwitchStage::Verifying,
            SwitchStage::Transferring,
            SwitchStage::RecoveryChecking,
            SwitchStage::Waiting,
            SwitchStage::Confirm,
            SwitchStage::Stopping,
            SwitchStage::Starting,
            SwitchStage::Restoring,
            SwitchStage::Recovering,
            SwitchStage::RecoveryStopping,
        ] {
            switches.reserve(1, change(stage));
            assert!(!switches.cancel(1, 6));
            assert!(!switches.confirm(1, 6, 99));
            assert!(switches.checked(1, 6).is_none());
            assert_eq!(switches.checked(1, 7).is_some(), stage == SwitchStage::Checking);
            let cancellable = matches!(
                stage,
                SwitchStage::Checking | SwitchStage::Verifying | SwitchStage::Waiting | SwitchStage::Confirm
            );
            assert_eq!(switches.cancel(1, 7), cancellable);
            assert_eq!(switches.contains(1), !cancellable);
            assert_eq!(switches.finish(1).is_some(), !cancellable);
            assert!(switches.checked(1, 7).is_none());
        }
        switches.reserve(1, change(SwitchStage::Confirm));
        assert!(switches.confirm(1, 7, 99));
        assert_eq!(switches.get(&1).unwrap().client, Some(99));
        assert_eq!(switches.check(1).unwrap().0, 7);
        switches.retain_panes(|_| false);
        assert!(!switches.contains(1));
        assert!(switches.checked(1, 7).is_none());
    }
}
