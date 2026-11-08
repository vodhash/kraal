//! What the keymap's actions do.

use super::plural;
use crate::input::encode_key;
use crate::keymap::Action;
use crate::layout::{Dir, Node, PaneId, Preset, neighbour};
use crate::pane::PaneSpec;
use crate::protocol::Placement;
use crate::server::ui::{ConfirmAction, Mode, Prompt, PromptAction};
use crate::server::{ClientId, Server};
use crate::theme::NoticeKind;

impl Server {
    /// Give the focused pane a prefix key: `key` as pressed, or the first prefix.
    pub(super) fn send_prefix(&mut self, id: ClientId, key: Option<crate::input::Key>) {
        let key = key.or_else(|| {
            let spec = self.keymap.prefix.first()?;
            Some(crate::input::Key { code: spec.code, mods: spec.mods, raw: Vec::new() })
        });
        if let (Some(p), Some(key)) = (self.client_focus(id), key)
            && let Some(pane) = self.panes.get_mut(&p)
        {
            pane.write_input(encode_key(&key, pane.input_modes()));
        }
    }

    pub(super) fn action(&mut self, id: ClientId, action: Action) -> Mode {
        match action {
            Action::AttachClipboardImage => return self.offer_attachment(id, None, true),
            Action::AttachFiles => return self.offer_attachments(id, None),
            Action::PurgeAttachments => {
                return Mode::confirm(
                    "Remove all attachments from this session? Previously pasted paths will stop working.",
                    ConfirmAction::PurgeAttachments,
                );
            }
            Action::Tunnels => return Mode::Picker(self.tunnel_picker()),
            Action::AddServer => return Mode::ServerForm(self.add_server_form(None, None)),
            Action::NewWorkspace => return Mode::WorkspaceForm(self.add_workspace_form(id, None)),
            Action::WorkspacePicker => {
                return Mode::Picker(self.workspace_picker(self.clients.get(&id).map_or(0, |c| c.workspace)));
            }
            Action::PanePicker => return Mode::Picker(self.pane_picker()),
            Action::EditPane if self.client_focus(id).is_none() => {
                self.toast(id, NoticeKind::Warning, "Add a pane before editing.")
            }
            Action::Accounts => {
                self.refresh_existing_accounts();
                return Mode::Picker(self.accounts_picker(None));
            }
            Action::AddAccount => {
                if let Some(agent) = self
                    .client_focus(id)
                    .and_then(|p| self.panes.get(&p))
                    .and_then(|p| p.agent.kind.as_deref())
                    .and_then(crate::agent_registry::find)
                    .filter(|a| a.accounts.is_none())
                {
                    self.toast(id, NoticeKind::Notice, agent.account_message());
                    return Mode::Normal;
                }
                self.refresh_existing_accounts();
                return Mode::AccountForm(self.account_form(id));
            }
            Action::NextAttention => {
                let queue = self.attention_queue();
                let next = queue.iter().find(|a| !self.attention_shown(id, a)).or(queue.first()).cloned();
                match next {
                    Some(a) => self.go_to(id, a),
                    None => self.toast(id, NoticeKind::Notice, "no agent needs you"),
                }
            }
            Action::ToggleSidebar => {
                if let Some(c) = self.clients.get(&id)
                    && (c.cols < 40 || self.sidebar_width(c.cols) == 0)
                {
                    return Mode::Picker(self.workspace_picker(c.workspace));
                }
                let shown = self.clients.get(&id).is_some_and(|c| self.sidebar_shown(c));
                if let Some(c) = self.clients.get_mut(&id) {
                    c.sidebar = Some(!shown);
                    c.encoder.invalidate();
                }
                self.relayout();
            }
            Action::ToggleBottomBar => {
                self.bottom_bar = !self.bottom_bar;
                self.relayout();
            }
            Action::Buffers => return Mode::Picker(self.buffer_picker()),
            Action::Detach => self.detach(id, "detached"),
            Action::Help => return Mode::Help { scroll: 0 },
            Action::Messages => return Mode::Messages { scroll: 0 },
            Action::CommandPalette => return Mode::Picker(self.command_picker()),
            Action::Restart => return self.ask_restart(),
            Action::Quit => return self.ask_quit(),
            _ => return self.workspace_action(id, action),
        }
        self.mark_persist();
        self.mark_dirty();
        Mode::Normal
    }

    fn workspace_action(&mut self, id: ClientId, action: Action) -> Mode {
        let Some((ws, wi)) = self.current_ws(id) else { return Mode::Normal };
        if self.workspaces[wi].kept.is_some()
            && !self.workspaces[wi].closed()
            && !matches!(action, Action::NextWorkspace | Action::PrevWorkspace | Action::CloseWorkspace)
        {
            return Mode::confirm(
                "The workspace has not finished closing. Retry Close to continue.",
                ConfirmAction::RetryCloseWorkspace(ws),
            );
        }
        let focus = self.workspaces[wi].tab().and_then(|t| t.focus);
        match action {
            Action::NewPane | Action::SplitRight | Action::SplitDown | Action::NewFloating | Action::NewTab => {
                let placement = match action {
                    Action::SplitRight => Placement::Right,
                    Action::SplitDown => Placement::Down,
                    Action::NewFloating => Placement::Floating,
                    Action::NewTab => Placement::Tab,
                    _ => Placement::Auto,
                };
                if let Err(e) = self.spawn_pane_in(ws, focus, PaneSpec::default(), placement, None) {
                    self.toast(id, NoticeKind::Error, format!("{e:#}"));
                }
            }
            Action::ClosePane => {
                if let Some(p) = focus {
                    let busy = self.panes.get(&p).is_some_and(|pn| {
                        pn.exit_code.is_none() && (pn.has_foreground_job() || !pn.spec.command.is_empty())
                    });
                    if busy {
                        let name = self.panes[&p].display_name();
                        return Mode::confirm(
                            format!("Close \"{name}\"? Its program is still running."),
                            ConfirmAction::ClosePane(p),
                        );
                    }
                    self.close_pane(p);
                }
            }
            Action::ToggleZoom => {
                let Some(tab) = self.workspaces[wi].tab_mut() else { return Mode::Normal };
                tab.zoom = match (tab.zoom, focus) {
                    (Some(_), _) => None,
                    (None, Some(f))
                        if !tab.is_floating(f) && tab.root.as_ref().is_some_and(|r| r.leaves().len() > 1) =>
                    {
                        Some(f)
                    }
                    _ => None,
                };
                self.relayout();
            }
            Action::ToggleFloating => {
                let Some(tab) = self.workspaces[wi].tab_mut() else { return Mode::Normal };
                if tab.floating.is_empty() {
                    if let Err(e) = self.spawn_pane_in(ws, focus, PaneSpec::default(), Placement::Floating, None) {
                        self.toast(id, NoticeKind::Error, format!("{e:#}"));
                    }
                } else {
                    tab.show_floating = !tab.show_floating;
                    if tab.show_floating {
                        let top = tab.floating.last().map(|f| f.pane);
                        if let Some(t) = top {
                            tab.set_focus(t);
                        }
                    } else if focus.is_some_and(|f| tab.is_floating(f)) {
                        let tiled = tab.recent.iter().copied().find(|p| !tab.is_floating(*p));
                        tab.focus = tiled.or_else(|| tab.root.as_ref().and_then(Node::first_shown));
                        tab.expand_focus();
                    }
                }
            }
            Action::FocusLeft
            | Action::FocusRight
            | Action::FocusUp
            | Action::FocusDown
            | Action::FocusOrTabLeft
            | Action::FocusOrTabRight => {
                let dir = match action {
                    Action::FocusLeft | Action::FocusOrTabLeft => Dir::Left,
                    Action::FocusRight | Action::FocusOrTabRight => Dir::Right,
                    Action::FocusUp => Dir::Up,
                    _ => Dir::Down,
                };
                let moved = self.move_focus(wi, dir);
                if !moved {
                    match action {
                        Action::FocusOrTabLeft => self.switch_tab(wi, -1),
                        Action::FocusOrTabRight => self.switch_tab(wi, 1),
                        _ => {}
                    }
                }
                self.mark_seen_focused(id);
            }
            Action::ResizeLeft | Action::ResizeRight | Action::ResizeUp | Action::ResizeDown => {
                let dir = match action {
                    Action::ResizeLeft => Dir::Left,
                    Action::ResizeRight => Dir::Right,
                    Action::ResizeUp => Dir::Up,
                    _ => Dir::Down,
                };
                self.resize_focused(wi, dir);
            }
            Action::NextLayout => {
                let Some(tab) = self.workspaces[wi].tab_mut() else { return Mode::Normal };
                match tab.root.as_ref().and_then(|root| Preset::next(tab.preset, root, tab.focus)) {
                    Some((preset, tree)) => {
                        tab.root = Some(tree);
                        tab.preset = Some(preset);
                        tab.zoom = None;
                        self.relayout();
                        self.toast(id, NoticeKind::Notice, format!("Layout: {}", preset.describe()));
                    }
                    None => self.toast(id, NoticeKind::Notice, "One pane fills the tab: there is no layout to choose."),
                }
            }
            Action::StackLeft | Action::StackRight | Action::StackUp | Action::StackDown => {
                let dir = match action {
                    Action::StackLeft => Dir::Left,
                    Action::StackRight => Dir::Right,
                    Action::StackUp => Dir::Up,
                    _ => Dir::Down,
                };
                if let Err(why) = self.stack_focused(wi, dir) {
                    self.toast(id, NoticeKind::Warning, why);
                }
            }
            Action::Unstack => {
                let Some(tab) = self.workspaces[wi].tab_mut() else { return Mode::Normal };
                let unstacked = match (focus, &mut tab.root) {
                    (Some(f), Some(root)) => root.unstack(f).then_some(f),
                    _ => None,
                };
                match unstacked {
                    Some(f) => {
                        tab.preset = None;
                        tab.zoom = None;
                        tab.set_focus(f);
                        self.relayout();
                    }
                    None => self.toast(id, NoticeKind::Notice, "This pane is not in a stack."),
                }
            }
            Action::NextTab => self.switch_tab(wi, 1),
            Action::PrevTab => self.switch_tab(wi, -1),
            Action::MoveTabLeft | Action::MoveTabRight => {
                let ws = &mut self.workspaces[wi];
                let to = if action == Action::MoveTabLeft { ws.active.checked_sub(1) } else { Some(ws.active + 1) };
                if let Some(to) = to.filter(|to| *to < ws.tabs.len()) {
                    ws.tabs.swap(ws.active, to);
                    ws.active = to;
                    self.mark_persist();
                    self.mark_dirty();
                }
            }
            Action::Tab(n) => {
                let ws = &mut self.workspaces[wi];
                if (n as usize) <= ws.tabs.len() {
                    ws.active = n as usize - 1;
                }
                self.mark_seen_focused(id);
            }
            Action::CloseTab => {
                let Some(tab) = self.workspaces[wi].tab() else { return Mode::Normal };
                let n = tab.panes().len();
                let action = ConfirmAction::CloseTab { workspace: ws, tab: tab.id };
                return Mode::confirm(
                    format!("Close tab \"{}\" and its {n} pane{}?", self.tab_label(tab), plural(n)),
                    action,
                );
            }
            Action::RenameTab => {
                let Some(tab) = self.workspaces[wi].tab() else { return Mode::Normal };
                return Mode::Prompt(Prompt::new(
                    "Rename tab",
                    &tab.name,
                    PromptAction::RenameTab { workspace: ws, tab: tab.id },
                ));
            }
            Action::EditPane => {
                if let Some(pane) = focus {
                    return self.edit_pane_form(id, pane, None);
                }
            }
            Action::RenamePane => {
                if let Some(p) = focus {
                    let name = self.panes.get(&p).and_then(|pn| pn.name.clone()).unwrap_or_default();
                    return Mode::Prompt(Prompt::new("Rename pane", &name, PromptAction::RenamePane(p)));
                }
            }
            Action::RenameWorkspace => {
                return self.rename_workspace_form(ws, None);
            }
            Action::NextWorkspace | Action::PrevWorkspace => {
                let open: Vec<_> = self.workspaces.iter().filter(|w| !w.closed()).map(|w| w.id).collect();
                if let Some(at) = open.iter().position(|id| *id == ws) {
                    let delta = if action == Action::NextWorkspace { 1 } else { -1 };
                    let next = open[(at as i32 + delta).rem_euclid(open.len() as i32) as usize];
                    self.show_workspace(id, next);
                }
            }
            Action::SshPicker => return Mode::Picker(self.ssh_picker()),
            Action::AgentPicker => {
                self.refresh_existing_accounts();
                return Mode::Picker(self.agent_picker(id, None));
            }
            Action::GitActions => match focus.and_then(|f| self.worktree_picker(f)) {
                Some(p) => return Mode::Picker(p),
                None => {
                    let how = self.keymap.hint(Action::AgentPicker).unwrap_or_default();
                    self.toast(
                        id,
                        NoticeKind::Warning,
                        format!("This pane is not in a git repository ({how} starts an agent in a worktree)."),
                    )
                }
            },
            Action::ToolPicker => return Mode::Picker(self.tool_picker(focus)),
            Action::Hints => return self.start_hints(id),
            Action::Runbooks => return self.runbooks_action(id),
            Action::SwitchAccount => {
                if let Some(pane) = focus {
                    return self.change_account_mode(id, pane);
                }
            }
            Action::SelectForBroadcast => {
                if let Some(p) = focus {
                    self.toggle_mark(id, p);
                }
            }
            Action::BroadcastSelected
            | Action::BroadcastTab
            | Action::BroadcastWorkspace
            | Action::BroadcastOff
            | Action::BroadcastToggle => {
                use crate::server::SyncScope;
                let ws = &mut self.workspaces[wi];
                let scope = match action {
                    Action::BroadcastSelected => SyncScope::Marked,
                    Action::BroadcastTab => SyncScope::Tab,
                    Action::BroadcastWorkspace => SyncScope::Workspace,
                    Action::BroadcastToggle if ws.sync == SyncScope::Off => SyncScope::Tab,
                    _ => SyncScope::Off,
                };
                if scope == SyncScope::Marked && ws.marked.is_empty() {
                    let how = self.keymap.hint(Action::SelectForBroadcast).unwrap_or_default();
                    self.toast(id, NoticeKind::Warning, format!("Mark the panes first ({how} in each)."));
                    return Mode::Normal;
                }
                ws.sync = scope;
                if scope == SyncScope::Off {
                    ws.marked.clear();
                    ws.sync_approved.clear();
                    self.toast(id, NoticeKind::Notice, "Broadcast stopped: typing goes to the focused pane only.");
                } else if action == Action::BroadcastToggle && !std::mem::replace(&mut self.broadcast_tip_shown, true) {
                    // Once per server run: the key that starts it also stops it.
                    let again = self.keymap.hint(Action::BroadcastToggle).unwrap_or_default();
                    let msg = match self.keymap.menu_hint("broadcast") {
                        Some(more) => format!("{again} again stops it; {more} for more."),
                        None => format!("{again} again stops it."),
                    };
                    self.toast(id, NoticeKind::Notice, msg);
                }
                self.mark_dirty();
            }
            Action::ScrollMode => {
                if let Some(p) = focus
                    && let Some(pane) = self.panes.get_mut(&p)
                {
                    if pane.is_terminal() {
                        pane.copy_mode(true);
                    } else {
                        if let crate::pane::Content::Files(f) = &mut pane.content {
                            f.set_viewer(id);
                        }
                        pane.scroll_page(true);
                    }
                    return Mode::Scroll;
                }
            }
            Action::Files => {
                let (left, right) = self.files_defaults(id);
                if let Err(e) = self.open_files(ws, left, right) {
                    self.toast(id, NoticeKind::Error, format!("{e:#}"));
                }
            }
            Action::Search => {
                if focus.is_some_and(|f| self.panes.get(&f).is_some_and(|p| p.term().is_some())) {
                    return Mode::Search { query: String::new(), found: false };
                }
            }
            Action::CopyLastOutput => self.copy_last_output(id, focus),
            Action::OpenPreview => {
                return Mode::Prompt(Prompt::new("Preview file (path)", "", PromptAction::OpenPreview));
            }
            Action::SaveWorkspace => {
                return self.export_workspace_form(id, ws, None);
            }
            Action::Upload | Action::Download => {
                let Some(server) = focus.and_then(|f| self.pane_server(f)) else {
                    self.toast(id, NoticeKind::Warning, "this pane is not connected to a server");
                    return Mode::Normal;
                };
                if action == Action::Upload {
                    return Mode::Prompt(Prompt::new(
                        &format!("Upload to {}:~/ (local paths)", server.name),
                        "~/",
                        PromptAction::UploadPick { server: server.name },
                    ));
                }
                // Prefill with the selected text, if it looks like a path.
                let selected = focus.and_then(|f| self.panes.get(&f)).and_then(|p| p.selection_text());
                let path = selected.filter(|s| !s.contains('\n') && s.len() < 300).unwrap_or_default();
                let dir = self.config.servers.download_dir.clone();
                return Mode::Prompt(Prompt::new(
                    &format!("Download into {dir}"),
                    &format!("{}:{}", server.name, path.trim()),
                    PromptAction::Download,
                ));
            }
            Action::ResizeMode => {
                let tiled =
                    self.workspaces[wi].tab().and_then(|t| t.root.as_ref()).is_some_and(|r| r.places().len() > 1);
                if !tiled {
                    self.toast(id, NoticeKind::Notice, "One pane fills the tab: there is nothing to resize.");
                    return Mode::Normal;
                }
                return Mode::Resize;
            }
            Action::Tunnels
            | Action::AddServer
            | Action::NewWorkspace
            | Action::WorkspacePicker
            | Action::PanePicker
            | Action::Accounts
            | Action::AddAccount
            | Action::NextAttention
            | Action::ToggleSidebar
            | Action::ToggleBottomBar
            | Action::Buffers
            | Action::Detach
            | Action::Help
            | Action::Messages
            | Action::CommandPalette
            | Action::AttachClipboardImage
            | Action::AttachFiles
            | Action::PurgeAttachments
            | Action::Restart
            | Action::Quit => unreachable!("handled before selecting a workspace"),
            Action::CloseWorkspace => return self.ask_close_workspace(id, ws),
            Action::EditServers => self.edit_servers(id),
            Action::SendPrefix => self.send_prefix(id, None),
        }
        self.mark_persist();
        // What changed (the tab shown, the focus, floating panes) changed for every client
        // looking at this workspace.
        self.mark_dirty();
        Mode::Normal
    }

    pub(super) fn empty_workspace_enter(&mut self, id: ClientId) -> Mode {
        let Some((ws, wi)) = self.current_ws(id) else { return Mode::Normal };
        if self.workspaces[wi].close_error.is_some() {
            return self.ask_close_workspace(id, ws);
        }
        if self.workspaces[wi].closed() {
            self.show_workspace(id, ws);
            Mode::Normal
        } else {
            self.action(id, Action::NewPane)
        }
    }

    pub(in crate::server) fn show_workspace(&mut self, id: ClientId, ws: u32) {
        if let Err(e) = self.reopen_workspace(ws) {
            self.toast(id, NoticeKind::Error, format!("{e:#}"));
            return;
        }
        if let Some(c) = self.clients.get_mut(&id) {
            c.workspace = ws;
        }
        self.mark_seen_focused(id);
        self.mark_dirty();
    }

    pub(in crate::server) fn current_ws(&self, id: ClientId) -> Option<(u32, usize)> {
        let c = self.clients.get(&id)?;
        Some((c.workspace, self.ws_index(c.workspace)?))
    }

    pub(super) fn switch_tab(&mut self, wi: usize, delta: i32) {
        let ws = &mut self.workspaces[wi];
        let n = ws.tabs.len() as i32;
        if n > 0 {
            ws.active = (ws.active as i32 + delta).rem_euclid(n) as usize;
        }
        self.mark_dirty();
    }

    pub(super) fn move_focus(&mut self, wi: usize, dir: Dir) -> bool {
        let area = self.layout_area;
        let Some(tab) = self.workspaces[wi].tab_mut() else { return false };
        let Some(f) = tab.focus else { return false };
        if tab.is_floating(f) && tab.show_floating {
            let ids: Vec<PaneId> = tab.floating.iter().map(|x| x.pane).collect();
            if ids.len() < 2 {
                return false;
            }
            let i = ids.iter().position(|p| *p == f).unwrap_or(0) as i32;
            let step = if matches!(dir, Dir::Left | Dir::Up) { -1 } else { 1 };
            let next = ids[(i + step).rem_euclid(ids.len() as i32) as usize];
            tab.set_focus(next);
            return true;
        }
        if tab.zoom.is_some() {
            return false;
        }
        let Some(root) = &tab.root else { return false };
        // Up and down go through the panes of a stack before leaving it.
        let in_stack = root.stack_of(f).filter(|_| matches!(dir, Dir::Up | Dir::Down)).and_then(|(panes, _)| {
            let i = panes.iter().position(|p| *p == f)?;
            let j = if dir == Dir::Up { i.checked_sub(1)? } else { i + 1 };
            panes.get(j).copied()
        });
        if let Some(n) = in_stack {
            tab.set_focus(n);
            self.mark_dirty();
            return true;
        }
        let rects = root.layout(area);
        match neighbour(&rects, root.holder(f), dir, &tab.recent) {
            Some(n) => {
                tab.set_focus(n);
                self.mark_dirty();
                true
            }
            None => false,
        }
    }

    /// Stack the focused pane with the pane on side `dir`: it moves into that pane's place, after
    /// the panes there when it comes from below or the right, before them otherwise, and keeps
    /// the focus.
    pub(super) fn stack_focused(&mut self, wi: usize, dir: Dir) -> Result<(), &'static str> {
        let area = self.layout_area;
        let tab = self.workspaces[wi].tab_mut().ok_or("No panes. Add a pane first.")?;
        let Some(f) = tab.focus.filter(|f| !tab.is_floating(*f)) else { return Err("Floating panes do not stack.") };
        let Some(root) = &mut tab.root else { return Err("Floating panes do not stack.") };
        let onto = neighbour(&root.layout(area), root.holder(f), dir, &tab.recent).ok_or("No pane on that side.")?;
        if !root.stack(f, onto, matches!(dir, Dir::Left | Dir::Up)) {
            return Err("No pane on that side.");
        }
        tab.preset = None;
        tab.zoom = None;
        tab.set_focus(f);
        self.relayout();
        Ok(())
    }

    pub(super) fn resize_focused(&mut self, wi: usize, dir: Dir) {
        let area = self.layout_area;
        let Some(tab) = self.workspaces[wi].tab_mut() else { return };
        let Some(f) = tab.focus else { return };
        if let Some(fl) = tab.floating.iter_mut().find(|x| x.pane == f) {
            let (dw, dh) = match dir {
                Dir::Left => (-0.03, 0.0),
                Dir::Right => (0.03, 0.0),
                Dir::Up => (0.0, -0.04),
                Dir::Down => (0.0, 0.04),
            };
            fl.rect.w = (fl.rect.w + dw).clamp(0.1, 1.0);
            fl.rect.h = (fl.rect.h + dh).clamp(0.1, 1.0);
            fl.rect.x = fl.rect.x.min(1.0 - fl.rect.w);
            fl.rect.y = fl.rect.y.min(1.0 - fl.rect.h);
        } else if let Some(root) = &mut tab.root {
            tab.preset = None;
            let cells = if matches!(dir, Dir::Left | Dir::Right) { 4 } else { 2 };
            if !root.resize(f, dir, cells, area) {
                // No border on that side: shrink from the opposite one instead.
                let opposite = match dir {
                    Dir::Left => Dir::Right,
                    Dir::Right => Dir::Left,
                    Dir::Up => Dir::Down,
                    Dir::Down => Dir::Up,
                };
                root.resize(f, opposite, -cells, area);
            }
        }
        self.relayout();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Mods, MouseButton, MouseEvent, MouseKind};

    #[test]
    fn empty_workspaces_fit_the_smallest_supported_terminal() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        server.workspaces[0].tabs.clear();
        server.clients.get_mut(&client).unwrap().cols = 10;
        server.clients.get_mut(&client).unwrap().rows = 3;
        for bottom in [false, true] {
            server.bottom_bar = bottom;
            let mut buffer = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 10, 3));
            server.compose(client, 0, &mut buffer);
            let text: String = crate::testing::buffer_text(&buffer);
            assert!(text.contains("+ Add pane"));
        }
    }

    #[test]
    fn a_tab_moves_left_and_right_and_stays_shown() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let ws = &mut server.workspaces[0];
        ws.tabs = (1..=3).map(|id| crate::server::Tab::new(id, String::new())).collect();
        ws.active = 2;
        let order = |server: &Server| server.workspaces[0].tabs.iter().map(|t| t.id).collect::<Vec<_>>();
        for (action, tabs, active) in [
            (Action::MoveTabLeft, [1, 3, 2], 1),
            (Action::MoveTabLeft, [3, 1, 2], 0),
            (Action::MoveTabLeft, [3, 1, 2], 0),
            (Action::MoveTabRight, [1, 3, 2], 1),
        ] {
            server.action(client, action);
            assert_eq!((order(&server), server.workspaces[0].active), (tabs.to_vec(), active));
        }
    }

    #[test]
    fn screen_empty_workspaces_render_and_add_a_pane_with_enter_or_a_click() {
        for (cols, rows, mouse) in [(80, 24, false), (124, 36, true)] {
            let (tx, _rx) = crate::server::event_channel(4096);
            let mut server = crate::server::tests::server(tx);
            let (client, _peer) = crate::server::tests::client(&mut server);
            server.clients.get_mut(&client).unwrap().cols = cols;
            server.clients.get_mut(&client).unwrap().rows = rows;
            server.workspaces[0].tabs.clear();
            let mut buffer = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, cols, rows));
            server.compose(client, 0, &mut buffer);
            let text: String = crate::testing::buffer_text(&buffer);
            assert!(text.contains("No panes."));
            assert!(text.contains("+ Add pane"));
            let (button, _) = server.empty_workspace_button(client).unwrap();
            let content: Vec<String> = (button.y - 2..=button.y + 2)
                .map(|y| {
                    (button.x..button.right()).map(|x| buffer[(x, y)].symbol()).collect::<String>().trim().to_string()
                })
                .filter(|row| !row.is_empty())
                .collect();
            assert_eq!(content, ["No panes.", "+ Add pane"]);
            for action in [
                Action::CloseTab,
                Action::RenameTab,
                Action::MoveTabLeft,
                Action::MoveTabRight,
                Action::NextLayout,
                Action::ToggleZoom,
                Action::Unstack,
            ] {
                server.action(client, action);
                assert!(server.workspaces[0].tabs.is_empty());
            }
            if mouse {
                let (button, _) = server.empty_workspace_button(client).unwrap();
                server.on_mouse(
                    client,
                    MouseEvent {
                        kind: MouseKind::Press(MouseButton::Left),
                        col: button.x,
                        row: button.y,
                        mods: Mods::NONE,
                    },
                    false,
                );
            } else {
                server.empty_workspace_enter(client);
            }
            assert_eq!(server.workspaces[0].tabs.len(), 1);
            assert_eq!(server.panes.len(), 1);
            assert!(!server.quitting);
        }
    }
}
