//! Agents from the command line: accounts, launching, worktrees and the hooks integration.

use anyhow::{Context, Result, bail};

use super::Session;
use super::args::{AccountsCmd, WtCmd};
use crate::protocol::{PaneInfo, Placement, Request, Response};
use crate::{account_status, accounts, codex_usage, git, integration, paths, worktree_setup};

pub(super) fn accounts(action: AccountsCmd) -> Result<()> {
    let mut all = accounts::Accounts::load()?;
    let discover = matches!(&action, AccountsCmd::List { .. } | AccountsCmd::Review { .. } | AccountsCmd::Refresh)
        || matches!(&action, AccountsCmd::Rename { account, .. }
            if find_account(&all, account).is_ok_and(|a| a.name == "default"));
    let discovered = if discover {
        let discovered = account_status::discover(&all)?;
        all = accounts::Accounts::load()?;
        discovered
    } else {
        Vec::new()
    };
    match action {
        AccountsCmd::List { json } => {
            let list = all.all();
            let now = accounts::now();
            let usage = accounts::UsageMap::load(&all);
            // Ask every agent in parallel whether its account is logged in.
            let status: Vec<account_status::Status> = std::thread::scope(|s| {
                let handles: Vec<_> = list.iter().map(|a| s.spawn(move || account_status::check(a))).collect();
                handles.into_iter().map(|h| h.join().unwrap_or(account_status::Status::CouldNotCheck)).collect()
            });
            if json {
                let mut v: Vec<serde_json::Value> = list
                    .iter()
                    .zip(&status)
                    .map(|(a, st)| {
                        serde_json::json!({
                            "agent": a.agent, "name": a.name, "home": a.home, "logged_in": st.logged_in(), "status": st,
                            "threshold": all.threshold(a), "usage": usage.get(a),
                            "joined_homes": joined_homes(&all, a),
                        })
                    })
                    .collect();
                for a in all.reviews() {
                    v.push(serde_json::json!({ "agent": a.agent, "name": accounts::review_question(&a),
                        "home": a.home, "logged_in": null, "status": "review_required", "usage": null }));
                }
                for (a, status) in &discovered {
                    if *status == account_status::Status::CouldNotCheck && all.unlisted().iter().any(|b| b.id == a.id) {
                        v.push(serde_json::json!({ "agent": a.agent, "name": accounts::existing_label(&a.agent),
                            "home": a.home, "logged_in": null, "status": status, "usage": null }));
                    }
                }
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            for (a, st) in list.iter().zip(&status) {
                let login = st.label();
                let u =
                    usage.get(a).map(|u| accounts::usage_text(u, now)).unwrap_or_else(|| "no usage seen yet".into());
                println!(
                    "{:<18} {:<14} {:<34} {u}",
                    accounts::label(a),
                    login,
                    paths::shorten_home(&paths::expand_tilde(&a.home))
                );
                for home in joined_homes(&all, a) {
                    println!("  {} (joined sign-in)", paths::shorten_home(&paths::expand_tilde(&home)));
                }
            }
            for a in all.reviews() {
                println!("{} · `kraal accounts review {}`", accounts::review_question(&a), a.agent);
            }
            for (a, status) in &discovered {
                if *status == account_status::Status::CouldNotCheck && all.unlisted().iter().any(|b| b.id == a.id) {
                    println!(
                        "{} · Could not verify. Run `kraal accounts list` to check again.",
                        accounts::existing_label(&a.agent)
                    );
                }
            }
            println!("threshold {}% · when reached: {:?}", all.limits.threshold, all.limits.mode);
            Ok(())
        }
        AccountsCmd::Add { agent, name, home, threshold, no_login } => {
            let existing = if home.is_none() && all.for_agent(&agent).is_empty() {
                match all.unlisted().into_iter().find(|a| a.agent == agent) {
                    Some(a) => match account_status::check(&a) {
                        account_status::Status::SignedIn => Some(a),
                        account_status::Status::CouldNotCheck => {
                            bail!("could not verify the existing sign-in; run `kraal accounts list` to check again")
                        }
                        _ => None,
                    },
                    None => None,
                }
            } else {
                None
            };
            let home = home
                .or_else(|| existing.as_ref().map(|a| a.home.clone()))
                .unwrap_or_else(|| accounts::default_home_for(&agent, &name).display().to_string());
            let a = accounts::Account::new(&agent, &name, accounts::absolute_home(&home)?, threshold, !no_login);
            // Adding a home already linked by Review uses its chosen account and home.
            let effective = all.clone().add(a.clone())?;
            let checked = if existing.as_ref().is_some_and(|a| a.id == effective.id && a.same_home(&effective)) {
                account_status::Status::SignedIn
            } else if paths::expand_tilde(&effective.home).is_dir()
                || all.by_home(&effective.agent, &paths::expand_tilde(&effective.home)).is_some()
            {
                account_status::check(&effective)
            } else {
                account_status::Status::NotSignedIn
            };
            anyhow::ensure!(
                no_login || checked != account_status::Status::CouldNotCheck,
                "could not verify this home's sign-in; run the add command again to check it"
            );
            let verified = checked == account_status::Status::SignedIn;
            let requested_name = a.name.clone();
            let (a, notes) = accounts::Accounts::update(|all| {
                let mut a = a;
                if let Some(existing) = &existing {
                    anyhow::ensure!(
                        all.for_agent(&a.agent).is_empty()
                            && all
                                .unlisted()
                                .iter()
                                .any(|current| current.id == existing.id && current.same_home(existing)),
                        "the account list changed; run `kraal accounts list` before adding an account"
                    );
                }
                a.sign_in_required = !no_login && !verified;
                all.add_prepared(a, Some((&effective, verified)))
            })?;
            for note in notes {
                println!("  {note}");
            }
            let action = if a.name == requested_name { "added" } else { "using existing account" };
            println!("{action} {} ({})", accounts::label(&a), paths::shorten_home(&paths::expand_tilde(&a.home)));
            if verified {
                // A forgotten account's directory is kept: adding its name again finds its sign-in.
                println!(
                    "  reusing the sign-in already in {}; `kraal accounts login {}` signs in again",
                    paths::shorten_home(&paths::expand_tilde(&a.home)),
                    accounts::key(&a.agent, &a.name)
                );
            }
            if no_login || verified {
                return Ok(());
            }
            println!("logging in with {}'s own login…", a.agent);
            login(&a)
        }
        AccountsCmd::Review { agent } => {
            if discovered
                .iter()
                .any(|(a, _)| a.agent == agent && all.by_id(&a.id).is_some_and(|b| !b.review && !b.same_home(a)))
            {
                Ok(())
            } else {
                review_account(&all, &agent)
            }
        }
        AccountsCmd::Login { account } => {
            let a = find_account(&all, &account)?;
            login(&a)
        }
        AccountsCmd::Rm { account } => {
            let a = find_account(&all, &account)?;
            accounts::Accounts::update(|all| {
                if !all.remove(&a.agent, &a.key()) {
                    bail!("no saved account to forget; run `kraal accounts list` first");
                }
                Ok(())
            })?;
            println!("forgot {} (its directory {} is kept)", accounts::label(&a), a.home);
            Ok(())
        }
        AccountsCmd::Rename { account, name } => {
            let a = find_account(&all, &account)?;
            accounts::rename(&a.agent, &a.key(), &name)?;
            println!("renamed {} to {}·{name}", accounts::label(&a), a.agent);
            Ok(())
        }
        AccountsCmd::Refresh => {
            for a in all.for_agent("codex") {
                let home = paths::expand_tilde(&a.home);
                match codex_usage::fetch("codex", &home, std::time::Duration::from_secs(20)) {
                    Ok(l) => {
                        let u = accounts::Usage::from(l);
                        accounts::save_usage(&a, &u)?;
                        println!("{:<18} {}", accounts::label(&a), accounts::usage_text(&u, accounts::now()));
                    }
                    Err(e) => println!("{:<18} {e:#}", accounts::label(&a)),
                }
            }
            println!("Claude accounts report their usage through the status line while they run.");
            Ok(())
        }
        AccountsCmd::Limits { threshold, mode } => {
            if let Some(t) = threshold
                && !(1..=100).contains(&t)
            {
                bail!("the threshold is a percentage (1-100)");
            }
            let limits = accounts::Accounts::update(|all| {
                if let Some(t) = threshold {
                    all.limits.threshold = t;
                }
                if let Some(m) = &mode {
                    all.limits.mode = match m.as_str() {
                        "ask" => accounts::Mode::Ask,
                        "auto" => accounts::Mode::Auto,
                        _ => accounts::Mode::Notify,
                    };
                }
                Ok(all.limits.clone())
            })?;
            println!("threshold {}% · when reached: {:?}", limits.threshold, limits.mode);
            Ok(())
        }
        AccountsCmd::Edit => {
            if !accounts::Accounts::path().exists() {
                all.save()?;
            }
            let editor = crate::edit::editor_argv(&accounts::Accounts::path(), None);
            std::process::Command::new(&editor[0]).args(&editor[1..]).status()?;
            accounts::Accounts::load().map(|_| ())
        }
    }
}

fn joined_homes(all: &accounts::Accounts, account: &accounts::Account) -> Vec<String> {
    let mut homes = Vec::new();
    for a in all.entries() {
        if a.linked_to.is_some()
            && !a.same_home(account)
            && all.by_id(&a.id).is_some_and(|target| target.id == account.id)
            && !homes.contains(&a.home)
        {
            homes.push(a.home.clone());
        }
    }
    homes
}

fn review_account(all: &accounts::Accounts, agent: &str) -> Result<()> {
    use std::io::{BufRead, IsTerminal, Write};
    let existing = all.reviews().into_iter().find(|a| a.agent == agent).context("no existing account needs review")?;
    anyhow::ensure!(
        std::io::stdin().is_terminal(),
        "run `kraal accounts review {agent}` in a terminal to choose an account"
    );
    let choices = all.for_agent(agent);
    println!("{}\nWhich account is this?", accounts::existing_label(agent));
    if let Some(identity) = &existing.public_identity {
        println!("{identity}");
    }
    for (i, a) in choices.iter().enumerate() {
        println!("{}. {}", i + 1, a.name);
    }
    println!("{}. A different account", choices.len() + 1);
    eprint!("Choose a number (Enter cancels): ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    if line.trim().is_empty() {
        return Ok(());
    }
    let n = line
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|n| *n > 0 && *n <= choices.len() + 1)
        .context("no account chosen; run the review again and choose a listed number")?;
    let target = choices.get(n - 1);
    let name = target.map_or(existing.name.as_str(), |a| &a.name);
    println!("Use {name}?\nYour running agents will keep working.\nThis account will appear once.");
    eprint!("Use account? [y/N] ");
    std::io::stderr().flush()?;
    line.clear();
    std::io::stdin().lock().read_line(&mut line)?;
    if matches!(line.trim(), "y" | "Y" | "yes") {
        accounts::Accounts::update(|all| all.review(&existing, target))?;
        println!("Account updated");
    }
    Ok(())
}

/// `agent:name` or a bare account name (searched across agents).
fn find_account(all: &accounts::Accounts, spec: &str) -> Result<accounts::Account> {
    if let Some((agent, name)) = spec.split_once(':').or_else(|| spec.split_once('·')) {
        return all.get(agent, name).with_context(|| accounts::missing_account(agent, name));
    }
    let matches: Vec<accounts::Account> = accounts::agents().filter_map(|kind| all.get(kind.name, spec)).collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => bail!("no account named {spec:?} (see `kraal accounts`)"),
        _ => bail!("several agents have an account named {spec:?}; use agent:name"),
    }
}

/// The agent's own login for account `a`, in this terminal; a failed login is an error.
fn login(a: &accounts::Account) -> Result<()> {
    let _lock = account_status::begin_login(a)?;
    let status = run_with(&accounts::login(a)?, false)?;
    if !status.success() {
        bail!("{}'s login did not succeed ({status})", accounts::label(a));
    }
    let checked = account_status::check(a);
    if checked != account_status::Status::SignedIn {
        bail!("{}: {}", accounts::label(a), checked.label());
    }
    account_status::finish_login(a)?;
    println!("{} is signed in", accounts::label(a));
    Ok(())
}

/// Run a command with an account's environment in this terminal.
fn run_with(l: &accounts::Launch, exec: bool) -> Result<std::process::ExitStatus> {
    let mut cmd = l.command();
    if exec {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        return Err(err).with_context(|| format!("running {}", l.argv[0]));
    }
    cmd.status().with_context(|| format!("running {}", l.argv[0]))
}

/// Claude's arguments with Remote Control on (`kraal agent claude --remote`): before `--`, or
/// last, where no word after it is taken for the session's name.
pub(super) fn remote_args(agent: &str, mut args: Vec<String>) -> Result<Vec<String>> {
    anyhow::ensure!(agent == "claude", "--remote is Claude Code's Remote Control: kraal agent claude --remote");
    let options = &args[..args.iter().position(|a| a == "--").unwrap_or(args.len())];
    if !options.iter().any(|a| matches!(a.split('=').next(), Some("--rc" | "--remote-control"))) {
        args.insert(options.len(), "--remote-control".into());
    }
    Ok(args)
}

/// `kraal agent`: a new agent pane in the session, or the agent here.
pub(super) fn agent(
    s: &Session,
    agent: String,
    account: Option<String>,
    placement: Placement,
    here: bool,
    teams: bool,
    mut args: Vec<String>,
) -> Result<()> {
    let inside = std::env::var_os("KRAAL_SOCKET").is_some();
    if teams {
        anyhow::ensure!(agent == "claude", "--teams is for Claude Code's agent teams: kraal agent claude --teams");
        anyhow::ensure!(inside, "--teams works inside kraal: Claude's teammates open as its panes");
        args = crate::tmux::lead_args(args);
    }
    if inside && !here {
        let opened = s.request(Request::OpenAgent {
            account_id: None,
            agent,
            account,
            placement,
            near: s.own_pane(),
            cwd: std::env::current_dir().ok(),
            args,
            teams,
        })?;
        super::print_opened(&opened);
        return Ok(());
    }
    // Here, in this pane: its workspace's account rules still apply.
    let workspace = if inside { s.own_workspace() } else { None };
    exec_agent(&agent, account.as_deref(), workspace.as_deref(), &args, &[])
}

/// Start an agent on the chosen account in this terminal, with `env` besides the account's.
fn exec_agent(
    agent: &str,
    account: Option<&str>,
    workspace: Option<&str>,
    args: &[String],
    env: &[(String, String)],
) -> Result<()> {
    let registered = crate::agent_registry::find(agent).with_context(|| format!("unknown agent {agent:?}"))?;
    if registered.accounts.is_none() {
        let mut launch = registered.native_launch(account, args, Some(&std::env::current_dir()?))?;
        launch.env.extend(env.iter().cloned());
        return run_with(&launch, true).map(|_| ());
    }
    let all = accounts::Accounts::load()?;
    accounts::agent_kind(agent)
        .with_context(|| format!("unknown agent {agent:?} (known: {})", accounts::agent_names()))?;
    let cwd = std::env::current_dir().ok();
    // Account rules see a worktree as its repository; never a pick among accounts a rule may
    // forbid here because git could not tell which repository that is.
    if let Some(c) = &cwd {
        git::project_path(c);
    }
    if account.is_none() && all.undecided(agent, cwd.as_deref()) {
        bail!("git did not say in time which repository this directory belongs to; try again");
    }
    let usage = accounts::UsageMap::load(&all);
    let a = match account {
        Some(name) => all.get(agent, name).with_context(|| accounts::missing_account(agent, name))?,
        None => all
            .choose(agent, workspace, cwd.as_deref(), &usage, accounts::now())
            .context("no account allowed here: the matching rule in accounts.toml names none")?,
    };
    let session = accounts::agent_kind(agent).and_then(|k| k.session_flag).map(|_| crate::identity::new_uuid());
    let mut launch = accounts::launch(&a, args, None, session.as_deref())?;
    launch.env.extend(env.iter().cloned());
    run_with(&launch, true).map(|_| ())
}

/// Panes that protect worktrees from removal. Only an absent server means there are none.
fn worktree_panes(s: &Session) -> Result<Vec<PaneInfo>> {
    let socket = s.socket();
    if socket.starts_with(paths::runtime_dir()) {
        paths::private_runtime_dir().context("checking the runtime directory")?;
    }
    let stream = match std::os::unix::net::UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {
            return Ok(Vec::new());
        }
        Err(e) => return Err(e).context("checking panes before removing worktrees"),
    };
    match crate::client::request_connected(&socket, stream, Request::ListPanes)? {
        Response::Panes(panes) => Ok(panes),
        _ => bail!("unexpected answer when checking panes before removing worktrees"),
    }
}

/// `kraal wt`: git worktrees for agents.
pub(super) fn worktrees(s: &Session, action: WtCmd) -> Result<()> {
    let cwd = std::env::current_dir()?;
    match action {
        WtCmd::New { name, agent, base, yes, no_setup } => {
            use std::io::{BufRead, IsTerminal, Write};
            if std::env::var_os("KRAAL_SOCKET").is_some() && agent.is_some() {
                crate::client::require_protocol(&s.socket())?;
            }
            let cfg = crate::config::Config::load()?;
            let user = worktree_setup::Setup::of_user(&cfg.worktrees);
            let plan = worktree_setup::Plan::new(&cwd, &name, base.as_deref(), user, !no_setup)?;
            // The repository's file is acted on once this version of it is approved: an act of
            // its own, which `--yes` (ask nothing) is not. Where someone can answer, it is shown
            // and the question asked; otherwise nothing is made, and the way to approve is said.
            if plan.needs_trust() {
                anyhow::ensure!(
                    !yes && std::io::stdin().is_terminal(),
                    "{}; --no-setup makes the worktree without it",
                    plan.from.not_approved(base.as_deref())
                );
                println!("{}", plan.trust_question());
                eprint!("Trust this version and prepare the worktree with it? [y/N] ");
                std::io::stderr().flush()?;
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line)?;
                if !matches!(line.trim(), "y" | "Y" | "yes") {
                    bail!("cancelled; nothing was created");
                }
                plan.trust()?;
            }
            let prepared = plan.create()?;
            let w = &prepared.worktree;
            println!("{} on {} (from {})", w.path.display(), w.branch, w.base);
            let env = worktree_setup::env(&w.name(), w.index, &cfg.worktrees);
            prepared.run_here(&env)?;
            if let Some(agent) = agent {
                if std::env::var_os("KRAAL_SOCKET").is_some() {
                    let opened = s.request(Request::OpenAgent {
                        account_id: None,
                        agent,
                        account: None,
                        placement: Placement::Tab,
                        near: s.own_pane(),
                        cwd: Some(w.path.clone()),
                        args: Vec::new(),
                        teams: false,
                    })?;
                    super::print_opened(&opened);
                } else {
                    std::env::set_current_dir(&w.path)?;
                    return exec_agent(&agent, None, None, &[], &env);
                }
            }
            Ok(())
        }
        WtCmd::Setup { base } => {
            let file = worktree_setup::RepoFile::read(&cwd, base.as_deref())?;
            match file.shown() {
                Some(text) => print!("{}\n\n{text}", file.place()),
                None => println!("no {}: a new worktree is prepared as [worktrees] in your config says", file.place()),
            }
            Ok(())
        }
        WtCmd::Trust { base } => {
            let file = worktree_setup::RepoFile::read(&cwd, base.as_deref())?;
            if file.trust()? {
                println!(
                    "the worktree setup, from the repository {}, is approved as it is now: kraal asks again when it \
                     changes",
                    paths::shorten_home(&file.repo)
                );
            } else {
                println!("no {}: nothing to approve", file.place());
            }
            Ok(())
        }
        WtCmd::List => {
            for w in git::list_worktrees(&cwd)? {
                let state = git::status(&w.path).map(|g| g.label(false)).unwrap_or_default();
                let number = if w.index > 0 { format!("#{}", w.index) } else { String::new() };
                println!("{:<24} {number:<4} {:<32} {}", w.name(), state, paths::shorten_home(&w.path));
            }
            Ok(())
        }
        WtCmd::Prune { dry_run, discard_ignored } => {
            // Panes of the running session that work in a worktree keep it: close them first.
            let panes = worktree_panes(s)?;
            let discard = if discard_ignored { git::Discard::Ignored } else { git::Discard::Nothing };
            let mut failed = 0;
            for (w, state) in git::worktree_states(&cwd)? {
                let inside: Vec<String> = panes
                    .iter()
                    .filter(|p| p.cwd.as_ref().is_some_and(|c| git::is_within(c, &w.path)))
                    .map(|p| p.name.clone().unwrap_or_else(|| p.id.to_string()))
                    .collect();
                let kept = match &state {
                    Err(e) => Some(e.clone()),
                    Ok(s) if !s.is_clean() => Some(format!("it has {}", s.describe())),
                    Ok(s) if !s.ignored.is_empty() && !discard_ignored => Some(format!(
                        "it has ignored files not in the main checkout ({}); --discard-ignored removes them too",
                        s.ignored_names()
                    )),
                    Ok(_) if !inside.is_empty() => Some(format!("panes work in it ({})", inside.join(", "))),
                    Ok(_) => None,
                };
                let how = match &state {
                    Ok(s) if s.unmerged > 0 => " (its changes are in its base)",
                    _ => "",
                };
                if let Some(why) = kept {
                    println!("kept {}: {why}", w.name());
                } else if dry_run {
                    println!("would remove {} and {}{how}", w.path.display(), w.branch);
                } else {
                    // Checked again as it is removed: nothing with changes or unmerged commits goes.
                    match git::remove_worktree(&w, discard) {
                        Ok(()) => println!("removed {} and {}", w.path.display(), w.branch),
                        Err(e) => {
                            eprintln!("kept {}: {e:#}", w.name());
                            failed += 1;
                        }
                    }
                }
            }
            anyhow::ensure!(
                failed == 0,
                "{failed} worktree{} could not be removed",
                if failed == 1 { "" } else { "s" }
            );
            Ok(())
        }
        WtCmd::Rm { name, force, discard_ignored } => {
            let w = git::list_worktrees(&cwd)?
                .into_iter()
                .find(|w| w.name() == name)
                .with_context(|| format!("no kraal worktree named {name:?} here"))?;
            // Panes of the running session that work inside it go first, or stop us.
            let panes = worktree_panes(s)?;
            if !panes.is_empty() {
                let inside: Vec<&PaneInfo> =
                    panes.iter().filter(|p| p.cwd.as_ref().is_some_and(|c| git::is_within(c, &w.path))).collect();
                if !inside.is_empty() && !force {
                    let names: Vec<String> =
                        inside.iter().map(|p| p.name.clone().unwrap_or_else(|| p.id.to_string())).collect();
                    // --force also deletes whatever the worktree has: say what.
                    let lost = match git::worktree_state(&w) {
                        Ok(state) if state.loses_nothing() => String::new(),
                        Ok(state) => format!(" and deletes its {}", state.describe()),
                        Err(_) => " and deletes whatever it has".into(),
                    };
                    bail!(
                        "panes work in {}: {} (close them, or --force closes them{lost})",
                        w.path.display(),
                        names.join(", ")
                    );
                }
                for p in inside {
                    s.request(Request::ClosePane { pane: p.id.to_string() })?;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            let discard = match (force, discard_ignored) {
                (true, _) => git::Discard::Everything,
                (false, true) => git::Discard::Ignored,
                (false, false) => git::Discard::Nothing,
            };
            git::remove_worktree(&w, discard).map_err(|e| match git::worktree_state(&w) {
                Ok(state) if state.is_clean() && !state.ignored.is_empty() => {
                    anyhow::anyhow!("{e:#}; --discard-ignored removes it with them")
                }
                _ => e,
            })?;
            println!("removed {} and {}", w.path.display(), w.branch);
            Ok(())
        }
    }
}

/// `kraal integration`: install, remove or check kraal's hooks in agents' settings, and the
/// lines that make shells mark their commands. `all` never edits a shell's startup file, and
/// covers the agents used on this machine besides Claude Code and Codex.
pub(super) fn integration(action: &str, agent: &str) -> Result<()> {
    let agents: Vec<&str> = match (agent, action) {
        ("all", "status") => {
            integration::agents_here().into_iter().chain(integration::SHELLS.iter().copied()).collect()
        }
        ("all", "show") => bail!("show what: zsh, bash or fish"),
        ("all", _) => integration::agents_here(),
        _ => vec![agent],
    };
    let mut failed = Vec::new();
    for a in agents {
        let r = match action {
            "install" => integration::install(a),
            "uninstall" => integration::uninstall(a),
            "show" => integration::show(a),
            _ => integration::status(a),
        };
        match r {
            Ok(msg) => println!("{msg}"),
            Err(e) => {
                eprintln!("{a}: {e:#}");
                failed.push(a);
            }
        }
    }
    if !failed.is_empty() {
        bail!("{action} failed for {}", failed.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_control_goes_where_it_takes_no_prompt_for_a_name() {
        let w = |s: &str| s.split(' ').filter(|w| !w.is_empty()).map(String::from).collect::<Vec<_>>();
        assert_eq!(remote_args("claude", w("")).unwrap(), w("--remote-control"));
        assert_eq!(remote_args("claude", w("--model opus fix")).unwrap(), w("--model opus fix --remote-control"));
        assert_eq!(
            remote_args("claude", w("--model opus -- fix it")).unwrap(),
            w("--model opus --remote-control -- fix it")
        );
        assert_eq!(remote_args("claude", w("--rc")).unwrap(), w("--rc"));
        assert_eq!(remote_args("claude", w("--remote-control=Shop")).unwrap(), w("--remote-control=Shop"));
        assert_eq!(remote_args("claude", w("--rc=Phone")).unwrap(), w("--rc=Phone"));
        assert_eq!(remote_args("claude", w("-- --rc")).unwrap(), w("--remote-control -- --rc"));
        assert!(remote_args("codex", w("")).is_err());
    }

    #[test]
    fn worktree_removal_refuses_old_servers_and_allows_absent_servers() {
        if !crate::testing::in_own_process(
            "cli::agents::tests::worktree_removal_refuses_old_servers_and_allows_absent_servers",
        ) {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kraal-wt-removal-{}", std::process::id()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [vec!["init", "-q", "-b", "main"], vec!["commit", "-q", "--allow-empty", "-m", "initial"]] {
            let out = std::process::Command::new("git")
                .current_dir(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        }
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(&repo).unwrap();
        let socket = dir.join("server.sock");
        // The test runs alone, before starting its server thread.
        unsafe { std::env::set_var("KRAAL_SOCKET", &socket) };
        let session = Session { name: "old".into(), explicit: None };
        for prune in [true, false] {
            let w = git::add_worktree(&repo, "fix", None).unwrap();
            let remove = || {
                worktrees(
                    &session,
                    if prune {
                        WtCmd::Prune { dry_run: false, discard_ignored: false }
                    } else {
                        WtCmd::Rm { name: "fix".into(), force: false, discard_ignored: false }
                    },
                )
            };
            for version in ["0.2.0", "0.3.0"] {
                let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
                let server = std::thread::spawn(move || {
                    for expected in ["list_panes", "ping"] {
                        let (mut stream, _) = listener.accept().unwrap();
                        let (_, payload) = crate::protocol::read_frame(&mut stream).unwrap().unwrap();
                        let msg: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                        assert_eq!(msg["request"]["cmd"], expected);
                        let result = if expected == "ping" {
                            serde_json::json!({"Ok": {"kind": "pong", "data": {
                                "pid": 1, "session": "old", "version": version
                            }}})
                        } else {
                            serde_json::json!({"Err": "bad message: incompatible protocol"})
                        };
                        crate::protocol::write_json(
                            &mut stream,
                            &serde_json::json!({"type": "response", "result": result}),
                        )
                        .unwrap();
                    }
                });
                let result = remove();
                server.join().unwrap();
                assert_eq!(
                    result.unwrap_err().to_string(),
                    crate::protocol::version_mismatch(crate::protocol::PROTOCOL_VERSION, 2, version, "old", None)
                );
                assert!(w.path.is_dir(), "a running pane's worktree must survive");
                assert_eq!(git::list_worktrees(&repo).unwrap().len(), 1);
                std::fs::remove_file(&socket).unwrap();
            }
            // Both a missing socket and one left by a stopped server allow cleanup.
            if !prune {
                drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
            }
            remove().unwrap();
            assert!(!w.path.exists());
            assert!(git::list_worktrees(&repo).unwrap().is_empty());
        }
        std::env::set_current_dir(previous).unwrap();
        unsafe { std::env::remove_var("KRAAL_SOCKET") };
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn worktrees_refuse_mismatched_servers_before_preparation() {
        if !crate::testing::in_own_process("cli::agents::tests::worktrees_refuse_mismatched_servers_before_preparation")
        {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kraal-native-compat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        for version in ["0.2.0", "0.3.0"] {
            for agent in ["claude", "gemini"] {
                let socket = dir.join(format!("{version}-{agent}.sock"));
                let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
                // This test runs alone in its own process, before starting the server thread.
                unsafe { std::env::set_var("KRAAL_SOCKET", &socket) };
                let server = std::thread::spawn(move || {
                    let (mut stream, _) = listener.accept().unwrap();
                    let (_, payload) = crate::protocol::read_frame(&mut stream).unwrap().unwrap();
                    let msg: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                    let cmd = msg["request"]["cmd"].as_str().unwrap();
                    let result = if cmd == "ping" {
                        serde_json::json!({"Ok": {"kind": "pong", "data": {
                            "pid": 1, "session": "old", "version": version
                        }}})
                    } else {
                        serde_json::json!({"Err": "unknown agent"})
                    };
                    crate::protocol::write_json(
                        &mut stream,
                        &serde_json::json!({
                            "type": "response", "result": result
                        }),
                    )
                    .unwrap();
                    cmd.to_string()
                });
                let session = Session { name: "old".into(), explicit: None };
                let result = worktrees(
                    &session,
                    WtCmd::New {
                        name: "fix".into(),
                        agent: Some(agent.into()),
                        base: None,
                        yes: true,
                        no_setup: false,
                    },
                );
                let error = result.unwrap_err().to_string();
                assert_eq!(
                    error,
                    crate::protocol::version_mismatch(crate::protocol::PROTOCOL_VERSION, 2, version, "old", None)
                );
                assert_eq!(server.join().unwrap(), "ping", "no launch request reaches an older server");
                std::fs::remove_file(&socket).unwrap();
                assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "no worktree preparation or creation");
            }
        }
        std::env::set_current_dir(previous).unwrap();
        unsafe { std::env::remove_var("KRAAL_SOCKET") };
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn run_with_preserves_arguments_and_the_accounts_environment() {
        let launch = accounts::Launch {
            argv: vec![
                "sh".into(),
                "-c".into(),
                r#"test "$#" = 3 && test "$1" = 'two words' && test -z "$2" && test "$3" = '$HOME;*' &&
                   test "$KRAAL_LOGIN_HOME" = 'account home' && test "${HOME+x}" != x && test -n "$PATH""#
                    .into(),
                "sh".into(),
                "two words".into(),
                "".into(),
                "$HOME;*".into(),
            ],
            env: vec![("KRAAL_LOGIN_HOME".into(), "first".into()), ("KRAAL_LOGIN_HOME".into(), "account home".into())],
            env_remove: vec!["HOME".into(), "KRAAL_LOGIN_HOME".into()],
        };
        assert!(run_with(&launch, false).unwrap().success());
    }
}
