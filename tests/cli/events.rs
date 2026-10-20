use super::support::{RawClient, finish, raw_frame};
use crate::common::Env;
use std::time::{Duration, Instant};

#[test]
fn events_stream_and_hooks() {
    let env = Env::new("events");
    let cfg = env.root.join("config/kraal/config.toml");
    let marker = env.root.join("hook.txt");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!(
        "\n[hooks]\n\"pane.failed\" = 'echo \"$KRAAL_EVENT $KRAAL_CODE $KRAAL_NAME\" > {}'\n",
        marker.display()
    ));
    std::fs::write(&cfg, text).unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    // From the start of the session: nothing is missed, however late the subscription is live.
    env.run(&["run", "--name", "job", "--", "sh", "-c", "exit 3"]);
    let (mut events, lines) = env.follow(&["events", "--since", "0", "pane.*"]);
    let mut seen = Vec::new();
    while seen.len() < 3 {
        let line = lines.recv_timeout(Duration::from_secs(10)).expect("an event in time");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if v["name"] != "job" {
            continue;
        }
        seen.push(v["type"].as_str().unwrap().to_string());
        if v["type"] == "pane.exited" {
            assert_eq!(v["code"], 3, "{line}");
            assert_eq!(v["command"][0], "sh", "{line}");
        }
    }
    assert_eq!(seen, vec!["pane.opened", "pane.exited", "pane.failed"]);
    let _ = events.kill();
    let _ = events.wait();
    let start = Instant::now();
    while crate::common::read_complete_lines(&marker).is_none() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(crate::common::read_complete_lines(&marker).unwrap().trim(), "pane.failed 3 job");
    // Live events come too, after the replay.
    let (mut live, lines) = env.follow(&["events", "--since", "0", "pane.failed"]);
    let line = lines.recv_timeout(Duration::from_secs(10)).expect("replayed event");
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!((v["type"].as_str(), v["name"].as_str()), (Some("pane.failed"), Some("job")), "{line}");
    assert!(v["seq"].as_u64().is_some() && v["boot"].as_u64().is_some(), "{line}");
    env.run(&["run", "--name", "again", "--", "sh", "-c", "exit 4"]);
    let line = lines.recv_timeout(Duration::from_secs(10)).expect("live event");
    assert!(line.contains("\"again\""), "{line}");
    let _ = live.kill();
    let _ = live.wait();
    // A replay longer than a client's queue still arrives whole.
    for i in 0..40 {
        env.run(&["run", "--name", &format!("many{i}"), "-x", "--", "true"]);
    }
    let (mut all, lines) = env.follow(&["events", "--since", "0", "pane.opened"]);
    let mut opened = 0;
    while opened < 40 {
        let line = lines.recv_timeout(Duration::from_secs(10)).expect("the whole replay");
        if line.contains("\"many") {
            opened += 1;
        }
    }
    let _ = all.kill();
    let _ = all.wait();
    let out = env.try_run(&["events", "pane.exitd"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no such event"));
}

/// The lines of `out` as they come, on the receiver: the first one, then the others once
/// something is sent to the sender. Until then, what writes to `out` has a reader that reads
/// nothing.
fn lines_when_asked(
    out: impl std::io::Read + Send + 'static,
) -> (std::sync::mpsc::Receiver<String>, std::sync::mpsc::Sender<()>) {
    use std::io::BufRead;
    let (tx, lines) = std::sync::mpsc::channel();
    let (go, asked) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = std::io::BufReader::new(out).lines().map_while(Result::ok);
        if let Some(first) = out.next() {
            let _ = tx.send(first);
        }
        let _ = asked.recv();
        for line in out {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (lines, go)
}

/// `kraal notify`, asked through the session's socket: no process to start for each one.
fn notify(env: &Env, title: &str, body: &str) {
    use std::io::Read;
    let socket = env.root.join("run/kraal").join(format!("{}.sock", env.session));
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    let request = serde_json::json!({ "cmd": "notify", "title": title, "body": body });
    let message = serde_json::json!({ "type": "request", "version": 6, "request": request });
    raw_frame(&mut stream, 0, message.to_string().as_bytes());
    // Answered: the event is out.
    stream.read_exact(&mut [0u8; 5]).unwrap();
}

/// A `kraal events` reader that falls too far behind is dropped, and told: after the events it
/// was sent, a last line says so and where to go on from, and the command fails with the reason.
/// Going on from there, it has missed nothing.
#[test]
fn a_reader_too_slow_for_the_events_is_told_and_misses_nothing() {
    use std::process::Stdio;
    use std::sync::mpsc::RecvTimeoutError;
    let env = Env::new("slowreader");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    notify(&env, "first", "");
    let mut reader = env
        .cmd(&["events", "--since", "0", "notification"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (lines, go) = lines_when_asked(reader.stdout.take().unwrap());
    // It follows the session (it read the event from before), then reads nothing while far more
    // happens than the system and kraal keep waiting for a reader.
    let first = lines.recv_timeout(Duration::from_secs(10)).expect("the event from before");
    assert!(first.contains(r#""title":"first""#), "{first}");
    const SENT: usize = 300;
    let body = "x".repeat(16 << 10);
    for i in 0..SENT {
        notify(&env, &i.to_string(), &body);
    }
    go.send(()).unwrap();
    let mut read: Vec<serde_json::Value> = Vec::new();
    loop {
        match lines.recv_timeout(Duration::from_secs(20)) {
            Ok(line) => read.push(serde_json::from_str(&line).unwrap()),
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => panic!("the stream never ended, {} lines read", read.len()),
        }
    }
    // The events it was sent, in order, then the last line.
    let last = read.pop().expect("a last line");
    let titles: Vec<&str> = read.iter().filter_map(|e| e["title"].as_str()).collect();
    assert_eq!(titles, (0..read.len()).map(|i| i.to_string()).collect::<Vec<_>>());
    assert!(read.len() < SENT, "never dropped");
    assert_eq!((last["type"].as_str(), last["reason"].as_str()), (Some("events.dropped"), Some("too slow")), "{last}");
    let (since, boot) = (last["since"].as_u64().unwrap(), last["boot"].as_u64().unwrap());
    let before = read.last().expect("some events before it was dropped");
    assert!(before["seq"].as_u64().unwrap() <= since && before["boot"] == boot, "{last}");
    let out = finish(reader, Duration::from_secs(10));
    let said = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(said.contains("too slow") && said.contains(&format!("--since {since} --boot {boot}")), "{said}");
    // From where the last line says: every event it was not sent.
    let (since, boot) = (since.to_string(), boot.to_string());
    let (mut rest, lines) = env.follow(&["events", "--since", &since, "--boot", &boot, "notification"]);
    for i in read.len()..SENT {
        let line = lines.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| panic!("event {i} never came"));
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(event["title"], i.to_string(), "{} {}", event["type"], event["seq"]);
    }
    let _ = rest.kill();
    let _ = rest.wait();
}

/// What else ends a `kraal events` stream ends it as before, without a word and with success:
/// the session stopped. Detaching or closing its last pane ends no stream.
#[test]
fn the_event_stream_ends_quietly_with_the_session() {
    use std::process::Stdio;
    let env = Env::new("eventsend");
    env.workspace("w", "layout {\n    pane name=\"only\"\n}\n");
    for end in [&["kill"][..], &["close", "only"]] {
        env.run(&["open", "w"]);
        let mut reader =
            env.cmd(&["events", "notification"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let (lines, go) = lines_when_asked(reader.stdout.take().unwrap());
        go.send(()).unwrap();
        let mut client = RawClient::attach(&env, "w");
        client.wait_for_message("\"attached\"");
        raw_frame(&mut client.stream, 0, br#"{"type":"detach"}"#);
        client.wait_for_message("detached");
        // The reader may not be following yet: said until it reads it.
        let start = Instant::now();
        loop {
            notify(&env, "still", "there");
            if lines.recv_timeout(Duration::from_millis(100)).is_ok() {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "no event after a terminal detached");
        }
        env.run(end);
        if end[0] == "close" {
            assert!(reader.try_wait().unwrap().is_none(), "the empty session still owns the stream");
            env.run(&["kill"]);
        }
        let out = finish(reader, Duration::from_secs(10));
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && said.is_empty(), "after {end:?}: {:?} {said}", out.status);
        assert!(lines.iter().all(|line| line.contains(r#""title":"still""#)), "after {end:?}");
    }
}
