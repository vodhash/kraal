//! Cross-version tests: this kraal with a session that another release started, and that
//! release refused by a session this kraal started. `KRAAL_OLD` is the other release's
//! binary: `tests/compat.sh` fetches the releases under test and runs these with
//! each of them.

/// Where the tests run, as the other end-to-end tests do.
#[path = "common/isolation.rs"]
mod isolation;

#[path = "common/script.rs"]
mod script;
use script::write_script;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const SKIP: &str = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it";

/// A kraal binary and its release.
struct Kraal {
    bin: PathBuf,
    version: String,
    protocol: u32,
}

impl Kraal {
    fn this() -> Kraal {
        Kraal { bin: env!("CARGO_BIN_EXE_kraal").into(), version: env!("CARGO_PKG_VERSION").into(), protocol: 6 }
    }

    fn state_dir(&self) -> &str {
        if self.protocol == 2 { "sessions" } else { "sessions-v2" }
    }

    fn old() -> Kraal {
        let bin = PathBuf::from(std::env::var_os("KRAAL_OLD").expect(SKIP));
        let out = Command::new(&bin).arg("--version").output().unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        let version = said.trim().trim_start_matches("kraal ").to_string();
        assert!(out.status.success() && !version.is_empty(), "{}: {said}", bin.display());
        let protocol = match version.as_str() {
            "0.2.0" | "0.3.0" => 2,
            "0.4.0" => 3,
            "0.4.1" => 4,
            v if v.starts_with("0.5.") => 5,
            v => panic!("which protocol kraal {v} speaks is not known here"),
        };
        Kraal { bin, version, protocol }
    }
}

/// A session in directories of its own, which kraal of either release drives.
struct Env {
    root: PathBuf,
    session: String,
}

impl Env {
    fn new(name: &str) -> Env {
        isolation::check();
        // Under /tmp: macOS's temporary directory is too long for the sockets kraal makes there.
        let root = PathBuf::from("/tmp").join(format!("kraal-compat-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "run", "state", "config/kraal/workspaces"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let sh = root.join("sh");
        write_script(&sh, "#!/bin/sh\nexport PS1='$ '\nexec sh\n");
        std::fs::write(
            root.join("config/kraal/config.toml"),
            format!("shell = {:?}\nssh_config_hosts = false\n", sh.display().to_string()),
        )
        .unwrap();
        std::fs::write(root.join("config/kraal/workspaces/w.kdl"), "layout {\n    pane name=\"first\"\n}\n").unwrap();
        Env { root, session: name.to_string() }
    }

    fn cmd(&self, kraal: &Kraal, args: &[&str]) -> Command {
        let mut c = Command::new(&kraal.bin);
        c.env("HOME", self.root.join("home"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .args(["-s", &self.session])
            .args(args);
        c
    }

    fn try_run(&self, kraal: &Kraal, args: &[&str]) -> Output {
        self.cmd(kraal, args).output().unwrap()
    }

    fn run(&self, kraal: &Kraal, args: &[&str]) -> String {
        let out = self.try_run(kraal, args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "kraal {} {args:?} failed: {err}", kraal.version);
        String::from_utf8(out.stdout).unwrap()
    }

    fn socket(&self) -> PathBuf {
        self.root.join("run/kraal").join(format!("{}.sock", self.session))
    }

    /// The panes, as `kraal panes --json` lists them.
    fn panes(&self, kraal: &Kraal) -> Vec<serde_json::Value> {
        let listed = self.run(kraal, &["panes", "--json"]);
        serde_json::from_str::<serde_json::Value>(&listed).unwrap().as_array().unwrap().clone()
    }

    fn wait_for_text(&self, kraal: &Kraal, pane: &str, text: &str) {
        let start = Instant::now();
        loop {
            let screen = self.run(kraal, &["read", "-p", pane, "--lines", "100"]);
            if screen.contains(text) {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "{text:?} never shown in {pane}:\n{screen}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn refused_attach(&self, kraal: &Kraal, proxy: Option<&Kraal>) -> String {
        let mut command = self.cmd(kraal, if proxy.is_some() { &["attach", "--host", "box"] } else { &["attach"] });
        if let Some(proxy) = proxy {
            let bin = self.root.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            write_script(&bin.join("ssh"), "#!/bin/sh\nexec \"$COMPAT_PROXY\" -s \"$COMPAT_SESSION\" proxy\n");
            command
                .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()))
                .env("COMPAT_PROXY", &proxy.bin)
                .env("COMPAT_SESSION", &self.session);
        }
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = portable_pty::CommandBuilder::new(command.get_program());
        cmd.args(command.get_args());
        for (key, value) in command.get_envs() {
            match value {
                Some(value) => cmd.env(key, value),
                None => cmd.env_remove(key),
            }
        }
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let read = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut buf = [0; 8192];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                bytes.extend_from_slice(&buf[..n]);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        });
        let start = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if start.elapsed() > Duration::from_secs(20) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("mismatched client did not exit");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        read.join().unwrap()
    }

    /// The pid of the server listening on this session's socket.
    fn server_pid(&self) -> u32 {
        self.pong()["pid"].as_u64().unwrap() as u32
    }

    /// What the server says of itself (its answer to a ping), asked as any release asks.
    fn pong(&self) -> serde_json::Value {
        self.ask(serde_json::json!({ "cmd": "ping" }))["result"]["Ok"]["data"].clone()
    }

    /// The server's answer to `request`, asked through its socket as any release asks.
    fn ask(&self, request: serde_json::Value) -> serde_json::Value {
        self.ask_version(2, request)
    }

    fn ask_version(&self, version: u64, request: serde_json::Value) -> serde_json::Value {
        let mut stream = std::os::unix::net::UnixStream::connect(self.socket()).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let message = serde_json::json!({ "type": "request", "version": version, "request": request }).to_string();
        let mut frame = (message.len() as u32).to_le_bytes().to_vec();
        frame.push(0);
        frame.extend_from_slice(message.as_bytes());
        stream.write_all(&frame).unwrap();
        let mut header = [0u8; 5];
        stream.read_exact(&mut header).unwrap();
        let mut payload = vec![0u8; u32::from_le_bytes(header[..4].try_into().unwrap()) as usize];
        stream.read_exact(&mut payload).unwrap();
        serde_json::from_slice(&payload).unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.try_run(&Kraal::this(), &["kill"]);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Both real clients refuse a server of the other generation without changing its session.
#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn different_protocols_refuse_attachment_in_both_directions() {
    let (old, this) = (Kraal::old(), Kraal::this());
    if old.protocol == this.protocol {
        return;
    }
    for (name, server, client) in [("refuse-new", &old, &this), ("refuse-old", &this, &old)] {
        let (cp, sp) = (client.protocol, server.protocol);
        let env = Env::new(name);
        env.run(server, &["open", "w"]);
        env.run(server, &["send", "-p", "first", "--enter", "echo", "kept-$((40+2))"]);
        env.wait_for_text(server, "first", "kept-42");
        let pid = env.server_pid();
        for remote in [false, true] {
            let shown = env.refused_attach(client, remote.then_some(server));
            assert!(shown.contains(&format!("client protocol {cp}, server protocol {sp}")), "{shown}");
            let fix = if cp < sp {
                "kraal update".into()
            } else if remote {
                format!("kraal -s {name} remote-install box")
            } else {
                format!("kraal -s {name} restart")
            };
            assert!(shown.contains(&fix), "{shown}");
            if cp > sp {
                assert!(!shown.contains("\x1b[?1049h"), "refusal must precede terminal setup: {shown}");
            }
        }
        assert_eq!(env.server_pid(), pid);
        env.wait_for_text(server, "first", "kept-42");
        let refused = env.try_run(client, &["run", "--name", "forbidden", "--", "sh"]);
        assert!(!refused.status.success());
        assert!(String::from_utf8_lossy(&refused.stderr).contains("protocol mismatch"));
        assert!(!env.panes(server).iter().any(|p| p["name"] == "forbidden"));
        if cp > sp {
            let listed = env.try_run(client, &["workspaces"]);
            assert!(!listed.status.success());
            assert!(String::from_utf8_lossy(&listed.stderr).contains("protocol mismatch"));
        }
    }
}

/// Once this kraal replaces the binary of an older one whose session still runs, a production
/// command it must ask that session about is refused, as other commands are, and does nothing.
#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn production_commands_are_refused_by_a_session_of_another_protocol() {
    let (old, this) = (Kraal::old(), Kraal::this());
    if old.protocol == this.protocol {
        return;
    }
    let env = Env::new("compat-production");
    env.run(&old, &["open", "w"]);
    let (bin, reached) = (env.root.join("bin"), env.root.join("reached"));
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("ssh"), &format!("#!/bin/sh\ntouch '{}'\n", reached.display()));
    std::fs::write(
        env.root.join("config/kraal/servers.toml"),
        "[[server]]\nname = \"web-prod-1\"\nhost = \"web-prod-1\"\n",
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    // As from a pane of that session: only then is it asked (outside kraal it just goes on).
    let pane = env.panes(&old)[0]["id"].to_string();
    let out = env
        .cmd(&this, &["exec", "web-prod-1", "--", "true"])
        .env("PATH", path)
        .env("KRAAL_SOCKET", env.socket())
        .env("KRAAL_SESSION", &env.session)
        .env("KRAAL_PANE", pane.trim_matches('"'))
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && error.contains("protocol mismatch"), "{error}");
    assert!(!reached.exists(), "the command reached the server");
}

#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn this_kraal_stops_other_releases_and_restores_only_current_state() {
    let (old, this) = (Kraal::old(), Kraal::this());
    let env = Env::new("compat-kill");
    env.run(&old, &["open", "w"]);
    env.run(&old, &["rename", "-p", "first", "kept"]);
    env.run(&old, &["send", "-p", "kept", "--enter", "echo", "before-$((40+2))"]);
    env.wait_for_text(&old, "kept", "before-42");
    let stopped = env.run(&this, &["kill"]);
    assert!(stopped.contains("stopped session compat-kill"), "{stopped}");
    assert!(std::os::unix::net::UnixStream::connect(env.socket()).is_err());
    let saved = env.root.join("state/kraal").join(old.state_dir()).join("compat-kill.json");
    let original = std::fs::read(&saved).unwrap();
    if old.protocol == 2 {
        let scrollback = saved_scrollback(&env, "scrollback");
        assert!(!scrollback.is_empty());
        for args in [&["open", "w"][..], &["workspaces", "restore", "w"]] {
            let out = env.try_run(&this, args);
            let error = String::from_utf8_lossy(&out.stderr);
            assert!(!out.status.success() && error.trim_end().ends_with("run kraal 0.4 once first"), "{error}");
        }
        let shown = env.refused_attach(&this, None);
        assert!(shown.trim_end().ends_with("run kraal 0.4 once first"), "{shown}");
        assert_eq!(std::fs::read(saved).unwrap(), original);
        assert_eq!(saved_scrollback(&env, "scrollback"), scrollback);
        assert!(!env.root.join("state/kraal/sessions-v2/compat-kill.json").exists());
        return;
    }
    env.run(&this, &["open", "w"]);
    env.wait_for_text(&this, "kept", "before-42");
    let before: serde_json::Value = serde_json::from_slice(&original).unwrap();
    env.run(&this, &["kill"]);
    let after: serde_json::Value = serde_json::from_slice(&std::fs::read(saved).unwrap()).unwrap();
    assert_eq!(after["workspaces"][0]["id"], before["workspaces"][0]["id"]);
}

/// A session one release runs, and listens for, is neither started again nor forgotten by the
/// other in the runtime directory they share, whichever of the two runs it: one command after
/// the other here, not what two of them do at the same instant.
#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn neither_release_starts_or_forgets_a_session_the_other_runs() {
    let (old, this) = (Kraal::old(), Kraal::this());
    for (name, runs, other) in [("compat-held-a", &old, &this), ("compat-held-b", &this, &old)] {
        let env = Env::new(name);
        env.run(runs, &["open", "w"]);
        let pid = env.server_pid();
        let dir = runs.state_dir();
        let saved = env.root.join("state/kraal").join(dir).join(format!("{name}.json"));
        let start = Instant::now();
        while !saved.exists() {
            assert!(start.elapsed() < Duration::from_secs(60), "the layout was never saved");
            std::thread::sleep(Duration::from_millis(50));
        }
        // Each ends by itself, refused: a second server that started would run until it is
        // killed here, and so would a `forget` that waited.
        let refused = |args: &[&str]| {
            let mut command = env.cmd(other, args).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
            let start = Instant::now();
            loop {
                if let Some(status) = command.try_wait().unwrap() {
                    break !status.success();
                }
                if start.elapsed() > Duration::from_secs(60) {
                    let _ = command.kill();
                    let _ = command.wait();
                    break false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let (theirs, ours) = (&other.version, &runs.version);
        assert!(refused(&["server", "--session", name]), "kraal {theirs} started a second server beside {ours}");
        assert!(refused(&["forget"]) && saved.exists(), "kraal {theirs} forgot a session kraal {ours} runs");
        assert_eq!(env.server_pid(), pid, "the session's server");
        env.run(runs, &["kill"]);
    }
}

#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn old_accounts_are_refused_without_conversion() {
    let (old, this) = (Kraal::old(), Kraal::this());
    if old.protocol != 2 {
        return;
    }
    let env = Env::new("compat-accounts");
    let file = env.root.join("config/kraal/accounts.toml");
    std::fs::write(&file, "[[account]]\nname = 'work'\nagent = 'claude'\nhome = '/work'\n").unwrap();
    env.run(&old, &["accounts", "limits", "--threshold", "75"]);
    let source = std::fs::read(&file).unwrap();
    for args in [&["accounts", "list"][..], &["accounts", "limits", "--threshold", "70"]] {
        let out = env.try_run(&this, args);
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && error.trim_end().ends_with("run kraal 0.4 once first"), "{error}");
        assert_eq!(std::fs::read(&file).unwrap(), source);
        assert!(!file.with_extension("toml.kraal-bak").exists());
    }
}

/// `kraal restart` brings a session of the other release to this kraal, with its panes and what
/// they showed.
#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn restarting_requires_state_from_at_least_04() {
    let (old, this) = (Kraal::old(), Kraal::this());
    let env = Env::new("compat-restart");
    env.run(&old, &["open", "w"]);
    env.run(&old, &["send", "-p", "first", "--enter", "echo", "before-$((40+2))"]);
    env.wait_for_text(&old, "first", "before-42");
    let before = env.server_pid();
    let listed = env.run(&this, &["ls"]);
    assert!(listed.contains(&old.version), "{listed}");
    if old.protocol == 2 {
        for args in [&["restart"][..], &["restart", "--yes"]] {
            let out = env.try_run(&this, args);
            let error = String::from_utf8_lossy(&out.stderr);
            assert!(!out.status.success() && error.trim_end().ends_with("run kraal 0.4 once first"), "{error}");
            assert_eq!(env.server_pid(), before);
            env.wait_for_text(&old, "first", "before-42");
        }
        return;
    }
    let out = env.run(&this, &["restart", "--yes"]);
    assert!(out.contains(&format!("restarted session {}", env.session)), "{out}");
    assert_ne!(env.server_pid(), before, "the same server still runs");
    let pong = env.pong();
    assert_eq!(pong["version"], this.version.as_str(), "{pong}");
    assert_eq!(pong["protocol"], this.protocol, "the session runs this kraal: {pong}");
    env.wait_for_text(&this, "first", "before-42");
    env.run(&this, &["send", "-p", "first", "--enter", "echo", "after-$((40+2))"]);
    env.wait_for_text(&this, "first", "after-42");
    // This server writes the newer state itself, so later restarts keep using it.
    env.run(&this, &["restart", "--yes"]);
    env.wait_for_text(&this, "first", "after-42");
    let ls = env.run(&this, &["ls"]);
    assert!(ls.contains(&format!("running  {}", this.version)), "{ls}");
}

fn saved_scrollback(env: &Env, folder: &str) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    let dir = env.root.join("state/kraal").join(folder).join(&env.session);
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .map(|entry| {
            let path = entry.unwrap().path();
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect()
}

#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn a_new_empty_session_is_invisible_to_an_older_release() {
    let (old, this) = (Kraal::old(), Kraal::this());
    if old.protocol != 2 {
        return;
    }
    let env = Env::new("compat-new-state");
    env.run(&this, &["open", "w"]);
    env.run(&this, &["close", "first"]);
    env.run(&this, &["kill"]);
    let current = env.root.join("state/kraal/sessions-v2").join(format!("{}.json", env.session));
    let saved = std::fs::read(&current).unwrap();
    let state: serde_json::Value = serde_json::from_slice(&saved).unwrap();
    assert_eq!(state["workspaces"][0]["empty"], true);
    assert_eq!(state["workspaces"][0]["tabs"], serde_json::json!([]));
    let listed = env.run(&old, &["ls"]);
    assert!(!listed.contains(&env.session), "{listed}");
    env.run(&old, &["forget"]);
    assert_eq!(std::fs::read(&current).unwrap(), saved);
    env.run(&old, &["open", "w"]);
    assert_eq!(env.panes(&old)[0]["name"], "first", "the old release opens its KDL");
    env.run(&old, &["kill"]);
    env.run(&old, &["forget"]);
    assert_eq!(std::fs::read(&current).unwrap(), saved);
    env.run(&this, &["open", "w"]);
    assert!(env.panes(&this).is_empty());
    let workspaces: serde_json::Value = serde_json::from_str(&env.run(&this, &["workspaces", "--json"])).unwrap();
    assert_eq!(workspaces[0]["id"], state["workspaces"][0]["id"]);
}
