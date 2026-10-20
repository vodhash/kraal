//! Questions: text prompts and confirmations, and what their answers do.

use anyhow::Context;

use crate::input::{InputEvent, KeyCode, MouseButton, MouseKind};
use crate::pane::PaneSpec;
use crate::paths;
use crate::protocol::Placement;
use crate::server::ui::{ConfirmAction, Mode, Prompt, PromptAction, Question};
use crate::server::workspace_ui::WorkspaceAction;
use crate::server::{ClientId, Server};
use crate::theme::NoticeKind;

impl Server {
    pub(super) fn on_prompt(&mut self, id: ClientId, mut p: Prompt, ev: InputEvent) -> Mode {
        let layout = crate::server::overlays::prompt_layout(&p, self.client_screen(id));
        p.scroll = p.scroll.min(layout.max_scroll);
        match ev {
            InputEvent::Key(k) => match k.code {
                KeyCode::Esc => return self.form_back(id, p.back),
                KeyCode::Char('c' | 'g') if k.mods.ctrl => return self.form_back(id, p.back),
                KeyCode::Enter => return self.submit_prompt(id, p),
                KeyCode::PageDown => {
                    p.scroll = (p.scroll + usize::from(layout.body).max(1)).min(layout.max_scroll);
                }
                KeyCode::PageUp => p.scroll = p.scroll.saturating_sub(usize::from(layout.body).max(1)),
                _ => {
                    let before = p.value.clone();
                    if crate::server::ui::edit_line(&mut p.value, &mut p.cursor, &k) && p.value != before {
                        p.error = None;
                        p.scroll = 0;
                    }
                }
            },
            InputEvent::Paste(data) => {
                let before = p.value.clone();
                let error =
                    crate::server::ui::paste_line(&mut p.value, &mut p.cursor, &String::from_utf8_lossy(&data)).err();
                if error.is_some() || p.value != before {
                    p.error = error;
                    p.scroll = 0;
                }
            }
            InputEvent::Mouse(m) => match m.kind {
                MouseKind::ScrollDown => p.scroll = (p.scroll + 2).min(layout.max_scroll),
                MouseKind::ScrollUp => p.scroll = p.scroll.saturating_sub(2),
                _ => {}
            },
            _ => {}
        }
        Mode::Prompt(p)
    }

    pub(super) fn submit_prompt(&mut self, id: ClientId, mut p: Prompt) -> Mode {
        p.scroll = 0;
        if matches!(p.data, PromptAction::RunbookValue) {
            return self.set_draft_value(id, p.value);
        }
        let title = p.title.clone();
        let (value, cursor, masked) = (p.value.clone(), p.cursor, p.masked);
        let back = p.back.clone();
        let renamed = match &p.data {
            PromptAction::RenameAccount { agent, account } => self.accounts.get(agent, account).map(|a| a.id),
            _ => None,
        };
        match self.commit_prompt(id, p) {
            Ok(()) => {
                let mode = self.form_back(id, back);
                match (renamed, mode) {
                    (Some(a), Mode::Picker(mut picker)) => {
                        crate::server::account_ui::select_account(&mut picker, &a);
                        Mode::Picker(picker)
                    }
                    (_, mode) => mode,
                }
            }
            // Not accepted: the prompt stays, with the reason under it.
            Err((action, why)) => {
                Mode::Prompt(Prompt { title, value, cursor, data: action, error: Some(why), scroll: 0, masked, back })
            }
        }
    }

    /// Act on a prompt's value; a value that cannot be used gives the action back with why.
    pub(super) fn commit_prompt(&mut self, id: ClientId, p: Prompt) -> Result<(), (PromptAction, String)> {
        let value = p.value.trim().to_string();
        match p.data {
            // Answered in submit_prompt, which goes on to the next question.
            PromptAction::RunbookValue => {}
            PromptAction::RenameTab { workspace, tab } => {
                // An empty name gives the tab back its automatic one (its focused pane's).
                if let Some(wi) = self.ws_index(workspace)
                    && let Some(t) = self.workspaces[wi].tabs.iter_mut().find(|t| t.id == tab)
                {
                    t.name = value;
                }
            }
            PromptAction::RenamePane(pane) => {
                self.rename_pane(pane, (!value.is_empty()).then_some(value));
            }
            PromptAction::OpenPreview => {
                if value.is_empty() {
                    return Err((p.data, "Type the path of a file or directory.".into()));
                }
                let base = self.client_focus(id).and_then(|p| self.panes.get(&p)).and_then(|p| p.current_cwd());
                let mut path = paths::expand_tilde(&value);
                if path.is_relative()
                    && let Some(b) = base
                {
                    path = b.join(path);
                }
                if !path.exists() {
                    return Err((p.data, format!("{} does not exist.", paths::shorten_home(&path))));
                }
                let Some(ws) = self.clients.get(&id).map(|c| c.workspace) else { return Ok(()) };
                let spec = PaneSpec { preview: Some(path), ..Default::default() };
                if let Err(e) = self.spawn_pane_in(ws, None, spec, Placement::Auto, None) {
                    self.toast(id, NoticeKind::Error, format!("{e:#}"));
                }
            }
            PromptAction::UploadPick { server } => {
                let base = paths::home_dir().unwrap_or_default();
                let mut args: Vec<String> = Vec::new();
                for word in value.split_whitespace() {
                    let p = paths::expand_tilde(word);
                    args.push(if p.is_relative() { base.join(p) } else { p }.display().to_string());
                }
                if args.is_empty() {
                    return Err((PromptAction::UploadPick { server }, "Type the local paths to upload.".into()));
                }
                args.push(format!("{server}:~/"));
                {
                    args.insert(0, "--".into());
                    args.insert(0, "cp".into());
                    self.start_job(id, "upload", args);
                }
            }
            PromptAction::Download => {
                if !value.contains(':') {
                    return Err((p.data, "Type server:path, the file to download.".into()));
                }
                let dir = paths::expand_tilde(&self.config.servers.download_dir);
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    return Err((p.data, format!("{}: {e}", dir.display())));
                }
                self.start_job(id, "download", vec!["cp".into(), "--".into(), value, format!("{}/", dir.display())]);
            }
            PromptAction::NewWorktree { agent, account } => {
                if let Err(e) = self.agent_in_worktree(id, &agent, account.as_deref(), &value) {
                    return Err((PromptAction::NewWorktree { agent, account }, format!("{e:#}")));
                }
            }
            PromptAction::Commit { pane } => self.commit_worktree(id, pane, &value),
            PromptAction::RunOn { targets } => {
                if value.is_empty() {
                    return Err((PromptAction::RunOn { targets }, "Type the command to run.".into()));
                }
                let Some(ws) = self.clients.get(&id).map(|c| c.workspace) else { return Ok(()) };
                let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "kraal".into());
                let mut command = vec![exe, "exec".to_string()];
                command.extend(targets.iter().cloned());
                command.push("--".into());
                command.push(value.clone());
                let spec = PaneSpec { command, ..Default::default() };
                let label = format!("» {} on {}", value.chars().take(30).collect::<String>(), targets.join(" "));
                if let Err(e) = self.spawn_pane_in(ws, None, spec, Placement::Floating, Some(label)) {
                    self.toast(id, NoticeKind::Error, format!("{e:#}"));
                }
            }
            PromptAction::RenameAccount { agent, account } => {
                if let Err(e) = self.rename_account(&agent, &account, &value) {
                    return Err((PromptAction::RenameAccount { agent, account }, format!("{e:#}")));
                }
                self.toast(id, NoticeKind::Notice, format!("renamed account to {agent}·{value}"));
            }
        }
        self.mark_persist();
        self.mark_dirty();
        Ok(())
    }

    pub(super) fn on_confirm(&mut self, id: ClientId, mut q: Question, ev: InputEvent) -> Mode {
        if matches!(q.action, ConfirmAction::OpenWorkspace(_)) {
            return self.on_confirm_inner(id, q, ev);
        }
        let back = q.workspace_back.take().map(|s| *s);
        let mode = self.on_confirm_inner(id, q, ev);
        match back {
            Some(state) => self.return_to_workspaces(id, mode, state),
            None => mode,
        }
    }

    fn on_confirm_inner(&mut self, id: ClientId, mut q: Question, ev: InputEvent) -> Mode {
        // Typed before a frame showed it: not an answer to it.
        if let ConfirmAction::ApproveProduction(ask) = q.action
            && self.ask_not_drawn(id, ask)
        {
            return Mode::Confirm(q);
        }
        let layout = crate::server::overlays::confirm_layout(&q, self.client_screen(id));
        let yes = match ev {
            InputEvent::Key(k) => {
                let plain = !k.mods.ctrl && !k.mods.alt;
                match k.code {
                    // Choose a button; Enter takes the selected one (Cancel at first).
                    KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab if plain => {
                        q.yes = !q.yes;
                        return Mode::Confirm(q);
                    }
                    // A long question scrolls, with the keys that scroll the help: by a row, or by
                    // as many as the box shows, so that paging through it skips none.
                    KeyCode::Up | KeyCode::Char('k') if plain => {
                        q.scroll = q.scroll.saturating_sub(1);
                        return Mode::Confirm(q);
                    }
                    KeyCode::Down | KeyCode::Char('j') if plain => {
                        q.scroll = (q.scroll + 1).min(layout.max_scroll);
                        return Mode::Confirm(q);
                    }
                    KeyCode::PageUp if plain => {
                        q.scroll = q.scroll.saturating_sub(layout.page());
                        return Mode::Confirm(q);
                    }
                    KeyCode::PageDown | KeyCode::Char(' ') if plain => {
                        q.scroll = (q.scroll + layout.page()).min(layout.max_scroll);
                        return Mode::Confirm(q);
                    }
                    KeyCode::Home | KeyCode::Char('g') if plain => {
                        q.scroll = 0;
                        return Mode::Confirm(q);
                    }
                    KeyCode::Enter if plain => q.yes,
                    KeyCode::Char('y' | 'Y') if plain => true,
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => false,
                    KeyCode::Char('c' | 'g') if k.mods.ctrl => false,
                    // Anything else does not answer.
                    _ => return Mode::Confirm(q),
                }
            }
            InputEvent::Mouse(m) => {
                let at = ratatui::layout::Position::new(m.col, m.row);
                let hit = |r: Option<ratatui::layout::Rect>| r.is_some_and(|r| r.contains(at));
                match m.kind {
                    MouseKind::Press(MouseButton::Left) if hit(layout.cancel) => false,
                    MouseKind::Press(MouseButton::Left) if hit(layout.ok) => true,
                    MouseKind::ScrollDown => {
                        q.scroll = (q.scroll + 2).min(layout.max_scroll);
                        return Mode::Confirm(q);
                    }
                    MouseKind::ScrollUp => {
                        q.scroll = q.scroll.saturating_sub(2);
                        return Mode::Confirm(q);
                    }
                    _ => return Mode::Confirm(q),
                }
            }
            _ => return Mode::Confirm(q),
        };
        // A yes needs the question to have been readable.
        if yes && ((!layout.lines.is_empty() && layout.body == 0) || layout.cancel.is_none()) {
            self.toast(
                id,
                NoticeKind::Warning,
                "The terminal is too small to show this question: make it larger to answer.",
            );
            return Mode::Confirm(q);
        }
        let action = q.action;
        let decision = if yes { "allowed" } else { "refused" };
        match action {
            ConfirmAction::RetrySsh(pane) => {
                if yes {
                    if let Some(p) = self.panes.get_mut(&pane).filter(|p| p.exit_code.is_some()) {
                        if let Err(e) = p.respawn(&self.env, &self.tx) {
                            return self.ssh_failure(pane, &format!("{e:#}"));
                        }
                        self.relayout();
                    }
                } else if self.panes.get(&pane).is_some_and(|p| p.exit_code.is_some()) {
                    self.close_pane(pane);
                }
                return Mode::Normal;
            }
            ConfirmAction::LoadWorkspace { prompt, inventory } => {
                return if yes {
                    self.load_source_form(id, prompt, Some(inventory))
                } else if prompt.data.undo {
                    self.workspace_form_back(id, prompt, None)
                } else {
                    Mode::WorkspaceForm(prompt)
                };
            }
            ConfirmAction::ApplyPane(form) => {
                return if yes { self.submit_pane_form(id, *form, true) } else { Mode::PaneForm(form) };
            }
            ConfirmAction::OpenWorkspace(action) => {
                let mut p = self.workspace_picker(self.clients[&id].workspace);
                if let Some(state) = q.workspace_back {
                    p.restore(*state);
                }
                return if yes {
                    self.pick(id, &p, crate::server::ui::PickItem::new("", "", action))
                } else {
                    Mode::Picker(p)
                };
            }
            ConfirmAction::RemoveAccount(a) => {
                if yes {
                    match self.update_accounts(|all| Ok(all.remove(&a.agent, &a.key()))) {
                        Ok(_) => {
                            self.reload_accounts();
                            self.toast(id, NoticeKind::Notice, "Account removed");
                        }
                        Err(e) => self.toast(id, NoticeKind::Error, format!("{e:#}")),
                    }
                }
                return Mode::Picker(self.accounts_picker(Some(&a.id)));
            }
            ConfirmAction::ReplaceWorkspaceFile { mut prompt, save } => {
                if !yes {
                    return Mode::WorkspaceForm(prompt);
                }
                let WorkspaceAction::Export(ws) = prompt.data.action else { unreachable!() };
                let result = self
                    .ws_index(ws)
                    .context("This workspace is no longer available; cancel and choose a workspace.")
                    .and_then(|wi| self.write_definition(wi, prompt.value.trim(), &save));
                match result {
                    Ok(path) => {
                        return self.exported_layout(prompt, &path);
                    }
                    Err(e) => {
                        prompt.error = Some(format!("{e:#}"));
                        prompt.scroll = 0;
                        return Mode::WorkspaceForm(prompt);
                    }
                }
            }
            ConfirmAction::UseExisting { existing, target, back } => {
                let mut picker = if yes {
                    if let Err(e) = self.confirm_existing_account(id, &existing, target.as_ref()) {
                        self.toast(id, NoticeKind::Error, format!("{e:#}"));
                        self.reload_accounts();
                    }
                    self.accounts_picker(None)
                } else {
                    self.review_account_picker(&existing)
                };
                picker.close_to = back;
                return Mode::Picker(picker);
            }
            ConfirmAction::StopAndSwitch { pane, token } => {
                self.confirm_switch(pane, token, id, yes);
                return Mode::Normal;
            }
            ConfirmAction::Production { workspace, approve, pending, asked, paste } => {
                return self.answer_production(id, yes, workspace, approve, pending, (asked, paste));
            }
            ConfirmAction::Edit(ref target) if let Some(why) = &target.production => {
                let sudo = if target.sudo { " --sudo" } else { "" };
                let plan = format!("kraal edit{sudo} {}:{}", target.server, target.path);
                let server = format!("{} ({})", target.server, crate::server::approvals::audit_why(why));
                self.record_yours(target.near, "edit", &plan, vec![server], decision);
            }
            ConfirmAction::ApproveProduction(ask) => {
                self.answer_production_ask(ask, yes);
                return Mode::Normal;
            }
            ConfirmAction::RunRunbook => {
                if let Some(d) = self.drafts.get(&id)
                    && let Some((plan, servers)) = d.asked.clone().filter(|(_, servers)| !servers.is_empty())
                {
                    self.record_yours(d.pane, "runbook", &plan, servers, decision);
                }
                self.confirm_runbook(id, yes);
                return Mode::Normal;
            }
            ConfirmAction::RunBlock { index, production } => {
                if let Some((pane, plan)) = production.as_ref().and_then(|_| self.block_record(id, index)) {
                    let server = self.prod_label(pane, &production.as_ref().unwrap().why);
                    self.record_yours(Some(pane), "runbook block", &plan, vec![server], decision);
                }
                return self.confirm_block(id, index, production, yes);
            }
            ConfirmAction::TrustRunbook => return self.confirm_trust(id, yes),
            ConfirmAction::TrustWorktreeSetup(new) => {
                self.confirm_worktree_setup(id, *new, yes);
                return Mode::Normal;
            }
            ConfirmAction::RunTool { pane, name, selection, reason, place } => {
                let server = pane
                    .map(|p| self.prod_label(p, &reason.why))
                    .unwrap_or_else(|| crate::server::approvals::audit_why(&reason.why).to_string());
                self.record_yours(pane, "tool", &name, vec![server], decision);
                if !yes {
                    return Mode::Normal;
                }
                // Asked about one production target, in one place: if the pane is now on another
                // (read again first), ask again; if it moved, nothing starts.
                return self.tool_request_after(id, pane, &name, selection, Some(reason), place);
            }
            _ => {}
        }
        match yes {
            true => {
                match action {
                    ConfirmAction::RetrySsh(_)
                    | ConfirmAction::LoadWorkspace { .. }
                    | ConfirmAction::RemoveAccount(_)
                    | ConfirmAction::ApplyPane(_)
                    | ConfirmAction::ReplaceWorkspaceFile { .. }
                    | ConfirmAction::OpenWorkspace(_)
                    | ConfirmAction::UseExisting { .. }
                    | ConfirmAction::StopAndSwitch { .. }
                    | ConfirmAction::Production { .. }
                    | ConfirmAction::ApproveProduction(_)
                    | ConfirmAction::RunRunbook
                    | ConfirmAction::RunBlock { .. }
                    | ConfirmAction::TrustRunbook
                    | ConfirmAction::TrustWorktreeSetup(_)
                    | ConfirmAction::RunTool { .. } => {
                        unreachable!("answered above")
                    }
                    ConfirmAction::Edit(target) => self.start_edit(Some(id), *target, None),
                    ConfirmAction::ClosePane(p) => self.close_pane(p),
                    ConfirmAction::CloseTab { workspace, tab } => {
                        if let Some(wi) = self.ws_index(workspace)
                            && let Some(t) = self.workspaces[wi].tabs.iter().find(|t| t.id == tab)
                        {
                            for p in t.panes() {
                                self.close_pane(p);
                            }
                        }
                    }
                    ConfirmAction::CloseWorkspace { workspace: ws, inventory } => {
                        let current = self.ask_close_workspace(id, ws);
                        if matches!(&current, Mode::Confirm(next)
                            if matches!(&next.action, ConfirmAction::CloseWorkspace { inventory: next, .. } if *next == inventory)
                                && next.message == q.message)
                        {
                            return self.close_workspace(id, ws);
                        }
                        return current;
                    }
                    ConfirmAction::RetryCloseWorkspace(ws) => {
                        return self.ask_close_workspace(id, ws);
                    }
                    ConfirmAction::ReorderWorkspace { workspace, neighbor, before, order } => {
                        return self.reorder_workspace_checked(id, workspace, neighbor, before, order);
                    }
                    ConfirmAction::DeleteWorkspace { target, .. } => {
                        let current = self.ask_delete_workspace(target.clone());
                        if matches!(&current, Mode::Confirm(next) if matches!(next.action, ConfirmAction::DeleteWorkspace { .. }) && next.message == q.message)
                        {
                            return self.delete_workspace(id, target);
                        }
                        return current;
                    }
                    ConfirmAction::RetryDeleteWorkspace(target) => {
                        if matches!(&target, super::super::workspace_lifecycle::DeleteTarget::Workspace(ws) if self.ws_index(*ws).is_some_and(|wi| self.workspaces[wi].delete_pending))
                        {
                            return self.delete_workspace(id, target);
                        }
                        return self.ask_delete_workspace(target);
                    }
                    ConfirmAction::Restart => self.restart(id),
                    ConfirmAction::AttachClipboardImage(pane) => return self.offer_attachment(id, Some(pane), true),
                    ConfirmAction::AttachFiles(pane) => return self.offer_attachments(id, Some(pane)),
                    ConfirmAction::PurgeAttachments => self.purge_attachments(id),
                    ConfirmAction::Quit => self.quit(id),
                    ConfirmAction::LaptopCopy { owner, spec } => self.start_laptop_copy(id, &owner, spec),
                    ConfirmAction::Job { label, args } => self.start_job(id, &label, args),
                    ConfirmAction::GitJob { label, script } => {
                        self.git_job(id, &label, script, crate::server::worktrees::JobEnd::CloseOnSuccess)
                    }
                    ConfirmAction::RemoveWorktrees(list) => self.remove_worktrees(id, list),
                    ConfirmAction::RetryAccountSwitch { pane, account, undo } => {
                        let result = self
                            .accounts
                            .by_id(&account.id)
                            .filter(|a| a.same_home(&account))
                            .context("the target account changed; choose an account again")
                            .and_then(|a| {
                                if undo {
                                    self.undo_account_change(pane, id)
                                } else {
                                    self.switch_account(pane, &a, Some(id), false)
                                }
                            });
                        if let Err(e) = result {
                            self.toast(id, NoticeKind::Error, format!("{e:#}"));
                        }
                    }
                    ConfirmAction::SignInAccount(account) => {
                        let result = self
                            .accounts
                            .by_id(&account.id)
                            .filter(|a| a.same_home(&account))
                            .context("the target account changed; choose an account again")
                            .and_then(|a| self.open_login(id, &a));
                        if let Err(e) = result {
                            self.toast(id, NoticeKind::Error, format!("{e:#}"));
                        }
                    }
                }
                Mode::Normal
            }
            false => Mode::Normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Key, Mods};

    fn key(code: KeyCode) -> InputEvent {
        InputEvent::Key(Key { code, mods: Mods::NONE, raw: Vec::new() })
    }

    #[test]
    fn close_keeps_cancel_first_and_readable_while_the_program_list_scrolls() {
        use crate::pane::PaneSpec;
        use crate::protocol::Placement;
        use crate::server::overlays::{confirm_layout, draw_confirm};
        use ratatui::buffer::Buffer;

        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let ws = server.clients[&client].workspace;
        crate::server::persistence::tests::writer(&mut server, vec![Ok(()), Ok(())]);
        for i in 0..30 {
            let placement = if i == 0 { Placement::Auto } else { Placement::Tab };
            let spec = PaneSpec { command: vec!["sleep".into(), "60".into()], ..Default::default() };
            server.spawn_pane_in(ws, None, spec, placement, None).unwrap();
        }
        for (cols, rows) in [(80, 24), (124, 36)] {
            let c = server.clients.get_mut(&client).unwrap();
            c.cols = cols;
            c.rows = rows;
            for cancel in [KeyCode::Enter, KeyCode::Esc, KeyCode::Char('n')] {
                let Mode::Confirm(q) = server.ask_close_workspace(client, ws) else { panic!("confirmation") };
                let layout = confirm_layout(&q, server.client_screen(client));
                assert!(layout.max_scroll > 0);
                let cancel_button = layout.cancel.unwrap();
                let close_button = layout.ok.unwrap();
                assert!(cancel_button.x < close_button.x);
                let mut buf = Buffer::empty(server.client_screen(client));
                draw_confirm(&q, server.client_screen(client), &mut buf);
                assert_ne!(buf[(cancel_button.x, cancel_button.y)].bg, buf[(close_button.x, close_button.y)].bg);
                assert!(!q.yes);
                assert_eq!(q.action.verb(), "Stop and close");
                let Mode::Confirm(q) = server.on_confirm(client, q, key(KeyCode::PageDown)) else { panic!("scroll") };
                assert!(q.scroll > 0);
                assert!(!q.yes);
                assert!(matches!(server.on_confirm(client, q, key(cancel)), Mode::Normal));
                assert_eq!(server.panes.len(), 30);
                assert_eq!(server.workspaces.len(), 1);
                assert!(!server.quitting);
            }
        }
        let Mode::Confirm(q) = server.ask_close_workspace(client, ws) else { panic!("confirmation") };
        let Mode::Confirm(q) = server.on_confirm(client, q, key(KeyCode::Tab)) else { panic!("choose") };
        assert!(q.yes);
        assert!(matches!(server.on_confirm(client, q, key(KeyCode::Enter)), Mode::Picker(_)));
        assert!(server.panes.is_empty());
        assert_eq!(server.workspaces.len(), 1);
        assert!(server.workspaces[0].closed());
        assert_eq!(server.workspaces[0].counts(), (30, 30));
        assert!(!server.quitting);
    }

    #[test]
    fn close_asks_again_when_another_client_changes_the_programs() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let ws = server.new_workspace("work".into(), None);
        let mut spec = crate::pane::PaneSpec { command: vec!["sleep".into(), "60".into()], ..Default::default() };
        let first = server
            .spawn_pane_in(ws, None, spec.clone(), crate::protocol::Placement::Tab, Some("first".into()))
            .unwrap();
        let Mode::Confirm(question) = server.ask_close_workspace(0, ws) else { panic!("confirm the program") };
        spec.command[1] = "61".into();
        let second =
            server.spawn_pane_in(ws, None, spec, crate::protocol::Placement::Tab, Some("second".into())).unwrap();
        for (pane, seconds) in [(first, "60"), (second, "61")] {
            let start = std::time::Instant::now();
            while server.panes[&pane].foreground_argv().is_none_or(|args| args != ["sleep", seconds]) {
                assert!(start.elapsed() < crate::testing::PATIENCE, "the program never started");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        let Mode::Confirm(updated) = server.on_confirm(0, question, key(KeyCode::Char('y'))) else {
            panic!("confirm the new inventory")
        };
        assert!(updated.message.contains("sleep 60") && updated.message.contains("sleep 61"), "{}", updated.message);
        assert!(!updated.yes);
        assert!(matches!(updated.action, ConfirmAction::CloseWorkspace { workspace: id, .. } if id == ws));
        assert!(server.workspaces[0].kept.is_none());
        for pane in [first, second] {
            assert!(server.panes[&pane].process_hold().unwrap().group_running());
        }
        assert!(matches!(server.on_confirm(0, updated, key(KeyCode::Enter)), Mode::Normal));
        for pane in [first, second] {
            server.close_pane(pane);
        }
    }

    #[test]
    fn close_asks_again_when_the_same_command_replaces_a_pane_or_process() {
        for same_pane in [false, true] {
            let (tx, _rx) = crate::server::queue::channel(32);
            let mut server = crate::server::tests::server(tx);
            let ws = server.new_workspace("work".into(), None);
            let spec = crate::pane::PaneSpec { command: vec!["sleep".into(), "60".into()], ..Default::default() };
            let first = server
                .spawn_pane_in(ws, None, spec.clone(), crate::protocol::Placement::Tab, Some("editor".into()))
                .unwrap();
            let ready = |server: &Server, pane| {
                let start = std::time::Instant::now();
                while server.panes[&pane].foreground_argv().is_none_or(|args| args != ["sleep", "60"]) {
                    assert!(start.elapsed() < crate::testing::PATIENCE, "the program never started");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            };
            ready(&server, first);
            let first_pid = server.panes[&first].pid().unwrap();
            let Mode::Confirm(mut question) = server.ask_close_workspace(0, ws) else { panic!("confirmation") };
            question.yes = true;
            let message = question.message.clone();
            let second = if same_pane {
                server.panes.get_mut(&first).unwrap().respawn(&server.env, &server.tx).unwrap();
                first
            } else {
                let second = server
                    .spawn_pane_in(ws, None, spec, crate::protocol::Placement::Tab, Some("editor".into()))
                    .unwrap();
                server.close_pane(first);
                second
            };
            ready(&server, second);
            assert_ne!(server.panes[&second].pid(), Some(first_pid));
            let Mode::Confirm(updated) = server.on_confirm(0, question, key(KeyCode::Enter)) else {
                panic!("the replacement needs its own confirmation")
            };
            assert_eq!(updated.message, message, "the labels did not change");
            assert!(!updated.yes);
            assert!(matches!(updated.action, ConfirmAction::CloseWorkspace { workspace: id, .. } if id == ws));
            assert!(server.workspaces[0].kept.is_none());
            assert!(server.panes[&second].process_hold().unwrap().group_running());
            crate::server::persistence::tests::writer(&mut server, vec![Ok(()), Ok(())]);
            assert!(matches!(server.on_confirm(0, updated, key(KeyCode::Char('y'))), Mode::Picker(_)));
            assert!(server.workspaces[0].closed());
            assert!(server.panes.is_empty());
        }
    }

    #[test]
    fn close_asks_again_when_the_workspace_is_renamed() {
        let _batch = crate::pane::batch();
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let ws = server.new_workspace("before".into(), None);
        let pane = server
            .spawn_pane_in(
                ws,
                None,
                crate::pane::PaneSpec { command: vec!["vim".into()], ..Default::default() },
                crate::protocol::Placement::Tab,
                None,
            )
            .unwrap();
        let Mode::Confirm(question) = server.ask_close_workspace(0, ws) else { panic!("confirmation") };
        assert_eq!(question.title.as_deref(), Some("Close before?"));
        let message = question.message.clone();
        server.workspaces[0].name = "after".into();
        let Mode::Confirm(updated) = server.on_confirm(0, question, key(KeyCode::Char('y'))) else {
            panic!("the renamed workspace needs confirmation")
        };
        assert_eq!(updated.title.as_deref(), Some("Close after?"));
        assert_eq!(updated.message, message);
        assert!(!updated.yes);
        assert!(matches!(updated.action, ConfirmAction::CloseWorkspace { workspace: id, .. } if id == ws));
        assert!(server.workspaces[0].kept.is_none());
        assert!(server.panes.contains_key(&pane));
        assert!(matches!(server.on_confirm(0, updated, key(KeyCode::Enter)), Mode::Normal));
        assert!(server.panes.contains_key(&pane));
    }

    #[test]
    fn close_keeps_the_full_workspace_name_readable_when_the_title_is_truncated() {
        use crate::server::overlays::{confirm_layout, draw_confirm};
        use ratatui::buffer::Buffer;

        let _batch = crate::pane::batch();
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let ws = server.clients[&client].workspace;
        server
            .spawn_pane_in(
                ws,
                None,
                crate::pane::PaneSpec { command: vec!["vim".into()], ..Default::default() },
                crate::protocol::Placement::Tab,
                None,
            )
            .unwrap();
        for prefix in ["x".repeat(60), "x".repeat(66), "🦀".repeat(1400)] {
            for suffix in ["alpha", "omega"] {
                let name = format!("{prefix}  {suffix}");
                server.workspaces[0].name = name.clone();
                let Mode::Confirm(mut q) = server.ask_close_workspace(client, ws) else { panic!("confirmation") };
                for (cols, rows) in [(80, 24), (124, 36), (80, 24)] {
                    let c = server.clients.get_mut(&client).unwrap();
                    c.cols = cols;
                    c.rows = rows;
                    q.scroll = 0;
                    let screen = server.client_screen(client);
                    let layout = confirm_layout(&q, screen);
                    let title = format!("Close {name}?");
                    assert_eq!(q.title.as_deref(), Some(title.as_str()));
                    let truncated = crate::text::columns(&title) > usize::from(layout.area.width - 4);
                    let mut read = vec![String::new(); layout.lines.len()];
                    loop {
                        let mut buf = Buffer::empty(screen);
                        draw_confirm(&q, screen, &mut buf);
                        let shown: Vec<String> = buf
                            .content
                            .chunks(cols as usize)
                            .map(|row| row.iter().map(|c| c.symbol()).collect())
                            .collect();
                        assert!(shown.iter().any(|row| row.contains("Cancel") && row.contains("Stop and close")));
                        assert!(!q.yes);
                        if !truncated {
                            assert!(shown.iter().any(|row| row.contains(&title)));
                            assert_eq!(layout.lines[0].text, "These programs will stop:");
                            break;
                        }
                        for row in 0..usize::from(layout.body).min(layout.lines.len() - q.scroll) {
                            let y = layout.area.y + 1 + row as u16;
                            let mut text = String::new();
                            let mut x = layout.area.x + 2;
                            while x < layout.area.right() - 2 {
                                let symbol = buf[(x, y)].symbol();
                                text.push_str(symbol);
                                x += crate::text::columns(symbol).max(1) as u16;
                            }
                            read[q.scroll + row] = text;
                        }
                        if q.scroll == layout.max_scroll {
                            break;
                        }
                        let Mode::Confirm(next) = server.on_confirm(client, q, key(KeyCode::PageDown)) else {
                            panic!("scroll the complete name")
                        };
                        q = next;
                    }
                    assert!(!truncated || read.concat().contains(&title), "{cols}x{rows}: {read:?}");
                }
            }
        }
    }

    #[test]
    fn retrying_an_unprepared_close_confirms_new_programs_from_every_entry() {
        use crate::input::{MouseButton, MouseEvent, MouseKind};
        use crate::server::render::SideItem;
        use crate::server::ui::{PickAction, PickItem};

        for entry in 0..4 {
            let (tx, _rx) = crate::server::queue::channel(64);
            let mut server = crate::server::tests::server(tx);
            let (client, _peer) = crate::server::tests::client(&mut server);
            let ws = server.clients[&client].workspace;
            crate::server::persistence::tests::writer(&mut server, vec![Err("disk full".into())]);
            let Mode::Confirm(retry) = server.ask_close_workspace(client, ws) else { panic!("retry") };
            assert!(matches!(retry.action, ConfirmAction::RetryCloseWorkspace(_)));
            assert!(server.workspaces[0].kept.is_none());
            let pane = server
                .spawn_pane_in(
                    ws,
                    None,
                    crate::pane::PaneSpec { command: vec!["sleep".into(), "60".into()], ..Default::default() },
                    crate::protocol::Placement::Tab,
                    Some("new editor".into()),
                )
                .unwrap();
            let start = std::time::Instant::now();
            while server.panes[&pane].foreground_argv().is_none_or(|args| args != ["sleep", "60"]) {
                assert!(start.elapsed() < crate::testing::PATIENCE, "the program never started");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let next = match entry {
                0 => server.on_confirm(client, retry, key(KeyCode::Char('y'))),
                1 => {
                    let picker = server.workspace_picker(ws);
                    server.pick(client, &picker, PickItem::new("Retry", "", PickAction::RetryWorkspaceClose(ws)))
                }
                2 => {
                    let row = (0..36)
                        .find(|&r| matches!(server.sidebar_hit(client, r), Some(SideItem::Workspace(w)) if w == ws))
                        .unwrap();
                    let press = MouseEvent { kind: MouseKind::Press(MouseButton::Left), col: 2, row, mods: Mods::NONE };
                    assert!(server.on_mouse(client, press, false).is_none());
                    server
                        .on_mouse(client, MouseEvent { kind: MouseKind::Release(MouseButton::Left), ..press }, false)
                        .unwrap()
                }
                _ => server.empty_workspace_enter(client),
            };
            let Mode::Confirm(question) = next else { panic!("confirm the new program") };
            assert!(question.message.contains("sleep 60"));
            assert!(matches!(question.action, ConfirmAction::CloseWorkspace { workspace: w, .. } if w == ws));
            assert!(!question.yes);
            assert!(server.panes[&pane].process_hold().unwrap().group_running());
            let cancelled = server.on_confirm(client, question, key(KeyCode::Enter));
            assert!(if matches!(entry, 1 | 2) {
                matches!(cancelled, Mode::Picker(_))
            } else {
                matches!(cancelled, Mode::Normal)
            });
            assert!(server.panes[&pane].process_hold().unwrap().group_running());
            server.close_pane(pane);
        }
    }

    #[test]
    fn a_failed_workspace_creation_keeps_the_name_and_cursor_for_retry_or_cancel() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let current = server.clients[&client].workspace;
        server.env.shell = "/nonexistent/kraal-shell".into();
        let mut p = Prompt::new(
            "New workspace",
            " café 日 ",
            crate::server::workspace_ui::WorkspaceForm::new(WorkspaceAction::New),
        );
        p.cursor = 3;
        for _ in 0..2 {
            let Mode::WorkspaceForm(next) = server.on_workspace_form(client, p, key(KeyCode::Enter)) else {
                panic!("keep input")
            };
            assert_eq!((next.value.as_str(), next.cursor), (" café 日 ", 3));
            assert!(matches!(next.data.action, WorkspaceAction::New));
            assert!(next.error.as_ref().unwrap().contains("Couldn't start the shell. No workspace was added."));
            assert_eq!(server.workspaces.len(), 1);
            assert_eq!(server.clients[&client].workspace, current);
            assert!(server.panes.is_empty());
            p = next;
        }
        assert!(matches!(server.on_workspace_form(client, p, key(KeyCode::Esc)), Mode::Normal));
        assert_eq!(server.workspaces[0].id, current);
    }

    #[test]
    fn workspace_name_validation_keeps_input_without_creating_a_workspace() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        for (value, error) in [("  ", "Type a name."), (" test ", "already used")] {
            let p = Prompt::new(
                "New workspace",
                value,
                crate::server::workspace_ui::WorkspaceForm::new(WorkspaceAction::New),
            );
            let Mode::WorkspaceForm(p) = server.on_workspace_form(client, p, key(KeyCode::Enter)) else {
                panic!("keep input")
            };
            assert_eq!(p.value, value);
            assert!(p.error.as_ref().unwrap().contains(error));
            assert_eq!(server.workspaces.len(), 1);
            assert!(server.panes.is_empty());
        }
    }

    #[test]
    fn preview_prompt_errors_scroll_without_changing_input_and_stay_until_an_edit() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        for (cols, rows, action) in [(80, 24, PromptAction::OpenPreview), (124, 36, PromptAction::OpenPreview)] {
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = (cols, rows);
            let mut p = Prompt::new("Preview file (path)", " café 日 ", action);
            let error = "Read this diagnostic.\n".repeat(100);
            p.error = Some(error.clone());
            let layout = crate::server::overlays::prompt_layout(&p, server.client_screen(client));
            let page = usize::from(layout.body);
            let wheel = |kind| InputEvent::Mouse(crate::input::MouseEvent { kind, col: 0, row: 0, mods: Mods::NONE });
            for (event, scroll) in [
                (key(KeyCode::PageDown), page),
                (key(KeyCode::PageDown), page * 2),
                (key(KeyCode::PageUp), page),
                (wheel(MouseKind::ScrollDown), page + 2),
                (wheel(MouseKind::ScrollUp), page),
                (key(KeyCode::Left), page),
                (InputEvent::Paste(Vec::new()), page),
            ] {
                let Mode::Prompt(next) = server.on_prompt(client, p, event) else { panic!("keep input") };
                assert_eq!(next.scroll, scroll);
                assert_eq!(next.value, " café 日 ");
                assert_eq!(next.error.as_deref(), Some(error.as_str()));
                p = next;
            }
            assert_eq!(p.cursor, 7);
            p.scroll = usize::MAX;
            let Mode::Prompt(next) = server.on_prompt(client, p, key(KeyCode::PageDown)) else { panic!() };
            assert_eq!(next.scroll, layout.max_scroll);
            p = next;
            for _ in 0..100 {
                let Mode::Prompt(next) = server.on_prompt(client, p, key(KeyCode::PageUp)) else { panic!() };
                p = next;
            }
            assert_eq!(p.scroll, 0);
            for event in [key(KeyCode::Char('x')), InputEvent::Paste(b"y".to_vec())] {
                p.error = Some(error.clone());
                p.scroll = page;
                let Mode::Prompt(next) = server.on_prompt(client, p, event) else { panic!() };
                assert!(next.error.is_none());
                assert_eq!(next.scroll, 0);
                p = next;
            }
            assert_eq!(p.value, " café 日xy ");
        }
    }
}
