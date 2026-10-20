use super::support::{PATIENCE, RawClient, wait_production};
#[cfg(target_os = "linux")]
use super::support::{TerminalClient, raw_frame, wait_gone};
use crate::common::{Env, wait_until};
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[test]
fn a_long_paste_into_production_is_asked_about_and_sent_whole() {
    let env = Env::new("prodpaste");
    let tf = env.root.join("infra");
    std::fs::create_dir_all(tf.join(".terraform")).unwrap();
    std::fs::write(tf.join(".terraform/environment"), "prod\n").unwrap();
    let got = env.root.join("pasted");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    // A program that takes 2 MB as they come (no line editing), then says it has them all.
    let script = format!("stty raw -echo; head -c 2000000 > {}; echo; echo GOT-ALL; sleep 60", got.display());
    env.run(&["run", "--cwd", &tf.display().to_string(), "--name", "ops", "--", "sh", "-c", &script]);
    wait_production(&env, "ops");
    env.run(&["focus", "ops"]);
    let mut client = RawClient::attach(&env, "w");
    // 20000 lines, more than the pieces a paste is passed on in.
    let data = ("x".repeat(99) + "\n").repeat(20000);
    client.paste(data.as_bytes());
    // One question about all of it, and nothing sent before the answer.
    client.wait_for_screen("Paste 20000 lines into production");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(std::fs::metadata(&got).map(|m| m.len()).unwrap_or(0), 0);
    client.send(b"y");
    env.wait_for_text("ops", "GOT-ALL");
    assert_eq!(std::fs::metadata(&got).unwrap().len(), 2_000_000);
}

#[test]
fn a_long_paste_keeps_going_where_it_started_and_never_into_production() {
    let env = Env::new("streampaste");
    let tf = env.root.join("infra");
    std::fs::create_dir_all(tf.join(".terraform")).unwrap();
    std::fs::write(tf.join(".terraform/environment"), "prod\n").unwrap();
    let (got, prod_got) = (env.root.join("pasted"), env.root.join("prod-pasted"));
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let take = |file: &Path| format!("stty raw -echo && cat > {}", file.display());
    env.run(&["run", "--cwd", &tf.display().to_string(), "--name", "ops", "--", "sh", "-c", &take(&prod_got)]);
    wait_production(&env, "ops");
    env.run(&["run", "--name", "sink", "--", "sh", "-c", &take(&got)]);
    env.run(&["focus", "sink"]);
    // The panes exist before their programs have entered raw mode and opened the files.
    wait_until(PATIENCE, "the paste receivers never became ready", || got.exists() && prod_got.exists());
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("sink");
    // More than kraal holds back (8 MiB): it goes out as it comes.
    let line = "x".repeat(99) + "\n";
    client.send(b"\x1b[200~");
    for chunk in line.repeat(90_000).as_bytes().chunks(64 << 10) {
        client.send(chunk);
    }
    let size = |f: &Path| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
    let start = Instant::now();
    while size(&got) == 0 {
        assert!(start.elapsed() < PATIENCE, "the paste never started going out:\n{}", client.rows().join("\n"));
        std::thread::sleep(Duration::from_millis(50));
    }
    // The focus moves to a production pane while the rest comes: the rest still goes where the
    // paste started, never there.
    env.run(&["focus", "ops"]);
    for chunk in line.repeat(10_000).as_bytes().chunks(64 << 10) {
        client.send(chunk);
    }
    client.send(b"\x1b[201~");
    let start = Instant::now();
    while size(&got) < 10_000_000 {
        assert!(start.elapsed() < PATIENCE, "only {} bytes arrived:\n{}", size(&got), client.rows().join("\n"));
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(size(&got), 10_000_000);
    assert_eq!(size(&prod_got), 0, "nothing of the paste reached production");
}

#[test]
fn a_paste_held_until_its_end_goes_where_it_started() {
    let env = Env::new("heldpaste");
    let (got, other_got) = (env.root.join("pasted"), env.root.join("other-pasted"));
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let take = |file: &Path| format!("stty raw -echo; cat > {}", file.display());
    env.run(&["run", "--name", "sink", "--", "sh", "-c", &take(&got)]);
    env.run(&["run", "--name", "other", "--", "sh", "-c", &take(&other_got)]);
    env.run(&["focus", "sink"]);
    let mut client = RawClient::attach(&env, "w");
    // Less than kraal holds back (8 MiB): it goes whole once it ends.
    let line = "x".repeat(99) + "\n";
    client.send(b"\x1b[200~");
    for chunk in line.repeat(10_000).as_bytes().chunks(64 << 10) {
        client.send(chunk);
    }
    std::thread::sleep(Duration::from_millis(300));
    // The focus moves before its end: all of it still goes where it started.
    env.run(&["focus", "other"]);
    client.send(b"end\x1b[201~");
    let size = |f: &Path| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
    let start = Instant::now();
    while size(&got) < 1_000_003 && size(&other_got) == 0 {
        assert!(start.elapsed() < Duration::from_secs(10), "only {} bytes arrived", size(&got));
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!((size(&got), size(&other_got)), (1_000_003, 0));
}

#[test]
fn copy_mode_releases_the_button_the_program_saw_pressed() {
    let env = Env::new("copyrelease");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    // A program that asks for SGR mouse reports and shows what it gets.
    env.run(&["run", "--name", "app", "--", "sh", "-c", "stty raw -echo; printf '\\033[?1000h\\033[?1006h'; cat -v"]);
    env.run(&["focus", "app"]);
    std::thread::sleep(Duration::from_millis(300));
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("app");
    // The right button goes down over the program, copy mode starts (ctrl+b v), the wheel
    // turns: the program is told the right button is up, and nothing more.
    client.send(b"\x1b[<2;110;20M");
    env.wait_for_text("app", "<2;");
    client.send(b"\x02v");
    std::thread::sleep(Duration::from_millis(200));
    client.send(b"\x1b[<64;110;20M");
    let screen = env.wait_for_text("app", "m");
    let reports: Vec<&str> = screen.split("^[[").filter(|r| r.starts_with('<')).collect();
    assert!(reports.iter().any(|r| r.starts_with("<2;") && r.trim_end().ends_with('m')), "{screen}");
    assert!(!reports.iter().any(|r| r.starts_with("<0;")), "{screen}");
}

#[test]
fn a_pane_is_told_when_the_terminal_loses_and_regains_the_focus() {
    let env = Env::new("focusreports");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    // A program that asks for focus reports and shows what it gets.
    env.run(&["run", "--name", "app", "--", "sh", "-c", "stty raw -echo; printf '\\033[?1004h'; cat -v"]);
    std::thread::sleep(Duration::from_millis(300));
    let mut client = RawClient::attach(&env, "w");
    env.wait_for_text("app", "^[[I");
    // Out and back in, in one read: both are told, in order.
    client.send(b"\x1b[O\x1b[I");
    env.wait_for_text("app", "^[[I^[[O^[[I");
    // The same when the focus moves away and back by commands.
    env.run(&["focus", "main"]);
    env.run(&["focus", "app"]);
    env.wait_for_text("app", "^[[I^[[O^[[I^[[O^[[I");
}

/// However a client leaves, it gives its terminal back once, and in order: detach, a signal
/// (also while frames are on their way), the server dying, quit.
#[cfg(target_os = "linux")]
#[test]
fn a_client_gives_its_terminal_back_once_however_it_leaves() {
    let env = Env::new("giveback");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let mut client = TerminalClient::attach(&env);
    assert!(!client.line_mode(), "raw mode while attached");
    client.type_keys(b"\x02d");
    assert_eq!(client.left("detach"), (0, "[kraal: detached]\r\n".to_string()));

    for (signal, name) in [(libc::SIGTERM, "SIGTERM"), (libc::SIGHUP, "SIGHUP"), (libc::SIGINT, "SIGINT")] {
        let client = TerminalClient::attach(&env);
        client.signal(signal);
        assert_eq!(client.left(name), (128 + signal as u32, String::new()));
    }

    // A frame on its way when the signal comes is written whole, and none follows.
    env.run(&["send", "-p", "shell", "--enter", "seq 1 100000000"]);
    let client = TerminalClient::attach(&env);
    client.wait_for_frames(20);
    client.signal(libc::SIGTERM);
    assert_eq!(client.left("SIGTERM while a pane prints"), (128 + libc::SIGTERM as u32, String::new()));
    env.run(&["keys", "-p", "shell", "ctrl+c"]);

    let client = TerminalClient::attach(&env);
    let server = env.server_pid().expect("the server runs");
    // SAFETY: plain syscall to the server this test started.
    unsafe { libc::kill(server, libc::SIGKILL) };
    let (status, said) = client.left("the server dies");
    assert!(status == 0 && said.starts_with("[kraal: the server went away"), "{status} {said:?}");
    wait_gone(&PathBuf::from(format!("/proc/{server}")));

    env.run(&["open", "w"]);
    let mut client = TerminalClient::attach(&env);
    client.type_keys(b"\x02q");
    client.wait_for("Quit kraal?");
    client.type_keys(b"y");
    let (status, said) = client.left("quit");
    assert!(status == 0 && said.starts_with("[kraal: stopped; kraal -s giveback starts it again"), "{status} {said:?}");
}

/// The server stops for a restart and none comes back: the client turned its input modes off
/// for the wait, and giving the wait up (ctrl+c, or a signal) does not turn them off again.
#[cfg(target_os = "linux")]
#[test]
fn a_restart_wait_given_up_gives_the_terminal_back_once() {
    for by_signal in [false, true] {
        let env = Env::new(if by_signal { "waitsignal" } else { "waitkey" });
        env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
        env.run(&["open", "w"]);
        let mut client = TerminalClient::attach(&env);
        let socket = env.root.join("run/kraal").join(format!("{}.sock", env.session));
        let mut asking = std::os::unix::net::UnixStream::connect(socket).unwrap();
        raw_frame(&mut asking, 0, br#"{"type":"request","version":6,"request":{"cmd":"kill_server","restart":true}}"#);
        client.wait_for("kraal: the session restarts");
        if by_signal {
            client.signal(libc::SIGINT);
            assert_eq!(client.left("SIGINT during the wait"), (128 + libc::SIGINT as u32, String::new()));
        } else {
            client.type_keys(b"\x03");
            let said = "[kraal: gave up waiting for the session to restart]\r\n";
            assert_eq!(client.left("ctrl+c during the wait"), (0, said.to_string()));
        }
    }
}

/// A signal that comes while the client is leaving anyway finds the terminal given back, or
/// gives it back itself: never both.
#[cfg(target_os = "linux")]
#[test]
fn a_signal_that_meets_a_normal_exit_gives_the_terminal_back_once() {
    let env = Env::new("exitsignal");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    // The client leaves some 0.2 ms after the keys: the signal is sent all around that.
    for delay in (0..500).step_by(20).map(Duration::from_micros) {
        let mut client = TerminalClient::attach(&env);
        client.type_keys(b"\x02d");
        let typed = Instant::now();
        while typed.elapsed() < delay {
            std::hint::spin_loop();
        }
        client.signal(libc::SIGTERM);
        let (status, said) = client.left(&format!("SIGTERM {delay:?} after a detach"));
        // Keys the signal overtook are still in the terminal, which shows them as a shell's
        // would; a goodbye is whole, or unsaid.
        let said_by_signal = ["", "^Bd", "[kraal: detached]\r\n"];
        let as_expected = match status {
            0 => said == "[kraal: detached]\r\n",
            status => status == 128 + libc::SIGTERM as u32 && said_by_signal.contains(&said.as_str()),
        };
        assert!(as_expected, "{delay:?}: {status} {said:?}");
    }
}
