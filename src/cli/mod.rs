//! The `kraal` command line: parsing, then one function per command.

mod agents;
mod args;
mod buffers;
#[cfg(test)]
mod docs;
mod panes;
mod remote;
mod restart;
mod runbooks;
mod session;
mod update;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use self::args::{AccountsCmd, Cli, Cmd, ServersCmd};
use crate::protocol::{Placement, Request, Response};
use crate::{client, config, hints, integration, paths, server, servers, theme, tools, workspace};

pub fn main() {
    #[cfg(debug_assertions)]
    if let Some(theirs) = paths::of_the_developer() {
        eprintln!("kraal: run by a test, {theirs}");
        std::process::exit(1);
    }
    // `tmux`, as `kraal agent --teams` puts this binary in Claude's PATH.
    let mut argv = std::env::args_os();
    if argv.next().is_some_and(|a| std::path::Path::new(&a).file_name() == Some("tmux".as_ref())) {
        std::process::exit(crate::tmux::main(argv.collect()));
    }
    let cli = Cli::read();
    // Plain CLI commands should die quietly when piped into `head`; the server and the attached
    // client handle broken sockets themselves.
    if !matches!(cli.command, None | Some(Cmd::Attach { .. }) | Some(Cmd::Server) | Some(Cmd::LaptopCp)) {
        // SAFETY: setting a signal disposition before any thread starts.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    // Before anything reads the session: it runs inside kraal's own helpers.
    if let Some(Cmd::Supervise { command }) = &cli.command {
        std::process::exit(crate::exec::supervise(command));
    }
    if let Err(e) = run(cli) {
        eprintln!("kraal: {e:#}");
        std::process::exit(if e.is::<server::Held>() { server::HELD_STATUS } else { 1 });
    }
}

/// The session a command works on, and how to reach its server.
struct Session {
    name: String,
    /// `-s` as given: without it, a command run inside kraal talks to the server it runs in.
    explicit: Option<String>,
}

impl Session {
    fn socket(&self) -> PathBuf {
        client::control_socket(self.explicit.as_deref())
    }

    /// The pane this command runs in, when it addresses that pane's own session: a pane id
    /// means nothing in another session (it may name an unrelated pane there).
    fn own_pane(&self) -> Option<String> {
        let here = std::env::var("KRAAL_SESSION").ok();
        let same = match &self.explicit {
            None => true,
            Some(s) => here.as_deref() == Some(s.as_str()),
        };
        std::env::var("KRAAL_PANE").ok().filter(|_| same)
    }

    /// The workspace of the pane this command runs in ([`Session::own_pane`]): where a command
    /// that names no workspace acts, rather than the one a terminal happens to show.
    fn own_workspace(&self) -> Option<String> {
        let pane = self.own_pane()?;
        let Ok(Response::Panes(list)) = self.request(Request::ListPanes) else { return None };
        list.into_iter().find(|p| p.id.to_string() == pane).map(|p| p.workspace)
    }

    fn request(&self, r: Request) -> Result<Response> {
        client::request(&self.socket(), r)
    }

    /// Start the session's server, unless this runs inside kraal (which has one).
    fn ensure_server(&self) -> Result<()> {
        if std::env::var_os("KRAAL_SOCKET").is_none() {
            client::ensure_server(&self.name)?;
        }
        Ok(())
    }
}

fn run(cli: Cli) -> Result<()> {
    // `$KRAAL_SESSION_NAME` chooses a session from outside kraal; inside a pane, the pane's own
    // session comes first (a name exported in a shell's startup files is inherited by panes of
    // every session).
    let inside = std::env::var_os("KRAAL_SESSION").is_some();
    let explicit = cli
        .session
        .clone()
        .or_else(|| if inside { None } else { std::env::var("KRAAL_SESSION_NAME").ok().filter(|s| !s.is_empty()) });
    let s = Session { name: session_name(&explicit)?, explicit };
    let done = |r: Result<Response>| r.map(|_| ());
    match cli.command {
        None => session::attach(&s.name, cli.workspace),
        Some(Cmd::Attach { workspace, host: None, .. }) => session::attach(&s.name, workspace),
        Some(Cmd::Attach { workspace, host: Some(host), pane }) => remote::attach(&s.name, &host, workspace, pane),
        Some(Cmd::RemoteInstall { server }) => remote::install(&s.name, &server),
        Some(Cmd::Update { check, to, force, restart, no_restart }) => {
            let restart = (restart || no_restart).then_some(restart);
            update::update(check, to, force, restart)
        }
        Some(Cmd::Proxy) => client::proxy(&s.name),
        Some(Cmd::Server) => server::run(&s.name),
        Some(Cmd::Ls) => session::list_sessions(),
        Some(Cmd::Kill) => session::kill(&s),
        Some(Cmd::Restart { yes }) => restart::restart(&s, yes),
        Some(Cmd::Forget) => session::forget(&s),
        Some(Cmd::Workspaces { json, deleted, command }) => session::workspaces(&s, json, deleted, command),
        Some(Cmd::Open { name }) => session::open(&s, name),
        Some(Cmd::Import { file, name, yes }) => session::import(&s, file, name, yes),
        Some(Cmd::Export { file, workspace, replace }) => session::export(&s, file, workspace, replace),
        Some(Cmd::ImportZellij { from, force }) => session::import_zellij(&s, from, force),
        Some(Cmd::Panes { json }) => panes::list(&s, json, false),
        Some(Cmd::Agents { follow: true, .. }) => panes::follow_agents(&s),
        Some(Cmd::Agents { json, .. }) => panes::list(&s, json, true),
        Some(Cmd::Run(args)) => panes::run(&s, args),
        Some(Cmd::Preview { path, place }) => panes::preview(&s, path, place),
        Some(Cmd::Send { pane, enter, production, text }) => panes::send(&s, pane, enter, production, text),
        Some(Cmd::Keys { pane, production, keys }) => {
            done(s.request(Request::SendKeys { pane: pane_ref(&s, pane)?, keys, production }))
        }
        Some(Cmd::Read { pane, lines }) => panes::read(&s, pane, lines),
        Some(Cmd::Wait { pane, until, timeout }) => panes::wait(&s, pane, until, timeout),
        Some(Cmd::Focus { pane }) => done(s.request(Request::FocusPane { pane })),
        Some(Cmd::Close { pane }) => {
            let pane = pane_or_own(&s, pane, "name it after `kraal close`")?;
            done(s.request(Request::ClosePane { pane }))
        }
        Some(Cmd::Rename { pane, name }) => done(s.request(Request::RenamePane { pane: pane_ref(&s, pane)?, name })),
        Some(Cmd::Notify { title, body }) => done(s.request(Request::Notify { title, body: body.unwrap_or_default() })),
        Some(Cmd::Buffer { action }) => buffers::run(&s, action),
        Some(Cmd::Events { types, list, since, boot }) => panes::events(&s, types, list, since, boot),
        Some(Cmd::Runbook { action }) => runbooks::run(action, &s.socket(), s.own_pane()),
        Some(Cmd::LaptopCp) => crate::laptop::copy::run(&s.socket(), &s.name),
        Some(Cmd::Cp { paths, dry_run, move_, no_replace, laptop_spool, allow, expect }) => {
            remote::copy(&paths, dry_run, move_, no_replace.then_some(allow.as_slice()), &expect, laptop_spool)
        }
        Some(Cmd::Rm { paths, expect }) => remote::remove(&paths, &expect),
        Some(Cmd::Files { left, right }) => remote::files(&s, left, right),
        Some(Cmd::Edit { target, line, production, sudo }) => remote::edit(&s, target, line, production, sudo),
        Some(Cmd::Servers { action }) => {
            remote::servers(action.unwrap_or(ServersCmd::List { json: false, group: None }))
        }
        Some(Cmd::Ssh { target, group, right, down, floating, tab, command }) => {
            remote::ssh(&s, target, group, placement(right, down, floating, tab), command)
        }
        Some(Cmd::Exec { targets, command, parallel, timeout, json }) => {
            remote::exec_fleet(&targets, &command, parallel, timeout, json)
        }
        Some(Cmd::Tunnel { action }) => remote::tunnel(&s, action),
        Some(Cmd::Accounts { action }) => agents::accounts(action.unwrap_or(AccountsCmd::List { json: false })),
        Some(Cmd::Agent { agent, account, right, down, floating, tab, here, teams, remote, args }) => {
            let placement = placement(right, down, floating, tab).unwrap_or(Placement::Auto);
            let args = if remote { agents::remote_args(&agent, args)? } else { args };
            agents::agent(&s, agent, account, placement, here, teams, args)
        }
        Some(Cmd::Wt { action }) => agents::worktrees(&s, action),
        Some(Cmd::Statusline) => {
            integration::statusline();
            Ok(())
        }
        Some(Cmd::Hook { agent, payload }) => {
            integration::run_hook(&agent, payload);
            Ok(())
        }
        // Run by `main` before the session is read.
        Some(Cmd::Supervise { .. }) => Ok(()),
        Some(Cmd::Integration { action, agent }) => agents::integration(&action, &agent),
        Some(Cmd::Config { action, .. }) if action == "reload" => {
            // The server keeps its configuration when the file cannot be read: that is an error.
            if let Response::Text(warnings) = s.request(Request::ReloadConfig)? {
                for w in warnings.lines() {
                    eprintln!("warning: {w}");
                }
            }
            println!("config reloaded");
            Ok(())
        }
        Some(Cmd::Config { action, markdown }) => config_cmd(&action, markdown),
    }
}

/// The session named by `-s`, else the one this command runs in (inside a pane), else `main`.
/// A session name inherited from a pane that is not valid is an error: never another session.
fn session_name(cli: &Option<String>) -> Result<String> {
    // Never `main` in place of a session this command runs in but cannot name.
    let s = cli
        .clone()
        .or_else(|| std::env::var("KRAAL_SESSION").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "main".into());
    if !paths::valid_session_name(&s) {
        bail!("invalid session name {s:?}");
    }
    Ok(s)
}

/// The pane a command acts on: the one it was given, else the pane it runs in (in its session).
/// Without either, the message says `how` that command is given a pane.
fn pane_or_own(s: &Session, p: Option<String>, how: &str) -> Result<String> {
    p.or_else(|| s.own_pane()).with_context(|| format!("which pane? {how} (an id or a name; see `kraal panes`)"))
}

/// [`pane_or_own`] for the commands that take their pane with `--pane`.
fn pane_ref(s: &Session, p: Option<String>) -> Result<String> {
    pane_or_own(s, p, "pass --pane")
}

/// Where `--right`, `--down`, `--floating` or `--tab` put a new pane, if one was given.
fn placement(right: bool, down: bool, floating: bool, tab: bool) -> Option<Placement> {
    [(right, Placement::Right), (down, Placement::Down), (floating, Placement::Floating), (tab, Placement::Tab)]
        .into_iter()
        .find_map(|(on, p)| on.then_some(p))
}

/// Every known server, computed without a running server.
fn known_servers(cfg: &config::Config) -> Result<Vec<servers::Known>> {
    let inv = servers::Inventory::load()?;
    let defs: Vec<workspace::WorkspaceDef> =
        cfg.workspace_dirs().iter().flat_map(|d| workspace::load_dir(d)).filter_map(|(_, d)| d.ok()).collect();
    let hosts = if cfg.ssh_config_hosts { server::ssh_config_hosts() } else { Vec::new() };
    Ok(servers::known(&inv, &defs, &hosts))
}

/// Servers named by `targets`: names, user@host, or @group.
fn resolve_targets(known: &[servers::Known], targets: &[String]) -> Result<Vec<servers::Server>> {
    let mut out: Vec<servers::Server> = Vec::new();
    for t in targets {
        let found: Vec<servers::Server> = match t.strip_prefix('@') {
            Some(group) => {
                let members: Vec<servers::Server> = known
                    .iter()
                    .filter(|k| k.server.groups.iter().any(|g| g == group))
                    .map(|k| k.server.clone())
                    .collect();
                if members.is_empty() {
                    bail!("no server in group {group:?} (see `kraal servers`)");
                }
                members
            }
            None => vec![servers::resolve(known, t).with_context(|| format!("not a server: {t}"))?],
        };
        for s in found {
            if !out.iter().any(|o| o.same_connection(&s)) {
                out.push(s);
            }
        }
    }
    Ok(out)
}

/// Print the id of the pane a command opened, alone on its line: what a script keeps to drive
/// that pane (`kraal wait -p`, `kraal read -p`).
fn print_opened(answer: &Response) {
    if let Response::Pane(pane) = answer {
        println!("{}", pane.id);
    }
}

/// `kraal config keys`: every key, grouped and ordered as the help shows them, then the names
/// that `[keys.leader]` and `[keys.direct]` take, with what each does and its keys now.
fn print_keys(keymap: &crate::keymap::Keymap, markdown: bool) {
    use crate::keymap::{Action, Command};
    let cell = |s: &str| s.replace('|', "\\|");
    let mut names: Vec<(String, String, String)> = Action::all()
        .map(|a| {
            let name = match a {
                Action::Tab(_) => "tab_1 … tab_9".to_string(),
                _ => a.name(),
            };
            (name, a.describe(), keymap.all_hints(&Command::Action(a)).join(", "))
        })
        .collect();
    for m in &keymap.menus {
        let keys = m.keys.iter().map(|k| format!("{} {k}", keymap.prefix_label())).collect::<Vec<_>>().join(", ");
        names.push((format!("menu_{}", m.id), format!("The {} menu", m.title), keys));
    }
    let title = format!("kraal keys: preset {}, prefix {}", keymap.preset, keymap.prefix_label());
    if markdown {
        println!("# {title}\n");
        for (section, rows) in keymap.reference(false, true) {
            println!("## {section}\n\n| Keys | What it does |\n|---|---|");
            for (k, what) in rows {
                println!("| `{}` | {} |", cell(&k), cell(&what));
            }
            println!();
        }
        println!("## Names for `[keys.leader]` and `[keys.direct]`\n\n| Name | What it does | Keys |\n|---|---|---|");
        for (name, what, keys) in names {
            println!("| `{name}` | {} | {} |", cell(&what), cell(&keys));
        }
        return;
    }
    println!("{title}\n");
    for (section, rows) in keymap.reference(false, true) {
        println!("{section}");
        let w = rows.iter().map(|(k, _)| crate::text::columns(k)).max().unwrap_or(0).min(28);
        for (k, what) in rows {
            let pad = w.saturating_sub(crate::text::columns(&k));
            println!("  {k}{}  {what}", " ".repeat(pad));
        }
        println!();
    }
    println!("Names for [keys.leader] and [keys.direct]");
    for (name, what, keys) in names {
        println!("  {name:<22} {what:<40} {keys}");
    }
}

/// What `kraal config check` warns about in a configuration that could be read: its keys, tools,
/// hints, hooks, theme and worktree setup.
fn config_warnings(cfg: &config::Config) -> Vec<String> {
    let (mut keymap, mut warnings) = crate::keymap::Keymap::build(&cfg.keys);
    warnings.extend(keymap.bind_tools(&cfg.tools));
    warnings.extend(tools::check(&cfg.tools));
    warnings.extend(hints::Custom::build(&cfg.hints, &cfg.tools).1);
    warnings.extend(server::events::read_hooks(&cfg.hooks).1);
    warnings.extend(theme::Theme::from_config(&cfg.theme).1);
    if let Err(e) = crate::worktree_setup::Setup::of_user(&cfg.worktrees).check() {
        warnings.push(format!("[worktrees] {e:#}"));
    }
    warnings
}

/// `kraal config`: print the defaults, the path or the keys, or check the configuration.
fn config_cmd(action: &str, markdown: bool) -> Result<()> {
    match action {
        "default" => print!("{}", config::DEFAULT_CONFIG),
        "path" => println!("{}", paths::config_path().display()),
        "keys" => {
            let cfg = config::Config::load()?;
            let (mut keymap, _) = crate::keymap::Keymap::build(&cfg.keys);
            let _ = keymap.bind_tools(&cfg.tools);
            print_keys(&keymap, markdown);
        }
        _ => {
            let (cfg, unknown_keys) = config::Config::load_with_warnings()?;
            let warnings = unknown_keys.into_iter().chain(config_warnings(&cfg));
            for w in warnings.chain(paths::runtime_dir_refused()) {
                eprintln!("warning: {w}");
            }
            // Warnings are advice; a file kraal cannot read at all is an error.
            let mut errors = 0;
            let commands = <Cli as clap::CommandFactory>::command();
            for (path, def) in cfg.workspace_dirs().iter().flat_map(|d| workspace::load_dir(d)) {
                match def {
                    Ok(d) => {
                        for w in d.warnings {
                            eprintln!("{}: {w}", path.display());
                        }
                        // `kraal <name>` opens a workspace, unless the name is a command.
                        if commands.find_subcommand(&d.name).is_some() {
                            let (path, name) = (path.display(), &d.name);
                            eprintln!(
                                "{path}: `kraal {name}` is a command of kraal: open this workspace with `kraal attach {name}`"
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!("error: {}: {e:#}", path.display());
                        errors += 1;
                    }
                }
            }
            let (accounts, inventory) = (crate::accounts::Accounts::path(), servers::Inventory::path());
            let checked = [
                (&accounts, crate::accounts::Accounts::check(&accounts)),
                (&inventory, servers::Inventory::check(&inventory)),
            ];
            for (path, checked) in checked {
                match checked {
                    Ok(warnings) => warnings.iter().for_each(|w| eprintln!("warning: {}: {w}", path.display())),
                    Err(e) => {
                        eprintln!("error: {e:#}");
                        errors += 1;
                    }
                }
            }
            // Your runbooks, and what the repository this runs in ships.
            let repo = runbooks::current_repo();
            let (_, unreadable) = crate::runbooks::all(repo.as_deref());
            for e in &unreadable {
                eprintln!("error: {e}");
            }
            errors += unreadable.len();
            if let Some(Err(e)) = repo.as_deref().map(crate::worktree_setup::check_repo_file) {
                eprintln!("error: {e:#}");
                errors += 1;
            }
            if errors > 0 {
                bail!("{errors} file{} could not be read", if errors == 1 { "" } else { "s" });
            }
            println!("config ok");
        }
    }
    Ok(())
}
