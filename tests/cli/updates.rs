#[cfg(target_os = "linux")]
use super::support::in_terminal;
use super::support::{RawClient, copy_program, fake_release, fake_release_of};
use crate::common::Env;
use std::process::Command;
use std::time::{Duration, Instant};

/// `kraal update` puts a release that matches its checksum in place of itself, and leaves
/// itself alone when the checksum is wrong.
#[test]
fn update_installs_a_checked_release_in_place() {
    let env = Env::new("update");
    let release = env.root.join("release");
    let asset = fake_release(&release);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let kraal = bin.join("kraal");
    copy_program(env!("CARGO_BIN_EXE_kraal"), &kraal);
    let url = format!("file://{}", release.display());
    let update =
        |args: &[&str]| env.isolated(&kraal).arg("update").args(args).env("KRAAL_DOWNLOAD_URL", &url).output().unwrap();

    let same = update(&[]);
    assert!(same.status.success(), "{}", String::from_utf8_lossy(&same.stderr));
    assert!(String::from_utf8_lossy(&same.stdout).contains("already installed"));

    let before = std::fs::metadata(&kraal).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let forced = update(&["--force"]);
    assert!(forced.status.success(), "{}", String::from_utf8_lossy(&forced.stderr));
    assert_ne!(std::fs::metadata(&kraal).unwrap().modified().unwrap(), before, "replaced");
    assert!(Command::new(&kraal).arg("--version").status().unwrap().success());
    let left: Vec<_> = std::fs::read_dir(&bin).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left, ["kraal"], "nothing left beside it");

    std::fs::write(release.join(format!("{asset}.sha256")), format!("{}  {asset}\n", "0".repeat(64))).unwrap();
    let before = std::fs::metadata(&kraal).unwrap().modified().unwrap();
    let wrong = update(&["--force"]);
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("does not match its checksum"));
    assert_eq!(std::fs::metadata(&kraal).unwrap().modified().unwrap(), before, "left alone");
}

/// After an update, `kraal update` names the sessions still on an older kraal with what a
/// restart stops in them, asks on a terminal (no by default when programs would stop), and has
/// the new kraal restart them when told to.
#[test]
fn update_offers_to_restart_the_sessions_left_on_an_older_kraal() {
    let env = Env::new("updrestart");
    env.workspace(
        "w",
        "layout {\n    pane name=\"job\" command=\"sh\" { args \"-c\" \"echo started; exec sleep 4243\"; }\n}\n",
    );
    env.run(&["open", "w"]);
    env.wait_for_text("job", "started");
    let release = env.root.join("release");
    fake_release_of(&release, Some("9.9.9"));
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let kraal = bin.join("kraal");
    // Each update replaces this copy with the release's kraal: put this build back first.
    let install = || {
        let _ = std::fs::remove_file(&kraal);
        copy_program(env!("CARGO_BIN_EXE_kraal"), &kraal);
    };
    let url = format!("file://{}", release.display());
    let update = |args: &[&str]| {
        let mut c = env.isolated(&kraal);
        c.arg("update").args(args).env("KRAAL_DOWNLOAD_URL", &url);
        c
    };
    let version = env!("CARGO_PKG_VERSION");
    let pid = env.server_pid().expect("the server runs");

    // Without a terminal to ask on, and without --restart: said, not done.
    install();
    let out = update(&[]).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{said}{}", String::from_utf8_lossy(&out.stderr));
    assert!(said.contains(&format!("Session updrestart still runs kraal {version}.")), "{said}");
    assert!(said.contains("These programs stop:") && said.contains("\"job\" in w)"), "{said}");
    assert!(said.contains(&format!("Session updrestart keeps kraal {version} until: kraal -s updrestart restart")));
    assert_eq!(env.server_pid(), Some(pid));

    // On a terminal: asked, and Enter says no, since a program would stop.
    #[cfg(target_os = "linux")]
    {
        install();
        let screen = in_terminal(update(&[]), b"\n");
        assert!(screen.contains("Restart it on kraal 9.9.9 now? [y/N]"), "{screen}");
        assert!(screen.contains("keeps kraal"), "{screen}");
        assert_eq!(env.server_pid(), Some(pid));
        // --no-restart answers beforehand.
        install();
        let screen = in_terminal(update(&["--no-restart"]), b"");
        assert!(screen.contains("keeps kraal") && !screen.contains("[y/N]"), "{screen}");
        assert_eq!(env.server_pid(), Some(pid));
    }

    // --restart: the kraal just installed restarts it.
    install();
    let out = update(&["--restart"]).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{said}{}", String::from_utf8_lossy(&out.stderr));
    assert!(said.contains("restarted session updrestart"), "{said}");
    let new = env.server_pid().expect("a server runs again");
    assert_ne!(new, pid);
    env.wait_for_text_since_restore("job", "restored session");
}

/// The server says what it runs to a client that attaches, and says once, to a client that
/// has a newer kraal installed, that a restart brings it. `kraal ls` shows each session's.
#[test]
fn a_client_with_a_newer_kraal_installed_is_told_the_session_runs_an_older_one() {
    let env = Env::new("notice");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let version = env!("CARGO_PKG_VERSION");
    let newer = RawClient::attach_installed(&env, "w", Some("99.0.0"));
    newer.wait_for_message(&format!(r#"{{"type":"attached","version":"{version}","protocol":6"#));
    newer.wait_for_screen(&format!("kraal 99.0.0 installed; this session runs {version}: ctrl+b W r restarts it"));
    let same = RawClient::attach_installed(&env, "w", Some(version));
    same.wait_for_message("\"attached\"");
    let older = RawClient::attach_installed(&env, "w", Some("0.0.1"));
    older.wait_for_message("\"attached\"");
    std::thread::sleep(Duration::from_millis(500));
    for client in [&same, &older] {
        assert!(!String::from_utf8_lossy(&client.screen.lock().unwrap()).contains(" installed; "));
    }
    let ls = env.run(&["ls"]);
    assert!(ls.lines().any(|l| l.starts_with("notice ") && l.ends_with(&format!("running  {version}"))), "{ls}");
    let keys = env.run(&["config", "keys"]);
    assert!(keys.lines().any(|l| l.trim_start().starts_with("restart ") && l.ends_with("ctrl+b W r")), "{keys}");
}

/// The restart in the menu says what stops and asks first; then the session comes back, and
/// attached clients are told to come back to the workspace they showed.
#[test]
fn the_menu_restarts_the_session_after_asking() {
    let env = Env::new("menurestart");
    env.workspace(
        "w",
        "layout {\n    pane name=\"job\" command=\"sh\" { args \"-c\" \"echo started; exec sleep 4244\"; }\n}\n",
    );
    env.run(&["open", "w"]);
    env.wait_for_text("job", "started");
    let pid = env.server_pid().expect("the server runs");
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wr");
    client.wait_for_screen("Restart session menurestart on the kraal installed now?");
    client.wait_for_screen("These programs stop:");
    client.wait_for_screen("\"job\" in w)");
    client.send(b"n");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(env.server_pid(), Some(pid), "no is no");
    client.screen.lock().unwrap().clear();
    client.send(b"\x02Wr");
    client.wait_for_screen("Restart session menurestart");
    client.send(b"y");
    client.wait_for_message(r#""reattach":"w""#);
    let start = Instant::now();
    while env.server_pid().is_none_or(|p| p == pid) {
        assert!(start.elapsed() < Duration::from_secs(20), "the session did not come back");
        std::thread::sleep(Duration::from_millis(50));
    }
    env.wait_for_text_since_restore("job", "restored session");
}

/// install.sh installs a release that matches its checksum, and nothing when it does not.
#[test]
fn install_script_installs_a_checked_release() {
    let env = Env::new("installsh");
    let release = env.root.join("release");
    let asset = fake_release(&release);
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh");
    let dir = env.root.join("bin");
    let install = || {
        Command::new("sh")
            .arg(script)
            .env("KRAAL_DOWNLOAD_URL", format!("file://{}", release.display()))
            .env("KRAAL_INSTALL_DIR", &dir)
            .output()
            .unwrap()
    };
    let done = install();
    assert!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));
    assert!(String::from_utf8_lossy(&done.stderr).contains("Installed kraal"));
    assert!(Command::new(dir.join("kraal")).arg("--version").status().unwrap().success());

    std::fs::remove_file(dir.join("kraal")).unwrap();
    std::fs::write(release.join(format!("{asset}.sha256")), format!("{}  {asset}\n", "0".repeat(64))).unwrap();
    let wrong = install();
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("does not match its checksum"));
    assert!(!dir.join("kraal").exists(), "nothing installed");
}
