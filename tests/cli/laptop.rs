use super::support::{PATIENCE, RawClient, fake_ssh, raw_frame};
use crate::common::{Env, wait_until, write_script};

#[test]
fn copy_job_output_describes_both_directions_and_finishes_below_progress() {
    use base64::Engine;
    use std::io::Read;
    use std::os::unix::net::{UnixListener, UnixStream};

    fn read(stream: &mut UnixStream) -> serde_json::Value {
        let mut header = [0; 5];
        stream.read_exact(&mut header).unwrap();
        assert_eq!(header[4], 0);
        let mut bytes = vec![0; u32::from_le_bytes(header[..4].try_into().unwrap()) as usize];
        stream.read_exact(&mut bytes).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["type"], "laptop_copy");
        assert_eq!(value["token"], "abc");
        value["message"].clone()
    }

    /// The next message other than the keepalive a slow installation sends.
    fn read_past_progress(stream: &mut UnixStream) -> serde_json::Value {
        loop {
            let message = read(stream);
            if message != "Progress" {
                return message;
            }
        }
    }

    fn reply(stream: &mut UnixStream, message: serde_json::Value) {
        let message = serde_json::json!({"type":"laptop_copy", "token":"abc", "message":message});
        raw_frame(stream, 0, message.to_string().as_bytes());
    }

    for to_laptop in [false, true] {
        for entries in [1, 2] {
            for (bytes, size) in [(0, "0 B"), (3, "3 B"), (10 * 1024 * 1024, "10 MiB")] {
                let env = Env::new(&format!("copy-output-{to_laptop}-{entries}-{bytes}"));
                let source = env.root.join("source");
                let destination = env.root.join("destination");
                std::fs::create_dir(&source).unwrap();
                std::fs::create_dir(&destination).unwrap();
                let contents = vec![b'x'; bytes];
                std::fs::write(source.join("report"), &contents).unwrap();
                let mut sources = vec![source.join("report")];
                if entries == 2 {
                    std::fs::write(source.join("empty"), []).unwrap();
                    sources.push(source.join("empty"));
                }
                let sockets = env.root.join("run/kraal");
                std::fs::create_dir(&sockets).unwrap();
                let listener = UnixListener::bind(sockets.join(format!("{}.sock", env.session))).unwrap();
                let mut command = env.cmd(&["laptop-cp"]);
                command.env("KRAAL_LAPTOP_COPY_TOKEN", "abc");
                let job = std::thread::spawn(move || command.output().unwrap());
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(PATIENCE)).unwrap();
                assert!(read(&mut stream)["Open"].is_object());
                reply(
                    &mut stream,
                    serde_json::json!({"Ready": {
                        "to_laptop":to_laptop, "remote":null, "sources":sources,
                        "destination":destination, "allow":[]
                    }}),
                );
                if to_laptop {
                    reply(&mut stream, serde_json::json!({"Ack":0}));
                    let mut received = Vec::new();
                    let mut sequence = 0;
                    loop {
                        let message = read(&mut stream);
                        assert_eq!(message["Data"]["sequence"], sequence);
                        let packet = &message["Data"]["packet"];
                        if let Some(chunk) = packet["Chunk"].as_str() {
                            received.extend(base64::engine::general_purpose::STANDARD.decode(chunk).unwrap());
                        }
                        reply(&mut stream, serde_json::json!({"Ack":sequence}));
                        sequence += 1;
                        if packet == "Done" {
                            break;
                        }
                    }
                    assert_eq!(received, contents);
                } else {
                    let mut sequence = 0;
                    let mut packet = |packet| {
                        reply(&mut stream, serde_json::json!({"Data":{"sequence":sequence, "packet":packet}}));
                        assert_eq!(read(&mut stream), serde_json::json!({"Ack":sequence}));
                        sequence += 1;
                    };
                    packet(serde_json::json!({"Begin":{"bytes":bytes, "entries":entries}}));
                    for path in &sources {
                        let data = std::fs::read(path).unwrap();
                        packet(serde_json::json!({"Entry":{
                            "path":path.file_name().unwrap().to_str().unwrap(), "kind":"File", "size":data.len(),
                            "mode":420, "seconds":1_600_000_000, "nanos":0
                        }}));
                        for chunk in data.chunks(64 * 1024) {
                            packet(
                                serde_json::json!({"Chunk":base64::engine::general_purpose::STANDARD.encode(chunk)}),
                            );
                        }
                        packet(serde_json::json!("End"));
                    }
                    packet(serde_json::json!("Done"));
                    assert_eq!(read_past_progress(&mut stream), serde_json::json!({"Finish":{"Ok":null}}));
                    assert_eq!(std::fs::read(destination.join("report")).unwrap(), contents);
                    if entries == 2 {
                        assert_eq!(std::fs::read(destination.join("empty")).unwrap(), b"");
                    }
                }
                reply(&mut stream, serde_json::json!({"Finish":{"Ok":null}}));
                let output = job.join().unwrap();
                let output_text = String::from_utf8(output.stderr).unwrap();
                assert!(output.status.success(), "{output_text}");
                let stdout = String::from_utf8(output.stdout).unwrap();
                for text in [&output_text, &stdout] {
                    assert!(!text.contains("laptop-v1"), "{text:?}");
                    assert!(!text.contains("/payload/report"), "{text:?}");
                }
                if !to_laptop {
                    let (_, installation) = output_text.split_once("Installing the copy.\n").unwrap();
                    let (progress, _) = installation.split_once("\n$ kraal put ").unwrap();
                    assert!(progress.starts_with("\rInstalling files"), "{output_text:?}");
                    assert!(progress.split("\rInstalling files").all(str::is_empty), "{output_text:?}");
                    assert!(output_text.contains("\ndone\n"), "{output_text:?}");
                }
                let noun = if entries == 1 { "entry" } else { "entries" };
                let announcement = format!("Copying {entries} {noun} ({size}). Close this window to cancel.\n");
                assert!(output_text.starts_with(&announcement), "{output_text:?}");
                assert!(output_text.ends_with("\nLaptop copy complete.\n"), "{output_text:?}");
                let verb = if to_laptop { "Sent" } else { "Received" };
                let after_progress = if to_laptop { "Laptop copy complete." } else { "Installing the copy." };
                if bytes == 0 {
                    let (transfer, _) = output_text.split_once(after_progress).unwrap();
                    assert!(!transfer.contains('\r'), "{output_text:?}");
                    assert!(output_text.starts_with(&format!("{announcement}{after_progress}\n")), "{output_text:?}");
                } else {
                    assert!(
                        output_text.contains(&format!("\r{verb} {size} / {size}\x1b[K\n{after_progress}\n")),
                        "{output_text:?}"
                    );
                    if bytes > 3 {
                        assert!(output_text.contains(&format!("\r{verb} 64 KiB / 10 MiB\x1b[K")), "{output_text:?}");
                        assert!(
                            output_text
                                .contains(&format!("\r{verb} 9.9 MiB / 10 MiB\x1b[K\r{verb} 10 MiB / 10 MiB\x1b[K")),
                            "{output_text:?}"
                        );
                    }
                }
            }
        }
    }
}

fn advertise(client: &mut RawClient, id: &str) {
    let message = serde_json::json!({
        "type": "attach", "version": 6, "cols": 120, "rows": 40,
        "workspace": "w", "remote": true,
        "laptop": { "id": id, "host": "travel", "os": "linux" }
    });
    raw_frame(&mut client.stream, 0, message.to_string().as_bytes());
}

#[test]
fn laptop_listing_is_private_and_disconnect_and_save_keep_a_usable_session() {
    let env = Env::new("laptop-list");
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    env.run(&["open", "w"]);
    let mut owner = RawClient::attach(&env, "w");
    advertise(&mut owner, "abc123");
    let observer = RawClient::attach(&env, "w");
    env.run(&["files"]);
    owner.wait_for_row("files");
    owner.send(b"o");
    owner.wait_for_row("this laptop");
    owner.send(b"travel\r");
    owner.wait_for_message("laptop_list");
    let request: serde_json::Value = owner
        .messages
        .lock()
        .unwrap()
        .iter()
        .filter_map(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .find(|m| m["type"] == "laptop_list")
        .unwrap();
    let pane = request["pane"].to_string();
    assert_eq!(request["path"], "~");
    assert!(!observer.messages.lock().unwrap().iter().any(|m| m.contains("laptop_list")));
    let response = serde_json::json!({
        "type": "laptop_listed", "pane": request["pane"], "side": request["side"], "request": request["request"],
        "result": { "Ok": ["~/private-path", [{"name":"secret-entry", "is_dir":false, "is_link":false, "size":7, "mtime":0, "exact":true}]] }
    });
    raw_frame(&mut owner.stream, 0, response.to_string().as_bytes());
    owner.wait_for_row("secret-entry");
    let rows = observer.wait_for_row("shown on that machine only");
    assert!(!rows.iter().any(|r| r.contains("secret-entry") || r.contains("private-path")));
    let read = env.run(&["read", "-p", &pane]);
    assert!(read.contains("shown on that machine only"));
    assert!(!read.contains("secret-entry") && !read.contains("private-path"));
    drop(owner);
    observer.wait_for_row("travel is not attached");
    let mut returning = RawClient::attach(&env, "w");
    advertise(&mut returning, "abc123");
    returning.wait_for_message("laptop_list");
    drop(returning);
    let mut fresh = RawClient::attach(&env, "w");
    advertise(&mut fresh, "def456");
    fresh.wait_for_row("travel is not attached");
    env.run(&["kill"]);
    wait_until(PATIENCE, "session saved", || env.state_file().exists());
    let saved = std::fs::read_to_string(env.state_file()).unwrap();
    assert!(!saved.contains("private-path") && !saved.contains("secret-entry") && !saved.contains("abc123"));
    env.run(&["open", "w"]);
    let panes = env.run(&["panes", "--json"]);
    assert!(panes.contains("shell"), "{panes}");
}

fn laptop_copy(env: &Env, destination: &std::path::Path, replace: bool) -> (RawClient, String) {
    laptop_action(env, destination, replace, "Start")
}

fn laptop_action(env: &Env, destination: &std::path::Path, replace: bool, action: &str) -> (RawClient, String) {
    let mut owner = laptop_panels(env, destination, false);
    owner.send(match action {
        "Move" => b"m",
        "Delete" => b"d",
        _ => b"c",
    });
    if replace {
        owner.wait_for_row("It replaces");
        assert!(!owner.messages.lock().unwrap().iter().any(|m| m.contains("laptop_copy")));
        owner.send(b"\t\r");
    }
    wait_until(PATIENCE, "laptop copy job requests the selected paths", || {
        owner.messages.lock().unwrap().iter().any(|m| {
            let m: serde_json::Value = serde_json::from_str(m).unwrap();
            m["message"][action].is_object()
        })
    });
    let start = owner
        .messages
        .lock()
        .unwrap()
        .iter()
        .filter_map(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .find(|m| m["type"] == "laptop_copy" && m["message"][action].is_object())
        .unwrap();
    assert_eq!(start["message"][action]["sources"], serde_json::json!(["~/report"]));
    (owner, start["token"].as_str().unwrap().into())
}

fn laptop_panels(env: &Env, destination: &std::path::Path, to_laptop: bool) -> RawClient {
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    env.run(&["open", "w"]);
    let mut owner = RawClient::attach(env, "w");
    advertise(&mut owner, "abc123");
    let destination = destination.to_str().unwrap();
    env.run(&if to_laptop { ["files", destination, "~"] } else { ["files", "~", destination] });
    owner.wait_for_row("files");
    owner.wait_for_rows("both directory listings are ready", |rows| {
        rows.iter().any(|row| row.contains("item")) && !rows.iter().any(|row| row.contains("loading"))
    });
    owner.send(if to_laptop { b"\to" } else { b"o" });
    owner.wait_for_row("this laptop");
    owner.send(b"travel\r");
    owner.wait_for_message("laptop_list");
    let request = owner
        .messages
        .lock()
        .unwrap()
        .iter()
        .filter_map(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .find(|m| m["type"] == "laptop_list")
        .unwrap();
    let response = serde_json::json!({
        "type": "laptop_listed", "pane": request["pane"], "side": request["side"], "request": request["request"],
        "result": { "Ok": ["~", [{"name":"report", "is_dir":false, "is_link":false, "size":3, "mtime":0, "exact":true}]] }
    });
    raw_frame(&mut owner.stream, 0, response.to_string().as_bytes());
    owner.wait_for_row("report");
    owner
}

fn copy_packet(owner: &mut RawClient, token: &str, sequence: u64, packet: serde_json::Value) {
    let message = serde_json::json!({ "type": "laptop_copy", "token": token, "message": { "Data": { "sequence": sequence, "packet": packet } } });
    raw_frame(&mut owner.stream, 0, message.to_string().as_bytes());
    wait_until(PATIENCE, "copy chunk acknowledged after writing", || {
        owner.messages.lock().unwrap().iter().any(|m| {
            let m: serde_json::Value = serde_json::from_str(m).unwrap();
            m["type"] == "laptop_copy" && m["token"] == token && m["message"]["Ack"] == sequence
        })
    });
}

fn send_report(owner: &mut RawClient, token: &str) {
    copy_packet(
        owner,
        token,
        1,
        serde_json::json!({ "Entry": {"path":"report","kind":"File","size":3,"mode":420,"seconds":1600000000,"nanos":0} }),
    );
    copy_packet(owner, token, 2, serde_json::json!({"Chunk":"YWJj"}));
    copy_packet(owner, token, 3, serde_json::json!("End"));
    copy_packet(owner, token, 4, serde_json::json!("Done"));
}

fn copy_finished(owner: &RawClient, token: &str) -> serde_json::Value {
    let mut result = None;
    wait_until(PATIENCE, "copy reports a final result", || {
        result = owner
            .messages
            .lock()
            .unwrap()
            .iter()
            .filter_map(|m| serde_json::from_str::<serde_json::Value>(m).ok())
            .find(|m| m["token"] == token && m["message"]["Finish"].is_object());
        result.is_some()
    });
    result.unwrap()["message"]["Finish"].clone()
}

#[test]
fn laptop_copy_installs_via_the_job_confirms_replacements_and_keeps_racing_destinations() {
    for outcome in ["new", "replace", "race"] {
        let env = Env::new(&format!("laptop-copy-{outcome}"));
        let destination = env.root.join("destination");
        std::fs::create_dir(&destination).unwrap();
        if outcome == "replace" {
            std::fs::write(destination.join("report"), b"old").unwrap();
        }
        let (mut owner, token) = laptop_copy(&env, &destination, outcome == "replace");
        copy_packet(&mut owner, &token, 0, serde_json::json!({"Begin":{"bytes":3,"entries":1}}));
        if outcome == "race" {
            std::fs::write(destination.join("report"), b"racing file").unwrap();
        }
        send_report(&mut owner, &token);
        let result = copy_finished(&owner, &token);
        if outcome == "race" {
            assert!(result["Err"].as_str().unwrap().contains("not fully installed"));
            assert_eq!(std::fs::read(destination.join("report")).unwrap(), b"racing file");
        } else {
            assert!(result.get("Ok").is_some(), "{result}");
            assert_eq!(std::fs::read(destination.join("report")).unwrap(), b"abc");
        }
        let spool = env.root.join("state/kraal/laptop-v1").join(&env.session).join(&token);
        wait_until(PATIENCE, "private spool removed", || !spool.exists());
    }
}

#[test]
fn laptop_move_installs_before_source_removal_and_refreshes_after_completion() {
    for changed in [false, true] {
        let env = Env::new(if changed { "laptop-move-changed" } else { "laptop-move" });
        let destination = env.root.join("destination");
        std::fs::create_dir(&destination).unwrap();
        let (mut owner, token) = laptop_action(&env, &destination, false, "Move");
        copy_packet(&mut owner, &token, 0, serde_json::json!({"Begin":{"bytes":3,"entries":1}}));
        send_report(&mut owner, &token);
        wait_until(PATIENCE, "source removal requested after installation", || {
            owner.messages.lock().unwrap().iter().any(|m| {
                let m: serde_json::Value = serde_json::from_str(m).unwrap();
                m["token"] == token && m["message"] == "Installed"
            })
        });
        assert_eq!(std::fs::read(destination.join("report")).unwrap(), b"abc");
        assert!(!owner.messages.lock().unwrap().iter().any(|m| {
            let m: serde_json::Value = serde_json::from_str(m).unwrap();
            m["token"] == token && m["message"]["Finish"].is_object()
        }));
        let result = if changed {
            serde_json::json!({"Err":"Source changed: report was kept. Check both locations before retrying"})
        } else {
            serde_json::json!({"Ok":null})
        };
        let message = serde_json::json!({"type":"laptop_copy", "token":token, "message":{"Finish":result}});
        raw_frame(&mut owner.stream, 0, message.to_string().as_bytes());
        assert_eq!(copy_finished(&owner, &token), result);
        wait_until(PATIENCE, "laptop source panel refreshes", || {
            owner.messages.lock().unwrap().iter().filter(|m| m.contains("laptop_list")).count() >= 2
        });
        assert_eq!(std::fs::read(destination.join("report")).unwrap(), b"abc");
    }
}

#[test]
fn laptop_delete_job_waits_for_local_result_and_refreshes_the_source_panel() {
    for cancelled in [false, true] {
        let env = Env::new(if cancelled { "laptop-delete-cancel" } else { "laptop-delete" });
        let destination = env.root.join("destination");
        std::fs::create_dir(&destination).unwrap();
        let (mut owner, token) = laptop_action(&env, &destination, false, "Delete");
        let result = if cancelled {
            serde_json::json!({"Err":"Operation cancelled: nothing was removed"})
        } else {
            serde_json::json!({"Ok":null})
        };
        let message = serde_json::json!({"type":"laptop_copy", "token":token, "message":{"Finish":result}});
        raw_frame(&mut owner.stream, 0, message.to_string().as_bytes());
        assert_eq!(copy_finished(&owner, &token), result);
        wait_until(PATIENCE, "laptop panel refreshes after deletion", || {
            owner.messages.lock().unwrap().iter().filter(|m| m.contains("laptop_list")).count() >= 2
        });
    }
}

#[test]
fn laptop_copy_window_close_and_disconnect_remove_partial_spools_without_installing() {
    for outcome in ["close", "disconnect"] {
        let env = Env::new(&format!("laptop-copy-{outcome}"));
        let destination = env.root.join("destination");
        std::fs::create_dir(&destination).unwrap();
        let (mut owner, token) = laptop_copy(&env, &destination, false);
        copy_packet(&mut owner, &token, 0, serde_json::json!({"Begin":{"bytes":3,"entries":1}}));
        copy_packet(
            &mut owner,
            &token,
            1,
            serde_json::json!({ "Entry": {"path":"report","kind":"File","size":3,"mode":420,"seconds":1600000000,"nanos":0} }),
        );
        let spool = env.root.join("state/kraal/laptop-v1").join(&env.session).join(&token);
        assert!(spool.join("payload/report").exists());
        if outcome == "close" {
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            let job = panes.as_array().unwrap().iter().find(|p| p["name"].as_str() == Some("⇅ copy")).unwrap();
            env.run(&["close", &job["id"].to_string()]);
            assert!(copy_finished(&owner, &token)["Err"].as_str().unwrap().contains("cancelled"));
        } else {
            drop(owner);
        }
        wait_until(PATIENCE, "cancelled spool removed", || !spool.exists());
        assert!(!destination.join("report").exists());
    }
}

#[test]
fn session_copy_to_laptop_sends_only_after_client_readiness_and_refuses_source_link_races() {
    for outcome in ["copy", "race", "move", "changed", "failed"] {
        let race = outcome == "race";
        let moving = matches!(outcome, "move" | "changed" | "failed");
        let env = Env::new(&format!("laptop-write-{outcome}"));
        let source = env.root.join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("report"), b"abc").unwrap();
        // Like /tmp on macOS, the directory shown by the file manager can be a link.
        let linked = env.root.join("linked-source");
        std::os::unix::fs::symlink(&source, &linked).unwrap();
        let secret = env.root.join("secret");
        std::fs::write(&secret, b"not transferred").unwrap();
        env.workspace("w", "layout { pane name=\"shell\" }\n");
        env.run(&["open", "w"]);
        let mut owner = RawClient::attach(&env, "w");
        advertise(&mut owner, "abc123");
        env.run(&["files", linked.to_str().unwrap(), "~"]);
        owner.wait_for_row("report");
        owner.send(b"\to");
        owner.wait_for_row("this laptop");
        owner.send(b"travel\r");
        owner.wait_for_message("laptop_list");
        let request = owner
            .messages
            .lock()
            .unwrap()
            .iter()
            .filter_map(|m| serde_json::from_str::<serde_json::Value>(m).ok())
            .find(|m| m["type"] == "laptop_list")
            .unwrap();
        let response = serde_json::json!({
            "type":"laptop_listed", "pane":request["pane"], "side":request["side"], "request":request["request"],
            "result":{"Ok":["~",[{"name":"report", "is_dir":false, "is_link":false, "size":1, "mtime":0, "exact":true}]]}
        });
        raw_frame(&mut owner.stream, 0, response.to_string().as_bytes());
        owner.send(if moving { b"\t\x1b[Bm" } else { b"\t\x1b[Bc" });
        if moving {
            let question = format!("Move {} to travel:~?", linked.join("report").display());
            let rows = owner.wait_for_row("Enter Cancel · Tab Choose · Esc Cancel");
            // A long source path puts the question in the body, wrapped between words.
            if !rows.iter().any(|row| row.contains(&question)) {
                assert_eq!(owner.read_question(b"\x1b[6~").join(" "), question);
            }
            owner.send(b"\t\r");
        }
        let mut write = None;
        wait_until(PATIENCE, "copy asks the laptop client to prepare its destination", || {
            write = owner
                .messages
                .lock()
                .unwrap()
                .iter()
                .filter_map(|m| serde_json::from_str::<serde_json::Value>(m).ok())
                .find(|m| m["message"]["Write"].is_object());
            write.is_some()
        });
        let write = write.unwrap();
        let token = write["token"].as_str().unwrap();
        assert_eq!(write["message"]["Write"]["destination"], "~");
        assert_eq!(
            write["message"]["Write"]["allow"],
            serde_json::json!([]),
            "the server cannot approve replacements on the laptop"
        );
        assert!(!owner.messages.lock().unwrap().iter().any(|m| m.contains("\"Data\"")));
        let ack = |owner: &mut RawClient, sequence| {
            let message = serde_json::json!({"type":"laptop_copy", "token":token, "message":{"Ack":sequence}});
            raw_frame(&mut owner.stream, 0, message.to_string().as_bytes());
        };
        ack(&mut owner, 0);
        let mut content = Vec::new();
        for sequence in 0..5 {
            let mut data = None;
            wait_until(PATIENCE, "job sends a bounded packet", || {
                data = owner
                    .messages
                    .lock()
                    .unwrap()
                    .iter()
                    .filter_map(|m| serde_json::from_str::<serde_json::Value>(m).ok())
                    .find(|m| {
                        m["token"] == token
                            && (m["message"]["Data"]["sequence"] == sequence || m["message"]["Finish"].is_object())
                    });
                data.is_some()
            });
            let data = data.unwrap();
            assert!(data["message"]["Data"].is_object(), "copy ended before packet {sequence}: {data}");
            let packet = &data["message"]["Data"]["packet"];
            if sequence == 0 {
                assert_eq!(packet["Begin"]["bytes"], 3);
                if race {
                    std::fs::remove_file(source.join("report")).unwrap();
                    std::os::unix::fs::symlink(&secret, source.join("report")).unwrap();
                }
            }
            if let Some(chunk) = packet["Chunk"].as_str() {
                use base64::Engine;
                content.extend(base64::engine::general_purpose::STANDARD.decode(chunk).unwrap());
            }
            ack(&mut owner, sequence);
            if race {
                break;
            }
        }
        if race {
            assert!(copy_finished(&owner, token).get("Err").is_some());
            assert!(content.is_empty());
        } else {
            assert_eq!(content, b"abc");
            assert_eq!(std::fs::read(source.join("report")).unwrap(), b"abc", "source stays until installation");
            if outcome == "changed" {
                std::fs::write(source.join("report"), b"changed").unwrap();
            }
            let installed = if outcome == "failed" {
                serde_json::json!({"Err":"Installation failed: source kept"})
            } else {
                serde_json::json!({"Ok":null})
            };
            let message = serde_json::json!({"type":"laptop_copy", "token":token, "message":{"Finish":installed}});
            raw_frame(&mut owner.stream, 0, message.to_string().as_bytes());
            let result = copy_finished(&owner, token);
            assert_eq!(result.get("Ok").is_some(), matches!(outcome, "copy" | "move"), "{result}");
            if outcome == "move" {
                assert!(!source.join("report").exists());
            } else if outcome == "changed" {
                assert_eq!(std::fs::read(source.join("report")).unwrap(), b"changed");
                assert!(result["Err"].as_str().unwrap().contains("Source changed"));
            } else {
                assert_eq!(std::fs::read(source.join("report")).unwrap(), b"abc");
            }
        }
        assert_eq!(std::fs::read(secret).unwrap(), b"not transferred");
    }
}

fn relay_session(env: &Env, remote: &Env) -> std::path::PathBuf {
    let (log, path) = fake_ssh(env, remote);
    // The shared fake SSH executes shell commands; use the portable tar leg on both systems.
    write_script(&env.root.join("bin/rsync"), "#!/bin/sh\nexit 127\n");
    std::fs::write(env.root.join("config/kraal/servers.toml"), "[[server]]\nname = \"web\"\nhost = \"web.lan\"\n")
        .unwrap();
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    let output = env.cmd(&["open", "w"]).env("PATH", path).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    log
}

#[test]
fn laptop_to_ssh_relay_installs_and_refuses_changed_connections_and_racing_names() {
    for outcome in ["copy", "replace", "race", "changed"] {
        let env = Env::new(&format!("laptop-ssh-{outcome}"));
        let remote = Env::new(&format!("laptop-ssh-target-{outcome}"));
        let log = relay_session(&env, &remote);
        let destination = remote.root.join("home");
        if outcome == "replace" {
            std::fs::write(destination.join("report"), b"old").unwrap();
        }
        let target = std::path::PathBuf::from(format!("web:{}", destination.display()));
        let (mut owner, token) = laptop_copy(&env, &target, outcome == "replace");
        copy_packet(&mut owner, &token, 0, serde_json::json!({"Begin":{"bytes":3,"entries":1}}));
        if outcome == "race" {
            std::fs::write(destination.join("report"), b"racing file").unwrap();
        }
        let calls = std::fs::read_to_string(&log).unwrap();
        if outcome == "changed" {
            std::fs::write(
                env.root.join("config/kraal/servers.toml"),
                "[[server]]\nname = \"web\"\nhost = \"other.lan\"\n",
            )
            .unwrap();
        }
        send_report(&mut owner, &token);
        let result = copy_finished(&owner, &token);
        if matches!(outcome, "copy" | "replace") {
            assert!(result.get("Ok").is_some(), "{result}");
            assert_eq!(std::fs::read(destination.join("report")).unwrap(), b"abc");
        } else {
            assert!(result.get("Err").is_some(), "{result}");
            if outcome == "race" {
                assert_eq!(std::fs::read(destination.join("report")).unwrap(), b"racing file");
            } else {
                assert!(!destination.join("report").exists());
                let after = std::fs::read_to_string(&log).unwrap();
                assert!(
                    !after[calls.len()..].contains("other.lan"),
                    "--expect must refuse the changed server before SSH"
                );
            }
        }
        let spool = env.root.join("state/kraal/laptop-v1").join(&env.session).join(&token);
        wait_until(PATIENCE, "relay spool removed", || !spool.exists());
    }
}

#[test]
fn ssh_to_laptop_relay_waits_for_consent_streams_the_spool_and_keeps_sources() {
    for outcome in ["copy", "changed", "disconnect", "link"] {
        let env = Env::new(&format!("ssh-laptop-{outcome}"));
        let remote = Env::new(&format!("ssh-laptop-source-{outcome}"));
        let log = relay_session(&env, &remote);
        let source = remote.root.join("home/report");
        if outcome == "link" {
            std::fs::write(remote.root.join("home/release-v1"), b"abc").unwrap();
            std::os::unix::fs::symlink("release-v1", &source).unwrap();
        } else {
            std::fs::write(&source, b"abc").unwrap();
        }
        let target = std::path::PathBuf::from(format!("web:{}", source.parent().unwrap().display()));
        let mut owner = laptop_panels(&env, &target, true);
        owner.send(if outcome == "link" { b"\t\x1b[B\x1b[Bc" } else { b"\t\x1b[Bc" });
        let mut write = None;
        wait_until(PATIENCE, "relay prepares the laptop destination", || {
            write = owner
                .messages
                .lock()
                .unwrap()
                .iter()
                .filter_map(|m| serde_json::from_str::<serde_json::Value>(m).ok())
                .find(|m| m["message"]["Write"].is_object());
            write.is_some()
        });
        let write = write.unwrap();
        let token = write["token"].as_str().unwrap();
        assert_eq!(write["message"]["Write"]["allow"], serde_json::json!([]));
        assert!(!owner.messages.lock().unwrap().iter().any(|m| m.contains("\"Data\"")));
        let before = std::fs::read_to_string(&log).unwrap();
        assert!(!before.contains("rsync") && !before.contains("tar "), "no SSH download before consent: {before}");
        if outcome == "changed" {
            std::fs::write(
                env.root.join("config/kraal/servers.toml"),
                "[[server]]\nname = \"web\"\nhost = \"other.lan\"\n",
            )
            .unwrap();
        }
        let ack = |owner: &mut RawClient, sequence| {
            let message = serde_json::json!({"type":"laptop_copy", "token":token, "message":{"Ack":sequence}});
            raw_frame(&mut owner.stream, 0, message.to_string().as_bytes());
        };
        ack(&mut owner, 0);
        let spool = env.root.join("state/kraal/laptop-v1").join(&env.session).join(token);
        if outcome == "changed" {
            assert!(copy_finished(&owner, token).get("Err").is_some());
            assert!(!std::fs::read_to_string(&log).unwrap()[before.len()..].contains("other.lan"));
        } else {
            for sequence in 0..if outcome == "link" { 4 } else { 5 } {
                let mut data = None;
                wait_until(PATIENCE, "relay sends bounded packets", || {
                    data = owner
                        .messages
                        .lock()
                        .unwrap()
                        .iter()
                        .filter_map(|m| serde_json::from_str::<serde_json::Value>(m).ok())
                        .find(|m| m["token"] == token && m["message"]["Data"]["sequence"] == sequence);
                    data.is_some()
                });
                if outcome == "link" {
                    assert_eq!(
                        std::fs::read_link(spool.join("payload/report")).unwrap(),
                        std::path::Path::new("release-v1")
                    );
                    assert!(!spool.join("payload/release-v1").exists());
                    if sequence == 1 {
                        assert_eq!(
                            data.as_ref().unwrap()["message"]["Data"]["packet"]["Entry"]["kind"]["Link"],
                            "release-v1"
                        );
                    }
                } else {
                    assert_eq!(std::fs::read(spool.join("payload/report")).unwrap(), b"abc");
                }
                if outcome == "disconnect" {
                    owner.stream.shutdown(std::net::Shutdown::Both).unwrap();
                    break;
                }
                if sequence == 2 && outcome != "link" {
                    assert_eq!(data.unwrap()["message"]["Data"]["packet"]["Chunk"], "YWJj");
                }
                ack(&mut owner, sequence);
            }
            if matches!(outcome, "copy" | "link") {
                let finished =
                    serde_json::json!({"type":"laptop_copy", "token":token, "message":{"Finish":{"Ok":null}}});
                raw_frame(&mut owner.stream, 0, finished.to_string().as_bytes());
                assert!(copy_finished(&owner, token).get("Ok").is_some());
            }
        }
        wait_until(PATIENCE, "download spool removed", || !spool.exists());
        assert_eq!(std::fs::read(source).unwrap(), b"abc");
    }
}

#[test]
fn laptop_ssh_moves_are_refused_without_starting_a_job_in_both_directions() {
    for to_laptop in [false, true] {
        let env = Env::new(if to_laptop { "ssh-laptop-move" } else { "laptop-ssh-move" });
        let remote = Env::new(if to_laptop { "ssh-laptop-move-source" } else { "laptop-ssh-move-target" });
        relay_session(&env, &remote);
        let file = remote.root.join("home/report");
        std::fs::write(&file, b"original").unwrap();
        let target = std::path::PathBuf::from(format!("web:{}", file.parent().unwrap().display()));
        let mut owner = laptop_panels(&env, &target, to_laptop);
        // The refusal's final advice must fit beside the file-manager title in the top bar.
        owner.resize((200, 40));
        if to_laptop {
            owner.send(b"\t\x1b[B");
        }
        owner.send(b"m");
        owner.wait_for_row("copy, then delete");
        assert!(!owner.messages.lock().unwrap().iter().any(|message| message.contains("laptop_copy")));
        assert!(!env.run(&["panes", "--json"]).contains("laptop-cp"));
        assert_eq!(std::fs::read(file).unwrap(), b"original");
    }
}
