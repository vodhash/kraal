//! Editing a file of a server in the local editor: a private copy, the editor in a floating
//! pane, each save sent back to the server, and a question when the server's file changed
//! meanwhile (see `crate::edit` for what runs there). Reading and writing files, here and on the
//! server, happens in background jobs: a slow disk or server never stops the event loop.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::contexts::Production;
use super::requests::ReplyHandle;
use super::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind};
use super::{ClientId, Dirty, Server};
use crate::edit::{self, Channel, RemoteFile, Saved, Stamp};
use crate::layout::PaneId;
use crate::pane::PaneSpec;
use crate::protocol::{Placement, Response};
use crate::theme::NoticeKind;

/// How long the pane asking for a sudo password waits for it.
const PASSWORD_WAIT: Duration = Duration::from_secs(300);

/// A file to edit, and where its editor opens.
#[derive(Clone, Debug)]
pub struct EditTarget {
    /// The server, by the name it is shown with.
    pub server: String,
    /// ssh arguments that reach it, destination last.
    pub ssh: Vec<String>,
    /// Absolute, `~/…`, or relative to the home directory there.
    pub path: String,
    pub line: Option<u32>,
    /// The workspace the editor opens in; else the client's, else the first one.
    pub ws: Option<u32>,
    /// The pane the editor opens over.
    pub near: Option<PaneId>,
    /// As root, through sudo on the server.
    pub sudo: bool,
}

/// What to do when the server's file changed since it was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Keep the edited version as a local file; the server keeps its own.
    KeepLocal,
    /// The server's version stays, and the editor's copy becomes it.
    KeepServer,
    /// The edited version replaces the server's.
    Overwrite,
}

/// A file being edited.
pub(super) struct Edit {
    id: u32,
    /// Who asked: told about each save, and asked when the server's file changed.
    client: Option<ClientId>,
    target: EditTarget,
    file: RemoteFile,
    /// The server's version as last read or written, and what identifies it there. Once kept as
    /// a local file: what that file holds.
    remote: Vec<u8>,
    identity: String,
    /// The private directory kraal made for this edit: the only one removed when it ends.
    dir: PathBuf,
    /// The editor's copy, in that directory.
    local: PathBuf,
    pane: PaneId,
    seen: Option<Stamp>,
    /// The copy changed at the last look: it is read once it holds still.
    settling: bool,
    /// A job for this file runs (reading the copy, ssh): nothing else starts meanwhile.
    busy: bool,
    /// The server's version changed since it was read (what identifies it now): nothing is sent
    /// until the user says what to do.
    conflict: Option<String>,
    /// The question was shown; it is not shown again before the next save.
    asked: bool,
    /// Kept as this local file: saves go there, no longer to the server.
    kept: Option<PathBuf>,
    /// The editor ended.
    closed: bool,
    /// How the server's file is read and written.
    channel: Channel,
}

impl Edit {
    fn name(&self) -> String {
        printable(&self.file.name)
    }
}

/// A file read, and its private copy made: what the editor opens on.
struct Ready {
    channel: Channel,
    opened: edit::Opened,
    dir: PathBuf,
    local: PathBuf,
}

/// What a look at the editor's copy did.
enum Checked {
    Unreadable(std::io::Error),
    /// Nothing new in it.
    Same,
    /// Its content went to the kept local file.
    Kept(Vec<u8>),
    KeepFailed(std::io::Error),
    /// Its content went to the server.
    Sent(Vec<u8>, anyhow::Result<Saved>),
}

/// A name as shown in one line: control characters as `?`.
fn printable(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

/// Where the private directories of edits go.
fn edit_base() -> std::io::Result<PathBuf> {
    Ok(crate::paths::private_runtime_dir()?.join("edit"))
}

/// The file read through `channel`, and its private copy.
fn read_into_copy(channel: Channel, path: &str, dir: Option<PathBuf>) -> anyhow::Result<Ready> {
    let opened = channel.open(path)?;
    let (dir, local) = match dir {
        Some(dir) => {
            let local = edit::copy_into(&dir, &opened.file.name, &opened.content)?;
            (dir, local)
        }
        None => edit::local_copy(&edit_base()?, &opened.file.name, &opened.content)?,
    };
    Ok(Ready { channel, opened, dir, local })
}

impl Server {
    /// Edit `target` for `client`, asking first when it is production.
    pub(super) fn edit_request(
        &mut self,
        client: ClientId,
        target: EditTarget,
        production: Option<Production>,
    ) -> Mode {
        if let Some(why) = production {
            let message = format!(
                "Edit {} on {}{}? It is production ({why}).\nEach save in the editor goes to the server.",
                printable(&target.path),
                target.server,
                if target.sudo { " as root" } else { "" }
            );
            return Mode::confirm(message, ConfirmAction::Edit(Box::new(target), Some(why.why)));
        }
        self.start_edit(Some(client), target, None);
        Mode::Normal
    }

    /// Why a server is production, if it may be (`docs/production.md`).
    pub(super) fn server_production(&self, s: &crate::servers::Server) -> Option<Production> {
        let words = &self.config.production.words;
        (!words.is_empty() && crate::context::server_production(s, words))
            .then(|| Production::new(format!("server {}", s.name), format!("server {s:?}")))
    }

    fn download_dir(&self) -> PathBuf {
        crate::paths::expand_tilde(&self.config.servers.download_dir)
    }

    /// Read the file off the event loop, then open the editor on a private copy of it. `reply`
    /// is a control request (`kraal edit`) waiting to know how that went.
    pub(super) fn start_edit(&mut self, client: Option<ClientId>, target: EditTarget, reply: Option<ReplyHandle>) {
        if target.sudo {
            return self.start_sudo_edit(client, target, reply);
        }
        let (channel, path) = (Channel::Ssh(target.ssh.clone()), target.path.clone());
        let started = self.background_owned(
            reply,
            move || read_into_copy(channel, &path, None),
            move |server, reply, ready| server.edit_ready(client, target, reply, ready),
        );
        if let Err(reply) = started {
            self.edit_failed(client, reply, "cannot start reading the file".into());
        }
    }

    /// Through sudo: one connection runs it for the whole edit. When it needs a password, a
    /// pane asks for it, and the file is read once sudo has it.
    fn start_sudo_edit(&mut self, client: Option<ClientId>, target: EditTarget, reply: Option<ReplyHandle>) {
        enum Started {
            Ready(Ready),
            Password(edit::Session, PathBuf, PathBuf),
        }
        let (ssh, path) = (target.ssh.clone(), target.path.clone());
        let started = self.background_owned(
            reply,
            move || -> anyhow::Result<Started> {
                let session = edit::Session::start(&ssh, &path)?;
                if session.needs_password {
                    let dir = edit::private_dir(&edit_base()?)?;
                    match edit::password_fifo(&dir) {
                        Ok(fifo) => Ok(Started::Password(session, dir, fifo)),
                        Err(e) => {
                            let _ = std::fs::remove_dir_all(&dir);
                            Err(e.into())
                        }
                    }
                } else {
                    let path = session.path.clone();
                    read_into_copy(Channel::Sudo(Arc::new(Mutex::new(session))), &path, None).map(Started::Ready)
                }
            },
            move |server, reply, started| match started {
                Ok(Started::Ready(ready)) => server.edit_ready(client, target, reply, Ok(ready)),
                Ok(Started::Password(session, dir, fifo)) => {
                    server.ask_sudo_password(client, target, reply, session, dir, fifo)
                }
                Err(e) => server.edit_ready(client, target, reply, Err(e)),
            },
        );
        if let Err(reply) = started {
            self.edit_failed(client, reply, "cannot start sudo".into());
        }
    }

    /// A pane asks for the sudo password; sudo gets it as soon as it is typed, then the file is
    /// read as root.
    fn ask_sudo_password(
        &mut self,
        client: Option<ClientId>,
        target: EditTarget,
        reply: Option<ReplyHandle>,
        session: edit::Session,
        dir: PathBuf,
        fifo: PathBuf,
    ) {
        let prompt =
            format!("[sudo] password on {} (kraal edits {} as root): ", target.server, printable(&target.path));
        let spec = PaneSpec {
            command: edit::password_command(&fifo, &prompt),
            cwd: Some(dir.clone()),
            close_on_exit: true,
            ..Default::default()
        };
        let ws = self.edit_workspace(client, &target);
        let label = Some(format!("sudo · {}", target.server));
        if let Err(e) = self.spawn_pane_in(ws, target.near, spec, Placement::Floating, label) {
            self.background(move || drop((session, std::fs::remove_dir_all(&dir))), |_, ()| {});
            return self.edit_failed(client, reply, format!("cannot edit {}: {e:#}", target.server));
        }
        let started = self.background_owned(
            reply,
            move || -> anyhow::Result<Ready> {
                let mut session = session;
                let result = (|| {
                    let password = edit::read_password(&fifo, PASSWORD_WAIT);
                    // Gone before the copy is made: the file may have its name.
                    let _ = std::fs::remove_file(&fifo);
                    let Some(mut password) = password? else {
                        anyhow::bail!("no password was given");
                    };
                    session.authenticate(&mut password)?;
                    let path = session.path.clone();
                    read_into_copy(Channel::Sudo(Arc::new(Mutex::new(session))), &path, Some(dir.clone()))
                })();
                if result.is_err() {
                    let _ = std::fs::remove_dir_all(&dir);
                }
                result
            },
            move |server, reply, ready| server.edit_ready(client, target, reply, ready),
        );
        if let Err(reply) = started {
            self.edit_failed(client, reply, "cannot wait for the password".into());
        }
    }

    /// The file is read (or could not be): its editor opens, or the user is told why not. A file
    /// the user may not write, asked for from the file manager or a hint, is offered as root.
    fn edit_ready(
        &mut self,
        client: Option<ClientId>,
        target: EditTarget,
        reply: Option<ReplyHandle>,
        ready: anyhow::Result<Ready>,
    ) {
        let shown = format!("{}:{}", target.server, printable(&target.path));
        let not_permitted = ready.as_ref().err().and_then(|e| e.downcast_ref::<edit::NotPermitted>());
        if let (Some(why), None, false, Some(c)) = (not_permitted, reply.as_ref(), target.sudo, client) {
            let message = format!(
                "{shown}: {why}.\nEdit it as root, through sudo on {}? A pane asks for your password; kraal does not keep it.",
                target.server
            );
            let target = EditTarget { sudo: true, ..target };
            let production =
                self.resolve_server(&target.server).and_then(|s| self.server_production(&s)).map(|p| p.why);
            self.set_mode_if_idle(c, Mode::confirm(message, ConfirmAction::Edit(Box::new(target), production)));
            return;
        }
        let result =
            ready.and_then(|r| self.open_editor(client, target, r)).map_err(|e| format!("cannot edit {shown}: {e:#}"));
        match (result, reply) {
            (Ok(p), Some(r)) => {
                let result = Ok(Response::Pane(Box::new(self.pane_info(p))));
                r.complete(self, result);
            }
            (Ok(_), None) => {}
            (Err(e), reply) => self.edit_failed(client, reply, e),
        }
    }

    /// Say why an edit did not start: to the waiting control request, else to the client.
    fn edit_failed(&mut self, client: Option<ClientId>, reply: Option<ReplyHandle>, why: String) {
        match reply {
            Some(r) => {
                r.complete(self, Err(why));
            }
            None => self.toast(client.unwrap_or(0), NoticeKind::Error, why),
        }
    }

    /// The workspace an edit's panes open in: the target's, else the client's, else the first.
    fn edit_workspace(&mut self, client: Option<ClientId>, target: &EditTarget) -> u32 {
        // The workspace may have closed while the file was read.
        let preferred = target.ws.or_else(|| client.and_then(|c| self.clients.get(&c)).map(|c| c.workspace));
        self.available_workspace(preferred).unwrap_or_else(|| self.new_workspace("main".into(), None))
    }

    /// The editor on the private copy of the file, in a floating pane.
    fn open_editor(&mut self, client: Option<ClientId>, target: EditTarget, ready: Ready) -> anyhow::Result<PaneId> {
        let Ready { channel, opened, dir, local } = ready;
        let command = edit::editor_argv(&local, target.line);
        // It ends with the editor, and does not come back after a restart (nor do transfer
        // windows): its copy is gone by then.
        let spec = PaneSpec {
            command,
            cwd: Some(dir.clone()),
            close_on_exit: true,
            keep_on_failure: true,
            close_on_success: true,
            ..Default::default()
        };
        let ws = self.edit_workspace(client, &target);
        let label = format!("{} · {}", printable(&opened.file.name), target.server);
        let pane = match self.spawn_pane_in(ws, target.near, spec, Placement::Floating, Some(label)) {
            Ok(p) => p,
            Err(e) => {
                self.background(move || drop((channel, std::fs::remove_dir_all(&dir))), |_, ()| {});
                return Err(e);
            }
        };
        let id = self.alloc_id();
        self.edits.push(Edit {
            id,
            client,
            target,
            file: opened.file,
            remote: opened.content,
            identity: opened.identity,
            seen: edit::stamp(&local),
            dir,
            local,
            pane,
            settling: false,
            busy: false,
            conflict: None,
            asked: false,
            kept: None,
            closed: false,
            channel,
        });
        Ok(pane)
    }

    fn edit_index(&self, id: u32) -> Option<usize> {
        self.edits.iter().position(|e| e.id == id)
    }

    /// The client to tell about an edit: the one who asked, else the main one.
    fn edit_client(&self, id: u32) -> ClientId {
        let attached = |c: &ClientId| self.clients.get(c).is_some_and(|c| c.attached);
        let asked = self.edit_index(id).and_then(|i| self.edits[i].client);
        asked.filter(attached).or(self.primary.filter(attached)).unwrap_or(0)
    }

    fn edit_toast(&mut self, id: u32, kind: NoticeKind, message: String) {
        let client = self.edit_client(id);
        self.toast(client, kind, message);
    }

    /// Look at each edited copy: send what was saved, ask what was not answered, and end the
    /// edits whose editor ended.
    pub(super) fn tick_edits(&mut self) {
        let ids: Vec<u32> = self.edits.iter().map(|e| e.id).collect();
        for id in ids {
            self.check_edit(id);
        }
    }

    fn check_edit(&mut self, id: u32) {
        let Some(i) = self.edit_index(id) else { return };
        let running = self.panes.get(&self.edits[i].pane).is_some_and(|p| p.exit_code.is_none());
        if self.edits[i].busy || self.edit_question_shown(id) {
            return;
        }
        let e = &mut self.edits[i];
        e.closed |= !running;
        let now = edit::stamp(&e.local);
        let changed = now != e.seen;
        if changed {
            e.seen = now;
            e.settling = true;
        }
        if e.conflict.is_some() {
            // A new save asks again.
            if changed {
                e.asked = false;
            }
            let (asked, closed) = (e.asked, e.closed);
            if !asked && self.ask_edit(id) {
                return;
            }
            if closed {
                self.keep_edit_locally(id, "nobody said what to do about the server's change");
            }
            return;
        }
        if !(e.closed || (e.settling && !changed)) {
            return;
        }
        e.settling = false;
        e.busy = true;
        let (local, kept, remote) = (e.local.clone(), e.kept.clone(), e.remote.clone());
        let (channel, file, expected) = (e.channel.clone(), e.file.clone(), e.identity.clone());
        let started = self.background(
            move || {
                let content = match crate::paths::read_regular(&local, edit::MAX_SIZE) {
                    Ok(c) => c,
                    Err(err) => return Checked::Unreadable(err),
                };
                if content == remote {
                    return Checked::Same;
                }
                match kept {
                    Some(path) => match crate::paths::write_private(&path, &content) {
                        Ok(()) => Checked::Kept(content),
                        Err(err) => Checked::KeepFailed(err),
                    },
                    None => {
                        let result = channel.save(&file, &expected, &content);
                        Checked::Sent(content, result)
                    }
                }
            },
            move |server, checked| server.edit_checked(id, checked),
        );
        if !started && let Some(i) = self.edit_index(id) {
            self.edits[i].busy = false;
        }
    }

    fn edit_checked(&mut self, id: u32, checked: Checked) {
        let Some(i) = self.edit_index(id) else { return };
        let e = &mut self.edits[i];
        e.busy = false;
        let (name, closed, dir) = (e.name(), e.closed, e.dir.clone());
        match checked {
            Checked::Unreadable(err) => {
                let gone = err.kind() == std::io::ErrorKind::NotFound;
                if closed && gone {
                    self.edit_toast(
                        id,
                        NoticeKind::Warning,
                        format!("{name}: the editor's copy is gone, nothing more was saved"),
                    );
                    self.finish_edit(id);
                } else if closed {
                    self.edit_toast(
                        id,
                        NoticeKind::Error,
                        format!("{name} was not saved: {err}; your copy stays in {}", dir.display()),
                    );
                    self.forget_edit(id);
                } else if !gone {
                    self.edit_toast(id, NoticeKind::Error, format!("{name} was not saved: {err}"));
                }
            }
            Checked::Same => {
                if closed {
                    self.finish_edit(id);
                }
            }
            Checked::Kept(content) => {
                e.remote = content;
                let shown = e.kept.as_deref().map(crate::paths::shorten_home).unwrap_or_default();
                self.edit_toast(id, NoticeKind::Notice, format!("{name} saved in {shown}"));
                if closed {
                    self.finish_edit(id);
                }
            }
            Checked::KeepFailed(err) => {
                self.edit_toast(id, NoticeKind::Error, format!("{name}: {err}; your copy stays in {}", dir.display()));
                self.forget_edit(id);
            }
            Checked::Sent(content, result) => self.edit_sent(id, content, result),
        }
    }

    fn edit_sent(&mut self, id: u32, content: Vec<u8>, result: anyhow::Result<Saved>) {
        let Some(i) = self.edit_index(id) else { return };
        let e = &mut self.edits[i];
        e.busy = false;
        let (name, server, closed) = (e.name(), e.target.server.clone(), e.closed);
        match result {
            Ok(Saved::Written(identity)) => {
                e.identity = identity;
                e.remote = content;
                e.conflict = None;
                self.edit_toast(id, NoticeKind::Notice, format!("{name} saved to {server}"));
                self.refresh_file_managers();
            }
            Ok(Saved::Changed(identity)) => {
                e.conflict = Some(identity);
                e.asked = false;
                self.edit_toast(
                    id,
                    NoticeKind::Warning,
                    format!("{name} changed on {server} since you opened it: your version was not sent"),
                );
            }
            Err(err) if closed => {
                self.edit_toast(id, NoticeKind::Error, format!("{name} was not saved to {server}: {err:#}"));
                self.keep_edit_locally(id, "it could not be saved");
            }
            Err(err) => self.edit_toast(id, NoticeKind::Error, format!("{name} was not saved to {server}: {err:#}")),
        }
    }

    /// Show the question about edit `id` to its client, or the main one, when it is free.
    /// False when nobody is there to answer.
    fn ask_edit(&mut self, id: u32) -> bool {
        let Some(i) = self.edit_index(id) else { return false };
        let attached = |c: &ClientId| self.clients.get(c).is_some_and(|c| c.attached);
        let candidates: Vec<ClientId> =
            [self.edits[i].client, self.primary].into_iter().flatten().filter(attached).collect();
        if candidates.is_empty() {
            return false;
        }
        // A client busy with something else is asked once it is done.
        let Some(client) =
            candidates.into_iter().find(|c| self.clients.get(c).is_some_and(|c| matches!(c.mode, Mode::Normal)))
        else {
            return true;
        };
        let picker = self.edit_question(&self.edits[i]);
        self.edits[i].asked = true;
        if let Some(c) = self.clients.get_mut(&client) {
            c.mode = Mode::Picker(picker);
            c.dirty = Dirty::Yes;
        }
        self.mark_dirty_client(client);
        true
    }

    fn edit_question(&self, e: &Edit) -> Picker {
        let (name, server) = (e.name(), &e.target.server);
        let downloads = crate::paths::shorten_home(&self.download_dir());
        let answer = |answer| PickAction::EditAnswer { edit: e.id, answer };
        let items = vec![
            PickItem::new(
                "Keep your version as a local file",
                format!("in {downloads}; {server} keeps its own"),
                answer(Answer::KeepLocal),
            ),
            PickItem::new(
                format!("Keep the version of {server}"),
                "your changes are dropped, and the editor's copy becomes that version",
                answer(Answer::KeepServer),
            ),
            PickItem::new(
                "Replace it with your version",
                format!("what changed on {server} is lost"),
                answer(Answer::Overwrite),
            ),
        ];
        Picker::new(PickerKind::EditConflict, &format!("{name} changed on {server} since you opened it"), items)
    }

    /// Whether a client shows the question about edit `id`.
    fn edit_question_shown(&self, id: u32) -> bool {
        self.clients.values().any(|c| match &c.mode {
            Mode::Picker(p) => {
                p.items.iter().any(|it| matches!(it.action, PickAction::EditAnswer { edit, .. } if edit == id))
            }
            _ => false,
        })
    }

    /// The user's answer about edit `id`.
    pub(super) fn answer_edit(&mut self, client: ClientId, id: u32, answer: Answer) {
        let Some(i) = self.edit_index(id) else {
            self.toast(client, NoticeKind::Notice, "that edit is over");
            return;
        };
        let e = &mut self.edits[i];
        e.client = Some(client);
        let Some(now) = e.conflict.clone() else { return };
        if e.busy {
            return;
        }
        let (name, server) = (e.name(), e.target.server.clone());
        let (local, channel, file) = (e.local.clone(), e.channel.clone(), e.file.clone());
        let started = match answer {
            Answer::Overwrite => {
                e.busy = true;
                self.background(
                    move || match crate::paths::read_regular(&local, edit::MAX_SIZE) {
                        Ok(content) => {
                            let result = channel.save(&file, &now, &content);
                            Checked::Sent(content, result)
                        }
                        Err(err) => Checked::Unreadable(err),
                    },
                    move |server, checked| match checked {
                        Checked::Unreadable(err) => {
                            if let Some(i) = server.edit_index(id) {
                                server.edits[i].busy = false;
                            }
                            server.toast(client, NoticeKind::Error, format!("{name} was not saved: {err}"));
                        }
                        checked => server.edit_checked(id, checked),
                    },
                )
            }
            Answer::KeepServer if e.closed => {
                self.toast(client, NoticeKind::Notice, format!("{name}: the version of {server} stays"));
                self.finish_edit(id);
                return;
            }
            Answer::KeepServer => {
                e.conflict = None;
                e.busy = true;
                // The answer applies to this version; replacement checks the entry moved aside.
                let answered = edit::stamp(&local);
                let path = file.path();
                self.background(
                    move || {
                        let opened = channel.open(&path)?;
                        let replaced = edit::replace_copy(&local, answered, &opened.content)?;
                        Ok((opened, replaced))
                    },
                    move |server, reloaded| server.edit_reloaded(id, now, reloaded),
                )
            }
            Answer::KeepLocal => {
                e.busy = true;
                let downloads = self.download_dir();
                self.background(
                    move || {
                        let content = crate::paths::read_regular(&local, edit::MAX_SIZE)?;
                        edit::keep(&downloads, &file.name, &content).map(|p| (p, content))
                    },
                    move |server, kept| server.edit_kept(client, id, kept),
                )
            }
        };
        if !started && let Some(i) = self.edit_index(id) {
            self.edits[i].busy = false;
        }
    }

    /// The answer "keep your version as a local file" was carried out, or could not be.
    fn edit_kept(&mut self, client: ClientId, id: u32, kept: std::io::Result<(PathBuf, Vec<u8>)>) {
        let Some(i) = self.edit_index(id) else { return };
        let e = &mut self.edits[i];
        e.busy = false;
        let (name, server) = (e.name(), e.target.server.clone());
        match kept {
            Ok((path, content)) => {
                e.conflict = None;
                e.kept = Some(path.clone());
                e.remote = content;
                let shown = crate::paths::shorten_home(&path);
                let next = if e.closed { "" } else { "; your next saves go there" };
                self.toast(client, NoticeKind::Notice, format!("{name} kept in {shown}, {server} keeps its own{next}"));
                if self.edits[i].closed {
                    self.finish_edit(id);
                }
            }
            Err(err) => self.toast(client, NoticeKind::Error, format!("{name} could not be kept: {err}")),
        }
    }

    /// The server's version, read again for the editor's copy after the user chose to keep it:
    /// the copy became it, unless it was saved again meanwhile (`None`), which asks again.
    fn edit_reloaded(&mut self, id: u32, conflict: String, reloaded: anyhow::Result<(edit::Opened, Option<Stamp>)>) {
        let Some(i) = self.edit_index(id) else { return };
        let running = self.panes.get(&self.edits[i].pane).is_some_and(|p| p.exit_code.is_none());
        let e = &mut self.edits[i];
        e.busy = false;
        let (name, server) = (e.name(), e.target.server.clone());
        match reloaded {
            Ok((_, Some(_))) if !running => {
                self.edit_toast(id, NoticeKind::Notice, format!("{name}: the version of {server} stays"));
                self.finish_edit(id);
            }
            Ok((o, Some(stamp))) => {
                e.seen = Some(stamp);
                e.settling = false;
                (e.file, e.remote, e.identity) = (o.file, o.content, o.identity);
                self.edit_toast(
                    id,
                    NoticeKind::Notice,
                    format!("{name}: the editor's copy is the version of {server} now (reload it)"),
                );
            }
            Ok((o, None)) => {
                e.conflict = Some(o.identity);
                e.asked = false;
                self.edit_toast(
                    id,
                    NoticeKind::Warning,
                    format!("{name}: you saved again meanwhile, nothing was replaced"),
                );
            }
            Err(err) => {
                // Asked again at the next save, or when the editor ends.
                e.conflict = Some(conflict);
                e.asked = true;
                self.edit_toast(
                    id,
                    NoticeKind::Error,
                    format!("{name}: the version of {server} could not be read: {err:#}"),
                );
            }
        }
    }

    /// End edit `id`, keeping the editor's version as a local file first.
    fn keep_edit_locally(&mut self, id: u32, why: &str) {
        let Some(i) = self.edit_index(id) else { return };
        let e = &mut self.edits[i];
        if e.busy {
            return;
        }
        e.busy = true;
        let (local, name) = (e.local.clone(), e.file.name.clone());
        let downloads = self.download_dir();
        let why = why.to_string();
        let started = self.background(
            move || {
                let content = crate::paths::read_regular(&local, edit::MAX_SIZE)?;
                edit::keep(&downloads, &name, &content)
            },
            move |server, kept| {
                let Some(i) = server.edit_index(id) else { return };
                let e = &server.edits[i];
                let (name, target, dir) = (e.name(), e.target.server.clone(), e.dir.display().to_string());
                match kept {
                    Ok(path) => {
                        let shown = crate::paths::shorten_home(&path);
                        server.edit_toast(
                            id,
                            NoticeKind::Warning,
                            format!("{name}: {why}; your version is in {shown}, {target} keeps its own"),
                        );
                        server.finish_edit(id);
                    }
                    Err(err) => {
                        server.edit_toast(
                            id,
                            NoticeKind::Error,
                            format!("{name}: {why}, and {err}: your copy stays in {dir}"),
                        );
                        server.forget_edit(id);
                    }
                }
            },
        );
        if !started && let Some(i) = self.edit_index(id) {
            self.edits[i].busy = false;
        }
    }

    /// Forget edit `id`, leaving its private copy where it is.
    fn forget_edit(&mut self, id: u32) {
        let Some(i) = self.edit_index(id) else { return };
        let e = self.edits.remove(i);
        self.background(move || drop(e.channel), |_, ()| {});
    }

    /// Forget edit `id` and remove its private directory (and only it).
    fn finish_edit(&mut self, id: u32) {
        let Some(i) = self.edit_index(id) else { return };
        let e = self.edits.remove(i);
        self.background(move || drop((e.channel, std::fs::remove_dir_all(&e.dir))), |_, ()| {});
    }

    /// The server stops: what was not sent is kept as local files (there is no time to send it),
    /// and the private copies go.
    pub(super) fn end_edits(&mut self) {
        let download_dir = self.download_dir();
        for e in std::mem::take(&mut self.edits) {
            // A copy that cannot be read stays where it is.
            let Ok(content) = crate::paths::read_regular(&e.local, edit::MAX_SIZE) else { continue };
            let kept = match &e.kept {
                _ if content == e.remote => Ok(None),
                Some(path) => crate::paths::write_private(path, &content).map(|()| Some(path.clone())),
                None => edit::keep(&download_dir, &e.file.name, &content).map(Some),
            };
            match kept {
                Ok(path) => {
                    if let Some(path) = path {
                        eprintln!(
                            "kraal: {} was not saved to {}: it is in {}",
                            e.name(),
                            e.target.server,
                            path.display()
                        );
                    }
                    let _ = std::fs::remove_dir_all(&e.dir);
                }
                Err(err) => eprintln!("kraal: {} was not saved: {err}; it stays in {}", e.name(), e.dir.display()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::requests::tests::{finish_job, response};
    use crate::server::tests::{client, server};

    #[test]
    fn remote_edit_start_and_read_failures_keep_their_single_reply() {
        for sudo in [false, true] {
            for fail_start in [false, true] {
                let (tx, rx) = crate::server::event_channel(32);
                let mut server = server(tx);
                let (id, _peer) = client(&mut server);
                let receive = server.clients.get_mut(&id).unwrap().out.capture();
                let target = EditTarget {
                    server: "test".into(),
                    ssh: vec![],
                    path: "/file".into(),
                    line: None,
                    ws: None,
                    near: None,
                    sudo,
                };
                super::super::jobs::FAIL_START.set(fail_start);
                let job = crate::server::jobs::JobId(server.next_id);
                server.start_edit(None, target, Some(ReplyHandle::new(id)));
                if !fail_start {
                    assert!(receive.is_empty());
                    finish_job(&mut server, &rx, job);
                }
                let expected = match (sudo, fail_start) {
                    (false, true) => "cannot start reading the file",
                    (true, true) => "cannot start sudo",
                    (_, false) => "cannot edit test:/file: no ssh destination",
                };
                assert_eq!(response(&receive).unwrap_err(), expected);
                assert!(server.panes.is_empty());
                assert!(server.jobs.is_empty());
                assert!(!server.clients.contains_key(&id));
                assert!(matches!(receive.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
            }
        }
    }
}
