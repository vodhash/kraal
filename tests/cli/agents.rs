use super::support::RawClient;
use crate::common::{Env, claude_hook, write_script};
#[cfg(target_os = "linux")]
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn hooks_drive_agent_state() {
    let env = Env::new("hooks");
    env.workspace("w", "layout {\n    pane name=\"agent\"\n}\n");
    env.run(&["open", "w"]);
    let socket = env.root.join("run/kraal/hooks.sock");
    let hook = |payload: &str| {
        let mut c = env.cmd(&["hook", "claude"]);
        c.env("KRAAL_PANE", "agent").env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        assert!(out.stdout.is_empty(), "hooks must not print");
    };
    hook(r#"{"hook_event_name":"SessionStart","session_id":"s-1"}"#);
    hook(r#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1"}"#);
    assert!(env.run(&["agents"]).contains("working"));
    hook(r#"{"hook_event_name":"Notification","message":"Claude needs your permission to use Bash"}"#);
    assert_eq!(env.run(&["wait", "-p", "agent", "--until", "blocked", "--timeout", "2"]).trim(), "blocked");
    hook(r#"{"hook_event_name":"Stop","session_id":"s-1"}"#);
    assert_eq!(env.run(&["wait", "-p", "agent", "--until", "done", "--timeout", "2"]).trim(), "done");
    let timeout = env.try_run(&["wait", "-p", "agent", "--until", "working", "--timeout", "0.2"]);
    assert!(!timeout.status.success());
}

/// Other agents' hooks, as each one calls `kraal hook`; and a hook for another agent than the
/// one a pane runs (Cursor runs Claude Code's hooks) changes nothing there.
#[test]
fn other_agents_hooks_drive_their_state() {
    let env = Env::new("hooks-other");
    env.workspace("w", "layout {\n    pane name=\"gemini\"\n    pane name=\"copilot\"\n    pane name=\"cursor\"\n}\n");
    env.run(&["open", "w"]);
    let socket = env.root.join("run/kraal/hooks-other.sock");
    let hook = |pane: &str, args: &[&str], payload: &str| {
        let mut c = env.cmd(&[&["hook"], args].concat());
        c.env("KRAAL_PANE", pane).env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(), "hooks must stay silent");
    };
    let wait = |pane: &str, state: &str| {
        assert_eq!(env.run(&["wait", "-p", pane, "--until", state, "--timeout", "3"]).trim(), state, "{pane}");
    };
    // Gemini CLI's hooks leave a declined question to the screen: it runs in its pane, busy.
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let gemini = bin.join("gemini");
    write_script(&gemini, "#!/bin/sh\necho started\nwhile :; do sleep 0.3; printf .; done\n");
    env.run(&["send", "-p", "gemini", "--enter", &gemini.display().to_string()]);
    env.wait_for_text("gemini", "started");
    hook("gemini", &["gemini"], r#"{"hook_event_name":"BeforeAgent","session_id":"g-1","prompt":"fix it"}"#);
    wait("gemini", "working");
    hook(
        "gemini",
        &["gemini"],
        r#"{"hook_event_name":"Notification","session_id":"g-1","notification_type":"ToolPermission","message":"Tool Shell requires execution","details":{"type":"exec"}}"#,
    );
    wait("gemini", "blocked");
    hook("gemini", &["gemini"], r#"{"hook_event_name":"AfterAgent","session_id":"g-1"}"#);
    wait("gemini", "done");
    // Copilot's own format names the event on the command line only.
    hook("copilot", &["copilot", "userPromptSubmitted"], r#"{"sessionId":"c-1","timestamp":1,"cwd":"/","prompt":"x"}"#);
    wait("copilot", "working");
    hook(
        "copilot",
        &["copilot", "notification"],
        r#"{"sessionId":"c-1","hook_event_name":"Notification","notification_type":"permission_prompt","message":"Copilot needs permission"}"#,
    );
    wait("copilot", "blocked");
    hook("copilot", &["copilot", "agentStop"], r#"{"sessionId":"c-1","stopReason":"end_turn"}"#);
    wait("copilot", "done");
    // A pane running Cursor's CLI.
    let cursor = bin.join("cursor-agent");
    write_script(&cursor, "#!/bin/sh\necho started\nsleep 30\n");
    env.run(&["send", "-p", "cursor", "--enter", &cursor.display().to_string()]);
    env.wait_for_text("cursor", "started");
    hook(
        "cursor",
        &["claude"],
        r#"{"hook_event_name":"UserPromptSubmit","session_id":"x","cursor_version":"2026.10.01"}"#,
    );
    hook("cursor", &["claude"], r#"{"hook_event_name":"UserPromptSubmit","session_id":"x"}"#);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let cursor_pane = panes.as_array().unwrap().iter().find(|p| p["name"] == "cursor").unwrap();
    assert_ne!(cursor_pane["agent"], "claude", "{cursor_pane}");
    hook(
        "cursor",
        &["cursor"],
        r#"{"hook_event_name":"stop","conversation_id":"c","status":"completed","loop_count":0}"#,
    );
    wait("cursor", "done");
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let cursor_pane = panes.as_array().unwrap().iter().find(|p| p["name"] == "cursor").unwrap();
    assert_eq!(cursor_pane["agent"], "cursor", "{cursor_pane}");
    env.run(&["keys", "-p", "cursor", "ctrl+c"]);
    env.run(&["keys", "-p", "gemini", "ctrl+c"]);
}

/// droid tells a cancelled turn by waiting for a prompt, without a stop: the pane is idle again.
/// It says the same after a turn that finished, which stays done until it is seen.
#[test]
fn a_cancelled_turn_ends_idle_and_a_finished_one_stays_done() {
    let env = Env::new("hooks-idle");
    env.workspace("w", "layout {\n    pane name=\"droid\"\n}\n");
    env.run(&["open", "w"]);
    let socket = env.root.join("run/kraal/hooks-idle.sock");
    let hook = |payload: &str| {
        let mut c = env.cmd(&["hook", "droid"]);
        c.env("KRAAL_PANE", "droid").env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        assert!(child.wait_with_output().unwrap().status.success());
    };
    let wait = |state: &str| {
        assert_eq!(env.run(&["wait", "-p", "droid", "--until", state, "--timeout", "3"]).trim(), state);
    };
    let prompt = r#"{"hook_event_name":"UserPromptSubmit","session_id":"d-1","prompt":"fix it"}"#;
    let waiting = r#"{"hook_event_name":"Notification","session_id":"d-1","notification_type":"idle_prompt","message":"Droid is waiting for input"}"#;
    hook(prompt);
    wait("working");
    hook(waiting);
    wait("idle");
    hook(prompt);
    hook(r#"{"hook_event_name":"Stop","session_id":"d-1"}"#);
    wait("done");
    hook(waiting);
    let still = env.try_run(&["wait", "-p", "droid", "--until", "idle", "--timeout", "0.5"]);
    assert!(!still.status.success(), "a finished turn nobody looked at went idle");
    wait("done");
}

/// OpenCode's plugin reports its changes only, never its end: run under a wrapper kraal does not
/// recognise (`sh -c 'opencode; …'`), the agent stays while the process its plugin reported
/// from runs, and goes when that process ends.
#[test]
fn a_hooked_agent_under_a_wrapper_stays_while_its_process_runs() {
    let env = Env::new("hooks-wrapped");
    env.workspace("w", "layout {\n    pane name=\"oc\"\n}\n");
    env.run(&["open", "w"]);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let opencode = bin.join("opencode");
    let go = env.root.join("go");
    // Stands for OpenCode: its plugin runs `kraal hook opencode <state>` as a child of its own.
    write_script(
        &opencode,
        &format!(
            "#!/bin/sh\n'{}' hook opencode working\necho reported\nwhile [ ! -e '{}' ]; do sleep 0.1; done\n",
            env!("CARGO_BIN_EXE_kraal"),
            go.display()
        ),
    );
    let wrapped = format!("sh -c '{}; echo finished; sleep 30'", opencode.display());
    env.run(&["send", "-p", "oc", "--enter", &wrapped]);
    env.wait_for_text("oc", "reported");
    let agent = || {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let pane = panes.as_array().unwrap().iter().find(|p| p["name"] == "oc").cloned().unwrap();
        (pane["agent"].as_str().map(String::from), pane["state"].as_str().map(String::from))
    };
    // A few scans of the foreground (the wrapper, no agent) later, still there.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(agent(), (Some("opencode".into()), Some("working".into())));
    std::fs::write(&go, "").unwrap();
    env.wait_for_text("oc", "finished");
    let start = Instant::now();
    while agent().0.is_some() {
        assert!(start.elapsed() < Duration::from_secs(5), "the agent outlived its process");
        std::thread::sleep(Duration::from_millis(100));
    }
    env.run(&["keys", "-p", "oc", "ctrl+c"]);
}

#[test]
fn agents_resume_after_a_restart_but_never_replay_a_one_shot_run() {
    let env = Env::new("resume");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    write_script(&claude, "#!/bin/sh\necho started\nsleep 0.5\n");
    let claude = claude.display().to_string();
    let socket = env.root.join("run/kraal/resume.sock");
    let hook = |pane: &str, session: &str| {
        let mut c = env.cmd(&["hook", "claude"]);
        c.env("KRAAL_PANE", pane).env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        let payload = format!(r#"{{"hook_event_name":"SessionStart","session_id":"{session}"}}"#);
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        assert!(child.wait_with_output().unwrap().status.success());
    };
    let codex = bin.join("codex");
    write_script(&codex, "#!/bin/sh\necho started\nsleep 0.5\n");
    let codex = codex.display().to_string();
    // An interactive run and a one-shot one, each with a known conversation, both exited; one
    // whose words kraal cannot tell from a prompt; a one-shot run hidden behind an option kraal
    // cannot read; and a Codex fork, whose conversation is the fork's own.
    env.run(&["run", "-n", "chat", "--", &claude, "--model", "opus"]);
    env.run(&["run", "-n", "oneshot", "--", &claude, "-p", "perform deployment"]);
    env.run(&["run", "-n", "odd", "--", &claude, "--brand-new-flag", "Deploy now"]);
    env.run(&["run", "-n", "printed", "--", &claude, "--debug", "api", "--print", "Deploy release"]);
    env.run(&["run", "-n", "fork", "--", &codex, "fork", "parent-1", "Deploy release"]);
    hook("chat", "s-1");
    hook("oneshot", "s-2");
    hook("odd", "s-3");
    hook("printed", "s-5");
    let mut c = env.cmd(&["hook", "codex", r#"{"type":"agent-turn-complete","thread-id":"child-1"}"#]);
    c.env("KRAAL_PANE", "fork").env("KRAAL_SOCKET", &socket);
    assert!(c.status().unwrap().success());
    for name in ["chat", "oneshot", "odd", "printed", "fork"] {
        env.run(&["wait", "-p", name, "--until", "exit", "--timeout", "10"]);
    }
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let command = |name: &str| -> Vec<String> {
        let p = panes.as_array().unwrap().iter().find(|p| p["name"] == name).unwrap();
        p["command"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect()
    };
    assert_eq!(command("chat"), [claude.as_str(), "--resume", "s-1", "--model", "opus"], "it resumes");
    assert_eq!(command("oneshot"), [claude.as_str(), "-p", "perform deployment"], "never run again as a resume");
    // Kept whole, and waiting: Enter runs it as shown.
    assert_eq!(command("odd"), [claude.as_str(), "--resume", "s-3", "--brand-new-flag", "Deploy now"]);
    let odd = panes.as_array().unwrap().iter().find(|p| p["name"] == "odd").unwrap();
    assert_eq!(odd["suspended"], true, "{odd}");
    assert_eq!(command("printed"), [claude.as_str(), "--debug", "api", "--print", "Deploy release"]);
    // The fork's own conversation, never the fork again with its prompt; waiting for Enter.
    assert_eq!(command("fork"), [codex.as_str(), "resume", "child-1"]);
    let fork = panes.as_array().unwrap().iter().find(|p| p["name"] == "fork").unwrap();
    assert_eq!(fork["suspended"], true, "{fork}");
}

#[test]
#[cfg(target_os = "linux")]
fn codex_usage_leaves_nothing_behind() {
    let env = Env::new("usage");
    let home = env.root.join("home");
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        "[[account]]\nid = 'test-codex-work'\nname = \"work\"\nagent = \"codex\"\nhome = \"~/.codex\"\n",
    )
    .unwrap();
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let pidfile = env.root.join("helper.pid");
    // A `codex` that leaves a helper behind (no environment, a session of its own), then answers.
    write_script(
        &bin.join("codex"),
        &format!(
            r#"#!/bin/sh
env -i setsid sh -c 'sleep 600 >/dev/null 2>&1 & echo $! > {}'
IFS= read -r line
printf '%s\n' '{{"id":1,"result":{{}}}}'
IFS= read -r line
IFS= read -r line
printf '%s\n' '{{"id":2,"result":{{"rateLimits":{{"primary":{{"usedPercent":42,"windowDurationMins":300}}}}}}}}'
sleep 600
"#,
            pidfile.display()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let mut c = env.cmd(&["accounts", "refresh"]);
    c.env("PATH", path).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    let out = c.output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("42%"), "{text}");
    let pid = std::fs::read_to_string(&pidfile).unwrap().trim().to_string();
    let start = Instant::now();
    while Path::new(&format!("/proc/{pid}")).exists() {
        assert!(start.elapsed() < Duration::from_secs(5), "the helper Codex left still runs");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_agent_that_may_send_its_prompt_again_is_shown_before_it_restarts() {
    let env = Env::new("showfirst");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    write_script(&claude, "#!/bin/sh\necho \"ran: $*\"\nsleep 0.3\n");
    let claude = claude.display().to_string();
    let socket = env.root.join("run/kraal/showfirst.sock");
    let mut client: Option<RawClient> = None;
    // An option kraal does not know; and `--worktree update`, where `update` names a worktree
    // (it is not Claude's `update` subcommand, and the conversation is resumed).
    for (pane, args, session) in [
        ("odd", ["--brand-new-flag", "Deploy now"].as_slice(), "s-9"),
        ("wt", ["--worktree", "update", "Deploy release"].as_slice(), "s-10"),
    ] {
        let mut run = vec!["run", "-n", pane, "--", claude.as_str()];
        run.extend_from_slice(args);
        env.run(&run);
        let mut c = env.cmd(&["hook", "claude"]);
        c.env("KRAAL_PANE", pane).env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        let start = format!(r#"{{"hook_event_name":"SessionStart","session_id":"{session}"}}"#);
        child.stdin.take().unwrap().write_all(start.as_bytes()).unwrap();
        assert!(child.wait_with_output().unwrap().status.success());
        env.run(&["wait", "-p", pane, "--until", "exit", "--timeout", "10"]);
        env.run(&["focus", pane]);
        let client = client.get_or_insert_with(|| RawClient::attach(&env, "w"));
        env.wait_for_text(pane, "ran:");
        // The first Enter shows what would run; nothing runs.
        client.send(b"\r");
        let screen = env.wait_for_text(pane, "cannot tell");
        assert_eq!(screen.matches("ran:").count(), 1, "{pane}: {screen}");
        // The second runs it, exactly as shown (once the note is up: Enter before that goes to it).
        std::thread::sleep(Duration::from_millis(500));
        client.send(b"\r");
        // Lines may wrap in the pane (a space at a line end is lost): compare without spaces.
        let flat = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        let expected = flat(&format!("ran: --resume {session} {}", args.join(" ")));
        let start = Instant::now();
        let screen = loop {
            let screen = flat(&env.run(&["read", "-p", pane]));
            if screen.contains(&expected) {
                break screen;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "{pane} never ran: {screen}");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(screen.matches("ran:").count(), 2, "{pane}: {screen}");
    }
}

#[test]
fn an_agent_waiting_for_its_account_waits_again_after_a_restart() {
    let env = Env::new("pendagent");
    let repo = env.root.join("company/repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    let feature = env.root.join("elsewhere/feature");
    git(&["worktree", "add", "-q", "-b", "feature", feature.to_str().unwrap()]);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join("claude"),
        "#!/bin/sh\n[ \"$2\" = status ] && exit 1\necho \"home=${CLAUDE_CONFIG_DIR:-default}\"\nexec sleep 100\n",
    );
    // A git that hangs on the question kraal asks while `slow` exists: the agent cannot get its
    // account before the session is stopped.
    let real_git =
        String::from_utf8(Command::new("sh").args(["-c", "command -v git"]).output().unwrap().stdout).unwrap();
    let slow = env.root.join("slow");
    write_script(
        &bin.join("git"),
        &format!(
            "#!/bin/sh\ncase \"$*\" in *--git-common-dir*) [ -e {slow:?} ] && exec sleep 8 ;; esac\nexec {} \"$@\"\n",
            real_git.trim()
        ),
    );
    std::fs::write(&slow, "").unwrap();
    let pro = env.root.join("pro-home");
    std::fs::create_dir_all(&pro).unwrap();
    let canonical = std::fs::canonicalize(&repo).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nid = 'test-claude-pro'\nname = \"pro\"\nagent = \"claude\"\nhome = {:?}\n\n[[rule]]\npath = {:?}\naccounts = [\"pro\"]\n",
            pro.display().to_string(),
            canonical.display().to_string()
        ),
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let run = |args: &[&str]| {
        let out = env.cmd(args).env("PATH", &path).current_dir(&env.root).output().unwrap();
        assert!(out.status.success(), "kraal {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    env.workspace("w", &format!("layout {{\n    pane name=\"ag\" agent=\"claude\" cwd={:?}\n}}\n", feature.display()));
    run(&["open", "w"]);
    env.wait_for_text("ag", "waits for the account rules");
    run(&["kill"]);
    let state = std::fs::read_to_string(env.state_file()).unwrap();
    assert!(state.contains("pending_agent"), "{state}");
    // Restarted, with git answering again: the agent starts on the account the rule names.
    std::fs::remove_file(&slow).unwrap();
    run(&["open", "w"]);
    env.wait_for_text("ag", &format!("home={}", pro.display()));
}

#[test]
fn agents_follow_prints_the_list_each_time_it_changes() {
    let env = Env::new("follow");
    // Before the session runs it has no agents, and the list waits for it.
    let (mut follow, lines) = env.follow(&["agents", "--follow"]);
    let next = || -> serde_json::Value {
        let line = lines.recv_timeout(Duration::from_secs(10)).expect("a list in time");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"))
    };
    assert_eq!(next(), serde_json::json!([]));
    env.workspace("w", "layout {\n    pane name=\"api\"\n}\n");
    env.run(&["open", "w"]);
    claude_hook(&env, "api", r#"{"hook_event_name":"SessionStart","session_id":"s-1"}"#);
    claude_hook(&env, "api", r#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1"}"#);
    let mut list = next();
    while list[0]["state"] != "working" {
        list = next();
    }
    assert_eq!((list[0]["name"].as_str(), list[0]["agent"].as_str()), (Some("api"), Some("claude")), "{list}");
    claude_hook(&env, "api", r#"{"hook_event_name":"Notification","message":"Claude needs your permission"}"#);
    while list[0]["state"] != "blocked" {
        list = next();
    }
    // The session ends: no agents again.
    env.run(&["kill"]);
    while list != serde_json::json!([]) {
        list = next();
    }
    let _ = follow.kill();
    let _ = follow.wait();
    // It ends once what reads it is gone, even when it has nothing new to say.
    let mut alone = env.cmd(&["agents", "--follow"]).stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut first = [0u8; 3];
    std::io::Read::read_exact(alone.stdout.as_mut().unwrap(), &mut first).unwrap();
    assert_eq!(&first, b"[]\n");
    drop(alone.stdout.take());
    let start = Instant::now();
    while alone.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < Duration::from_secs(10), "still running with nobody to read it");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `kraal agent claude --teams`: the tmux commands Claude Code runs for a teammate open, title,
/// start and close a kraal pane; its other tmux commands reach the real tmux.
#[test]
fn agent_teams_open_teammates_as_kraal_panes() {
    let env = Env::new("teams");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("tmux"), "#!/bin/sh\necho \"real-tmux $* TMUX=$TMUX\"\n");
    // Claude Code's own calls, inside tmux.
    write_script(
        &bin.join("claude"),
        r#"#!/bin/sh
[ "$2" = status ] && exit 1
t() { tmux -S "${TMUX%%,*}" "$@"; }
me=$(t display-message -p '#{pane_id}')
win=$(t display-message -t "$me" -p '#{window_id}')
mate=$(t split-window -d -t "$me" -h -l 70% -P -F '#{pane_id}' -- cat)
t select-pane -t "$mate" -T researcher
t set-option -p -t "$mate" pane-border-style fg=red
t set-option -p -t "$mate" remain-on-exit failed
t respawn-pane -k -t "$mate" -- 'echo "mate tmux=$TMUX key=$ANTHROPIC_API_KEY"; exec sleep 100'
failing=$(t split-window -d -t "$mate" -v -P -F '#{pane_id}' -- cat)
t respawn-pane -k -t "$failing" -- 'echo boom; exit 1'
echo "lead $me $win $(t list-panes -t "$win" -F '#{pane_id}' | tr '\n' ' ')"
echo "teams=$CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS args=$*"
tmux new-session -d -s build
t kill-pane -t %1 2>&1 | tr -d '\n'; echo " (outside)"
read -r go
t kill-pane -t "$mate" && echo closed
exec sleep 100
"#,
    );
    let home = env.root.join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!("[[account]]\nid = \"test-claude-work\"\nagent = \"claude\"\nname = \"work\"\nhome = {home:?}\n"),
    )
    .unwrap();
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    // A key in the server's environment, which an account's agent must not use.
    let out = env.cmd(&["open", "w"]).env("PATH", &path).env("ANTHROPIC_API_KEY", "server-key").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let socket = env.root.join("run/kraal/teams.sock");
    let out =
        env.cmd(&["agent", "claude", "--teams", "--remote", "--tab"]).env("KRAAL_SOCKET", &socket).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let lead = String::from_utf8(out.stdout).unwrap().trim().to_string();
    let screen = env.wait_for_text(&lead, "(outside)");
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let panes = panes.as_array().unwrap();
    let mate = panes.iter().find(|p| p["name"] == "researcher").expect("the teammate's pane");
    let (mate, tab) = (mate["id"].to_string(), mate["tab"].clone());
    assert!(screen.contains(&format!("lead %{lead} @{lead} %{lead} %{mate} %")), "{screen}");
    assert!(screen.contains("teams=1 args=") && screen.contains("--teammate-mode auto"), "{screen}");
    assert!(screen.contains("--remote-control"), "{screen}");
    // Its own tmux: the real one, outside the team's; and no pane outside the team.
    assert!(screen.contains("real-tmux new-session -d -s build TMUX="), "{screen}");
    assert!(screen.contains("%1 is not a pane of this agent team (outside)"), "{screen}");
    // The teammate runs next to its lead, on its account, and is not inside tmux itself.
    let lead_tab =
        panes.iter().find(|p| p["id"].as_u64().is_some_and(|i| i.to_string() == lead)).unwrap()["tab"].clone();
    assert_eq!(tab, lead_tab);
    env.wait_for_text(&mate, "mate tmux= key=\n");
    // One that fails stays, with what it said.
    let failing = panes.iter().map(|p| p["id"].to_string()).max_by_key(|id| id.parse::<u32>().unwrap()).unwrap();
    assert_ne!(failing, mate);
    env.wait_for_text(&failing, "boom");
    env.run(&["send", "-p", &lead, "go\r"]);
    env.wait_for_text(&lead, "closed");
    assert!(!env.run(&["panes"]).contains("researcher"));
    assert!(env.run(&["read", "-p", &failing]).contains("boom"));
    // Not for other agents, nor outside kraal, nor in this terminal.
    let out = env.cmd(&["agent", "codex", "--teams"]).env("KRAAL_SOCKET", &socket).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("--teams is for Claude Code"));
    let out = env.cmd(&["agent", "claude", "--teams"]).env_remove("KRAAL_SOCKET").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("--teams works inside kraal"));
    let out = env.cmd(&["agent", "claude", "--teams", "--here"]).env("KRAAL_SOCKET", &socket).output().unwrap();
    assert!(!out.status.success());
}
