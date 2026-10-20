//! Servers from the command line: attach and install remotely, the inventory, ssh, fan-out,
//! copies, file manager, remote edits and tunnels.

use anyhow::{Context, Result, bail};

use super::args::{ServersCmd, TunnelCmd};
use super::{Session, known_servers, resolve_targets};
use crate::protocol::{Placement, Request, Response};
use crate::{client, config, fleet, paths, server, servers, transfer, workspace};

pub(super) fn attach(session: &str, host: &str, workspace: Option<String>, pane: Option<String>) -> Result<()> {
    // A configuration that cannot be read does not say what the user allows:
    // servers.toml is left as it is then.
    let (cfg, may_update) = match config::Config::load() {
        Ok(cfg) => (cfg, true),
        Err(e) => {
            eprintln!("kraal: {e:#}; servers.toml is left as it is");
            (config::Config::default(), false)
        }
    };
    let known = known_servers(&cfg)?;
    let s = servers::resolve(&known, host).with_context(|| format!("not a server: {host}"))?;
    let target = client::Target::Remote {
        ssh: remote_ssh_args(&known, &s),
        session: session.to_string(),
        label: s.name.clone(),
        host: host.to_string(),
        pane,
        clipboard_native: s.clipboard_native,
    };
    // Once a kraal there answered, the sidebar lists that session's agents from then on.
    let remember = || {
        if !may_update {
            return;
        }
        let marked = servers::Inventory::update(|inv| Ok(inv.watch_agents(&s, session)));
        match marked {
            Err(e) => eprintln!("kraal: the agents of {} will not be listed: {e:#}", s.name),
            Ok(None) if cfg.servers.auto_add => eprintln!(
                "kraal: {} is not saved. Use `kraal servers edit` to save its connection with agents = {session:?}.",
                s.address()
            ),
            _ => {}
        }
    };
    client::attach(target, workspace, cfg.mouse, remember)
}

/// ssh arguments (up to the destination) for kraal's own connection to a server: no shared
/// connection (a stale master would delay reconnecting), a server that does not answer (not
/// even ssh's greeting) given up after 15 s, and dead links detected in ~15 s.
fn remote_ssh_args(known: &[servers::Known], s: &servers::Server) -> Vec<String> {
    let mut args: Vec<String> = [
        "-T",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "ConnectTimeout=15",
        "-o",
        "ServerAliveInterval=5",
        "-o",
        "ServerAliveCountMax=3",
    ]
    .iter()
    .map(|a| a.to_string())
    .collect();
    args.extend(servers::ssh_args(s, known, false));
    args.push(s.destination());
    args
}

pub(super) fn install(session: &str, server: &str) -> Result<()> {
    let cfg = config::Config::load()?;
    let known = known_servers(&cfg)?;
    let s = servers::resolve(&known, server).with_context(|| format!("not a server: {server}"))?;
    let mut ssh = remote_ssh_args(&known, &s);
    let dest = ssh.pop().unwrap();
    // Bounded, with a deadline: a process left holding ssh's output never hangs the install.
    let run = |cmd: &str| -> Result<String> {
        let mut c = std::process::Command::new("ssh");
        c.args(&ssh).arg(&dest).arg(cmd);
        let out = crate::exec::run_interactive(
            &mut c,
            std::process::Stdio::null(),
            std::time::Duration::from_secs(60),
            1 << 20,
        )?;
        if out.timed_out {
            bail!("{cmd} on {} took more than a minute", s.name);
        }
        if !out.success() {
            bail!("{} failed on {}: {}", cmd, s.name, out.stderr_text().trim());
        }
        Ok(out.stdout_text().trim().to_string())
    };
    let remote = run("uname -sm")?;
    let os = match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        other => other,
    };
    let arch = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "arm64",
        (_, a) => a,
    };
    let local = format!("{os} {arch}");
    if remote != local {
        bail!("{} runs {remote}, this kraal is built for {local}; build kraal there (cargo install --path .)", s.name);
    }
    let script = install_script(&format!("kraal {}", env!("CARGO_PKG_VERSION")));
    let exe = std::env::current_exe()?;
    let binary = std::fs::File::open(&exe).with_context(|| format!("reading {}", exe.display()))?;
    let mut c = std::process::Command::new("ssh");
    c.args(&ssh).arg(&dest).arg(format!("sh -c {}", crate::shell::quote(&script)));
    let out = crate::exec::run_interactive(&mut c, binary.into(), std::time::Duration::from_secs(600), 1 << 20)
        .context("starting ssh")?;
    if out.timed_out {
        bail!("installing on {} took more than 10 minutes", s.name);
    }
    if !out.success() {
        bail!("installing on {} failed: {}", s.name, out.stderr_text().trim());
    }
    let version = out.stdout_text().trim().to_string();
    println!("{version} installed on {} (~/.local/bin/kraal).", s.name);
    let sessions = run(&crate::shell::sh_c("exec \"$HOME/.local/bin/kraal\" ls"))?;
    if sessions.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next() == Some(session) && fields.next() == Some("running")
    }) {
        use std::io::IsTerminal;
        let mut restart = std::process::Command::new("ssh");
        restart.args(&ssh);
        if std::io::stdin().is_terminal() {
            restart.arg("-t");
        }
        let command = format!("exec \"$HOME/.local/bin/kraal\" -s {} restart", crate::shell::quote(session));
        let status =
            restart.arg(&dest).arg(crate::shell::sh_c(&command)).status().context("restarting the remote session")?;
        anyhow::ensure!(
            status.success(),
            "kraal is installed on {}, but session {session} was not restarted; retry with `kraal -s {session} remote-install {server}`",
            s.name
        );
    }
    println!("Attach with `kraal -s {session} attach --host {server}`");
    Ok(())
}

/// The remote side of `remote-install`, reading the binary on stdin. It uploads next to the old
/// binary under a name of its own, checks that the copy runs there and says `expected`, and only
/// then puts it in place: a failed copy never breaks the installed one.
fn install_script(expected: &str) -> String {
    format!(
        "umask 077; d=\"$HOME/.local/bin\"; mkdir -p \"$d\" || exit 1; \
         if [ -d \"$d/kraal\" ]; then echo \"$d/kraal is a directory\" >&2; exit 1; fi; \
         t=$(mktemp \"$d/.kraal.new.XXXXXX\") || exit 1; \
         trap 'rm -f -- \"$t\"' EXIT; trap 'exit 1' HUP INT TERM; \
         cat > \"$t\" || exit 1; chmod 755 \"$t\" || exit 1; \
         v=$(\"$t\" --version 2>&1) || v=\"it does not run: $v\"; \
         if [ \"$v\" != {} ]; then echo \"the copy says: $v\" >&2; exit 1; fi; \
         mv -f -- \"$t\" \"$d/kraal\" || exit 1; echo \"$v\"",
        crate::shell::quote(expected)
    )
}

/// The options of `kraal servers add` (in `given`) that would change the entry `saved` for that
/// address: adding it again does not set them.
fn options_not_saved(given: &servers::Server, saved: &servers::Server) -> Vec<&'static str> {
    let differs = |given: &Option<String>, saved: &Option<String>| given.is_some() && given != saved;
    [
        ("--name", !given.name.is_empty() && given.name != saved.name),
        ("--group", given.groups.iter().any(|g| !saved.groups.contains(g))),
        ("--jump", differs(&given.jump, &saved.jump)),
        ("--identity", differs(&given.identity, &saved.identity)),
        ("--env", differs(&given.env, &saved.env)),
        ("--note", differs(&given.note, &saved.note)),
    ]
    .into_iter()
    .filter_map(|(option, dropped)| dropped.then_some(option))
    .collect()
}

pub(super) fn servers(action: ServersCmd) -> Result<()> {
    let cfg = config::Config::load()?;
    match action {
        ServersCmd::List { json, group } => {
            let known: Vec<servers::Known> = known_servers(&cfg)?
                .into_iter()
                .filter(|k| group.as_ref().is_none_or(|g| k.server.groups.contains(g)))
                .collect();
            if json {
                let list: Vec<&servers::Server> = known.iter().map(|k| &k.server).collect();
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            for k in &known {
                let source = match &k.source {
                    servers::Source::Inventory => "saved".to_string(),
                    servers::Source::Workspace(w) => format!("workspace {w}"),
                    servers::Source::SshConfig => "ssh config".to_string(),
                };
                let env = k.server.env.as_deref().map(|e| format!("env={e}")).unwrap_or_default();
                println!(
                    "{:<22} {:<42} {:<20} {:<12} {source}",
                    k.server.name,
                    k.server.address(),
                    k.server.groups.join(","),
                    env
                );
            }
            if known.is_empty() {
                println!("no servers yet: `kraal servers add user@host`, or `kraal servers import`");
            }
            Ok(())
        }
        ServersCmd::Add { destination, name, group, jump, identity, env, note, agents } => {
            let (user, host, port) = servers::parse_destination(&destination)
                .with_context(|| format!("not a destination: {destination}"))?;
            if let Some(a) = agents.as_deref().filter(|a| !paths::valid_session_name(a)) {
                bail!("invalid session name {a:?}");
            }
            let s = servers::Server {
                name: name.unwrap_or_default(),
                host,
                user,
                port,
                jump,
                identity,
                more_identities: Vec::new(),
                proxy: None,
                groups: group,
                env,
                note,
                agents,
                clipboard_native: false,
            };
            let file = paths::shorten_home(&servers::Inventory::path());
            let done = servers::Inventory::update(|inv| {
                let Some(saved) = inv.saved_mut(&s) else {
                    return Ok(format!("saved {} in {file}", inv.add(s)?));
                };
                // An address already saved keeps its entry: nothing is said saved that was not.
                let dropped = options_not_saved(&s, saved);
                if !dropped.is_empty() {
                    bail!(
                        "{} is already saved as {}, and adding it again leaves its entry as it is: {} would not \
                         be set. Change it with `kraal servers edit`",
                        s.address(),
                        saved.name,
                        dropped.join(", ")
                    );
                }
                // But for the session whose agents are listed.
                Ok(match s.agents.filter(|a| saved.agents.as_ref() != Some(a)) {
                    Some(session) => {
                        let done = format!(
                            "{} is already saved in {file}: it now lists the agents of the session {session} there",
                            saved.name
                        );
                        saved.agents = Some(session);
                        done
                    }
                    None => format!("{} is already saved in {file}", saved.name),
                })
            })?;
            println!("{done}");
            Ok(())
        }
        ServersCmd::Rm { name } => {
            servers::Inventory::update(|inv| {
                if !inv.remove(&name) {
                    bail!("no saved server named {name:?}");
                }
                Ok(())
            })?;
            println!("removed {name}");
            Ok(())
        }
        ServersCmd::Import => {
            let defs: Vec<workspace::WorkspaceDef> =
                cfg.workspace_dirs().iter().flat_map(|d| workspace::load_dir(d)).filter_map(|(_, d)| d.ok()).collect();
            let added = servers::Inventory::update(|inv| {
                let before = inv.servers.len();
                for k in servers::from_workspaces(&defs) {
                    inv.add(k.server)?;
                }
                Ok(inv.servers.len() - before)
            })?;
            println!("imported {} servers into {}", added, paths::shorten_home(&servers::Inventory::path()));
            Ok(())
        }
        ServersCmd::Edit => {
            let path = servers::Inventory::path();
            if !path.exists() {
                servers::Inventory::empty().save()?;
            }
            let editor = crate::edit::editor_argv(&path, None);
            let status = std::process::Command::new(&editor[0]).args(&editor[1..]).status()?;
            if !status.success() {
                bail!("the editor exited with {status}");
            }
            servers::Inventory::load().map(|inv| println!("{} servers", inv.servers.len()))
        }
    }
}

/// `kraal ssh`: a grid or a pane in the session, or plain ssh in this terminal.
pub(super) fn ssh(
    s: &Session,
    target: String,
    group: bool,
    placement: Option<Placement>,
    command: Vec<String>,
) -> Result<()> {
    if group || placement.is_some() {
        s.ensure_server()?;
        s.request(Request::OpenSsh {
            target,
            group,
            placement: placement.unwrap_or_default(),
            near: s.own_pane(),
            command,
        })?;
        return Ok(());
    }
    exec_ssh(&target, &command)
}

/// Replace this process with `ssh` to a known server.
fn exec_ssh(target: &str, command: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let cfg = config::Config::load()?;
    let known = known_servers(&cfg)?;
    let s = servers::resolve(&known, target).with_context(|| format!("not a server: {target}"))?;
    approve_production("ssh", production_names(&cfg, [&s]))?;
    let mut args = servers::ssh_args(&s, &known, cfg.servers.multiplex);
    args.push(s.destination());
    args.extend(command.iter().cloned());
    let err = std::process::Command::new("ssh").args(&args).exec();
    Err(err).context("running ssh")
}

/// `kraal cp`. With `no_replace`, nothing in the destination is replaced but the names it
/// allows. A move renames what it can, and otherwise deletes only what rsync copied.
pub(super) fn copy(
    args: &[String],
    dry_run: bool,
    move_: bool,
    no_replace: Option<&[String]>,
    expect: &[String],
    laptop_spool: bool,
) -> Result<()> {
    let cfg = config::Config::load()?;
    let known = known_servers(&cfg)?;
    let (dest, sources) = args.split_last().context("kraal cp SOURCE... DEST")?;
    let sources: Vec<transfer::Endpoint> =
        sources.iter().map(|s| transfer::parse_endpoint(s, &known)).collect::<Result<_>>()?;
    let mut dest = transfer::parse_endpoint(dest, &known)?;
    if sources.len() > 1 {
        // Several sources go into the destination directory, whatever is renamed on the way.
        dest = dest.into_dir();
    }
    let ssh = |s: &servers::Server| servers::ssh_args(s, &known, cfg.servers.multiplex);
    check_expected(sources.iter().chain([&dest]), expect, &ssh)?;
    if !dry_run {
        approve_production("cp", production_names(&cfg, remote_servers(sources.iter().chain([&dest]))))?;
    }
    let laptop_install = laptop_spool && sources.iter().all(|source| matches!(source, transfer::Endpoint::Local(_)));
    if laptop_spool && !laptop_install {
        anyhow::ensure!(!move_, "a laptop spool download cannot move sources");
        return crate::laptop::copy::fetch(&sources, &dest, &ssh);
    }
    if let Some(allowed) = no_replace {
        let there: Vec<String> =
            transfer::existing_in(&sources, &dest, &ssh)?.into_iter().filter(|n| !allowed.contains(n)).collect();
        if !there.is_empty() {
            bail!(
                "{} already has {}: nothing was copied (copy again to replace them)",
                dest.display(),
                there.join(", ")
            );
        }
    }
    let opts = transfer::Options {
        rsync: transfer::rsync_capable(),
        replace: no_replace.map(<[String]>::to_vec),
        moving: move_,
        dest_dir: None,
    };
    if move_ {
        transfer::check_move(&sources, &dest)?;
    }
    if dry_run {
        for s in &transfer::plan(&sources, &dest, &ssh, &opts)? {
            println!("{}", s.shell_line());
        }
        return Ok(());
    }
    if move_ {
        transfer::check_move_across(&sources, &dest, &ssh)?;
    }
    let sources = if move_ && !laptop_install { transfer::rename_local(&sources, &dest, &opts)? } else { sources };
    if sources.is_empty() {
        eprintln!("done");
        return Ok(());
    }
    let steps = transfer::plan(&sources, &dest, &ssh, &opts)?;
    if laptop_install {
        transfer::run_laptop_install(&steps)?;
    } else {
        let what: Vec<String> = sources.iter().map(|s| s.display()).collect();
        eprintln!("{} {} → {}", if move_ { "moving" } else { "copying" }, what.join(", "), dest.display());
        transfer::run(&steps)?;
    }
    if move_ {
        let kept = transfer::prune_moved(&sources, &ssh)?;
        if !kept.is_empty() {
            eprintln!("left in place, not empty (files that were not moved): {}", kept.join(", "));
        }
    }
    eprintln!("done");
    Ok(())
}

/// The production servers among `servers`, each with where it is reached, port included: two
/// servers may have one name (`db` of `ops@db.prod.eu.example.net` and of `db.prod.us.example.net`).
fn production_names<'a>(cfg: &config::Config, servers: impl IntoIterator<Item = &'a servers::Server>) -> Vec<String> {
    let words = &cfg.production.words;
    servers
        .into_iter()
        .filter(|s| crate::context::server_production(s, words))
        .map(|s| format!("{} ({})", s.name, s.address()))
        .collect()
}

fn remote_servers<'a>(endpoints: impl Iterator<Item = &'a transfer::Endpoint>) -> Vec<&'a servers::Server> {
    endpoints
        .filter_map(|e| match e {
            transfer::Endpoint::Remote { server, .. } => Some(server),
            transfer::Endpoint::Local(_) => None,
        })
        .collect()
}

/// Before this command acts on the production servers named: when it runs in a kraal pane (its
/// environment, or that of the nearest process it runs under, says so), the session of that pane
/// asks the user if an agent runs there, whatever `-s` says: only that one knows the agent.
/// Outside kraal, no agent of kraal's runs it. Any error stops the command.
fn approve_production(kind: &str, servers: Vec<String>) -> Result<()> {
    if servers.is_empty() {
        return Ok(());
    }
    let env = crate::process::kraal_env(std::process::id());
    if env.is_empty() {
        return Ok(());
    }
    let socket = match (env.get("KRAAL_SOCKET"), env.get("KRAAL_SESSION")) {
        (Some(socket), _) => std::path::PathBuf::from(socket),
        (None, Some(session)) => paths::socket_path(session),
        (None, None) => bail!("this runs in a kraal pane whose session kraal cannot tell: nothing was done"),
    };
    let ask = Request::ApproveProduction { kind: kind.into(), plan: client::command_line(), servers };
    client::request(&socket, ask)
        .map(drop)
        .map_err(|e| if format!("{e:#}").contains("nothing was done") { e } else { e.context("nothing was done") })
}

/// `--expect SERVER=SSH`: the file manager listed SERVER through these ssh arguments; refuse
/// when the inventory now reaches it differently, or no longer knows it by that name (an address
/// left from a removed entry), so nothing acts on another machine than the one the panel showed.
fn check_expected<'a>(
    endpoints: impl Iterator<Item = &'a transfer::Endpoint>,
    expect: &[String],
    ssh: transfer::SshOptions,
) -> Result<()> {
    let servers = remote_servers(endpoints);
    for x in expect {
        let Some((name, args)) = x.split_once('=') else { bail!("--expect {x:?}: SERVER=SSH ARGUMENTS") };
        let found: Vec<&&servers::Server> = servers.iter().filter(|s| s.name == name).collect();
        if found.is_empty() {
            bail!(
                "{name} is not the server its panel listed any more (its entry changed or is gone): nothing was done"
            );
        }
        for server in found {
            let mut now = ssh(server);
            now.push(server.destination());
            if crate::shell::join(&now) != args {
                bail!("{name} is not reached as when its panel was listed (its settings changed): nothing was done");
            }
        }
    }
    Ok(())
}

pub(super) fn remove(args: &[String], expect: &[String]) -> Result<()> {
    let cfg = config::Config::load()?;
    let known = known_servers(&cfg)?;
    let targets: Vec<transfer::Endpoint> =
        args.iter().map(|s| transfer::parse_endpoint(s, &known)).collect::<Result<_>>()?;
    let ssh = |s: &servers::Server| servers::ssh_args(s, &known, cfg.servers.multiplex);
    check_expected(targets.iter(), expect, &ssh)?;
    approve_production("rm", production_names(&cfg, remote_servers(targets.iter())))?;
    transfer::remove(&targets, &ssh)?;
    eprintln!("deleted {} item{}", targets.len(), if targets.len() == 1 { "" } else { "s" });
    Ok(())
}

/// `kraal files`: the two-panel file manager, on this directory and home by default.
pub(super) fn files(s: &Session, left: Option<String>, right: Option<String>) -> Result<()> {
    s.ensure_server()?;
    let home = || paths::home_dir().unwrap_or_default().display().to_string();
    let abs = |p: String| -> String {
        if p.contains(':') && !p.starts_with('/') && !p.starts_with('.') {
            return p;
        }
        std::path::absolute(paths::expand_tilde(&p)).map(|a| a.display().to_string()).unwrap_or(p)
    };
    let left = left
        .map(abs)
        .unwrap_or_else(|| std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_else(|_| home()));
    let right = right.map(abs).unwrap_or_else(home);
    s.request(Request::OpenFiles { left, right, near: s.own_pane() })?;
    Ok(())
}

/// `kraal edit`: a server's file in the local editor, in a floating pane of the session (next
/// to this pane inside kraal); kraal sends each save back.
pub(super) fn edit(s: &Session, target: String, line: Option<u32>, production: bool, sudo: bool) -> Result<()> {
    s.ensure_server()?;
    s.request(Request::EditRemote { target, line, near: s.own_pane(), production, sudo })?;
    Ok(())
}

pub(super) fn exec_fleet(
    targets: &[String],
    command: &[String],
    parallel: usize,
    timeout: u64,
    json: bool,
) -> Result<()> {
    use std::io::IsTerminal;
    let cfg = config::Config::load()?;
    let known = known_servers(&cfg)?;
    let list = resolve_targets(&known, targets)?;
    approve_production("exec", production_names(&cfg, &list))?;
    let line = command.join(" ");
    let ssh = |s: &servers::Server| servers::ssh_args(s, &known, cfg.servers.multiplex);
    let color = std::io::stdout().is_terminal();
    if !json {
        eprintln!("running `{line}` on {} server{}…", list.len(), if list.len() == 1 { "" } else { "s" });
    }
    let print = |o: &fleet::Outcome| {
        if !json {
            print!("{}", fleet::format_outcome(o, color));
        }
    };
    // The ssh processes have process groups of their own (a timeout kills each with what it
    // started), so ctrl+c does not reach them: it cancels the run, which kills them.
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP] {
        signal_hook::flag::register(sig, cancel.clone()).context("handling ctrl+c")?;
    }
    let plan = fleet::Plan {
        command: &line,
        ssh_options: &ssh,
        parallel,
        timeout: std::time::Duration::from_secs(timeout),
        cancel: &cancel,
        // Printed as each arrives; only `--json` prints them again at the end.
        keep_output: json,
    };
    let total = list.len();
    let results = fleet::run_all(list, &plan, &print);
    if cancel.load(std::sync::atomic::Ordering::SeqCst) {
        eprintln!("cancelled: {} stopped, {} not started", results.len(), total - results.len());
        std::process::exit(130);
    }
    let failed = results.iter().filter(|o| !o.ok()).count();
    if json {
        let v: Vec<serde_json::Value> = results
            .iter()
            .map(|o| {
                serde_json::json!({
                    "server": o.server, "code": o.code, "ok": o.ok(), "error": o.error,
                    "stdout": o.stdout, "stderr": o.stderr, "seconds": o.elapsed.as_secs_f64(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        eprintln!("{} ok, {failed} failed", results.len() - failed);
    }
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

pub(super) fn tunnel(s: &Session, action: TunnelCmd) -> Result<()> {
    s.ensure_server()?;
    let print = |list: Vec<server::net::TunnelInfo>| {
        for t in list {
            let err = t.error.map(|e| format!("  ({e})")).unwrap_or_default();
            println!("{:<12} {}{err}", format!("{:?}", t.state).to_lowercase(), t.spec.describe());
        }
    };
    match action {
        TunnelCmd::Add { server, remote, local } => {
            if let Response::Tunnels(t) = s.request(Request::TunnelAdd { server, remote, local })? {
                print(t);
            }
        }
        TunnelCmd::List => {
            if let Response::Tunnels(t) = s.request(Request::TunnelList)? {
                if t.is_empty() {
                    println!("no tunnels");
                }
                print(t);
            }
        }
        TunnelCmd::Rm { port } => {
            s.request(Request::TunnelRemove { key: port })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_servers_are_told_apart_by_where_they_are_reached() {
        let cfg = config::Config::default();
        let at = |port| servers::Server {
            name: "db".into(),
            host: "db.prod.example.net".into(),
            user: Some("ops".into()),
            port: Some(port),
            ..Default::default()
        };
        let names = production_names(&cfg, [&at(2201), &at(2202)]);
        assert_eq!(names, ["db (ops@db.prod.example.net:2201)", "db (ops@db.prod.example.net:2202)"]);
    }

    #[test]
    fn attach_refusal_preserves_the_explicit_destination() {
        if !crate::testing::in_own_process("cli::remote::tests::attach_refusal_preserves_the_explicit_destination") {
            return;
        }
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: valid output pointers; no custom terminal settings or size.
        assert_eq!(
            unsafe {
                libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
            },
            0
        );
        // SAFETY: these descriptors were just opened and are owned only here.
        let (_master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        // SAFETY: this isolated process owns stdin; attach needs a terminal but refuses before reading it.
        assert_eq!(unsafe { libc::dup2(slave.as_raw_fd(), libc::STDIN_FILENO) }, libc::STDIN_FILENO);
        let dir = std::env::temp_dir().join(format!("kraal-remote-refusal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reply = dir.join("reply");
        crate::testing::write_script(
            &dir.join("ssh"),
            &format!("#!/bin/sh\ncat {}\nexec cat >/dev/null\n", crate::shell::quote(reply.to_str().unwrap())),
        );
        // The isolated test has not started any workers yet.
        unsafe {
            std::env::set_var("PATH", format!("{}:{}", dir.display(), std::env::var("PATH").unwrap()));
        }
        for reason in [
            "protocol mismatch: server v2, client v3; run `kraal kill`, then `kraal` again (the layout is kept)",
            "session work runs kraal 0.3.0, too old for this kraal: `kraal -s work restart` restarts it on this one (its layout is kept)",
        ] {
            let mut bytes = Vec::new();
            crate::protocol::write_json(&mut bytes, &crate::protocol::ServerMsg::exit(reason)).unwrap();
            std::fs::write(&reply, bytes).unwrap();
            for (host, word) in
                [("deploy@db.lan:2222", "deploy@db.lan:2222"), ("deploy@[::1]:2222", "'deploy@[::1]:2222'")]
            {
                for (session, prefix) in [("main", "kraal"), ("work", "kraal -s work")] {
                    let error = attach(session, host, None, None).unwrap_err().to_string();
                    assert!(error.starts_with("protocol mismatch:"), "{error}");
                    assert!(error.ends_with(&format!("run `{prefix} remote-install {word}`")), "{error}");
                }
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_panel_server_must_still_be_the_one_listed() {
        let db = servers::Server { name: "db".into(), host: "db.lan".into(), ..Default::default() };
        let ssh = |_: &servers::Server| Vec::new();
        let at = |server: &servers::Server| transfer::Endpoint::Remote { server: server.clone(), path: "/x".into() };
        let listed = format!("db={}", crate::shell::join(&[db.destination()]));
        assert!(check_expected([at(&db)].iter(), std::slice::from_ref(&listed), &ssh).is_ok());
        // The same address, known by another name now (its entry was removed): refused.
        let renamed = servers::Server { name: "db.lan".into(), ..db.clone() };
        assert!(check_expected([at(&renamed)].iter(), &[listed], &ssh).is_err());
        // Reached differently: refused.
        let moved = servers::Server { port: Some(2222), ..db.clone() };
        let listed = format!("db={}", crate::shell::join(&["-p".to_string(), "2222".to_string(), db.destination()]));
        assert!(check_expected([at(&moved)].iter(), &[listed], &ssh).is_err());
    }

    #[test]
    fn remote_install_keeps_the_old_binary_unless_the_copy_works() {
        let home = std::env::temp_dir().join(format!("kraal-install-{}", std::process::id()));
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("kraal"), "old").unwrap();
        let install = |copy: &str| {
            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg(install_script("kraal 9.9"))
                .env("HOME", &home)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            std::io::Write::write_all(&mut child.stdin.take().unwrap(), copy.as_bytes()).unwrap();
            child.wait_with_output().unwrap()
        };
        let leftovers =
            || std::fs::read_dir(&bin).unwrap().filter(|e| e.as_ref().unwrap().file_name() != "kraal").count();
        // A copy that does not run, or is another version, is dropped.
        for bad in ["\x7fELF garbage", "#!/bin/sh\necho kraal 0.1\n"] {
            let out = install(bad);
            assert!(!out.status.success());
            assert!(String::from_utf8_lossy(&out.stderr).contains("the copy says"), "{out:?}");
            assert_eq!(std::fs::read_to_string(bin.join("kraal")).unwrap(), "old");
            assert_eq!(leftovers(), 0);
        }
        let out = install("#!/bin/sh\necho kraal 9.9\n");
        assert!(out.status.success(), "{out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "kraal 9.9");
        assert!(std::fs::read_to_string(bin.join("kraal")).unwrap().contains("echo kraal 9.9"));
        assert_eq!(leftovers(), 0);
        let _ = std::fs::remove_dir_all(home);
    }
}
