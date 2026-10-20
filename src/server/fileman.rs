//! The two-panel file manager pane: opening it, and acting on what it asks for (copy, move,
//! delete, preview, edit, shell, choosing a server for a panel).

use std::sync::Arc;

use anyhow::Result;

use super::edits::EditTarget;
use super::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind};
use super::{ClientId, FloatRect, Server};
use crate::files::{FileKey, FilesAction, FilesPane, Location, Remote, Side};
use crate::input::{Key, KeyCode};
use crate::layout::PaneId;
use crate::pane::{Content, PaneSpec};
use crate::paths;
use crate::protocol::Placement;
use crate::theme::NoticeKind;

impl Server {
    /// `/path`, `~/x` or `server:path` as a panel location.
    pub(super) fn files_location(&self, spec: &str) -> Location {
        if let Some((head, path)) = crate::transfer::split_remote(spec)
            && let Some(s) = self.known_servers.iter().find(|k| k.server.name == head).map(|k| k.server.clone())
        {
            let mut ssh_args = self.ssh_options(&s);
            ssh_args.push(s.destination());
            let path = if path.is_empty() { "~".to_string() } else { path.to_string() };
            return Location::Remote(Remote { server: s.name.clone(), ssh_args, path });
        }
        Location::Local(paths::expand_tilde(spec))
    }

    pub(super) fn file_manager(&self, left: &str, right: &str) -> FilesPane {
        let tx = self.tx.clone();
        // Called from the listing's own thread: waiting for room in the queue costs nothing there,
        // while a wake-up dropped would leave the panel loading for good.
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = tx.wake();
        });
        FilesPane::new(self.files_location(left), self.files_location(right), wake)
    }

    /// Open a file manager as a large floating pane.
    pub fn open_files(&mut self, ws: u32, left: String, right: String) -> Result<PaneId> {
        let spec = PaneSpec { files: Some([left, right]), ..Default::default() };
        let id = self.spawn_pane_in(ws, None, spec, Placement::Floating, None)?;
        if let Some((wi, ti)) = self.workspace_of(id)
            && let Some(f) = self.workspaces[wi].tabs[ti].floating.iter_mut().find(|f| f.pane == id)
        {
            f.rect = FloatRect { x: 0.03, y: 0.04, w: 0.94, h: 0.92 };
        }
        self.relayout();
        Ok(id)
    }

    /// Default panels for a client: the focused pane's directory, and its server if it has one.
    pub(super) fn files_defaults(&self, client: ClientId) -> (String, String) {
        let focus = self.client_focus(client).and_then(|p| self.panes.get(&p));
        let local = focus
            .filter(|p| p.ssh.is_none())
            .and_then(|p| p.current_cwd())
            .unwrap_or_else(|| paths::home_dir().unwrap_or_default());
        let right = match focus.and_then(|p| p.ssh.as_ref()) {
            Some(s) if self.known_servers.iter().any(|k| k.server.name == s.name) => format!("{}:~", s.name),
            _ => paths::home_dir().unwrap_or_default().display().to_string(),
        };
        (local.display().to_string(), right)
    }

    /// The size this client sees the file manager at: a smaller terminal shows less of it, and
    /// clicks, pages and the wheel are counted in what it shows. As the pane is drawn: its frame
    /// clipped to the client's screen, then the content inside it.
    pub(super) fn files_view(&self, client: ClientId, pane: PaneId) -> Option<(u16, u16)> {
        let c = self.clients.get(&client)?;
        let wi = self.ws_index(c.workspace)?;
        let (_, frame, _) = self
            .tab_rects(self.workspaces[wi].tab()?, self.client_area(client))
            .into_iter()
            .find(|(p, _, _)| *p == pane)?;
        let (_, inner) = self.placed(frame, self.screen_above_bar(c.cols, c.rows));
        let p = self.panes.get(&pane)?;
        Some((inner.width.min(p.cols), inner.height.min(p.rows)))
    }

    /// A key typed in a file manager pane.
    pub(super) fn files_key(&mut self, client: ClientId, pane: PaneId, k: &Key) -> Mode {
        let fk = match k.code {
            _ if k.mods.ctrl || k.mods.alt => return Mode::Normal,
            KeyCode::Up => FileKey::Up,
            KeyCode::Down => FileKey::Down,
            KeyCode::PageUp => FileKey::PageUp,
            KeyCode::PageDown => FileKey::PageDown,
            KeyCode::Home => FileKey::Home,
            KeyCode::End => FileKey::End,
            KeyCode::Enter => FileKey::Enter,
            KeyCode::Backspace => FileKey::Backspace,
            KeyCode::Tab | KeyCode::BackTab => FileKey::Tab,
            KeyCode::Esc => FileKey::Esc,
            KeyCode::Delete => FileKey::Delete,
            KeyCode::Insert => FileKey::Insert,
            KeyCode::F(n) => FileKey::F(n),
            KeyCode::Char(c) => FileKey::Char(c),
            KeyCode::Left | KeyCode::Right => FileKey::Tab,
        };
        let view = self.files_view(client, pane);
        let Some(Content::Files(f)) = self.panes.get_mut(&pane).map(|p| &mut p.content) else { return Mode::Normal };
        f.set_viewer(client);
        if let Some((w, h)) = view {
            f.set_view(client, w, h);
        }
        let action = f.key(fk);
        self.mark_dirty();
        self.files_action(client, pane, action)
    }

    /// Mouse click in a file manager pane, at pane-relative cell (col, row).
    pub(super) fn files_click(&mut self, client: ClientId, pane: PaneId, col: u16, row: u16, double: bool) -> Mode {
        let view = self.files_view(client, pane);
        let Some(Content::Files(f)) = self.panes.get_mut(&pane).map(|p| &mut p.content) else { return Mode::Normal };
        f.set_viewer(client);
        if let Some((w, h)) = view {
            f.set_view(client, w, h);
        }
        let action = f.click(col, row, double);
        self.mark_dirty();
        self.files_action(client, pane, action)
    }

    pub(super) fn files_action(&mut self, client: ClientId, pane: PaneId, action: FilesAction) -> Mode {
        // The places a job acts on, as the CLI reads them; a local path that is not UTF-8 cannot be
        // passed exactly, and a server whose settings changed since its panel was listed would be
        // another machine than the one shown.
        let places: Vec<&Location> = match &action {
            FilesAction::Copy { sources, dest, .. } | FilesAction::Move { sources, dest, .. } => {
                sources.iter().chain([dest]).collect()
            }
            FilesAction::Delete(targets) => targets.iter().collect(),
            FilesAction::Shell(loc) | FilesAction::Edit(loc) => vec![loc],
            _ => Vec::new(),
        };
        if places.iter().any(|l| !matches!(l, Location::Laptop { .. }) && l.exact_display().is_none()) {
            self.toast(
                client,
                NoticeKind::Error,
                "a path here is not valid UTF-8: kraal cannot point at it safely; use a shell",
            );
            return Mode::Normal;
        }
        if let Some(server) = self.stale_server(&places) {
            self.reconnect_panels(pane);
            self.toast(
                client,
                NoticeKind::Warning,
                format!("{server} is not reached as when its panel was listed: check the panel and retry"),
            );
            return Mode::Normal;
        }
        if places.iter().any(|l| matches!(l, Location::Laptop { .. })) {
            return self.laptop_files_action(client, action);
        }
        let expect: Vec<String> = places
            .iter()
            .filter_map(|l| match l {
                Location::Remote(r) => Some(format!("--expect={}={}", r.server, crate::shell::join(&r.ssh_args))),
                Location::Local(_) | Location::Laptop { .. } => None,
            })
            .collect();
        let dir_arg = |l: &Location| {
            let shown = l.display();
            if shown.ends_with('/') { shown } else { format!("{shown}/") }
        };
        match action {
            FilesAction::None => Mode::Normal,
            FilesAction::Copy { sources, dest, replaces } => {
                // What the panel shows can be out of date: the copy itself checks again, and
                // replaces only what the user agreed to.
                let mut args = vec!["cp".to_string(), "--no-replace".to_string()];
                for name in &replaces {
                    args.push(format!("--allow={name}"));
                }
                args.extend(expect);
                args.push("--".into());
                args.extend(sources.iter().map(|s| s.display()));
                args.push(dir_arg(&dest));
                if replaces.is_empty() {
                    self.start_job(client, "copy", args);
                    return Mode::Normal;
                }
                // Copying over files is a decision, not a side effect.
                // Every name, one per line: the question scrolls.
                let message = format!(
                    "Copy {} to {}?\nIt replaces what is there:\n  {}",
                    describe(&sources),
                    dest.short_display(),
                    replaces.join("\n  ")
                );
                Mode::confirm(message, ConfirmAction::Job { label: "copy and replace".into(), args })
            }
            FilesAction::Move { sources, dest, replaces } => {
                let mut args = vec!["cp".to_string(), "--move".to_string(), "--no-replace".to_string()];
                for name in &replaces {
                    args.push(format!("--allow={name}"));
                }
                args.extend(expect);
                args.push("--".into());
                args.extend(sources.iter().map(|s| s.display()));
                args.push(dir_arg(&dest));
                let mut message = format!("Move {} to {}?", describe(&sources), dest.short_display());
                if !replaces.is_empty() {
                    message.push_str(&format!("\nIt replaces what is there:\n  {}", replaces.join("\n  ")));
                }
                Mode::confirm(message, ConfirmAction::Job { label: "move".into(), args })
            }
            FilesAction::Delete(targets) => {
                let mut args = vec!["rm".to_string()];
                args.extend(expect);
                args.push("--".into());
                args.extend(targets.iter().map(|s| s.display()));
                let list: Vec<String> = targets.iter().map(Location::short_display).collect();
                let what = if list.len() == 1 { list[0].clone() } else { format!("these {} items", list.len()) };
                let mut message = format!("Delete {what} for good?");
                if list.len() > 1 {
                    message.push_str(&format!("\n  {}", list.join("\n  ")));
                }
                Mode::confirm(message, ConfirmAction::Job { label: "delete".into(), args })
            }
            FilesAction::Preview(path) => {
                let Some(ws) = self.clients.get(&client).map(|c| c.workspace) else { return Mode::Normal };
                let spec = PaneSpec { preview: Some(path), ..Default::default() };
                if let Err(e) = self.spawn_pane_in(ws, Some(pane), spec, Placement::Floating, None) {
                    self.toast(client, NoticeKind::Error, format!("{e:#}"));
                }
                Mode::Normal
            }
            FilesAction::Shell(loc) => {
                let Some(ws) = self.clients.get(&client).map(|c| c.workspace) else { return Mode::Normal };
                let result = match &loc {
                    Location::Laptop { .. } => unreachable!("laptop actions are refused before launching jobs"),
                    Location::Local(p) => {
                        let spec = PaneSpec { cwd: Some(p.clone()), ..Default::default() };
                        self.spawn_pane_in(ws, None, spec, Placement::Tab, None)
                    }
                    Location::Remote(r) => match self.resolve_server(&r.server) {
                        Some(s) => {
                            let mut command = self.ssh_command(&s);
                            command.insert(1, "-t".into());
                            // A POSIX script, whatever the login shell there (fish, csh…); the
                            // path may still be `~/x` as typed, before a listing expanded it.
                            let script = format!(
                                "cd -- {} && exec \"${{SHELL:-sh}}\" -l",
                                crate::files::remote_dir_arg(&r.path)
                            );
                            command.push(crate::shell::sh_c(&script));
                            let spec = PaneSpec { command, server: Some(s.name.clone()), ..Default::default() };
                            self.spawn_pane_in(ws, None, spec, Placement::Tab, Some(s.name.clone()))
                        }
                        None => Err(anyhow::anyhow!("unknown server {}", r.server)),
                    },
                };
                if let Err(e) = result {
                    self.toast(client, NoticeKind::Error, format!("{e:#}"));
                }
                Mode::Normal
            }
            FilesAction::Edit(Location::Laptop { .. }) => Mode::Normal,
            FilesAction::Edit(Location::Local(path)) => {
                let Some(ws) = self.clients.get(&client).map(|c| c.workspace) else { return Mode::Normal };
                let command = crate::edit::editor_argv(&path, None);
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                let spec = PaneSpec { command, close_on_exit: true, keep_on_failure: true, ..Default::default() };
                if let Err(e) = self.spawn_pane_in(ws, Some(pane), spec, Placement::Floating, name) {
                    self.toast(client, NoticeKind::Error, format!("{e:#}"));
                }
                Mode::Normal
            }
            FilesAction::Edit(Location::Remote(r)) => {
                let Some(ws) = self.clients.get(&client).map(|c| c.workspace) else { return Mode::Normal };
                let production = self
                    .known_servers
                    .iter()
                    .find(|k| k.server.name == r.server)
                    .and_then(|k| self.server_production(&k.server));
                let target = EditTarget {
                    server: r.server,
                    ssh: r.ssh_args,
                    path: r.path,
                    line: None,
                    ws: Some(ws),
                    near: Some(pane),
                    sudo: false,
                    production: None,
                };
                self.edit_request(client, target, production)
            }
            FilesAction::PickLocation(side) => Mode::Picker(self.location_picker(client, pane, side)),
            FilesAction::Refused(why) => {
                self.toast(client, NoticeKind::Error, why);
                Mode::Normal
            }
            FilesAction::Close => {
                self.close_pane(pane);
                Mode::Normal
            }
        }
    }

    fn location_picker(&self, client: ClientId, pane: PaneId, side: Side) -> Picker {
        let mut items = vec![PickItem {
            label: if self.clients.get(&client).is_some_and(|c| c.remote) {
                crate::context::hostname().unwrap_or_else(|| "session".into()).split('.').next().unwrap().into()
            } else {
                "local".into()
            },
            detail: "the session's machine · ~".into(),
            action: PickAction::FilesSet { pane, side, target: "~".into() },
            state: None,
            dim: false,
        }];
        if let Some(laptop) = self.clients.get(&client).and_then(|c| c.laptop.as_ref()) {
            items.push(PickItem {
                label: laptop.host.clone(),
                detail: "this laptop · ~ · asks first".into(),
                action: PickAction::FilesLaptop { pane, side },
                state: None,
                dim: false,
            });
        }
        for k in &self.known_servers {
            let groups =
                if k.server.groups.is_empty() { String::new() } else { format!(" · {}", k.server.groups.join(", ")) };
            items.push(PickItem {
                label: k.server.name.clone(),
                detail: format!("{}{groups}", k.server.address()),
                action: PickAction::FilesSet { pane, side, target: format!("{}:~", k.server.name) },
                state: None,
                dim: false,
            });
        }
        let which = if side == Side::Left { "left" } else { "right" };
        Picker::new(PickerKind::Pane, &format!("Show in the {which} panel"), items)
    }

    /// The ssh arguments that reach `server` now, destination last (what a panel lists with).
    pub(super) fn panel_ssh_args(&self, server: &str) -> Option<Vec<String>> {
        let s = self.known_servers.iter().find(|k| k.server.name == server)?.server.clone();
        let mut args = self.ssh_options(&s);
        args.push(s.destination());
        Some(args)
    }

    /// A server among `places` that is not reached as its panel listed it any more.
    fn stale_server(&self, places: &[&Location]) -> Option<String> {
        places.iter().find_map(|l| match l {
            Location::Remote(r) if self.panel_ssh_args(&r.server).as_ref() != Some(&r.ssh_args) => {
                Some(r.server.clone())
            }
            _ => None,
        })
    }

    /// List the remote panels of a file manager again with their servers' current settings.
    fn reconnect_panels(&mut self, pane: PaneId) {
        for side in [Side::Left, Side::Right] {
            let Some(Content::Files(f)) = self.panes.get(&pane).map(|p| &p.content) else { return };
            let Location::Remote(r) = f.location(side) else { continue };
            // A server gone from the inventory stays as it was: its listing fails, and so would a job.
            if self.panel_ssh_args(&r.server).is_none_or(|now| now == r.ssh_args) {
                continue;
            }
            let loc = self.files_location(&format!("{}:{}", r.server, r.path));
            if let Some(Content::Files(f)) = self.panes.get_mut(&pane).map(|p| &mut p.content) {
                f.set_location(side, loc);
            }
        }
        self.mark_dirty();
    }

    pub(super) fn files_laptop(&mut self, client: ClientId, pane: PaneId, side: Side) {
        let Some(laptop) = self.clients.get(&client).and_then(|c| c.laptop.as_ref()) else {
            return;
        };
        let loc = Location::Laptop { owner_id: laptop.id.clone(), host: laptop.host.clone(), path: "~".into() };
        if let Some(Content::Files(f)) = self.panes.get_mut(&pane).map(|p| &mut p.content)
            && !f.set_laptop(side, loc, client)
        {
            self.toast(
                client,
                NoticeKind::Error,
                "Only one panel can show the laptop: choose the session's machine for the other panel",
            );
            return;
        }
        self.mark_persist();
        self.mark_dirty();
    }

    pub(super) fn laptop_requests(&mut self) {
        let mut requests = Vec::new();
        for (&pane, p) in &mut self.panes {
            if let Content::Files(f) = &mut p.content {
                requests.extend(
                    f.laptop_requests()
                        .into_iter()
                        .map(|(client, side, request, path)| (client, pane, side, request, path)),
                );
            }
        }
        for (client, pane, side, request, path) in requests {
            let mut bytes = Vec::new();
            let sent = crate::protocol::write_json(
                &mut bytes,
                &crate::protocol::ServerMsg::LaptopList { pane, side, request, path },
            )
            .is_ok()
                && self.clients.get(&client).is_some_and(|c| {
                    c.attached && c.out.send_timeout(bytes, std::time::Duration::from_millis(200)).is_ok()
                });
            if !sent && let Some(Content::Files(f)) = self.panes.get_mut(&pane).map(|p| &mut p.content) {
                f.laptop_result(client, side, request, Err("Cannot reach the laptop: reconnect and press r".into()));
                self.mark_dirty();
            }
        }
    }

    pub(super) fn files_set(&mut self, client: ClientId, pane: PaneId, side: Side, target: &str) {
        let loc = self.files_location(target);
        if let Some(Content::Files(f)) = self.panes.get_mut(&pane).map(|p| &mut p.content) {
            if !f.can_set(side, client) {
                self.toast(client, NoticeKind::Error, "This panel belongs to another laptop: use the other panel");
                return;
            }
            f.set_location(side, loc);
        }
        self.mark_persist();
        self.mark_dirty();
    }

    /// Reload every file manager (after a transfer finished).
    pub(super) fn refresh_file_managers(&mut self) {
        for p in self.panes.values_mut() {
            if let Content::Files(f) = &mut p.content {
                f.refresh();
            }
        }
    }

    /// Run `kraal <args>` in a floating pane that closes itself a few seconds after success.
    pub(super) fn start_job(&mut self, client: ClientId, label: &str, args: Vec<String>) {
        self.start_job_env(client, label, args, Vec::new());
    }

    pub(super) fn start_job_env(
        &mut self,
        client: ClientId,
        label: &str,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> Option<PaneId> {
        let ws = self.clients.get(&client)?.workspace;
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "kraal".into());
        let mut command = vec![exe];
        command.extend(args);
        let spec = PaneSpec { command, env, close_on_success: true, ..Default::default() };
        let previous = self.client_focus(client);
        match self.spawn_pane_in(ws, None, spec, Placement::Floating, Some(format!("⇅ {label}"))) {
            Ok(id) => {
                // A small window in the bottom-right corner, on top, that does not take the focus.
                if let Some((wi, ti)) = self.workspace_of(id) {
                    let tab = &mut self.workspaces[wi].tabs[ti];
                    if let Some(f) = tab.floating.iter_mut().find(|f| f.pane == id) {
                        f.rect = FloatRect { x: 0.5, y: 0.58, w: 0.48, h: 0.4 };
                    }
                    if let Some(prev) = previous.filter(|p| tab.contains(*p)) {
                        tab.focus = Some(prev);
                        tab.recent.retain(|p| *p != prev);
                        tab.recent.insert(0, prev);
                    }
                }
                self.relayout();
                Some(id)
            }
            Err(e) => {
                self.toast(client, NoticeKind::Error, format!("{e:#}"));
                None
            }
        }
    }
}

/// What a file operation acts on: the place, or the items by name.
pub(super) fn describe(locs: &[Location]) -> String {
    match locs {
        [one] => one.short_display(),
        many => {
            let shown: Vec<String> = many.iter().filter_map(Location::name).collect();
            format!("{} items ({})", many.len(), names(&shown))
        }
    }
}

/// Up to five names, then how many more.
fn names(list: &[String]) -> String {
    let mut out = list.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    if list.len() > 5 {
        out.push_str(&format!(" and {} more", list.len() - 5));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::laptop::Identity;
    use crate::protocol::{self, ClientMsg, ServerMsg};
    use crate::server::tests::{client, server};

    #[test]
    fn file_questions_shorten_only_session_paths_and_fit_their_contents() {
        use crate::server::overlays::{confirm_layout, draw_confirm};
        use ratatui::{buffer::Buffer, layout::Rect};

        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        let (owner, _peer) = client(&mut server);
        server.clients.get_mut(&owner).unwrap().laptop =
            Some(Identity { id: "abc".into(), host: "travel".into(), os: "linux".into() });
        let laptop = Location::Laptop { owner_id: "abc".into(), host: "travel".into(), path: "~/Docs".into() };
        for (root, shown) in [(paths::home_dir().unwrap(), "~"), ("/outside".into(), "/outside")] {
            let source = Location::Local(root.join("work"));
            let dest = Location::Local(root.join("Docs"));
            for (action, message, verb, height) in [
                (FilesAction::Delete(vec![source.clone()]), format!("Delete {shown}/work for good?"), "Delete", 4),
                (
                    FilesAction::Delete(vec![source.clone(), dest.clone()]),
                    format!("Delete these 2 items for good?\n  {shown}/work\n  {shown}/Docs"),
                    "Delete",
                    7,
                ),
                (
                    FilesAction::Move { sources: vec![source.clone()], dest: laptop.clone(), replaces: vec![] },
                    format!("Move {shown}/work to travel:~/Docs?"),
                    "Move",
                    4,
                ),
                (
                    FilesAction::Move { sources: vec![source.clone()], dest: dest.clone(), replaces: vec![] },
                    format!("Move {shown}/work to {shown}/Docs?"),
                    "Move",
                    4,
                ),
                (
                    FilesAction::Copy {
                        sources: vec![source.clone()],
                        dest: dest.clone(),
                        replaces: vec!["work".into()],
                    },
                    format!("Copy {shown}/work to {shown}/Docs?\nIt replaces what is there:\n  work"),
                    "Copy and replace",
                    7,
                ),
                (
                    FilesAction::Copy {
                        sources: vec![laptop.clone()],
                        dest: dest.clone(),
                        replaces: vec!["Docs".into()],
                    },
                    format!("Copy travel:~/Docs to {shown}/Docs?\nIt replaces what is there:\n  Docs"),
                    "Copy and replace",
                    7,
                ),
            ] {
                let Mode::Confirm(q) = server.files_action(owner, 1, action) else { panic!("confirmation") };
                assert_eq!(q.message, message);
                assert_eq!(q.action.verb(), verb);
                match &q.action {
                    ConfirmAction::Job { args, .. } => {
                        assert!(args.iter().any(|a| a.starts_with(root.to_str().unwrap())))
                    }
                    ConfirmAction::LaptopCopy { spec, .. } if spec.to_laptop => {
                        assert_eq!(spec.sources, [source.display()])
                    }
                    ConfirmAction::LaptopCopy { spec, .. } => assert_eq!(spec.destination, root.join("Docs")),
                    _ => panic!("file action"),
                }
                for (w, h) in [(80, 24), (124, 36)] {
                    let screen = Rect::new(0, 0, w, h);
                    let layout = confirm_layout(&q, screen);
                    assert_eq!(layout.area.height, height, "{message}");
                    let mut buf = Buffer::empty(screen);
                    draw_confirm(&q, screen, &mut buf);
                    let rows: Vec<String> = (layout.area.y..layout.area.bottom())
                        .map(|y| (layout.area.x..layout.area.right()).map(|x| buf[(x, y)].symbol()).collect())
                        .collect();
                    assert!(rows[0].contains(message.lines().next().unwrap()), "{rows:?}");
                    assert!(rows[height as usize - 3].contains(verb), "{rows:?}");
                    assert!(rows[height as usize - 2].contains("Enter Cancel · Tab Choose · Esc Cancel"), "{rows:?}");
                    for line in message.lines().skip(1) {
                        assert!(rows.iter().any(|row| row.contains(line)), "{rows:?}");
                    }
                }
            }
            let remote = Location::Remote(Remote { server: "web".into(), ssh_args: vec![], path: source.display() });
            assert_eq!(describe(&[remote]), format!("web:{}", root.join("work").display()));
        }
    }

    #[test]
    fn a_name_to_replace_starting_with_a_dash_is_not_an_option() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        let (owner, _peer) = client(&mut server);
        let (sources, dest) = (vec![Location::Local("/a/-notes.txt".into())], Location::Local("/b".into()));
        let replaces = vec!["-notes.txt".to_string()];
        for action in [
            FilesAction::Copy { sources: sources.clone(), dest: dest.clone(), replaces: replaces.clone() },
            FilesAction::Move { sources: sources.clone(), dest: dest.clone(), replaces: replaces.clone() },
        ] {
            let Mode::Confirm(q) = server.files_action(owner, 1, action) else { panic!("confirmation") };
            let ConfirmAction::Job { args, .. } = &q.action else { panic!("job") };
            assert!(
                args.contains(&"--allow=-notes.txt".to_string()) && !args.contains(&"--allow".to_string()),
                "{args:?}"
            );
        }
    }

    #[test]
    fn a_shell_on_a_server_goes_to_a_home_path_not_yet_listed() {
        let _batch = crate::pane::batch();
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        let (owner, _peer) = client(&mut server);
        let mut inv = crate::servers::Inventory::empty();
        inv.servers =
            vec![crate::servers::Server { name: "db".into(), host: "db.invalid".into(), ..Default::default() }];
        server.known_servers = crate::servers::known(&inv, &[], &[]);
        // As a panel holds them before (or without) a listing.
        for spec in ["db:~/huge", "db:"] {
            let place = server.files_location(spec);
            server.files_action(owner, 1, FilesAction::Shell(place));
        }
        let scripts: Vec<String> = server
            .panes
            .values()
            .filter(|p| p.spec.command.first().is_some_and(|c| c == "ssh"))
            .map(|p| crate::shell::unwrap_sh_c(p.spec.command.last().unwrap()))
            .collect();
        for cd in ["cd -- \"$HOME\"/'huge' &&", "cd -- \"$HOME\" &&"] {
            assert!(scripts.iter().any(|s| s.starts_with(cd)), "{scripts:?}");
        }
    }

    #[test]
    fn picker_offers_only_this_clients_laptop_and_names_the_sessions_machine() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        let (id, _peer) = client(&mut server);
        let picker = server.location_picker(id, 1, Side::Left);
        assert_eq!(picker.items[0].label, "local");
        assert!(!picker.items.iter().any(|i| matches!(i.action, PickAction::FilesLaptop { .. })));
        let c = server.clients.get_mut(&id).unwrap();
        c.remote = true;
        c.laptop = Some(Identity { id: "abc123".into(), host: "travel".into(), os: "linux".into() });
        let picker = server.location_picker(id, 1, Side::Left);
        assert_ne!(picker.items[0].label, "local");
        assert_eq!(picker.items[0].detail, "the session's machine · ~");
        assert_eq!(picker.items[1].label, "travel");
        let (other, _peer) = client(&mut server);
        assert!(!server.location_picker(other, 1, Side::Left).items.iter().any(|i| i.label == "travel"));
    }

    #[test]
    fn listing_routes_to_owner_saves_without_laptop_paths_and_detach_clears_it() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = server(tx);
        let (id, _peer) = client(&mut server);

        let c = server.clients.get_mut(&id).unwrap();
        c.remote = true;
        c.laptop = Some(Identity { id: "abc123".into(), host: "travel".into(), os: "linux".into() });
        let ws = c.workspace;
        let pane = server.open_files(ws, "~".into(), "~".into()).unwrap();
        let incoming = server.clients.get_mut(&id).unwrap().out.capture();
        server.files_laptop(id, pane, Side::Left);
        server.laptop_requests();
        let (requested, side, request, path) = loop {
            let frame = incoming.recv_timeout(crate::testing::PATIENCE).unwrap();
            let (_, bytes) = protocol::read_frame(&mut frame.as_slice()).unwrap().unwrap();
            if let ServerMsg::LaptopList { pane, side, request, path } = serde_json::from_slice(&bytes).unwrap() {
                break (pane, side, request, path);
            }
        };
        assert_eq!((requested, side, path.as_str()), (pane, Side::Left, "~"));
        let reply = ClientMsg::LaptopListed {
            pane,
            side,
            request,
            result: Ok((
                "~/private-path".into(),
                vec![crate::files::Entry {
                    name: "secret-entry".into(),
                    is_dir: false,
                    is_link: false,
                    size: 1,
                    mtime: 0,
                    exact: true,
                }],
            )),
        };
        let (other, _peer) = client(&mut server);
        server.client_frame(other, protocol::JSON, serde_json::to_vec(&reply).unwrap());
        let Content::Files(f) = &server.panes[&pane].content else { panic!("not files") };
        assert_eq!(f.location(side).display(), "travel:~");
        server.client_frame(id, protocol::JSON, serde_json::to_vec(&reply).unwrap());
        let Content::Files(f) = &server.panes[&pane].content else { panic!("not files") };
        assert_eq!(f.location(side).display(), "travel:~/private-path");
        let saved = serde_json::to_string(&crate::server::persistence::snapshot(&server)).unwrap();
        assert!(!saved.contains("private-path") && !saved.contains("secret-entry") && !saved.contains("abc123"));
        server.clients.get_mut(&other).unwrap().dirty = crate::server::Dirty::No;
        server.drop_client(id);
        assert!(matches!(server.clients[&other].dirty, crate::server::Dirty::Yes));
        let Content::Files(f) = &server.panes[&pane].content else { panic!("not files") };
        assert!(f.visible_text().contains("travel is not attached"));
    }
}
