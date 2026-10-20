#[cfg(target_os = "linux")]
use super::support::{EditBench, Sshd, in_terminal, read, set_buffer};
use super::support::{
    PATIENCE, RawClient, TerminalClient, buffer_list, click_account_text, fake_ssh, raw_frame, text_after,
};
use crate::common;
use crate::common::{Env, claude_hook, wait_until, write_script};
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::process::Command;
use std::process::Output;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
#[test]
fn a_protocol_three_remote_refusal_names_remote_install_for_the_selected_session() {
    let mut env = Env::new("rproto3");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let refusal = env.root.join("refusal");
    write_script(&bin.join("ssh"), &format!("#!/bin/sh\ncat '{}'\n", refusal.display()));
    for session in ["main", "work"] {
        let reason = format!(
            "protocol mismatch: client protocol 6, server protocol 3 (kraal 0.4.0, session {session}); run `kraal -s {session} restart`"
        );
        let payload = serde_json::to_vec(&serde_json::json!({ "type": "exit", "reason": reason })).unwrap();
        let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
        frame.push(0);
        frame.extend(payload);
        std::fs::write(&refusal, frame).unwrap();
        env.session = session.into();
        let mut command = env.cmd(&["attach", "--host", "box"]);
        command.env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()));
        let shown = in_terminal(command, b"");
        let recovery = if session == "main" { "kraal remote-install box" } else { "kraal -s work remote-install box" };
        assert!(shown.contains(recovery), "{shown}");
        assert!(shown.contains("client protocol 6, server protocol 3 (kraal 0.4.0"), "{shown}");
        assert!(shown.contains(&format!("session {session}")), "{shown}");
        assert!(!shown.contains(" restart") && !shown.contains("\x1b[?1049h"), "{shown}");
    }
}

/// A name given where a workspace's goes that names no workspace but reads as a server
/// (`user@host`, or a server kraal knows) is answered with the command that was meant, and
/// kraal attaches nowhere; any other name gets the plain answer. No session is started only to
/// say that a name is unknown.
#[test]
fn a_server_given_where_a_workspace_goes_is_told_how_to_attach_to_it() {
    let env = Env::new("hostname");
    std::fs::write(env.root.join("config/kraal/servers.toml"), "[[server]]\nname = \"db\"\nhost = \"db.lan\"\n")
        .unwrap();
    let said = |name: &str| {
        let out = env.try_run(&["open", name]);
        assert!(!out.status.success(), "{name}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let meant = |name: &str| format!("; to attach to kraal on that server: kraal attach --host {name}");
    let open_says = |when: &str| {
        for name in ["fpa@anvil", "db"] {
            let said = said(name);
            assert!(said.contains(&format!("no workspace named {name:?} in ")), "{when}: {said}");
            assert!(said.trim_end().ends_with(&meant(name)), "{when}: {said}");
        }
        let plain = said("nothing");
        assert!(plain.contains("no workspace named \"nothing\" in ") && !plain.contains("--host"), "{when}: {plain}");
    };

    open_says("without a session");
    assert_eq!(env.server_pid(), None, "a session was started only to say that a name is unknown");

    // A session running: its server answers the same, and a terminal is not attached to it.
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    open_says("with a session");
    let refused = RawClient::attach(&env, "fpa@anvil");
    refused.wait_for_message(&meant("fpa@anvil"));
    assert!(!refused.messages.lock().unwrap().iter().any(|m| m.contains("\"attached\"")));
    // Any other unknown name attaches all the same, with the plain message.
    let client = RawClient::attach(&env, "nothing");
    client.wait_for_message("\"attached\"");
    let rows = client.wait_for_row("no workspace named \"nothing\"");
    assert!(!rows.iter().any(|r| r.contains("--host")), "{rows:?}");
    // A workspace named like a server is a workspace.
    env.workspace("db", "layout {\n    pane name=\"data\"\n}\n");
    env.run(&["open", "db"]);
    let named = RawClient::attach(&env, "db");
    named.wait_for_message("\"attached\"");
    named.wait_for_row("data");
}

/// `kraal attach fpa@anvil` and `kraal fpa@anvil`, typed for `kraal attach --host fpa@anvil`:
/// the terminal gets the answer and is not attached to the local session, running or not.
#[cfg(target_os = "linux")]
#[test]
fn attaching_to_a_server_given_as_a_workspace_says_what_was_meant() {
    let env = Env::new("hostattach");
    let forms: [&[&str]; 2] = [&["attach", "fpa@anvil"], &["fpa@anvil"]];
    let check = |when: &str| {
        for args in forms {
            let shown = in_terminal(env.cmd(args), b"");
            // Said in the terminal as it is: the screen is not taken and given back.
            assert!(shown.starts_with("kraal: no workspace named \"fpa@anvil\" in "), "{when}, {args:?}: {shown:?}");
            assert!(shown.contains("to attach to kraal on that server: kraal attach --host fpa@anvil"), "{shown}");
        }
    };
    check("without a session");
    assert_eq!(env.server_pid(), None, "a session was started only to say that a name is unknown");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    check("with a session");
}

/// `kraal servers add` for an address already saved leaves its entry as it is, and says what it
/// did: it never says "saved" for options it dropped. `--agents` is the one thing it sets there.
#[test]
fn adding_a_saved_server_again_never_claims_a_save() {
    let env = Env::new("readd");
    let file = env.root.join("config/kraal/servers.toml");
    let add = |args: &[&str]| env.try_run(&[&["servers", "add", "admin@db.lan"], args].concat());
    let said =
        |out: &Output| format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let saved = || std::fs::read_to_string(&file).unwrap();

    let out = add(&["--name", "db", "--group", "data"]);
    assert!(out.status.success() && said(&out).starts_with("saved db in "), "{}", said(&out));
    let first = saved();
    // An option that would change the entry is refused, by its name, and nothing is written.
    let options: [&[&str]; 7] = [
        &["--env", "prod"],
        &["--group", "web"],
        &["--jump", "bastion"],
        &["--identity", "~/.ssh/db"],
        &["--note", "primary"],
        &["--name", "other"],
        &["--env", "prod", "--agents", "main"],
    ];
    for option in options {
        let out = add(option);
        let said = said(&out);
        assert!(!out.status.success(), "{option:?}: {said}");
        assert!(said.contains("admin@db.lan is already saved as db") && said.contains(option[0]), "{option:?}: {said}");
        assert!(said.contains("`kraal servers edit`") && !said.contains("saved db in"), "{option:?}: {said}");
        assert_eq!(saved(), first, "{option:?}");
    }
    // What the entry already has changes nothing, and is not refused.
    for same in [&[][..], &["--name", "db", "--group", "data"]] {
        let out = add(same);
        assert!(out.status.success() && said(&out).starts_with("db is already saved in "), "{}", said(&out));
        assert_eq!(saved(), first);
    }
    // The session whose agents are listed is set on a saved server too, and said.
    let out = add(&["--agents", "main"]);
    assert!(out.status.success(), "{}", said(&out));
    assert!(said(&out).starts_with("db is already saved in ") && said(&out).contains("session main"), "{}", said(&out));
    assert_eq!(saved(), format!("{first}agents = \"main\"\n"));
    // With another port it is another address: a new entry, with what it was given.
    let out = env.try_run(&["servers", "add", "admin@db.lan:2222", "--env", "prod"]);
    assert!(out.status.success() && said(&out).starts_with("saved db-admin in "), "{}", said(&out));
    assert_eq!(saved().matches("env = \"prod\"").count(), 1);
}

#[test]
fn server_inventory_and_copy_plans() {
    let env = Env::new("servers");
    env.workspace(
        "lab",
        "layout {\n    pane split_direction=\"vertical\" {\n        pane name=\"web\" command=\"ssh\" { args \"-p\" \"2222\" \"admin@web.lab.invalid\"; }\n        pane name=\"db\" command=\"ssh\" { args \"admin@db.lab.invalid\"; }\n    }\n}\n",
    );
    let list = env.run(&["servers"]);
    assert!(
        list.contains("web") && list.contains("admin@web.lab.invalid:2222") && list.contains("workspace lab"),
        "{list}"
    );
    env.run(&["servers", "add", "ops@bastion.invalid", "--group", "infra"]);
    env.run(&["servers", "import"]);
    let list = env.run(&["servers", "list", "--group", "lab"]);
    assert_eq!(list.lines().count(), 2, "{list}");
    assert!(list.contains("saved"), "imported servers are saved: {list}");
    let json = env.run(&["servers", "list", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 3);
    // Which kraal session's agents to list: given to a server saved already, never a bad name.
    env.run(&["servers", "add", "ops@bastion.invalid", "--agents", "main"]);
    let json = env.run(&["servers", "list", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let bastion = v.as_array().unwrap().iter().find(|s| s["name"] == "bastion").cloned().unwrap();
    assert_eq!(bastion["agents"], "main", "{bastion}");
    assert!(!env.try_run(&["servers", "add", "ops@other.invalid", "--agents", "../x"]).status.success());

    // Copy plans, without touching the network.
    let file = env.root.join("report.txt");
    std::fs::write(&file, "x").unwrap();
    let up = env.run(&["cp", "--dry-run", file.to_str().unwrap(), "web:/tmp/"]);
    assert!(up.contains("-p 2222") && up.contains("admin@web.lab.invalid"), "{up}");
    let s2s = env.run(&["cp", "--dry-run", "web:/etc/hosts", "db:~/in/"]);
    assert!(s2s.contains("tar cf -") && s2s.contains("admin@db.lab.invalid"), "{s2s}");
    let bad = env.try_run(&["cp", "nosuch:/x", "."]);
    assert!(String::from_utf8_lossy(&bad.stderr).contains("unknown server"));

    env.run(&["servers", "rm", "bastion"]);
    assert!(!env.run(&["servers"]).contains("bastion"));
}

/// A name allowed to be replaced is matched whole on a server, though it holds a newline or ends
/// in a carriage return; the names next to it stay refused.
#[cfg(unix)]
#[test]
fn a_copy_without_replacing_matches_odd_names_whole_on_a_server() {
    let env = Env::new("odd-names");
    let (_, path) = fake_ssh(&env, &env);
    std::fs::write(env.root.join("config/kraal/servers.toml"), "[[server]]\nname='box'\nhost='names.invalid'\n")
        .unwrap();
    let (src, dest) = (env.root.join("src"), env.root.join("dest"));
    for dir in [&src, &dest] {
        std::fs::create_dir_all(dir).unwrap();
        for name in ["a\nb", "c\r", "a"] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
    }
    let cp = |names: &[&str], allowed: &[&str]| {
        let mut c = env.cmd(&["cp", "--dry-run", "--no-replace"]);
        c.args(allowed.iter().map(|a| format!("--allow={a}")));
        c.args(names.iter().map(|n| src.join(n)));
        c.arg(format!("box:{}/", dest.display()));
        c.env("PATH", &path).output().unwrap()
    };
    let out = cp(&["a\nb", "c\r"], &["a\nb", "c\r"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = cp(&["a\nb", "a"], &["a\nb"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("already has a: nothing was copied"), "{err}");
}

/// The commands that write servers.toml and accounts.toml edit the file the user wrote: its
/// comments, its order and the keys this kraal does not read are there after each of them.
#[test]
fn commands_that_write_the_inventories_keep_what_the_user_wrote() {
    let env = Env::new("keepfile");
    let config = env.root.join("config/kraal");
    let run = |args: &[&str]| {
        let out = env.cmd(args).output().unwrap();
        assert!(out.status.success(), "kraal {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    let read = |file: &Path| std::fs::read_to_string(file).unwrap();

    let servers = config.join("servers.toml");
    let by_hand = "# Ours. Ask ops first.\n\n\
                   [[server]]\nhost = \"bastion.example.com\"  # the way in\nname = \"bastion\"\ncolor = \"red\"\n";
    std::fs::write(&servers, by_hand).unwrap();
    run(&["servers", "add", "deploy@web.example.com", "--group", "web"]);
    let web = "\n[[server]]\nname = \"web\"\nhost = \"web.example.com\"\nuser = \"deploy\"\ngroups = [\"web\"]\n";
    assert_eq!(read(&servers), format!("{by_hand}{web}"));
    // What `kraal attach --host bastion` writes too: the session whose agents to list.
    run(&["servers", "add", "bastion.example.com", "--agents", "main"]);
    let watched = by_hand.replace("color = \"red\"\n", "color = \"red\"\nagents = \"main\"\n");
    assert_eq!(read(&servers), format!("{watched}{web}"));
    env.workspace("lab", "layout {\n    pane name=\"db\" command=\"ssh\" { args \"admin@db.example.com\"; }\n}\n");
    run(&["servers", "import"]);
    let db = "\n[[server]]\nname = \"db\"\nhost = \"db.example.com\"\nuser = \"admin\"\ngroups = [\"lab\"]\n";
    assert_eq!(read(&servers), format!("{watched}{web}{db}"));
    run(&["servers", "rm", "web"]);
    assert_eq!(read(&servers), format!("{watched}{db}"));
    // A file kraal cannot read is not written over.
    std::fs::write(&servers, "# mine\n[[server]\n").unwrap();
    assert!(!env.try_run(&["servers", "add", "ops@other.example.com"]).status.success());
    assert_eq!(read(&servers), "# mine\n[[server]\n");

    let accounts = config.join("accounts.toml");
    run(&["accounts", "limits", "--threshold", "80"]);
    let fresh = read(&accounts);
    assert!(fresh.starts_with("# kraal agent accounts."), "{fresh}");
    assert!(!fresh.contains("rule = []") && !fresh.contains("[claude]"), "{fresh}");

    let source = "# The team's accounts.\nrule = [] # no rules yet\n\n\
                  [claude] # reserved\n\n[limits]\nthreshold = 85\nfuture = 'kept'\n";
    std::fs::write(&accounts, source).unwrap();
    run(&["accounts", "limits", "--threshold", "80"]);
    assert_eq!(read(&accounts), source.replace("threshold = 85", "threshold = 80"));
    let broken = "# mine\nrule = []\n[claude]\n[[account]\n";
    std::fs::write(&accounts, broken).unwrap();
    assert!(!env.try_run(&["accounts", "limits", "--threshold", "80"]).status.success());
    assert_eq!(read(&accounts), broken);

    let by_hand = "# The team's accounts.\n\n[limits]\nmode = \"ask\"  # never by itself\nthreshold = 85\nwarn_at = 70\n\n\
                   # Work first.\n[[rule]]\nworkspace = \"work\"\naccounts = [\"work\", \"default\"]\n";
    std::fs::write(&accounts, by_hand).unwrap();
    run(&["accounts", "limits", "--threshold", "80"]);
    // Older servers keep the readable names; resolved identities go in their own field.
    let limited = format!(
        "{}account_ids = [\n    \"work\",\n    \"claude:@default-claude\",\n    \"codex:@default-codex\",\n]\n",
        by_hand.replace("threshold = 85", "threshold = 80")
    );
    assert_eq!(read(&accounts), limited);
    let home = env.root.join("codex-work").display().to_string();
    run(&["accounts", "add", "codex", "work", "--home", &home, "--no-login"]);
    let parsed: toml::Value = toml::from_str(&read(&accounts)).unwrap();
    let id = parsed["account"][0]["id"].as_str().unwrap();
    let account = format!("\n[[account]]\nid = {id:?}\nname = \"work\"\nagent = \"codex\"\nhome = \"{home}\"\n");
    // The previously unresolved name binds when its account is added, and stays bound.
    let limited = limited.replace("    \"work\",", &format!("    \"codex:@{id}\","));
    assert_eq!(read(&accounts), format!("{limited}{account}"));
    run(&["accounts", "rename", "codex:work", "job"]);
    let renamed = format!("{limited}{account}")
        .replace("name = \"work\"", "name = \"job\"")
        .replace("accounts = [\"work\",", "accounts = [\"job\",");
    assert_eq!(read(&accounts), renamed);
    run(&["accounts", "rm", "codex:job"]);
    assert_eq!(read(&accounts), limited.replace("accounts = [\"work\",", &format!("accounts = [\"codex:@{id}\",")));
}

/// Each `ssh.started` has its `ssh.ended`, as a reader keeping count of the connections needs:
/// when the connection drops (the pane stays, Enter connects again) and when the pane closes.
#[test]
fn every_ssh_that_starts_ends() {
    let env = Env::new("sshends");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // A connection to `drop` drops after a moment; the others stay.
    let ssh = bin.join("ssh");
    write_script(
        &ssh,
        "#!/bin/sh\necho \"connected $*\"\n[ \"$1\" = admin@drop ] && sleep 1 && exit 255\nexec sleep 1000\n",
    );
    let ssh = ssh.display().to_string();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let (mut events, lines) = env.follow(&["events", "--since", "0", "ssh.*"]);
    let mut seen: Vec<String> = Vec::new();
    let mut wait = |what: &str| {
        while !seen.iter().any(|s| s == what) {
            let line = lines.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| panic!("no {what}: {seen:?}"));
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            seen.push(format!("{} {}", v["type"].as_str().unwrap(), v["name"].as_str().unwrap()));
        }
    };
    env.run(&["run", "--name", "dropped", "--", &ssh, "admin@drop"]);
    wait("ssh.started dropped");
    wait("ssh.ended dropped");
    env.wait_for_text("dropped", "connected admin@drop");
    env.run(&["run", "--name", "closed", "--", &ssh, "admin@db"]);
    wait("ssh.started closed");
    env.run(&["close", "closed"]);
    wait("ssh.ended closed");
    let _ = events.kill();
    let _ = events.wait();
}

#[cfg(unix)]
#[test]
fn tunnels_are_up_when_ssh_listens_and_survive_a_restart() {
    let env = Env::new("tunnels");
    // A fake ssh: with "ok" in the destination, says it listens and makes its listener (as
    // `ssh -v` does); with "taken", says it listens but cannot bind (another program has the
    // port); else says nothing. Then it stays, as a tunnel does. Its pid goes in a file.
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let pids = env.root.join("ssh-pids");
    write_script(
        &bin.join("ssh"),
        &format!(
            "#!/bin/sh\nprev=; for a; do [ \"$prev\" = -L ] && fwd=$a; prev=$a; dest=$a; done\n\
             port=$(echo \"$fwd\" | cut -d: -f2)\necho $$ >> {}\n\
             case $dest in *ok*|*taken*) echo \"debug1: Local forwarding listening on 127.0.0.1 port $port.\" >&2;; esac\n\
             case $dest in *ok*) echo \"debug1: channel 0: new port-listener [port listener] (inactive timeout: 0)\" >&2;;\n\
             *taken*) echo \"bind [127.0.0.1]:$port: Address already in use\" >&2;; esac\n\
             exec sleep 600\n",
            pids.display()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let run = |args: &[&str]| {
        let out = env.cmd(args).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "kraal {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let free = || std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
    let state_of = |port: u16| {
        run(&["tunnel", "list"])
            .lines()
            .find(|l| l.ends_with(&format!("localhost:{port}")))
            .map(|l| l.split_whitespace().next().unwrap_or("").to_string())
    };
    let wait_state = |port: u16, want: &str| {
        let start = Instant::now();
        while state_of(port).as_deref() != Some(want) {
            assert!(start.elapsed() < Duration::from_secs(10), "{port}: {:?}", run(&["tunnel", "list"]));
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    run(&["open", "w"]);

    let (up, quiet) = (free(), free());
    run(&["tunnel", "add", "ok@a.lan", "5432", "--local", &up.to_string()]);
    wait_state(up, "up");
    // Another program listening on the port of a tunnel whose ssh is still connecting does not
    // make the tunnel up.
    run(&["tunnel", "add", "quiet@b.lan", "5433", "--local", &quiet.to_string()]);
    let squatter = std::net::TcpListener::bind(("127.0.0.1", quiet)).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(state_of(quiet).as_deref(), Some("connecting"));
    drop(squatter);
    // ssh announcing its listener is not enough: the bind may fail after it.
    let taken = free();
    run(&["tunnel", "add", "taken@e.lan", "5434", "--local", &taken.to_string()]);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(state_of(taken).as_deref(), Some("connecting"));
    // Two tunnels without a local port never get the same one, even before either listens.
    let wanted = free();
    run(&["tunnel", "add", "quiet@c.lan", &wanted.to_string()]);
    run(&["tunnel", "add", "quiet@d.lan", &wanted.to_string()]);
    let list = run(&["tunnel", "list"]);
    assert_eq!(list.matches(&format!("localhost:{wanted}")).count(), 1, "{list}");

    // Closing a tunnel stops its ssh.
    let before: Vec<i32> =
        std::fs::read_to_string(&pids).unwrap().lines().filter_map(|l| l.trim().parse().ok()).collect();
    run(&["tunnel", "rm", &quiet.to_string()]);
    let alive = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;
    let start = Instant::now();
    while before.iter().filter(|p| alive(**p)).count() > 4 {
        assert!(start.elapsed() < Duration::from_secs(5), "the closed tunnel's ssh still runs");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The local port is what names a tunnel, and the list shows it: of two tunnels to the same
    // port of two servers, each is closed by its own. A tunnel has no other name.
    let to_wanted = |list: &str| -> Vec<(String, u16)> {
        let lines = list.lines().filter(|l| l.contains(&format!(":{wanted} → localhost:")));
        lines
            .map(|l| {
                let (server, local) = l.split_once(" → localhost:").unwrap();
                (server.split_whitespace().last().unwrap().to_string(), local.trim().parse().unwrap())
            })
            .collect()
    };
    let both = to_wanted(&run(&["tunnel", "list"]));
    assert_eq!(both.len(), 2, "{both:?}");
    let out = env.cmd(&["tunnel", "rm", "#1"]).env("PATH", &path).output().unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && said.contains("not a local port: #1") && said.contains("kraal tunnel list"),
        "{said}"
    );
    run(&["tunnel", "rm", &both[1].1.to_string()]);
    assert_eq!(to_wanted(&run(&["tunnel", "list"])), [both[0].clone()]);

    // A tunnel through a connection kraal knows by no name comes back through that connection.
    run(&["kill"]);
    run(&["open", "w"]);
    wait_state(up, "up");
    let list = run(&["tunnel", "list"]);
    assert!(list.contains(&format!("ok@a.lan:5432 → localhost:{up}")), "{list}");
}

#[cfg(unix)]
#[test]
fn an_ssh_pane_keeps_the_account_it_was_opened_with() {
    let env = Env::new("account");
    // A fake ssh: logs its arguments; runs a remote script here, and ends at once otherwise (so
    // kraal never sees an ssh process in the pane, only what it started).
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = env.root.join("ssh.log");
    write_script(
        &bin.join("ssh"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nfor a; do cmd=$a; done\ncase $cmd in *' '*) exec sh -c \"$cmd\";; esac\nexit 0\n",
            log.display()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    // The inventory's `web` is root's.
    std::fs::write(
        env.root.join("config/kraal/servers.toml"),
        "[[server]]\nname = \"web\"\nhost = \"web.lan\"\nuser = \"root\"\n",
    )
    .unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    let run = |args: &[&str], pane: Option<&str>| {
        let mut c = env.cmd(args);
        c.env("PATH", &path);
        if let Some(p) = pane {
            c.env("KRAAL_PANE", p).env("KRAAL_SESSION", &env.session);
        }
        let out = c.output().unwrap();
        assert!(out.status.success(), "kraal {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    run(&["open", "w"], None);
    run(&["ssh", "deploy@web.lan", "--tab"], None);
    let panes: serde_json::Value = serde_json::from_str(&run(&["panes", "--json"], None)).unwrap();
    let pane =
        panes.as_array().unwrap().iter().find(|p| p["server"] == "web").unwrap_or_else(|| panic!("{panes}"))["id"]
            .to_string();
    // A runbook on that pane (whose ssh already ended) goes as deploy, not as the inventory's root.
    let file = env.root.join("config/kraal/runbooks/where.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "on = \"pane\"\nsteps = [\"echo here\"]\n").unwrap();
    let out = run(&["runbook", "run", "where", "--yes", "--wait"], Some(&pane));
    assert!(out.contains("finished: 1 ok"), "{out}");
    let log = std::fs::read_to_string(&log).unwrap();
    let remote = log.lines().filter(|l| l.contains("sh -c")).collect::<Vec<_>>();
    assert!(!remote.is_empty() && remote.iter().all(|l| l.contains("deploy@web.lan")), "{log}");
}

#[cfg(unix)]
#[test]
fn host_attach_delivers_copies_to_one_clients_terminal() {
    use base64::Engine;

    let local = Env::new("copylocal");
    let remote = Env::new("copyremote");
    for env in [&local, &remote] {
        let config = env.root.join("config/kraal/config.toml");
        let text = std::fs::read_to_string(&config).unwrap();
        std::fs::write(config, format!("{text}\n[clipboard]\nmode = 'terminal'\n")).unwrap();
    }
    // A noninteractive shell and no tty echo: only program output can satisfy the markers.
    remote.workspace(
        "w",
        "layout {\n    pane name=\"main\" command=\"sh\" { args \"-c\" \"stty -echo; echo SELECT-ME-PLEASE; cat | sh\"; }\n}\n",
    );
    remote.run(&["open", "w"]);
    remote.wait_for_text("main", "SELECT-ME-PLEASE");
    let (log, path) = fake_ssh(&local, &remote);
    let mut direct = TerminalClient::start(remote.cmd(&["attach", "w"]));
    let mut cmd = local.isolated(Path::new(env!("CARGO_BIN_EXE_kraal")));
    cmd.args(["-s", &remote.session, "attach", "--host", "copy.invalid", "w"]);
    cmd.env("PATH", &path);
    let mut host = TerminalClient::start(cmd);
    direct.wait_for("SELECT-ME-PLEASE");
    host.wait_for("SELECT-ME-PLEASE");
    assert!(std::fs::read_to_string(log).unwrap().contains("proxy -s copyremote"));

    let osc = |text: &str| format!("\x1b]52;c;{}\x07", base64::engine::general_purpose::STANDARD.encode(text));
    let copy = |client: &mut TerminalClient, text: &str| {
        std::thread::sleep(Duration::from_millis(150));
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        client.type_keys(format!("printf '\\033]52;c;{encoded}\\007{text}\\n'\r").as_bytes());
    };
    // Each client's input precedes the copy its program emits. The other client's drawn
    // marker is a barrier after that copy, so absence there is checked after delivery.
    copy(&mut host, "FROM-HOST");
    host.wait_for(&osc("FROM-HOST"));
    direct.wait_for("FROM-HOST");
    copy(&mut direct, "FROM-LOCAL");
    direct.wait_for(&osc("FROM-LOCAL"));
    host.wait_for("FROM-LOCAL");

    // Coordinates come from the rendered cells, not a particular sidebar width or frame.
    let rows = text_after(&host.written.lock().unwrap(), 100, 30);
    let (y, row) = rows.iter().enumerate().find(|(_, row)| row.contains("SELECT-ME-PLEASE")).unwrap();
    let x = row.split("SELECT-ME-PLEASE").next().unwrap().chars().count() + 1;
    let end = x + "SELECT-ME-PLEASE".len() - 1;
    let y = y + 1;
    std::thread::sleep(Duration::from_millis(150));
    host.type_keys(format!("\x1b[<0;{x};{y}M\x1b[<32;{end};{y}M\x1b[<0;{end};{y}m").as_bytes());
    host.wait_for(&osc("SELECT-ME-PLEASE"));
    host.wait_for("copy sent to terminal via OSC 52");
    assert_eq!(remote.run(&["buffer", "show"]), "SELECT-ME-PLEASE");
    // A later frame on both clients also bounds the check for an unwanted broadcast.
    copy(&mut direct, "LOCAL-AGAIN");
    direct.wait_for(&osc("LOCAL-AGAIN"));
    host.wait_for("LOCAL-AGAIN");
    let count = |client: &TerminalClient, sequence: &str| {
        client.written.lock().unwrap().windows(sequence.len()).filter(|w| *w == sequence.as_bytes()).count()
    };
    for (text, direct_count, host_count) in
        [("FROM-HOST", 0, 1), ("FROM-LOCAL", 1, 0), ("SELECT-ME-PLEASE", 0, 1), ("LOCAL-AGAIN", 1, 0)]
    {
        assert_eq!(count(&direct, &osc(text)), direct_count, "local client: {text}");
        assert_eq!(count(&host, &osc(text)), host_count, "host client: {text}");
    }
    assert_eq!(count(&direct, "\x1b]52;"), 2);
    assert_eq!(count(&host, "\x1b]52;"), 2);
    direct.type_keys(b"\x02d");
    host.type_keys(b"\x02d");
    assert_eq!(direct.left("local detach").0, 0);
    assert_eq!(host.left("host detach").0, 0);
}

#[cfg(unix)]
#[test]
fn host_clipboard_policy_is_local_and_native_copy_never_also_sends_osc52() {
    use base64::Engine;

    for (case, allowed, mode, limit, fails, intermediate) in [
        ("native", false, "auto", 1048576, false, false),
        ("allowed", true, "auto", 1048576, false, false),
        ("off", true, "off", 1048576, false, false),
        ("limit", true, "auto", 4, false, false),
        ("failure", true, "auto", 1048576, true, false),
        ("intermediate", true, "auto", 1048576, false, true),
        ("vscode", true, "auto", 1048576, false, true),
        ("broken", true, "off", 1048576, false, false),
        #[cfg(target_os = "linux")]
        ("x11", true, "auto", 1048576, false, false),
        #[cfg(target_os = "linux")]
        ("xsel", true, "auto", 1048576, false, false),
    ] {
        let local = Env::new(&format!("clip-{case}"));
        let remote = Env::new(&format!("clip-server-{case}"));
        remote.workspace("w", "layout {\n    pane name=\"main\" command=\"sh\" { args \"-c\" \"stty -echo; echo READY; cat | sh\"; }\n}\n");
        remote.run(&["open", "w"]);
        remote.wait_for_text("main", "READY");
        let other = RawClient::attach(&remote, "w");
        let (_, path) = fake_ssh(&local, &remote);
        let bin = local.root.join("bin");
        let native = local.root.join("clipboard");
        let calls = local.root.join("calls");
        for tool in ["pbcopy", "wl-copy", "xclip", "xsel"] {
            write_script(
                &bin.join(tool),
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {calls}\ncat > {native}\nexit {code}\n",
                    calls = calls.display(),
                    native = native.display(),
                    code = if fails || (case == "xsel" && tool == "xclip") { 1 } else { 0 }
                ),
            );
        }
        let config = local.root.join("config/kraal/config.toml");
        let text = std::fs::read_to_string(&config).unwrap();
        let invalid = if case == "broken" { "mouse = [\n" } else { "" };
        std::fs::write(config, format!("{text}\n[clipboard]\nmode = '{mode}'\nmax_bytes = {limit}\n{invalid}"))
            .unwrap();
        std::fs::write(
            local.root.join("config/kraal/servers.toml"),
            format!("[[server]]\nname = 'box'\nhost = 'copy.invalid'\nclipboard_native = {allowed}\n"),
        )
        .unwrap();
        let mut cmd = local.isolated(Path::new(env!("CARGO_BIN_EXE_kraal")));
        cmd.args(["-s", &remote.session, "attach", "--host", "box", "w"]);
        cmd.env("PATH", path).env("WAYLAND_DISPLAY", "fake-wayland").env("DISPLAY", ":42");
        if matches!(case, "x11" | "xsel") {
            cmd.env_remove("WAYLAND_DISPLAY");
        }
        for name in [
            "SSH_CONNECTION",
            "SSH_CLIENT",
            "SSH_TTY",
            "VSCODE_IPC_HOOK_CLI",
            "VSCODE_AGENT_FOLDER",
            "WSL_DISTRO_NAME",
            "WSL_INTEROP",
        ] {
            cmd.env_remove(name);
        }
        if intermediate {
            cmd.env(if case == "vscode" { "VSCODE_IPC_HOOK_CLI" } else { "SSH_CONNECTION" }, "intermediate");
        }
        let mut host = TerminalClient::start(cmd);
        host.wait_for("READY");
        let encoded = base64::engine::general_purpose::STANDARD.encode("PROGRAM-COPY");
        host.type_keys(format!("printf '\\033]52;c;{encoded}\\007PROGRAM-DONE\\n'\r").as_bytes());
        host.wait_for("PROGRAM-DONE");
        other.wait_for_screen("PROGRAM-DONE");
        let program_native = allowed && mode == "auto" && limit > 4 && !intermediate && !fails;
        let program_terminal = mode != "off" && limit > 4 && !program_native;
        if program_native {
            common::wait_until(Duration::from_secs(10), "native program copy", || {
                std::fs::read_to_string(&native).ok().as_deref() == Some("PROGRAM-COPY")
            });
        } else if program_terminal {
            host.wait_for(&format!("\x1b]52;c;{encoded}\x07"));
        } else if limit == 4 {
            host.wait_for("copy too large");
        }
        // Recopying a buffer is the user's own gesture even when a program filled it.
        std::thread::sleep(Duration::from_millis(200));
        let before = host.written.lock().unwrap().len();
        host.type_keys(b"\x02=\x19");
        let user_native = mode == "auto" && limit > 4 && !intermediate && !fails;
        let expected = if mode == "off" {
            "external copy disabled"
        } else if limit == 4 {
            "copy too large"
        } else if user_native {
            "copied to laptop clipboard"
        } else {
            "copy sent to terminal via OSC 52"
        };
        if limit == 4 {
            // The same refusal is already drawn for the program copy and need not redraw.
            host.wait_for(expected);
        } else {
            common::wait_until(Duration::from_secs(10), expected, || {
                String::from_utf8_lossy(&host.written.lock().unwrap()[before..]).contains(expected)
            });
        }
        assert_eq!(remote.run(&["buffer", "show"]), "PROGRAM-COPY", "{case}");
        let written = host.written.clone();
        host.type_keys(b"\x02d");
        assert_eq!(host.left("clipboard detach").0, 0);
        let terminal = String::from_utf8_lossy(&written.lock().unwrap()).into_owned();
        assert_eq!(
            terminal.matches("\x1b]52;").count(),
            usize::from(program_terminal) + usize::from(!user_native && mode != "off" && limit > 4),
            "{case}"
        );
        assert!(!String::from_utf8_lossy(&other.screen.lock().unwrap()).contains("\x1b]52;"), "{case}: broadcast");
        if mode == "off" || limit == 4 || intermediate {
            assert!(!calls.exists(), "{case}: native tool must not run");
        } else if !fails {
            assert_eq!(std::fs::read_to_string(native).unwrap(), "PROGRAM-COPY");
            let calls = std::fs::read_to_string(calls).unwrap();
            let attempts = if case == "xsel" { 2 } else { 1 };
            assert_eq!(calls.lines().count(), attempts * (1 + usize::from(program_native)));
            if case == "x11" {
                assert!(calls.lines().all(|args| args == "-selection clipboard -in"));
            } else if case == "xsel" {
                assert_eq!(
                    calls.lines().collect::<Vec<_>>(),
                    [
                        "-selection clipboard -in",
                        "--clipboard --input",
                        "-selection clipboard -in",
                        "--clipboard --input"
                    ]
                );
            } else {
                #[cfg(target_os = "linux")]
                assert!(calls.lines().all(|args| args == "--type text/plain;charset=utf-8"));
            }
        }
    }
}

/// A helper stopped before reading cannot write after its client has detached or been
/// signalled, even though its process group is independent of the client's.
#[cfg(target_os = "linux")]
#[test]
fn client_exit_cancels_a_native_copy_before_its_pending_write() {
    for signal in [None, Some(libc::SIGTERM), Some(libc::SIGHUP), Some(libc::SIGINT), Some(libc::SIGQUIT)] {
        let env = Env::new(&format!("copy-exit-{}", signal.unwrap_or(0)));
        env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
        env.run(&["open", "w"]);
        // More than a pipe can hold: the stopped helper cannot complete the handoff.
        assert!(set_buffer(&env, &[], &"x".repeat(1 << 20)).status.success());
        let bin = env.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let pidfile = env.root.join("helper-pid");
        let copied = env.root.join("copied");
        write_script(
            &bin.join("wl-copy"),
            &format!("#!/bin/sh\necho $$ > {}\nkill -STOP $$\ncat > {}\n", pidfile.display(), copied.display()),
        );
        let mut cmd = env.cmd(&["attach", "w"]);
        cmd.env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()))
            .env("WAYLAND_DISPLAY", "fake-wayland");
        for name in [
            "SSH_CONNECTION",
            "SSH_CLIENT",
            "SSH_TTY",
            "VSCODE_AGENT_FOLDER",
            "VSCODE_IPC_HOOK_CLI",
            "WSL_DISTRO_NAME",
            "WSL_INTEROP",
            "DISPLAY",
        ] {
            cmd.env_remove(name);
        }
        let mut client = TerminalClient::start(cmd);
        client.type_keys(b"\x02=\x19");
        wait_until(PATIENCE, "clipboard helper paused", || common::read_complete_lines(&pidfile).is_some());
        let pid: libc::pid_t = crate::common::read_complete_lines(&pidfile).unwrap().trim().parse().unwrap();
        // Waiting for the stopped state makes the pause deterministic.
        wait_until(PATIENCE, "clipboard helper stopped", || {
            std::fs::read_to_string(format!("/proc/{pid}/status"))
                .is_ok_and(|s| s.lines().any(|line| line.starts_with("State:") && line.contains("T (stopped)")))
        });
        if let Some(signal) = signal {
            client.signal(signal);
        } else {
            client.type_keys(b"\x02d");
        }
        assert_eq!(client.left("pending clipboard copy").0, signal.map_or(0, |s| (128 + s) as u32));
        // SIGKILL may still be pending until the kernel schedules the helper. It may
        // briefly remain stopped, but it must disappear or become a zombie without writing.
        let running = || {
            std::fs::read_to_string(format!("/proc/{pid}/status"))
                .is_ok_and(|s| !s.lines().any(|line| line.starts_with("State:") && line.contains("Z (zombie)")))
        };
        let start = Instant::now();
        while running() && start.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let alive = running();
        if alive {
            // SAFETY: the paused helper this test started still owns its pid and group.
            unsafe {
                libc::kill(-pid, libc::SIGCONT);
            }
            let start = Instant::now();
            while !copied.exists() && running() && start.elapsed() < Duration::from_secs(1) {
                std::thread::sleep(Duration::from_millis(5));
            }
            if running() {
                // SAFETY: this is still the helper whose pending write we resumed.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
        }
        assert!(!alive, "unfinished clipboard helper survived client exit");
        assert!(!copied.exists(), "clipboard was overwritten after client exit");
    }
}

#[test]
fn panes_keep_the_servers_display_environment_and_native_copy_stays_there() {
    let env = Env::new("copydisplay");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    let out = env.cmd(&["open", "w"]).env("DISPLAY", ":42").env("WAYLAND_DISPLAY", "wayland-test").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let client = RawClient::attach(&env, "w");
    let clipboard = env.root.join("server-clipboard");
    let native = env.root.join("native-copy");
    // Stand in for a native clipboard tool without touching any graphical session.
    write_script(&native, &format!("#!/bin/sh\ncat > {}\n", clipboard.display()));
    // Keep the screen marker first so a narrow split cannot wrap it between terminal sequences.
    let script = format!(
        "printf 'server-only' | {}; printf 'NATIVE-DONE DISPLAY=%s WAYLAND_DISPLAY=%s\\n' \"$DISPLAY\" \"$WAYLAND_DISPLAY\"; exec sleep 60",
        native.display()
    );
    let out = env
        .cmd(&["run", "--name", "copy", "--", "sh", "-c", &script])
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    client.wait_for_screen("NATIVE-DONE");
    env.wait_for_text("copy", "NATIVE-DONE DISPLAY=:42 WAYLAND_DISPLAY=wayland-test");
    assert_eq!(std::fs::read_to_string(clipboard).unwrap(), "server-only");
    assert!(buffer_list(&env).is_empty());
    assert!(!String::from_utf8_lossy(&client.screen.lock().unwrap()).contains("\x1b]52;"));
}

#[cfg(unix)]
#[test]
fn agents_of_a_remote_session_are_listed_and_reached() {
    let local = Env::new("rlocal");
    let remote = Env::new("rremote");
    // The server's kraal: an agent in a tab that is not the one shown.
    remote.workspace("w", "layout {\n    pane name=\"api\"\n}\n");
    remote.run(&["open", "w"]);
    remote.run(&["run", "--tab", "-n", "other", "--", "sh"]);
    claude_hook(&remote, "api", r#"{"hook_event_name":"SessionStart","session_id":"s-1"}"#);
    claude_hook(&remote, "api", r#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1"}"#);
    let remote_api = || -> serde_json::Value {
        let panes: serde_json::Value = serde_json::from_str(&remote.run(&["panes", "--json"])).unwrap();
        panes.as_array().unwrap().iter().find(|p| p["name"] == "api").unwrap().clone()
    };
    let api = remote_api()["id"].to_string();
    assert_eq!(remote_api()["focused"], false);

    let (log, path) = fake_ssh(&local, &remote);
    let inventory = local.root.join("config/kraal/servers.toml");
    std::fs::write(
        &inventory,
        "[[server]]\nname = \"gpu\"\nhost = \"gpu.lan\"\nagents = \"rremote\"\n\n\
         [[server]]\nname = \"down\"\nhost = \"down.lan\"\nagents = \"main\"\n\n\
         [[server]]\nname = \"gpu2\"\nhost = \"gpu2.lan\"\n",
    )
    .unwrap();
    local.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    let out = local.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let local_panes = || -> Vec<serde_json::Value> {
        let panes: serde_json::Value = serde_json::from_str(&local.run(&["panes", "--json"])).unwrap();
        panes.as_array().unwrap().clone()
    };
    let ssh_log = || std::fs::read_to_string(&log).unwrap_or_default();

    // The sidebar lists the agent after its server's name. The server that does not answer is
    // said once, and tried again later, not at once.
    let mut client = RawClient::attach(&local, "w");
    client.wait_for_screen("gpu › ");
    client.wait_for_screen("working · claude · w");
    client.wait_for_screen("the agents of down cannot be listed");
    std::thread::sleep(Duration::from_secs(3));
    let tries = ssh_log().matches("down.lan").count();
    assert!(tries <= 2, "a server that is down is not hammered: {tries} tries in about 3 s");

    // `kraal agents` lists them too, after the server's name, with their id there.
    let listed = local.run(&["agents"]);
    assert!(listed.lines().any(|l| l.contains("gpu › w") && l.contains("api") && l.contains("working")), "{listed}");
    let agents: serde_json::Value = serde_json::from_str(&local.run(&["agents", "--json"])).unwrap();
    let there = agents.as_array().unwrap().iter().find(|a| a["server"] == "gpu").cloned().unwrap();
    assert_eq!((there["session"].as_str(), there["kind"].as_str()), (Some("rremote"), Some("remote")), "{there}");
    assert_eq!((there["id"].to_string(), there["agent"].as_str()), (api.clone(), Some("claude")), "{there}");
    // What another kraal watching this one reads lists this session's own agents only.
    let (mut follow, lines) = local.follow(&["agents", "--follow"]);
    assert_eq!(lines.recv_timeout(Duration::from_secs(10)).expect("a list").trim(), "[]");
    let _ = follow.kill();
    let _ = follow.wait();

    // Blocked there: told and announced here, as a local agent is.
    let (mut events, notified) = local.follow(&["events", "notification", "agent.*"]);
    claude_hook(&remote, "api", r#"{"hook_event_name":"Notification","message":"Claude needs your permission"}"#);
    let mut seen = std::collections::HashMap::new();
    while seen.len() < 2 {
        let line = notified.recv_timeout(Duration::from_secs(10)).expect("an event");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        seen.insert(v["type"].as_str().unwrap().to_string(), v);
    }
    let v = &seen["notification"];
    let said = (v["title"].as_str(), v["body"].as_str());
    assert_eq!(said, (Some("claude · api on gpu"), Some("needs your input")), "{v}");
    let v = &seen["agent.blocked"];
    assert_eq!(
        (v["server"].as_str(), v["session"].as_str(), v["pane"].to_string()),
        (Some("gpu"), Some("rremote"), api.clone())
    );
    assert_eq!(
        (v["name"].as_str(), v["agent"].as_str(), v["previous"].as_str()),
        (Some("api"), Some("claude"), Some("working"))
    );
    let _ = events.kill();
    let _ = events.wait();
    client.wait_for_screen("blocked · claude · w");

    // alt+a: the next agent needing you is there; a tab attaches to that session, its pane shown.
    client.send(b"\x1ba");
    let attach = |p: &serde_json::Value| {
        let cmd: Vec<&str> = p["command"].as_array().unwrap().iter().filter_map(|a| a.as_str()).collect();
        cmd.get(1..).is_some_and(|a| a == ["-s", "rremote", "attach", "--host", "gpu", "--pane", api.as_str()])
    };
    let start = Instant::now();
    while !local_panes().iter().any(attach) {
        assert!(start.elapsed() < Duration::from_secs(10), "no tab attached: {:?}", local_panes());
        std::thread::sleep(Duration::from_millis(50));
    }
    let start = Instant::now();
    while remote_api()["focused"] != true {
        assert!(start.elapsed() < Duration::from_secs(10), "the agent's pane is not shown there: {}", remote_api());
        std::thread::sleep(Duration::from_millis(50));
    }
    // The tab shows another pane there; again: the tab already attached is used, and the
    // client in it asked to show the agent's pane.
    remote.run(&["focus", "other"]);
    let start = Instant::now();
    while remote_api()["focused"] != false {
        assert!(start.elapsed() < Duration::from_secs(10), "{}", remote_api());
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(500));
    client.send(b"\x02Vl");
    let rows = client.wait_for_rows("the remote agent in the strip", |rows| {
        rows.iter().position(|r| r.starts_with("AG │")).is_some_and(|y| rows[y + 1].starts_with(" 1●│"))
    });
    let row = rows.iter().position(|r| r.starts_with("AG │")).unwrap() + 2;
    client.send(format!("\x1b[<0;2;{row}M\x1b[<0;2;{row}m").as_bytes());
    let start = Instant::now();
    while remote_api()["focused"] != true {
        assert!(start.elapsed() < Duration::from_secs(10), "the agent's pane is not shown again: {}", remote_api());
        std::thread::sleep(Duration::from_millis(50));
    }
    client.send(b"\x02Vl");
    client.wait_for_row("WORKSPACES");
    assert!(!ssh_log().contains(" focus "), "asked through the tab, not over another connection: {}", ssh_log());
    assert_eq!(local_panes().len(), 2, "{:?}", local_panes());

    // Attaching to a server makes kraal list that session's agents from then on.
    let kraal = env!("CARGO_BIN_EXE_kraal");
    local.run(&[
        "run",
        "-n",
        "gpu2",
        "--",
        "env",
        "-u",
        "KRAAL_PANE",
        kraal,
        "-s",
        "rremote",
        "attach",
        "--host",
        "gpu2",
    ]);
    let start = Instant::now();
    while std::fs::read_to_string(&inventory).unwrap().matches("agents = \"rremote\"").count() < 2 {
        assert!(start.elapsed() < Duration::from_secs(10), "{}", std::fs::read_to_string(&inventory).unwrap());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(std::fs::read_to_string(&inventory).unwrap().contains("agents = \"main\""), "the others stay as written");

    // A new remote connection is offered for explicit addition, never silently saved.
    let before_unknown = std::fs::read_to_string(&inventory).unwrap();
    local.run(&[
        "run",
        "-n",
        "unsaved",
        "--",
        "env",
        "-u",
        "KRAAL_PANE",
        kraal,
        "-s",
        "rremote",
        "attach",
        "--host",
        "unsaved.lan",
    ]);
    local.wait_for_text("unsaved", "api");
    assert_eq!(std::fs::read_to_string(&inventory).unwrap(), before_unknown);

    // A configuration that cannot be read does not say that servers may be added: none is,
    // though the attach works.
    let broken = local.root.join("broken");
    std::fs::create_dir_all(broken.join("kraal")).unwrap();
    std::fs::copy(&inventory, broken.join("kraal/servers.toml")).unwrap();
    std::fs::write(broken.join("kraal/config.toml"), "[servers]\nauto_add = false\nmouse = [\n").unwrap();
    let before = std::fs::read_to_string(broken.join("kraal/servers.toml")).unwrap();
    let config = format!("XDG_CONFIG_HOME={}", broken.display());
    let args = ["run", "-n", "new", "--", "env", "-u", "KRAAL_PANE", &config, kraal, "-s", "rremote", "attach"];
    local.run(&[&args[..], &["--host", "new.lan"]].concat());
    local.wait_for_text("new", "api");
    std::thread::sleep(Duration::from_millis(500));
    assert!(ssh_log().contains("new.lan"));
    assert_eq!(std::fs::read_to_string(broken.join("kraal/servers.toml")).unwrap(), before);
    drop(client);
}

/// A tab attached to a server's session by hand is the one used, and each agent waiting there is
/// shown in it in turn: in the tab, whatever another terminal attached to that session shows.
#[cfg(unix)]
#[test]
fn a_tab_attached_to_a_server_shows_each_agent_there_in_turn() {
    let local = Env::new("tlocal");
    let remote = Env::new("tremote");
    // Two workspaces there, an agent shown in each, both waiting for an answer.
    remote.workspace("one", "layout {\n    pane name=\"api\"\n}\n");
    remote.workspace("two", "layout {\n    pane name=\"web\"\n}\n");
    remote.run(&["open", "one"]);
    remote.run(&["open", "two"]);
    for (pane, n, word) in [("api", 1, "ONE"), ("web", 2, "TWO")] {
        remote.run(&["send", "-p", pane, "--enter", &format!("echo IN-$(({n}))-{word}")]);
        remote.wait_for_text(pane, &format!("IN-{n}-{word}"));
        for event in ["SessionStart", "UserPromptSubmit", "Notification"] {
            let payload = format!(
                r#"{{"hook_event_name":"{event}","session_id":"s-{n}","message":"Claude needs your permission"}}"#
            );
            claude_hook(&remote, pane, &payload);
        }
    }
    let (_log, path) = fake_ssh(&local, &remote);
    std::fs::write(
        local.root.join("config/kraal/servers.toml"),
        "[[server]]\nname = \"gpu\"\nhost = \"gpu.lan\"\nagents = \"tremote\"\n",
    )
    .unwrap();
    local.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    let out = local.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // Attached by hand, with a command line of its own.
    let kraal = env!("CARGO_BIN_EXE_kraal");
    local.run(&[
        "run",
        "--tab",
        "-n",
        "by-hand",
        "--",
        "env",
        "-u",
        "KRAAL_PANE",
        kraal,
        "-s",
        "tremote",
        "attach",
        "--host",
        "gpu",
    ]);
    local.wait_for_text("by-hand", "IN-1-ONE");
    let mut client = RawClient::attach(&local, "w");
    client.wait_for_screen("gpu › ");
    client.wait_for_screen("blocked · claude · two");
    // Another terminal attached there, used last: what that session is asked without saying
    // which client goes to that one.
    let mut other = RawClient::attach(&remote, "one");
    std::thread::sleep(Duration::from_millis(500));
    let shown = |word: &str, not: &str| {
        let start = Instant::now();
        loop {
            let screen = local.run(&["read", "-p", "by-hand"]);
            if screen.contains(word) && !screen.contains(not) {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "{word} is not what the tab shows:\n{screen}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    // The client here looks at that tab (opened last, it is the one its workspace shows), which
    // shows the agent of `one`: that one is seen already, so alt+a goes to the other, then back.
    // Each time, the tab must come to show what it did not show before alt+a.
    let panes: serde_json::Value = serde_json::from_str(&local.run(&["panes", "--json"])).unwrap();
    let tab = panes.as_array().unwrap().iter().find(|p| p["name"] == "by-hand").cloned().unwrap();
    assert_eq!(tab["focused"], true, "{tab}");
    for (word, not) in [("IN-2-TWO", "IN-1-ONE"), ("IN-1-ONE", "IN-2-TWO"), ("IN-2-TWO", "IN-1-ONE")] {
        shown(not, word);
        // The terminal there gained the focus: it is the one used last there.
        other.send(b"\x1b[I");
        std::thread::sleep(Duration::from_millis(200));
        client.send(b"\x1ba");
        shown(word, not);
    }
    let panes: serde_json::Value = serde_json::from_str(&local.run(&["panes", "--json"])).unwrap();
    assert_eq!(panes.as_array().unwrap().len(), 2, "no other tab attached: {panes}");
}

/// Successful saves retain only the previous versions, with no locks or staged new files.
#[cfg(target_os = "linux")]
fn assert_remote_previous(dir: &Path, name: &str, expected: &[&[u8]]) {
    let entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
    assert_eq!(entries.len(), expected.len() + 1, "only the file and its previous versions remain");
    let mut contents = Vec::new();
    for entry in entries {
        if entry.file_name() == name {
            assert!(entry.file_type().unwrap().is_file());
            continue;
        }
        let filename = entry.file_name();
        let filename = filename.to_string_lossy();
        assert!(filename.starts_with(".kraal-edit-") && !filename.starts_with(".kraal-edit-lock-"), "{filename}");
        assert!(entry.file_type().unwrap().is_dir());
        let recovery = entry.path();
        assert_eq!(std::fs::read_dir(&recovery).unwrap().count(), 1, "only the old version is retained");
        assert_eq!(std::fs::read_dir(recovery.join("old")).unwrap().count(), 1);
        contents.push(std::fs::read(recovery.join("old").join(name)).unwrap());
    }
    contents.sort();
    let mut expected = expected.to_vec();
    expected.sort();
    assert_eq!(contents, expected);
}

/// A file of a server opens in the local editor, each save goes back to the server (its mode
/// kept), a change made on the server meanwhile is never overwritten unasked, and each answer
/// does what it says. The server is a real sshd; the editor is a script told what to do.
#[cfg(target_os = "linux")]
#[test]
fn a_server_file_is_edited_in_the_local_editor() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("redit");
    let Some(sshd) = Sshd::start(&env.root.join("sshd")) else {
        eprintln!("skipped: no sshd can run here");
        return;
    };
    let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
    let entry = |name: &str| {
        format!(
            "[[server]]\nname = \"{name}\"\nhost = \"127.0.0.1\"\nport = {}\nuser = \"{}\"\n",
            sshd.port,
            user.trim()
        )
    };
    std::fs::write(env.root.join("config/kraal/servers.toml"), format!("{}\n{}", entry("box"), entry("box-prod")))
        .unwrap();
    let downloads = env.root.join("downloads");
    let cfg = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!("\n[servers]\nmultiplex = false\ndownload_dir = \"{}\"\n", downloads.display()));
    std::fs::write(&cfg, text).unwrap();
    // The editor says which file it edits, runs each step it is given (`$f` is the file), and
    // ends when told.
    let root = env.root.display().to_string();
    let editor = env.root.join("editor");
    write_script(
        &editor,
        &format!(
            "#!/bin/sh\nfor f; do :; done\nprintf '%s' \"$f\" > '{root}/editing.tmp' && mv '{root}/editing.tmp' '{root}/editing'\n\
             n=0\nwhile :; do\n  if [ -e '{root}/step'$n ]; then . '{root}/step'$n; n=$((n + 1)); fi\n  \
             if [ -e '{root}/quit' ]; then rm -f '{root}/quit' '{root}/editing' '{root}/step'*; exit 0; fi\n  sleep 0.05\ndone\n"
        ),
    );
    let path = format!("{}:{}", sshd.bin.display(), std::env::var("PATH").unwrap_or_default());
    let kraal =
        |args: &[&str]| env.cmd(args).env("PATH", &path).env("EDITOR", &editor).env_remove("VISUAL").output().unwrap();
    let wait_until = |what: &str, ok: &dyn Fn() -> bool| {
        let start = Instant::now();
        while !ok() {
            assert!(start.elapsed() < PATIENCE, "timed out waiting: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    let step = |n: u32, script: &str| {
        let tmp = env.root.join("step.tmp");
        std::fs::write(&tmp, script).unwrap();
        std::fs::rename(&tmp, env.root.join(format!("step{n}"))).unwrap();
    };
    // Open `target`: the editor's copy.
    let open = |target: &str, extra: &[&str]| -> PathBuf {
        let mut args = vec!["edit"];
        args.extend_from_slice(extra);
        args.push(target);
        let out = kraal(&args);
        assert!(out.status.success(), "kraal edit {target}: {}", String::from_utf8_lossy(&out.stderr));
        let editing = env.root.join("editing");
        wait_until("the editor starts", &|| editing.exists());
        PathBuf::from(read(&editing))
    };
    // End the editor: its private copy goes.
    let quit = |local: &Path| {
        std::fs::write(env.root.join("quit"), "").unwrap();
        let dir = local.parent().unwrap().to_path_buf();
        wait_until("the private copy is removed", &|| !dir.exists());
    };

    env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
    let out = kraal(&["open", "w"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut client = RawClient::attach(&env, "w");
    std::thread::sleep(Duration::from_millis(500));
    let srv = env.root.join("srv").join("it's a \"dir\" $(touch pwned)");
    std::fs::create_dir_all(&srv).unwrap();

    // Each save goes to the server, through a new file renamed in place with the old one's mode.
    let notes = srv.join("notes 'q' $HOME;.conf");
    std::fs::write(&notes, "hello\n").unwrap();
    std::fs::set_permissions(&notes, std::fs::Permissions::from_mode(0o640)).unwrap();
    let local = open(&format!("box:{}", notes.display()), &[]);
    assert!(local.starts_with(env.root.join("run")), "the copy is in the private runtime directory: {local:?}");
    assert_eq!(read(&local), "hello\n");
    assert_eq!(mode(&local), 0o600);
    assert_eq!(mode(local.parent().unwrap()), 0o700);
    step(0, "printf 'added\\n' >> \"$f\"");
    wait_until("the first save reaches the server", &|| read(&notes) == "hello\nadded\n");
    assert_eq!(mode(&notes), 0o640, "the mode is kept");
    client.wait_for_screen("saved to box");
    step(1, "printf 'more\\n' >> \"$f\"");
    wait_until("the second save reaches the server", &|| read(&notes) == "hello\nadded\nmore\n");
    quit(&local);
    assert_remote_previous(&srv, "notes 'q' $HOME;.conf", &[b"hello\n", b"hello\nadded\n"]);
    assert!(!env.root.join("srv/pwned").exists());

    // Changed on the server meanwhile: nothing is sent; kept as a local file, later saves go there.
    let file = srv.join("conflict.txt");
    std::fs::write(&file, "v1\n").unwrap();
    let local = open(&format!("box:{}", file.display()), &[]);
    std::fs::write(&file, "theirs\n").unwrap();
    client.screen.lock().unwrap().clear();
    step(0, "printf 'mine\\n' > \"$f\"");
    client.wait_for_screen("Keep your version as a local file");
    assert_eq!(read(&file), "theirs\n");
    client.send(b"\r");
    let kept = downloads.join("conflict.txt");
    wait_until("the copy is kept", &|| read(&kept) == "mine\n");
    assert_eq!(mode(&kept), 0o600);
    step(1, "printf 'mine2\\n' > \"$f\"");
    wait_until("the next save goes to the kept file", &|| read(&kept) == "mine2\n");
    quit(&local);
    assert_eq!(read(&file), "theirs\n", "the server's change stays");

    // Replaced, once asked.
    let file = srv.join("over.txt");
    std::fs::write(&file, "v1\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o604)).unwrap();
    let local = open(&format!("box:{}", file.display()), &[]);
    std::fs::write(&file, "theirs\n").unwrap();
    client.screen.lock().unwrap().clear();
    step(0, "printf 'mine\\n' > \"$f\"");
    client.wait_for_screen("Replace it with your version");
    // Typing does not answer it.
    client.send(b"replace");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(read(&file), "theirs\n");
    for key in [&b"\x1b[B"[..], b"\x1b[B", b"\r"] {
        client.send(key);
        std::thread::sleep(Duration::from_millis(100));
    }
    wait_until("the server's version is replaced", &|| read(&file) == "mine\n");
    assert_eq!(mode(&file), 0o604);
    quit(&local);

    // The server's version kept: the editor's copy becomes it, and nothing is sent.
    let file = srv.join("theirs.txt");
    std::fs::write(&file, "v1\n").unwrap();
    let local = open(&format!("box:{}", file.display()), &[]);
    std::fs::write(&file, "theirs\n").unwrap();
    client.screen.lock().unwrap().clear();
    step(0, "printf 'mine\\n' > \"$f\"");
    client.wait_for_screen("Keep the version of box");
    client.send(b"\x1b[B");
    std::thread::sleep(Duration::from_millis(100));
    client.send(b"\r");
    wait_until("the editor's copy is the server's version", &|| read(&local) == "theirs\n");
    quit(&local);
    assert_eq!(read(&file), "theirs\n");
    assert!(!downloads.join("theirs.txt").exists());

    // Production asks first: from the command line, --production says yes.
    let file = srv.join("prod.txt");
    std::fs::write(&file, "v1\n").unwrap();
    let target = format!("box-prod:{}", file.display());
    let refused = kraal(&["edit", &target]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("production"));
    let local = open(&target, &["--production", "--line", "1"]);
    step(0, "printf 'v2\\n' > \"$f\"");
    wait_until("a production file is saved once allowed", &|| read(&file) == "v2\n");
    quit(&local);

    // From the file manager: `e` on a server's file, asked first on a production server.
    let dir = env.root.join("fm");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("fm.txt");
    std::fs::write(&file, "v1\n").unwrap();
    client.screen.lock().unwrap().clear();
    let out = kraal(&["files", &env.root.display().to_string(), &format!("box-prod:{}", dir.display())]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    client.wait_for_screen("fm.txt");
    for key in [&b"\t"[..], b"\x1b[B", b"e"] {
        client.send(key);
        std::thread::sleep(Duration::from_millis(150));
    }
    client.wait_for_row("It is production");
    client.wait_for_row("Each save in the editor goes to the server.");
    client.wait_for_row("Enter Cancel");
    assert!(!env.root.join("editing").exists(), "nothing opens before the answer");
    client.send(b"y");
    let editing = env.root.join("editing");
    wait_until("the editor starts", &|| editing.exists());
    let local = PathBuf::from(read(&editing));
    step(0, "printf 'v2\\n' > \"$f\"");
    wait_until("a save from the file manager's editor", &|| read(&file) == "v2\n");
    quit(&local);

    // What cannot be edited is said.
    for (target, why) in [
        (format!("box:{}", srv.display()), "directory"),
        (format!("box:{}", env.root.join("nope").display()), "no such file"),
        (env.root.join("editor").display().to_string(), "not on a server"),
    ] {
        let out = kraal(&["edit", &target]);
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(why), "{target}: {err}");
    }
    drop(sshd);
}

/// `kraal edit --sudo`: a pane asks for the password, sudo gets it, and each save is written as
/// root, through the same checks; a wrong password opens nothing, and a file one may not write
/// is refused with the way to edit it.
#[cfg(target_os = "linux")]
#[test]
fn a_server_file_is_edited_as_root_through_sudo() {
    use std::os::unix::fs::PermissionsExt;
    let Some(b) = EditBench::new("rsudo") else {
        eprintln!("skipped: no sshd can run here");
        return;
    };
    let mut client = RawClient::attach(&b.env, "w");
    std::thread::sleep(Duration::from_millis(500));
    // Named like the FIFO the password goes through, in the same private directory.
    let file = b.srv.join("sudo");
    std::fs::write(&file, "v1\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
    let target = format!("box:{}", file.display());
    let piped = std::process::Stdio::piped;
    // Its pane says so (once it is there).
    let asked = || {
        let out = b.env.try_run(&["read", "-p", "sudo · box"]);
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("password on box")
    };
    let cli = b.kraal(&["edit", "--sudo", &target]).stdout(piped()).stderr(piped()).spawn().unwrap();
    b.wait_until("the password is asked in its pane", &asked);
    assert!(!b.env.root.join("editing").exists(), "nothing opens before the password");
    client.send(b"pass word\r");
    let local = b.editing();
    let out = cli.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(read(&local), "v1\n");
    b.step(0, "printf 'v2\\n' > \"$f\"");
    b.wait_until("the save reaches the server", &|| read(&file) == "v2\n");
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777, 0o640, "the mode is kept");
    b.step(1, "printf 'v3\\n' > \"$f\"");
    b.wait_until("the next save, through the same sudo", &|| read(&file) == "v3\n");
    b.quit(&local);
    assert_remote_previous(&b.srv, "sudo", &[b"v1\n", b"v2\n"]);

    // A wrong password: refused, nothing opens.
    let cli = b.kraal(&["edit", "--sudo", &target]).stdout(piped()).stderr(piped()).spawn().unwrap();
    b.wait_until("the password is asked in its pane", &asked);
    client.send(b"wrong\r");
    let out = cli.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("sudo refused"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!b.env.root.join("editing").exists());

    // Without --sudo, a file one may not write is refused, saying how to edit it.
    let ro = b.srv.join("ro.conf");
    std::fs::write(&ro, "x\n").unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o444)).unwrap();
    let out = b.kraal(&["edit", &format!("box:{}", ro.display())]).output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("kraal edit --sudo"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// "Keep the version of the server" while the editor saves again before that version arrived:
/// the new save is not overwritten, and the question comes back.
#[cfg(target_os = "linux")]
#[test]
fn keeping_the_servers_version_never_drops_a_later_save() {
    let Some(b) = EditBench::new("rkeep") else {
        eprintln!("skipped: no sshd can run here");
        return;
    };
    let mut client = RawClient::attach(&b.env, "w");
    std::thread::sleep(Duration::from_millis(500));
    let file = b.srv.join("theirs.txt");
    std::fs::write(&file, "v1\n").unwrap();
    let out = b.kraal(&["edit", &format!("box:{}", file.display())]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let local = b.editing();
    std::fs::write(&file, "theirs\n").unwrap();
    client.screen.lock().unwrap().clear();
    b.step(0, "printf 'mine\\n' > \"$f\"");
    client.wait_for_screen("Keep the version of box");
    // Reading the server's version takes a while; the user saves again meanwhile.
    std::fs::write(b.env.root.join("slow"), "").unwrap();
    client.send(b"\x1b[B");
    std::thread::sleep(Duration::from_millis(100));
    client.screen.lock().unwrap().clear();
    client.send(b"\r");
    b.step(1, "printf 'mine2\\n' > \"$f\"");
    b.wait_until("the second save", &|| read(&local) == "mine2\n");
    // Once the server's version arrived: asked again, the later save still there.
    client.wait_for_screen("Keep your version as a local file");
    assert_eq!(read(&local), "mine2\n", "the later save is still there");
    client.send(b"\r");
    let kept = b.env.root.join("downloads/theirs.txt");
    b.wait_until("the later save is kept", &|| read(&kept) == "mine2\n");
    assert_eq!(read(&file), "theirs\n");
    b.quit(&local);
}

#[test]
fn servers_add_saves_before_connecting_and_a_failed_connection_keeps_the_entry() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("server-form-{}", size.0));
        let bin = env.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = env.root.join("ssh.log");
        write_script(
            &bin.join("ssh"),
            &format!("#!/bin/sh\necho attempt >> '{}'\necho 'Connection refused' >&2\nexit 255\n", log.display()),
        );
        env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let out = env.cmd(&["open", "w"]).env("PATH", path).output().unwrap();
        assert!(out.status.success());
        let mut client = RawClient::attach_sized(&env, Some("w"), size);
        client.send(b"\x02s");
        click_account_text(&mut client, "+ Add server");
        client.wait_for_row("Address");
        client.send(b"admin@down.lan\r");
        client.wait_for_row("Server added");
        assert!(!log.exists(), "saving never invokes SSH");
        let inventory = env.root.join("config/kraal/servers.toml");
        let saved = std::fs::read_to_string(&inventory).unwrap();
        assert!(saved.contains("name = \"down\""));
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        assert_eq!(panes.as_array().unwrap().len(), 1);
        client.send(b"\r");
        client.wait_for_row("The server is saved.");
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
        click_account_text(&mut client, "Try again");
        common::wait_until(PATIENCE, "second SSH attempt", || {
            std::fs::read_to_string(&log).unwrap().lines().count() == 2
        });
        client.wait_for_row("The server is saved.");
        client.send(b"\r");
        common::wait_until(PATIENCE, "failed SSH pane closed", || {
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            panes.as_array().unwrap().len() == 1
        });
        assert_eq!(std::fs::read_to_string(&inventory).unwrap(), saved);
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 2, "Enter defaults to Close");
    }
}

#[test]
fn saving_beside_a_watched_server_does_not_start_another_ssh() {
    let env = Env::new("server-form-watched");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = env.root.join("ssh.log");
    write_script(
        &bin.join("ssh"),
        &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec sleep 60\n", log.display()),
    );
    let inventory = env.root.join("config/kraal/servers.toml");
    let original = "[[server]]\nname = \"box\"\nhost = \"box.lan\"\nagents = \"main\"\n";
    std::fs::write(&inventory, original).unwrap();
    env.workspace("w", "layout { pane; }\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let out = env.cmd(&["open", "w"]).env("PATH", path).output().unwrap();
    assert!(out.status.success());
    common::wait_until(PATIENCE, "the saved server's agent watch started", || {
        common::read_complete_lines(&log).is_some_and(|text| text.lines().count() == 1)
    });
    let before = std::fs::read_to_string(&log).unwrap();
    assert_eq!(before.lines().count(), 1);
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24));
    client.send(b"\x02Sn");
    client.wait_for_row("Address");
    client.send(b"box.lan:22\t\x15box22\r");
    client.wait_for_row("Server added");
    client.settle();
    assert_eq!(std::fs::read_to_string(&log).unwrap(), before, "saving must not start another SSH");
    let saved = std::fs::read_to_string(&inventory).unwrap();
    assert!(saved.starts_with(original));
    let parsed: toml::Table = toml::from_str(&saved).unwrap();
    let entries = parsed["server"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1]["name"].as_str(), Some("box22"));
    assert_eq!(entries[1]["host"].as_str(), Some("box.lan"));
    assert_eq!(entries[1]["port"].as_integer(), Some(22));
    assert!(entries[1].get("agents").is_none());
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    assert_eq!(panes.as_array().unwrap().len(), 1);
}

#[test]
fn server_form_save_error_retries_without_losing_fields_and_concurrent_adds_do_not_duplicate() {
    let env = Env::new("server-form-retry");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let inventory = env.root.join("config/kraal/servers.toml");
    let broken = "# Keep this file\n[[server]\n";
    std::fs::write(&inventory, broken).unwrap();
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24));
    client.send(b"\x02Sn");
    client.wait_for_row("Address");
    client.send(b"admin@db.lan:2222\t\x15database\r");
    client.wait_for_row("Couldn't save server.");
    client.wait_for_row("admin@db.lan:2222");
    client.wait_for_row("database");
    assert_eq!(std::fs::read_to_string(&inventory).unwrap(), broken);
    let original = "# Keep this file\n[[server]]\nname = \"web\"\nhost = \"web.lan\"\ncustom = \"kept\"\n";
    std::fs::write(&inventory, original).unwrap();
    let mut other = RawClient::attach_sized(&env, Some("w"), (124, 36));
    other.send(b"\x02Sn");
    other.wait_for_row("Address");
    other.send(b"admin@db.lan:2222\t\x15database");
    client.send(b"\r");
    other.send(b"\r");
    for client in [&mut client, &mut other] {
        client.wait_for_rows("save result", |rows| {
            rows.iter().any(|r| r.contains("Server added") || r.contains("Server already saved"))
        });
    }
    let saved = std::fs::read_to_string(&inventory).unwrap();
    assert!(saved.starts_with(original));
    assert_eq!(saved.matches("name = \"database\"").count(), 1);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    assert_eq!(panes.as_array().unwrap().len(), 1);
    client.send(b"\x15nothing-matches");
    client.wait_for_row("+ Add server");
    client.send(b"\r");
    client.wait_for_row("Address   nothing-matches");
    client.send(b"\x1b");
    client.wait_for_row("nothing-matches");
    assert_eq!(std::fs::read_to_string(&inventory).unwrap(), saved, "cancel does not save");
}

#[test]
fn tunnel_form_shows_the_port_and_live_results_can_retry_and_stop() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("tunnel-form-{}", size.0));
        let bin = env.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(
            &bin.join("ssh"),
            "#!/bin/sh\n\
             prev=; for a; do [ \"$prev\" = -L ] && fwd=$a; prev=$a; done\n\
             echo started >> \"$HOME/tunnel-starts\"\n\
             while [ ! -f \"$HOME/tunnel-release\" ]; do sleep 0.05; done\n\
             if [ ! -f \"$HOME/tunnel-ready\" ]; then echo 'Cannot reach db' >&2; exit 255; fi\n\
             port=${fwd#127.0.0.1:}; port=${port%%:*}\n\
             echo \"debug1: Local forwarding listening on 127.0.0.1 port $port.\" >&2\n\
             echo 'debug1: channel 0: new port-listener [port listener]' >&2\n\
             exec sleep 600\n",
        );
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let config = env.root.join("config/kraal/config.toml");
        let text = std::fs::read_to_string(&config).unwrap();
        std::fs::write(&config, format!("{text}[sidebar]\nvisible = false\n")).unwrap();
        std::fs::write(env.root.join("config/kraal/servers.toml"), "[[server]]\nname = \"db\"\nhost = \"db.lan\"\n")
            .unwrap();
        env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
        let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let mut client = RawClient::attach_sized(&env, Some("w"), size);
        client.send(b"\x02So");
        client.wait_for_row("+ Add tunnel");
        click_account_text(&mut client, "+ Add tunnel");
        client.wait_for_row("Server    Name or user@host");
        client.close_overlay("Server    Name or user@host");
        client.close_overlay("+ Add tunnel");
        client.send(b"\x02sdb\x0f");
        client.wait_for_row("Server    db");
        let busy = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = busy.local_addr().unwrap().port();
        client.send(format!("localhost:{port}").as_bytes());
        client.wait_for_row(&format!("Local     {port}"));
        client.send(b"\r");
        client.wait_for_row("used by another program");
        let rows = client.wait_for_row("Use ");
        let suggested: u16 = rows
            .iter()
            .filter_map(|r| r.split("Use ").nth(1))
            .find_map(|s| s.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok())
            .unwrap();
        assert_ne!(suggested, port);
        assert_eq!(env.run(&["tunnel", "list"]).trim(), "no tunnels");
        click_account_text(&mut client, &format!("Use {suggested}"));
        client.wait_for_row(&format!("Local     {suggested}"));
        assert_eq!(env.run(&["tunnel", "list"]).trim(), "no tunnels");
        click_account_text(&mut client, " Add ");
        client.wait_for_row("Connecting");
        assert!(env.run(&["tunnel", "list"]).contains(&format!("localhost:{suggested}")));
        client.send(b"\r");
        client.wait_for_row("Tunnel · db");
        client.wait_for_row("Enter Close");
        std::fs::write(env.root.join("home/tunnel-release"), "").unwrap();
        client.wait_for_row("Cannot reach db");
        client.wait_for_row("Retrying in ");
        std::fs::write(env.root.join("home/tunnel-ready"), "").unwrap();
        click_account_text(&mut client, " Retry now ");
        client.wait_for_row("Ready");
        assert!(env.run(&["tunnel", "list"]).starts_with("up"));
        client.close_overlay("Tunnel · db");
        client.wait_for_row("Ready");
        client.send(b"\t");
        client.wait_for_row("Stop");
        click_account_text(&mut client, "Stop");
        client.wait_for_rows("the stopped tunnel left the list", |rows| {
            rows.iter().any(|r| r.contains("+ Add tunnel")) && !rows.iter().any(|r| r.contains("Ready"))
        });
        assert_eq!(env.run(&["tunnel", "list"]).trim(), "no tunnels");
        let workspaces: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        assert!(
            workspaces.as_array().unwrap().iter().any(|w| w["name"] == "w" && w["open"] == true),
            "stopping the tunnel keeps the workspace open"
        );
    }
}

#[cfg(unix)]
#[test]
fn palette_attachment_transfers_a_complete_batch_over_host_attach_and_keeps_it_after_detach() {
    use std::os::unix::fs::PermissionsExt;

    let local = Env::new("attachment-local");
    let remote = Env::new("attachment-host");
    remote.workspace(
        "w",
        "layout {\n    pane name=\"main\" command=\"sh\" { args \"-c\" \"stty -echo; trap 'echo CTRL_C_REACHED_PANE' INT; echo READY; while :; do cat; done\"; }\n}\n",
    );
    remote.run(&["open", "w"]);
    remote.wait_for_text("main", "READY");
    let other = RawClient::attach(&remote, "w");
    let (_, path) = fake_ssh(&local, &remote);
    std::fs::write(local.root.join("config/kraal/servers.toml"), "[[server]]\nname='box'\nhost='attachment.invalid'\n")
        .unwrap();
    let first = local.root.join("notes client.txt");
    let second = local.root.join("capture.bin");
    std::fs::write(&first, b"notes from the laptop").unwrap();
    let content = vec![0x91; 2 * 64 * 1024 + 7];
    std::fs::write(&second, &content).unwrap();
    let mut command = local.isolated(Path::new(env!("CARGO_BIN_EXE_kraal")));
    command.args(["-s", &remote.session, "attach", "--host", "box", "w"]).env("PATH", path);
    let mut host = TerminalClient::start(command);
    host.wait_for("READY");
    host.type_keys(b"\x02 Attach files\r");
    host.wait_for("Enter absolute local paths");
    host.type_keys(format!("'{}' {}\r", first.display(), second.display()).as_bytes());
    host.wait_for("Sent to box");
    host.wait_for("2 files");
    host.wait_for("(100%)");
    let root = remote.root.join("state/kraal/attachments-v1").join(&remote.session);
    let batches: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(batches.len(), 1);
    let batch = &batches[0];
    assert_eq!(std::fs::read(batch.join("0/notes client.txt")).unwrap(), b"notes from the laptop");
    assert_eq!(std::fs::read(batch.join("1/capture.bin")).unwrap(), content);
    assert_eq!(std::fs::metadata(batch).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(batch.join("1/capture.bin")).unwrap().permissions().mode() & 0o777, 0o600);
    // cat cannot print the paths until the user sends Enter.
    assert!(!remote.run(&["read", "-p", "main"]).contains("attachments-v1"));
    host.type_keys(b"\r");
    let pasted = remote.wait_for_text("main", "attachments-v1");
    assert!(pasted.contains("notes client.txt"));
    assert!(pasted.contains("capture.bin"));
    other.wait_for_screen("capture.bin");
    host.type_keys(b"\x03");
    remote.wait_for_text("main", "CTRL_C_REACHED_PANE");
    wait_until(PATIENCE, "attachment success disappears", || {
        !text_after(&host.written.lock().unwrap(), 100, 30).iter().any(|row| row.contains("Sent to box"))
    });
    host.type_keys(b"\x02d");
    assert_eq!(host.left("attachment detach").0, 0);
    assert_eq!(std::fs::read(batch.join("1/capture.bin")).unwrap(), content);
    remote.run(&["kill"]);
    assert!(batch.exists());
    remote.run(&["forget"]);
    assert!(!root.exists());
}

#[cfg(unix)]
#[test]
fn clipboard_images_use_local_private_storage_and_host_attachment_transport() {
    use std::os::unix::fs::PermissionsExt;

    for remote in [false, true] {
        let local = Env::new(if remote { "image-host-client" } else { "image-local-client" });
        let server = Env::new(if remote { "image-host-server" } else { "image-local-server" });
        let destination = if remote { &server } else { &local };
        let config = destination.root.join("config/kraal/config.toml");
        let text = std::fs::read_to_string(&config).unwrap();
        std::fs::write(config, format!("{text}\n[keys.leader]\nattach_clipboard_image = ['I']\n")).unwrap();
        destination.workspace(
            "w",
            "layout {\n    pane name=\"main\" command=\"sh\" { args \"-c\" \"stty -echo; echo READY; cat\"; }\n}\n",
        );
        destination.run(&["open", "w"]);
        destination.wait_for_text("main", "READY");
        let (_, path) = fake_ssh(&local, &server);
        let png = local.root.join("source.png");
        image::RgbaImage::new(2, 2).save(&png).unwrap();
        let log = local.root.join("image-reads");
        #[cfg(not(target_os = "macos"))]
        let (tool, script) =
            ("wl-paste", format!("#!/bin/sh\necho read >> '{}'\ncat '{}'\n", log.display(), png.display()));
        #[cfg(target_os = "macos")]
        let (tool, script) = (
            "osascript",
            format!(
                "#!/bin/sh\necho read >> '{}'\nfor arg; do file=$arg; done\ncat '{}' > \"$file\"\n",
                log.display(),
                png.display()
            ),
        );
        write_script(&local.root.join("bin").join(tool), &script);
        std::fs::write(local.root.join("config/kraal/servers.toml"), "[[server]]\nname='box'\nhost='image.invalid'\n")
            .unwrap();
        let mut command = local.isolated(Path::new(env!("CARGO_BIN_EXE_kraal")));
        command.args(["-s", &destination.session, "attach"]);
        if remote {
            command.args(["--host", "box"]);
        }
        command.arg("w").env("PATH", path).env("WAYLAND_DISPLAY", "test").env_remove("DISPLAY");
        let mut client = TerminalClient::start(command);
        client.wait_for("READY");
        if remote {
            client.type_keys(b"\x02 Attach clipboard image\r");
        } else {
            client.type_keys(b"\x02I");
        }
        client.wait_for("Send reads the local clipboard once.");
        assert!(!log.exists(), "offering the action must not read the clipboard");
        client.type_keys(b"\t\r");
        client.wait_for(if remote { "Sent to box" } else { "Sent to this machine" });
        client.wait_for("clipboard.png");
        client.wait_for("(100%)");
        let root = destination.root.join("state/kraal/attachments-v1").join(&destination.session);
        let batches: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(batches.len(), 1);
        let stored = batches[0].join("0/clipboard.png");
        assert_eq!(std::fs::read(&stored).unwrap(), std::fs::read(&png).unwrap());
        assert_eq!(std::fs::metadata(&batches[0]).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&stored).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(log).unwrap(), "read\n");
        assert!(!destination.run(&["read", "-p", "main"]).contains("clipboard.png"), "no Enter is added");
        client.type_keys(b"\r");
        destination.wait_for_text("main", "clipboard.png");
        client.type_keys(b"\x02d");
        assert_eq!(client.left("image detach").0, 0);
        assert!(stored.exists());
    }
}

#[cfg(unix)]
#[test]
fn host_control_v_sends_agent_images_and_preserves_shell_local_and_text_clipboards() {
    use std::io::Write;
    let local = Env::new("image-key-client");
    let server = Env::new("image-key-server");
    let input = server.root.join("input");
    let script = format!("stty raw -echo; echo READY; exec cat > '{}'", input.display());
    server.workspace("w", &format!("layout {{ pane name=\"main\" command=\"sh\" {{ args \"-c\" {script:?}; }} }}"));
    server.run(&["open", "w"]);
    server.wait_for_text("main", "READY");
    let (_, path) = fake_ssh(&local, &server);
    let png = local.root.join("source.png");
    image::RgbaImage::new(2, 2).save(&png).unwrap();
    let types = local.root.join("types");
    let log = local.root.join("clipboard-calls");
    #[cfg(not(target_os = "macos"))]
    let (tool, image_types, script) = (
        "wl-paste",
        "image/png\n",
        format!(
            "#!/bin/sh\nif [ \"$1\" = --list-types ]; then echo types >> '{}'; cat '{}'; elif [ \"$3\" = text/plain ]; then echo text >> '{}'; printf 'clipboard text'; else echo image >> '{}'; cat '{}'; fi\n",
            log.display(),
            types.display(),
            log.display(),
            log.display(),
            png.display()
        ),
    );
    #[cfg(target_os = "macos")]
    let (tool, image_types, script) = (
        "osascript",
        "«class PNGf», 80\n",
        format!(
            "#!/bin/sh\nif [ \"$2\" = 'clipboard info' ]; then echo types >> '{}'; cat '{}'; else echo image >> '{}'; for arg; do file=$arg; done; cat '{}' > \"$file\"; fi\n",
            log.display(),
            types.display(),
            log.display(),
            png.display()
        ),
    );
    write_script(&local.root.join("bin").join(tool), &script);
    #[cfg(target_os = "macos")]
    write_script(
        &local.root.join("bin/pbpaste"),
        &format!("#!/bin/sh\necho text >> '{}'; printf 'clipboard text'\n", log.display()),
    );
    std::fs::write(&types, image_types).unwrap();
    std::fs::write(local.root.join("config/kraal/servers.toml"), "[[server]]\nname='box'\nhost='image-key.invalid'\n")
        .unwrap();
    let attach = |remote: bool| {
        let mut command = if remote {
            local.isolated(Path::new(env!("CARGO_BIN_EXE_kraal")))
        } else {
            server.isolated(Path::new(env!("CARGO_BIN_EXE_kraal")))
        };
        command.args(["-s", &server.session, "attach"]);
        if remote {
            command.args(["--host", "box"]);
        }
        command.arg("w").env("PATH", &path).env("WAYLAND_DISPLAY", "test").env_remove("DISPLAY");
        let client = TerminalClient::start(command);
        client.wait_for("READY");
        client
    };
    let received = || std::fs::read(&input).unwrap_or_default();
    let mut client = attach(true);
    client.type_keys(b"\x16shell");
    wait_until(PATIENCE, "shell receives Ctrl+V", || received() == b"\x16shell");
    client.wait_for("this pane is not recognized as an agent");
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "types\n", "shell panes inspect types but never read the image");
    std::fs::remove_file(&log).unwrap();
    // Expiry on this 100x30 terminal must not reclaim the layout from a 124x36 client.
    let pane_size = || {
        let panes: serde_json::Value = serde_json::from_str(&server.run(&["panes", "--json"])).unwrap();
        (panes[0]["cols"].as_u64().unwrap(), panes[0]["rows"].as_u64().unwrap())
    };
    let first_size = pane_size();
    let other = RawClient::attach_sized(&server, Some("w"), (124, 36));
    other.wait_for_row("READY");
    let working_size = pane_size();
    assert_ne!(first_size, working_size);
    assert!(
        text_after(&client.written.lock().unwrap(), 100, 30).iter().any(|row| row.contains("Ctrl+V did not attach"))
    );
    wait_until(PATIENCE, "clipboard notice expires", || {
        !text_after(&client.written.lock().unwrap(), 100, 30).iter().any(|row| row.contains("Ctrl+V did not attach"))
    });
    assert_eq!(pane_size(), working_size, "notice expiry must preserve the working client's PTY size");
    drop(other);
    client.type_keys(b"\x02d");
    assert_eq!(client.left("shell detach").0, 0);

    let mut hook = server.cmd(&["hook", "claude"]);
    hook.env("KRAAL_PANE", "main")
        .env("KRAAL_SOCKET", server.root.join("run/kraal").join(format!("{}.sock", server.session)))
        .stdin(std::process::Stdio::piped());
    let mut child = hook.spawn().unwrap();
    child.stdin.take().unwrap().write_all(br#"{"hook_event_name":"SessionStart","session_id":"image-key"}"#).unwrap();
    assert!(child.wait().unwrap().success());
    assert!(server.run(&["agents"]).contains("claude"));

    let mut client = attach(false);
    client.type_keys(b"\x16local");
    wait_until(PATIENCE, "local image path precedes typing", || received().ends_with(b"clipboard.pnglocal"));
    assert!(!String::from_utf8_lossy(&client.written.lock().unwrap()).contains("Send the clipboard image"));
    let local_input = received();
    let pasted = std::str::from_utf8(&local_input[b"\x16shell".len()..local_input.len() - b"local".len()]).unwrap();
    assert_eq!(std::fs::read(pasted.trim_matches('\'')).unwrap(), std::fs::read(&png).unwrap());
    assert!(!pasted.contains('\n'), "no Enter is added");
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "types\nimage\n");
    std::fs::remove_file(&log).unwrap();
    client.type_keys(b"\x02d");
    assert_eq!(client.left("local detach").0, 0);

    std::fs::write(&types, "text/plain\n").unwrap();
    let mut expected = received();
    for remote in [false, true] {
        let mut client = attach(remote);
        client.type_keys(b"\x16text");
        expected.extend_from_slice(b"clipboard texttext");
        wait_until(PATIENCE, "clipboard text precedes following typing", || received() == expected);
        client.type_keys(b"\x02d");
        assert_eq!(client.left("text detach").0, 0);
    }
    #[cfg(not(target_os = "macos"))]
    let text_calls = "types\ntypes\ntext\ntypes\ntypes\ntext\n";
    #[cfg(target_os = "macos")]
    let text_calls = "types\ntext\ntypes\ntext\n";
    assert_eq!(std::fs::read_to_string(&log).unwrap(), text_calls);
    let mut client = attach(true);
    std::fs::write(&types, image_types).unwrap();
    client.type_keys(b"\x16");
    client.wait_for("Always for box");
    client.type_keys(b"\t\t\r");
    client.wait_for("Attachment stopped: cancelled");
    client.type_keys(b"\rcancelled");
    wait_until(PATIENCE, "cancel releases typing", || received().ends_with(b"cancelled"));
    assert_eq!(std::fs::read_to_string(&log).unwrap(), format!("{text_calls}types\n"));
    let before = received();
    client.type_keys(b"\x1b[118;5u");
    wait_until(PATIENCE, "second image question", || {
        text_after(&client.written.lock().unwrap(), 100, 30).iter().any(|row| row.contains("Always for box"))
    });
    client.type_keys(b"\t\r");
    client.wait_for("Sent to box");
    client.wait_for("clipboard.png");
    client.wait_for("(100%)");
    wait_until(PATIENCE, "server image path pasted", || received().ends_with(b"clipboard.png"));
    let pasted = String::from_utf8(received()[before.len()..].to_vec()).unwrap();
    assert!(!pasted.contains('\x16'));
    assert!(!pasted.contains('\n'), "no Enter is added");
    assert_eq!(std::fs::read(pasted.trim_matches('\'')).unwrap(), std::fs::read(&png).unwrap());
    let permission = std::fs::read_to_string(local.root.join("config/kraal/paste-files.toml")).unwrap();
    assert!(permission.contains("[[clipboard_image]]"));
    client.type_keys(b"\x02d");
    assert_eq!(client.left("image detach").0, 0);
    let before = received().len();
    let mut client = attach(true);
    client.type_keys(b"\x16");
    client.wait_for("Sent to box");
    client.wait_for("clipboard.png");
    client.wait_for("(100%)");
    wait_until(PATIENCE, "remembered image path pasted", || received().len() > before);
    assert!(!String::from_utf8_lossy(&client.written.lock().unwrap()).contains("Send the clipboard image"));
    assert_eq!(received().iter().filter(|&&b| b == 0x16).count(), 1);
    client.type_keys(b"\x02d");
    assert_eq!(client.left("remembered image detach").0, 0);
}

#[cfg(unix)]
#[test]
fn attachment_wire_cancellation_disconnect_and_repeated_finish_do_not_paste_partial_or_duplicate_paths() {
    let env = Env::new("attachment-wire");
    env.workspace(
        "w",
        "layout {\n    pane name=\"main\" command=\"sh\" { args \"-c\" \"stty -echo; echo READY; cat\"; }\n}\n",
    );
    env.run(&["open", "w"]);
    env.wait_for_text("main", "READY");
    let root = env.root.join("state/kraal/attachments-v1").join(&env.session);
    for outcome in ["cancel", "disconnect", "finish"] {
        let mut client = RawClient::attach(&env, "w");
        raw_frame(
            &mut client.stream,
            0,
            br#"{"type":"attach","version":6,"cols":120,"rows":40,"workspace":"w","remote":true}"#,
        );
        client.send(b"\x02 Attach files\r");
        client.wait_for_message("attachment_offer");
        let token = client
            .messages
            .lock()
            .unwrap()
            .iter()
            .find_map(|m| {
                let value: serde_json::Value = serde_json::from_str(m).ok()?;
                (value["type"] == "attachment_offer").then(|| value["token"].as_str().unwrap().to_string())
            })
            .unwrap();
        let send = |client: &mut RawClient, value: serde_json::Value| {
            raw_frame(&mut client.stream, 0, &serde_json::to_vec(&value).unwrap())
        };
        send(
            &mut client,
            serde_json::json!({"type":"attachment_start","token":token,"files":[{"name":"note","size":3}]}),
        );
        client.wait_for_message("attachment_ack");
        send(
            &mut client,
            serde_json::json!({"type":"attachment_chunk","token":token,"file":0,"offset":0,"data":"YWJj"}),
        );
        assert!(!env.run(&["read", "-p", "main"]).contains("attachments-v1"));
        match outcome {
            "cancel" => {
                send(&mut client, serde_json::json!({"type":"attachment_cancel","token":token}));
                client.wait_for_message("No paths or queued typing pasted");
            }
            "disconnect" => {
                drop(client);
            }
            "finish" => {
                send(&mut client, serde_json::json!({"type":"attachment_finish","token":token}));
                client.wait_for_message("Files received");
                send(&mut client, serde_json::json!({"type":"attachment_finish","token":token}));
                client.send(b"\r");
                let text = env.wait_for_text("main", "attachments-v1");
                assert_eq!(text.matches("attachments-v1").count(), 1);
                assert_eq!(std::fs::read(root.join(&token).join("0/note")).unwrap(), b"abc");
            }
            _ => unreachable!(),
        }
        wait_until(PATIENCE, "incomplete attachment removed", || !root.join(format!(".{token}")).exists());
        if outcome != "finish" {
            assert!(!root.join(token).exists());
        }
    }
}

#[cfg(unix)]
#[test]
fn host_pasted_paths_preserve_text_send_complete_batches_and_remember_revocable_permission() {
    let local = Env::new("pasted-path-local");
    let remote = Env::new("pasted-path-host");
    remote.workspace(
        "w",
        "layout {\n    pane name=\"main\" command=\"sh\" { args \"-c\" \"stty -echo -icanon; echo READY; cat\"; }\n}\n",
    );
    remote.run(&["open", "w"]);
    remote.wait_for_text("main", "READY");
    let (_, path) = fake_ssh(&local, &remote);
    std::fs::write(local.root.join("config/kraal/servers.toml"), "[[server]]\nname='box'\nhost='pasted.invalid'\n")
        .unwrap();
    let first = local.root.join("notes client.txt");
    let second = local.root.join("capture.bin");
    std::fs::write(&first, b"laptop notes").unwrap();
    std::fs::write(&second, b"laptop capture").unwrap();
    let mut command = local.isolated(Path::new(env!("CARGO_BIN_EXE_kraal")));
    command.args(["-s", &remote.session, "attach", "--host", "box", "w"]).env("PATH", path);
    let mut host = TerminalClient::start(command);
    host.wait_for("READY");
    let original = format!("  '{}' file://{}  ", first.display(), second.display());
    let pasted = format!("\x1b[200~{original}\x1b[201~");
    let visible = |host: &TerminalClient, text: &str| {
        wait_until(PATIENCE, text, || {
            text_after(&host.written.lock().unwrap(), 100, 30).iter().any(|row| row.contains(text))
        });
    };
    // The markers can arrive across separate reads of the laptop's terminal.
    host.type_keys(&pasted.as_bytes()[..3]);
    host.type_keys(&pasted.as_bytes()[3..]);
    visible(&host, "Send 2 files to box?");
    let root = remote.root.join("state/kraal/attachments-v1").join(&remote.session);
    assert!(!root.exists());
    host.type_keys(b"\t\t\r");
    visible(&host, "Text pasted");
    let output = remote.wait_for_text("main", "capture.bin");
    assert!(output.contains("notes client.txt") && output.contains("file://"), "{output}");
    assert!(!root.exists());
    host.type_keys(pasted.as_bytes());
    visible(&host, "Send 2 files to box?");
    host.type_keys(b"\t\r");
    visible(&host, "Sent to box");
    visible(&host, "2 files");
    visible(&host, "(100%)");
    let permission = local.root.join("config/kraal/paste-files.toml");
    assert!(std::fs::read_to_string(&permission).unwrap().contains("pasted.invalid"));
    let batches = || {
        std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|p| !p.file_name().unwrap().to_string_lossy().starts_with('.'))
            .collect::<Vec<_>>()
    };
    assert_eq!(batches().len(), 1);
    for batch in batches() {
        assert_eq!(std::fs::read(batch.join("0/notes client.txt")).unwrap(), b"laptop notes");
        assert_eq!(std::fs::read(batch.join("1/capture.bin")).unwrap(), b"laptop capture");
    }
    host.type_keys(pasted.as_bytes());
    wait_until(PATIENCE, "automatic second batch", || batches().len() == 2);
    visible(&host, "Sent to box");
    visible(&host, "2 files");
    visible(&host, "(100%)");
    std::fs::remove_file(permission).unwrap();
    host.type_keys(pasted.as_bytes());
    visible(&host, "Send 2 files to box?");
    assert_eq!(batches().len(), 2);
    host.type_keys(b"\x03");
    visible(&host, "cancelled");
    assert_eq!(batches().len(), 2);
    host.type_keys(b"\x03");
    wait_until(PATIENCE, "cancel result dismissed", || {
        !text_after(&host.written.lock().unwrap(), 100, 30).iter().any(|row| row.contains("Transfer interrupted"))
    });
    host.type_keys(b"\x02d");
    assert_eq!(host.left("pasted path detach").0, 0);
}

#[test]
fn remote_install_restarts_only_the_selected_running_session_and_keeps_failed_restarts_explicit() {
    for running in [false, true] {
        for succeeds in [false, true] {
            let env = Env::new(&format!("remote-install-{running}-{succeeds}"));
            let bin = env.root.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let log = env.root.join("ssh.log");
            write_script(
                &bin.join("ssh"),
                r#"#!/bin/sh
for command do :; done
printf '%s\n' "$command" >> "$SSH_LOG"
case "$command" in
    'uname -sm') uname -sm ;;
    *' ls'*)
        printf 'other-session        running  0.2.0\n'
        if [ "$REMOTE_RUNNING" = true ]; then printf '%s  running  0.2.0\n' "$REMOTE_SESSION"; fi
        ;;
    *' restart'*) [ "$RESTART_SUCCEEDS" = true ] ;;
    *) cat >/dev/null; printf 'kraal installed\n' ;;
esac
"#,
            );
            let out = env
                .cmd(&["remote-install", "box"])
                .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()))
                .env("SSH_LOG", &log)
                .env("REMOTE_SESSION", &env.session)
                .env("REMOTE_RUNNING", running.to_string())
                .env("RESTART_SUCCEEDS", succeeds.to_string())
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            let commands = std::fs::read_to_string(log).unwrap();
            assert_eq!(commands.contains(" restart"), running, "{commands}");
            assert!(!commands.contains("-s other-session"), "{commands}");
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert_eq!(out.status.success(), !running || succeeds, "{stdout}\n{stderr}");
            if running && !succeeds {
                assert!(stderr.contains("was not restarted"), "{stderr}");
                assert!(stderr.contains(&format!("kraal -s {} remote-install box", env.session)), "{stderr}");
                assert!(!stdout.contains("Attach with"));
            } else {
                assert!(stdout.contains(&format!("kraal -s {} attach --host box", env.session)), "{stdout}");
            }
        }
    }
}

/// From a pane whose session cannot be asked (gone here), production commands do nothing: one
/// that acts from this terminal (with or without the session's socket in its environment), and
/// one addressed to another session with `-s`, which that session refuses.
#[test]
fn production_commands_from_a_pane_whose_session_cannot_be_asked_do_nothing() {
    let env = Env::new("prod-unasked");
    let (bin, reached) = (env.root.join("bin"), env.root.join("reached"));
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("ssh"), &format!("#!/bin/sh\ntouch '{}'\n", reached.display()));
    std::fs::write(
        env.root.join("config/kraal/servers.toml"),
        "[[server]]\nname = \"web-prod-1\"\nhost = \"web-prod-1\"\n",
    )
    .unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let from_pane = |args: &[&str]| {
        env.cmd(args)
            .env("PATH", &path)
            .env("KRAAL_SOCKET", env.root.join("run/gone.sock"))
            .env("KRAAL_SESSION", "gone")
            .env("KRAAL_PANE", "1")
            .output()
            .unwrap()
    };
    let cases = [
        (&["exec", "web-prod-1", "--", "true"][..], "gone.sock"),
        (&["ssh", "web-prod-1", "--tab", "--", "true"][..], "from a pane of another kraal session"),
    ];
    for (args, why) in cases {
        let out = from_pane(args);
        assert!(!out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stdout));
        assert!(String::from_utf8_lossy(&out.stderr).contains(why), "{}", String::from_utf8_lossy(&out.stderr));
    }
    // Without its socket in the environment, the pane's session is still the one asked.
    let out = env
        .cmd(&["exec", "web-prod-1", "--", "true"])
        .env("PATH", &path)
        .env("KRAAL_SESSION", "gone")
        .env("KRAAL_PANE", "1")
        .output()
        .unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("gone"));
    assert!(!reached.exists(), "the command reached the server");
}

/// An agent's production command waits in a real attached client until the person allows it:
/// that client says when it displayed the question, and only a key after that answers it.
#[cfg(target_os = "linux")]
#[test]
fn an_agents_production_command_runs_once_allowed_in_the_attached_client() {
    let env = Env::new("prod-allow");
    let (bin, reached) = (env.root.join("bin"), env.root.join("reached"));
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("ssh"), &format!("#!/bin/sh\ntouch '{}'\n", reached.display()));
    std::fs::write(
        env.root.join("config/kraal/servers.toml"),
        "[[server]]\nname = \"web-prod-1\"\nhost = \"web-prod-1\"\n",
    )
    .unwrap();
    // The agent: a program named claude that deploys through kraal, then waits.
    let claude = bin.join("claude");
    let kraal = env!("CARGO_BIN_EXE_kraal");
    write_script(
        &claude,
        &format!("#!/bin/sh\nPATH='{}':$PATH '{kraal}' exec web-prod-1 -- deploy\nsleep 30\n", bin.display()),
    );
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let mut client = TerminalClient::attach(&env);
    env.run(&["run", "-n", "agent", "--", &claude.display().to_string()]);
    client.wait_for("to act on production?");
    assert!(!reached.exists(), "nothing runs before the answer");
    // Read, then allowed: the move to Allow and the confirmation come after it was displayed.
    std::thread::sleep(Duration::from_millis(600));
    client.type_keys(b"y");
    wait_until(PATIENCE, "the allowed command to run", || reached.exists());
}
