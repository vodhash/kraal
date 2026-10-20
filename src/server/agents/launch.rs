use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use super::super::Server;
use crate::accounts::{self, Account};
use crate::layout::PaneId;
use crate::pane::PaneSpec;
use crate::protocol::Placement;

impl Server {
    pub(super) fn spec_account(&self, spec: &PaneSpec) -> Option<Account> {
        self.accounts.pane_account(spec)
    }

    /// Spec of a pane running `agent` on account `a`, resuming `resume` if given.
    pub fn agent_spec(
        &self,
        a: &Account,
        cwd: Option<PathBuf>,
        resume: Option<&str>,
        args: &[String],
    ) -> Result<PaneSpec> {
        agent_spec(a, cwd, resume, args)
    }

    /// Rebuild an agent pane's spec so that starting it again resumes its conversation, with
    /// the options it was started with (never a prompt given on its command line: it would be
    /// sent again). `session` is the conversation seen running; the id the pane was started
    /// with counts only once a conversation exists under it (a pane that never ran starts the
    /// conversation it was meant to). The flag says whether that may happen by itself (a
    /// restore): not for a one-shot run (`claude -p`), which stays as it was, nor when a prompt
    /// was left out or would be sent; those wait for Enter.
    pub fn resume_spec(&self, spec: &PaneSpec, session: Option<&str>) -> (PaneSpec, bool) {
        resume_spec(&self.accounts, spec, session)
    }

    /// The spec to start pane `p` with again, if it is an agent that had a conversation: the
    /// conversation is resumed rather than a new one started. Agents run as plain commands
    /// (`kraal run -- claude`) too, when their conversation is known. The flag is false when the
    /// command must be shown before it runs (see [`accounts::resumable_args`]).
    pub fn restart_spec(&self, p: PaneId) -> Option<(PaneSpec, bool)> {
        let pane = self.panes.get(&p)?;
        let session = pane.agent.session_id.as_deref();
        if pane.spec.account.is_some() {
            return Some(self.resume_spec(&pane.spec, session));
        }
        let program = pane.spec.command.first()?.rsplit('/').next()?;
        let kind = accounts::agents().find(|k| k.command == program)?;
        let (command, exact) = accounts::resume_argv(kind, &pane.spec.command, session?)?;
        Some((PaneSpec { command, ..pane.spec.clone() }, exact))
    }

    /// Whether pane `p`'s agent can be started again on its own, without showing its command
    /// first: false when kraal could not tell its options from a prompt it would send again.
    pub(in crate::server) fn restarts_by_itself(&self, p: PaneId) -> bool {
        let Some(pane) = self.panes.get(&p) else { return false };
        let Some(kind) =
            pane.spec.account.as_deref().and_then(|k| k.split_once(':')).and_then(|(a, _)| accounts::agent_kind(a))
        else {
            return false;
        };
        accounts::resumable_args(kind, &accounts::launch_args(kind, &pane.spec.command)).is_some_and(|(_, exact)| exact)
    }

    /// Start `agent` in a new pane, on `account` or the one the rules and thresholds pick.
    pub fn launch_agent(
        &mut self,
        ws: u32,
        target: Option<PaneId>,
        agent: &str,
        account: Option<&str>,
        placement: Placement,
        cwd: Option<PathBuf>,
    ) -> Result<PaneId> {
        self.launch_agent_with(ws, target, agent, account, placement, cwd, &[], false)
    }

    /// [`Server::launch_agent`], giving the agent `args` too.
    #[expect(clippy::too_many_arguments, reason = "the arguments of `launch_agent`, and the agent's own and teams")]
    pub fn launch_agent_with(
        &mut self,
        ws: u32,
        target: Option<PaneId>,
        agent: &str,
        account: Option<&str>,
        placement: Placement,
        cwd: Option<PathBuf>,
        args: &[String],
        teams: bool,
    ) -> Result<PaneId> {
        let registered = crate::agent_registry::find(agent).with_context(|| format!("unknown agent {agent:?}"))?;
        let teams = if teams { crate::tmux::lead_env(&self.env.socket) } else { Vec::new() };
        if registered.accounts.is_none() {
            let cwd = self.agent_cwd(ws, target, cwd);
            let launch = registered.native_launch(account, args, cwd.as_deref())?;
            let mut spec = PaneSpec { command: launch.argv, env: teams, cwd, ..Default::default() };
            spec.agent_clipboard();
            return self.spawn_pane_in(ws, target, spec, placement, Some(agent.into()));
        }
        let wi = self.ws_index(ws).context("no such workspace")?;
        let ws_name = self.workspaces[wi].name.clone();
        let cwd = self.agent_cwd(ws, target, cwd);
        let a = match account {
            Some(name) => self.accounts.get(agent, name).with_context(|| accounts::missing_account(agent, name))?,
            None => {
                // Never a pick among accounts a rule may forbid here: git says first which
                // repository this directory belongs to (callers that can wait do so).
                if let Some(dir) = cwd.clone().filter(|d| self.accounts.undecided(agent, Some(d))) {
                    self.ask_git_about(dir);
                    bail!(
                        "which account the rules allow here depends on this directory's repository; try again in a moment"
                    );
                }
                self.accounts
                    .choose(agent, Some(&ws_name), cwd.as_deref(), &self.usage, accounts::now())
                    .context("no account allowed here: the matching rule in accounts.toml names none")?
            }
        };
        let mut spec = self.agent_spec(&a, cwd, None, args)?;
        spec.env.extend(teams);
        self.spawn_pane_in(ws, target, spec, placement, Some(a.agent.clone()))
    }

    /// Where an agent started in workspace `ws` next to `target` works: `cwd` if given, else the
    /// directory of that pane (or the focused one), when it is on this machine.
    pub(in crate::server) fn agent_cwd(
        &self,
        ws: u32,
        target: Option<PaneId>,
        cwd: Option<PathBuf>,
    ) -> Option<PathBuf> {
        cwd.or_else(|| {
            let wi = self.ws_index(ws)?;
            let w = &self.workspaces[wi];
            let focus = target.or_else(|| w.tabs.get(w.active).and_then(|t| t.focus));
            focus.and_then(|p| self.panes.get(&p)).filter(|p| p.ssh.is_none()).and_then(|p| p.current_cwd())
        })
    }
}

fn agent_spec(a: &Account, cwd: Option<PathBuf>, resume: Option<&str>, args: &[String]) -> Result<PaneSpec> {
    let kind = accounts::agent_kind(&a.agent).context("unknown agent")?;
    let session = match resume {
        Some(id) => Some(id.to_string()),
        None => kind.session_flag.map(|_| crate::identity::new_uuid()),
    };
    let new_session = if resume.is_none() { session.as_deref() } else { None };
    let mut l = accounts::launch(a, args, resume, new_session)?;
    l.env_remove.extend(["DISPLAY".into(), "WAYLAND_DISPLAY".into()]);
    Ok(PaneSpec {
        command: l.argv,
        env: l.env,
        env_remove: l.env_remove,
        cwd,
        account: Some(accounts::key(&a.agent, &a.name)),
        account_id: Some(a.id.clone()),
        session,
        ..Default::default()
    })
}

pub(super) fn resume_spec(accounts: &accounts::Accounts, spec: &PaneSpec, session: Option<&str>) -> (PaneSpec, bool) {
    let mut bound = spec.clone();
    accounts.bind_pane(&mut bound);
    bound.prepare_agent_clipboard();
    let spec = &bound;
    let Some(a) = accounts.pane_account(spec) else {
        return (spec.clone(), false);
    };
    let Some(kind) = accounts::agent_kind(&a.agent) else {
        return (spec.clone(), false);
    };
    let Some((args, exact)) = accounts::resumable_args(kind, &accounts::launch_args(kind, &spec.command)) else {
        return (spec.clone(), false);
    };
    // When its files cannot be read, the conversation may exist: resume it, but only on Enter.
    let (reserved, readable) = match spec.session.as_deref().map(|id| (id, accounts::conversation_exists(&a, id))) {
        Some((id, Ok(seen))) => ((seen != Some(false)).then_some(id), true),
        Some((id, Err(_))) => (Some(id), false),
        None => (None, true),
    };
    let exact = exact && readable;
    let Some(session) = session.or(reserved) else { return (spec.clone(), exact) };
    match agent_spec(&a, spec.cwd.clone(), Some(session), &args) {
        Ok(fresh) => {
            let mut fresh = PaneSpec {
                worktree: spec.worktree.clone(),
                close_on_exit: spec.close_on_exit,
                account_undo: spec.account_undo.clone(),
                ..fresh
            };
            if let Some(program) = spec.command.first().filter(|p| std::path::Path::new(p).is_absolute()) {
                fresh.command[0] = program.clone();
            }
            fresh.env.extend(spec.env.iter().filter(|(k, _)| k == "PATH").cloned());
            crate::worktree_setup::carry_env(&spec.env, &mut fresh.env);
            crate::tmux::carry_env(&spec.env, &mut fresh.env);
            (fresh, exact)
        }
        Err(_) => (spec.clone(), false),
    }
}
