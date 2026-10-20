//! Keyboard and mouse input from attached clients: modes, keys going to panes, the production
//! guard on input, pastes.

mod actions;
mod keyboard;
mod mouse;
mod pickers;
mod prompts;
mod scroll;

use std::time::Instant;

use super::contexts::Production;
use super::ui::{ConfirmAction, InputItem, Mode, PendingInput};
use super::{ClientId, Dirty, Server};
use crate::input::{InputEvent, Key, KeyCode, MouseEvent, MouseKind, encode_key, encode_paste_after};
use crate::keymap::{Action, Command, Lookup};
use crate::layout::{Dir, PaneId};
use crate::theme::NoticeKind;

/// Input kept while a pane it goes to has its context read again (see `Server::hold_input`),
/// with where it goes as it was when it was typed: its workspace, the focused pane and the panes
/// typing was broadcast to then. Where it goes is never worked out again: what was typed in a
/// tab goes to that tab's panes, whatever is shown by the time it goes.
pub(in crate::server) struct Held {
    client: ClientId,
    workspace: u32,
    focused: PaneId,
    targets: Vec<PaneId>,
    item: InputItem,
    /// The client said yes to it for these panes as they were then: it goes without another
    /// question if they still are, and not at all otherwise.
    approved: Option<Vec<(PaneId, Option<Production>)>>,
}

/// A question about input going to production: what it says, its workspace, and the panes a
/// yes approves for typing in sync.
struct ProductionAsk {
    message: String,
    workspace: u32,
    approve: Vec<(PaneId, Production)>,
}

/// A bracketed paste that arrives in pieces (a long one, or one slow to come) is still one paste:
/// it is held until its end, then pasted like any other, asked about as a whole when it goes to
/// production. Only one too big to hold goes out as it comes, and never to production. Either
/// way it goes to the panes it started going to, wherever the focus moves meanwhile.
pub(in crate::server) enum PasteFlow {
    Held {
        focused: PaneId,
        targets: Vec<PaneId>,
        data: Vec<u8>,
    },
    /// Going out as it comes (none of its panes was production when it started: it stops if
    /// one becomes production); `after_cr`: the last piece ended with CR.
    Streaming {
        focused: PaneId,
        targets: Vec<PaneId>,
        after_cr: bool,
    },
    /// Too big to hold for a question about production: none of it goes.
    Dropped,
}

/// Most of a paste held until its end: half of what a pane may have waiting, so that what was
/// held still fits when it goes out at once.
const PASTE_HOLD: usize = 8 << 20;

impl Server {
    pub(super) fn client_input(&mut self, id: ClientId, bytes: &[u8]) {
        let Some(c) = self.clients.get_mut(&id) else { return };
        if !c.attached {
            return;
        }
        // An action can reserve input midway through this frame. Keep the remaining raw
        // bytes before parsing them, just as if they had arrived in the next frame.
        let mut quiet = true;
        for (index, byte) in bytes.iter().enumerate() {
            if self.attachment_input(id, &bytes[index..]) {
                break;
            }
            let Some(c) = self.clients.get_mut(&id) else { break };
            let events = c.parser.feed(std::slice::from_ref(byte));
            c.pending_since = c.parser.has_pending().then(Instant::now);
            // Reserve the destination before a focus change, without waiting for a piece
            // or the parser's idle timeout. An empty paste reserves nothing.
            if c.paste.is_none() && matches!(c.mode, Mode::Normal) && c.parser.has_pending_paste() {
                self.paste_piece(id, Vec::new());
            }
            for event in events {
                quiet &= self.client_event(id, event);
            }
        }
        if !quiet {
            self.chrome_changed();
        }
        self.typed_quietly = quiet;
    }

    pub(super) fn flush_client_input(&mut self, id: ClientId) {
        let Some(c) = self.clients.get_mut(&id) else { return };
        c.pending_since = None;
        let events = c.parser.flush();
        self.client_events(id, events);
    }

    /// Act on what a client typed. Unless it all went quietly to programs, what the chrome
    /// shows may have changed.
    fn client_events(&mut self, id: ClientId, events: Vec<InputEvent>) {
        let mut quiet = true;
        for ev in events {
            quiet &= self.client_event(id, ev);
        }
        if !quiet {
            self.chrome_changed();
        }
        self.typed_quietly = quiet;
    }

    /// Act on one thing a client typed: whether it only went to programs, changing nothing on
    /// screen until they answer.
    fn client_event(&mut self, id: ClientId, ev: InputEvent) -> bool {
        // Motion reports update hover without taking control of the shared pane sizes.
        if !matches!(ev, InputEvent::Mouse(MouseEvent { kind: MouseKind::Move, .. })) {
            if let Some(c) = self.clients.get_mut(&id) {
                c.last_input = Instant::now();
            }
            if self.primary != Some(id) {
                self.primary = Some(id);
                self.relayout();
            }
        }
        if !matches!(ev, InputEvent::Mouse(_))
            && let Some(client) = self.clients.get_mut(&id)
            && matches!(client.drag, Some(super::ui::Drag::Workspace { .. }))
        {
            client.drag = None;
            if matches!(ev, InputEvent::Key(Key { code: KeyCode::Esc, .. })) {
                return false;
            }
        }
        if let InputEvent::Raw(raw) = &ev
            && (self.keyboard_reply(id, raw) || self.attached_request(id, raw) || self.terminal_reply(id, raw))
        {
            return false;
        }
        match &ev {
            InputEvent::FocusGained | InputEvent::FocusLost => {
                let gained = matches!(ev, InputEvent::FocusGained);
                if let Some(c) = self.clients.get_mut(&id) {
                    c.terminal_focused = gained;
                    if !gained
                        && let Mode::Picker(p) = &mut c.mode
                        && p.drag.take().is_some()
                    {
                        p.message = None;
                    }
                }
                if gained {
                    self.mark_seen_focused(id);
                    // Back from another window, where files may have been edited.
                    self.git_soon_for(id);
                }
                // The pane is told now, so that leaving and coming back are both told, in order.
                self.follow_focus();
                return false;
            }
            _ => {}
        }
        // The panes get a paste whole (`PasteFlow`); a text field takes the pieces as they come.
        let normal = self.clients.get(&id).is_some_and(|c| matches!(c.mode, Mode::Normal));
        let ev = match ev {
            InputEvent::PastePiece(data) if normal => {
                self.paste_piece(id, data);
                return false;
            }
            InputEvent::PastePiece(data) => InputEvent::Paste(data),
            InputEvent::Paste(_) if !normal && self.clients.get(&id).is_some_and(|c| c.paste.is_some()) => {
                // Something else took the keyboard while the paste was coming.
                match self.clients.get_mut(&id).and_then(|c| c.paste.take()) {
                    Some(PasteFlow::Held { .. }) => {
                        self.toast(id, NoticeKind::Warning, "the paste was interrupted: nothing was pasted")
                    }
                    Some(PasteFlow::Streaming { .. }) => {
                        // Pieces still waiting for a context read are part of the rest.
                        self.held_input.retain(|h| !(h.client == id && matches!(h.item, InputItem::Piece(..))));
                        self.toast(id, NoticeKind::Warning, "the paste was interrupted: the rest was not pasted")
                    }
                    _ => {}
                }
                return false;
            }
            ev => ev,
        };
        // Typing: drawn at once, and so is what the program echoes.
        let typed =
            matches!(ev, InputEvent::Key(_) | InputEvent::Raw(_) | InputEvent::KittyKey { .. } | InputEvent::Paste(_));
        let quiet = self.goes_quietly(id, &ev);
        let Some(c) = self.clients.get_mut(&id) else { return false };
        let mode = std::mem::replace(&mut c.mode, Mode::Normal);
        if typed
            && matches!(mode, Mode::Normal)
            && let Some(p) = self.client_focus(id)
        {
            self.echo.insert(p, Instant::now() + super::ECHO_WAIT);
        }
        let next = match mode {
            Mode::Normal => self.on_normal(id, ev),
            Mode::Leader { typed, miss } => self.on_leader(id, typed, miss, ev),
            Mode::Resize => self.on_resize(id, ev),
            Mode::Scroll => self.on_scroll(id, ev),
            Mode::Search { query, found } => self.on_search(id, query, found, ev),
            Mode::Picker(p) => self.on_picker(id, p, ev),
            Mode::Prompt(p) => self.on_prompt(id, p, ev),
            Mode::ServerForm(p) => self.on_server_form(id, p, ev),
            Mode::TunnelForm(p) => self.on_tunnel_form(id, p, ev),
            Mode::WorkspaceForm(p) => self.on_workspace_form(id, p, ev),
            Mode::AccountForm(form) => self.on_account_form(id, form, ev),
            Mode::PaneForm(form) => self.on_pane_form(id, *form, ev),
            Mode::TunnelDetails(details) => self.on_tunnel_details(id, details, ev),
            Mode::AccountDetails(details) => self.on_account_details(id, details, ev),
            Mode::Help { scroll } => {
                let lines = self.help_lines().len();
                self.on_help(id, scroll, lines, ev).unwrap_or(Mode::Help { scroll })
            }
            Mode::Messages { scroll } => {
                let lines = self.message_lines().len();
                self.on_help(id, scroll, lines, ev).map_or(Mode::Messages { scroll }, |m| match m {
                    Mode::Help { scroll } => Mode::Messages { scroll },
                    other => other,
                })
            }
            Mode::Confirm(q) => self.on_confirm(id, q, ev),
            Mode::Hints(h) => self.on_hints(id, h, ev),
        };
        // A key that only went to programs changes nothing on screen until they answer.
        let shown = !(quiet && matches!(next, Mode::Normal));
        if let Some(c) = self.clients.get_mut(&id) {
            c.mode = next;
            if shown {
                c.dirty = Dirty::Yes;
            }
        }
        // An agent's action on production waits for a client free to ask.
        if !self.production_asks.is_empty() && self.clients.get(&id).is_some_and(|c| matches!(c.mode, Mode::Normal)) {
            self.show_production_ask();
        }
        if shown {
            self.mark_dirty_client(id);
            if typed {
                self.render_now();
            }
        }
        // Each key may move the focus: a pane left and found again in one read is told both.
        if shown {
            self.follow_focus();
        }
        !shown
    }

    /// What a client attached from a pane of another kraal asks of this session, in what it
    /// sends as typed: to be told which pane it shows (`OSC 7701 ; report`), or to show one
    /// (`OSC 7701 ; show ; <pane id>`). An older kraal drops these, as any OSC typed.
    fn attached_request(&mut self, id: ClientId, raw: &[u8]) -> bool {
        let Some(rest) = raw.strip_prefix(b"\x1b]7701;") else { return false };
        let rest = rest.strip_suffix(b"\x07").or_else(|| rest.strip_suffix(b"\x1b\\")).unwrap_or(rest);
        if rest == b"report" {
            if let Some(c) = self.clients.get_mut(&id) {
                c.told_shown = Some(None);
            }
            self.mark_dirty_client(id);
        } else if let Some(pane) = rest.strip_prefix(b"show;") {
            let pane = std::str::from_utf8(pane).ok().and_then(|p| p.parse::<PaneId>().ok());
            if let Some(p) = pane.filter(|p| self.panes.contains_key(p)) {
                self.reveal(id, p);
            }
        } else {
            return false;
        }
        true
    }

    /// Whether `ev` only goes to the programs of the focused pane (and of those typing goes to),
    /// with nothing to draw for it: a key that is no command, typed into terminal panes none of
    /// which is scrolled back, without a selection to clear in the focused one.
    fn goes_quietly(&self, id: ClientId, ev: &InputEvent) -> bool {
        if !self.clients.get(&id).is_some_and(|c| matches!(c.mode, Mode::Normal)) {
            return false;
        }
        let Some(focus) = self.client_focus(id) else { return false };
        let at_bottom = |p: &PaneId| self.panes.get(p).is_some_and(|p| p.display_offset() == 0);
        let typing =
            |p: &crate::pane::Pane| p.is_terminal() && p.exit_code.is_none() && !p.has_selection() && !p.in_copy_mode();
        if !self.panes.get(&focus).is_some_and(typing) || !at_bottom(&focus) {
            return false;
        }
        match ev {
            InputEvent::Key(k) => {
                !self.keymap.is_prefix(k)
                    && self.keymap.direct(k).is_none()
                    && self.input_targets(id, focus).iter().all(at_bottom)
            }
            InputEvent::KittyKey { .. } => self.input_targets(id, focus).iter().all(at_bottom),
            InputEvent::Raw(_) => true,
            _ => false,
        }
    }

    /// Replies from the outer terminal to queries the server sent (colours, DA, CPR).
    fn terminal_reply(&mut self, id: ClientId, raw: &[u8]) -> bool {
        let s = String::from_utf8_lossy(raw);
        if self.graphics_reply(id, &s) {
            return true;
        }
        for (code, fg) in [("\x1b]10;", true), ("\x1b]11;", false)] {
            if let Some(rest) = s.strip_prefix(code) {
                let value = rest.trim_end_matches(['\x07', '\\']).trim_end_matches('\x1b').to_string();
                let (mut f, mut b) = self.outer_colors.clone().unwrap_or_default();
                if fg {
                    f = value;
                } else {
                    b = value;
                }
                self.outer_colors = Some((f.clone(), b.clone()));
                crate::pane::set_outer_colors(&f, &b);
                return true;
            }
        }
        s.starts_with("\x1b]")
            || s.starts_with("\x1bP")
            || (s.starts_with("\x1b[?") && (s.ends_with('c') || s.ends_with('R')))
    }

    // ---- modes ----------------------------------------------------------------------------

    fn on_normal(&mut self, id: ClientId, ev: InputEvent) -> Mode {
        match ev {
            InputEvent::Key(k) => {
                if self.keymap.is_prefix(&k) {
                    self.dismiss_welcome(id);
                    return Mode::leader();
                }
                if let Some(a) = self.keymap.direct(&k) {
                    return self.action(id, a);
                }
                self.key_to_pane(id, &k)
            }
            InputEvent::Mouse(m) => self.on_mouse(id, m, false).unwrap_or(Mode::Normal),
            InputEvent::Paste(data) => self.paste_end(id, data),
            InputEvent::Raw(raw) => {
                if let Some(p) = self.client_focus(id)
                    && let Some(pane) = self.panes.get_mut(&p)
                {
                    pane.write_input(raw);
                }
                Mode::Normal
            }
            // Only to a program that enabled the kitty protocol (flag 2 for releases, 8 for a
            // modifier pressed alone): a shell would print the rest of the sequence after its
            // first bytes.
            InputEvent::KittyKey { raw, release } => {
                let Some(p) = self.client_focus(id) else { return Mode::Normal };
                // Where typing goes (a broadcast), but never a question for these: other
                // production panes do not get them.
                let mut targets = self.input_targets(id, p);
                targets.retain(|t| *t == p || (self.pane_production(*t).is_none() && !self.pane_context_reading(*t)));
                let needs = crate::input::kitty_key_flag(&raw, release);
                let wanted = |flags: u8| flags != 0 && flags & needs == needs;
                for t in targets {
                    if let Some(pane) = self.panes.get_mut(&t)
                        && pane.exit_code.is_none()
                        && pane.is_terminal()
                        && wanted(pane.input_modes().kitty_flags)
                    {
                        pane.write_input(raw.clone());
                    }
                }
                Mode::Normal
            }
            _ => Mode::Normal,
        }
    }

    /// Keys after the leader: follow the menu until a command runs. A click on an entry presses
    /// its key; a click outside the menu closes it.
    fn on_leader(&mut self, id: ClientId, mut typed: Vec<Key>, miss: Option<String>, ev: InputEvent) -> Mode {
        let k = match ev {
            InputEvent::Key(k) => k,
            InputEvent::Mouse(m) => {
                let items = crate::server::overlays::menu_items(&self.keymap.menu(&typed), typed.is_empty());
                let layout = crate::server::overlays::menu_layout(&items, self.client_screen(id));
                let on_menu = layout.area.contains(ratatui::layout::Position::new(m.col, m.row));
                return match m.kind {
                    MouseKind::Press(crate::input::MouseButton::Left) if on_menu => {
                        match layout.item_at(m.col, m.row).and_then(|i| items[i].press.clone()) {
                            Some(spec) => {
                                let key = Key { code: spec.code, mods: spec.mods, raw: Vec::new() };
                                self.on_leader(id, typed, None, InputEvent::Key(key))
                            }
                            None => Mode::Leader { typed, miss },
                        }
                    }
                    // A click elsewhere closes the menu, and goes no further (not to the pane under it).
                    MouseKind::Press(_) if !on_menu => Mode::Normal,
                    _ => Mode::Leader { typed, miss },
                };
            }
            _ => return Mode::Leader { typed, miss },
        };
        match k.code {
            KeyCode::Esc => return Mode::Normal,
            KeyCode::Backspace if typed.is_empty() => return Mode::Normal,
            KeyCode::Backspace => {
                typed.pop();
                return Mode::Leader { typed, miss: None };
            }
            _ => {}
        }
        typed.push(self.keymap.leader_key(&typed, k));
        match self.keymap.lookup(&typed) {
            // The program gets the prefix: the one pressed when it was pressed twice (there may be
            // several), the first one when send_prefix has another key.
            Lookup::Run(Command::Action(Action::SendPrefix)) => {
                let pressed = typed.last().filter(|k| self.keymap.is_prefix(k)).cloned();
                self.send_prefix(id, pressed);
                Mode::Normal
            }
            Lookup::Run(command) => self.command(id, command),
            Lookup::More => Mode::Leader { typed, miss: None },
            Lookup::Nothing => {
                // Stay in the menu: the user sees what the keys are.
                let key =
                    typed.pop().map(|k| crate::input::KeySpec::new(k.code, k.mods).to_string()).unwrap_or_default();
                Mode::Leader { typed, miss: Some(format!("{key} does nothing here")) }
            }
        }
    }

    /// Run a command of the keymap.
    pub(super) fn command(&mut self, id: ClientId, command: Command) -> Mode {
        match command {
            Command::Action(a) => self.action(id, a),
            Command::Tool(name) => {
                let focus = self.client_focus(id);
                self.tool_request(id, focus, &name, None)
            }
        }
    }

    /// Resize mode: arrows (or hjkl) move the focused pane's borders until Enter or Esc.
    fn on_resize(&mut self, id: ClientId, ev: InputEvent) -> Mode {
        let InputEvent::Key(k) = ev else { return Mode::Resize };
        let dir = match k.code {
            KeyCode::Left | KeyCode::Char('h') => Dir::Left,
            KeyCode::Right | KeyCode::Char('l') => Dir::Right,
            KeyCode::Up | KeyCode::Char('k') => Dir::Up,
            KeyCode::Down | KeyCode::Char('j') => Dir::Down,
            KeyCode::Enter | KeyCode::Esc => {
                self.mark_persist();
                return Mode::Normal;
            }
            _ => return Mode::Resize,
        };
        if let Some((_, wi)) = self.current_ws(id) {
            self.resize_focused(wi, dir);
        }
        Mode::Resize
    }

    /// Keys of a scrolling viewer (help, messages): the next scroll position as `Help`, or
    /// another mode; `None` keeps it as it is.
    /// Keys in a viewer of `lines` (help, messages): scrolling stops at the last line.
    fn on_help(&mut self, id: ClientId, scroll: u16, lines: usize, ev: InputEvent) -> Option<Mode> {
        let scroll = match ev {
            InputEvent::Key(k) => match k.code {
                KeyCode::Down | KeyCode::Char('j') => scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => scroll.saturating_sub(1),
                KeyCode::PageDown | KeyCode::Char(' ') => scroll.saturating_add(10),
                KeyCode::PageUp => scroll.saturating_sub(10),
                KeyCode::Home | KeyCode::Char('g') => 0,
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => return Some(Mode::Normal),
                _ => return None,
            },
            InputEvent::Mouse(MouseEvent { kind: MouseKind::ScrollDown, .. }) => scroll.saturating_add(2),
            InputEvent::Mouse(MouseEvent { kind: MouseKind::ScrollUp, .. }) => scroll.saturating_sub(2),
            InputEvent::Mouse(MouseEvent { kind: MouseKind::Press(_), .. }) => return Some(Mode::Normal),
            _ => return None,
        };
        let max = crate::server::overlays::viewer_max_scroll(lines, self.client_screen(id));
        Some(Mode::Help { scroll: scroll.min(max) })
    }

    // ---- panes ----------------------------------------------------------------------------

    fn key_to_pane(&mut self, id: ClientId, k: &Key) -> Mode {
        if let Some((ws, wi)) = self.current_ws(id)
            && self.workspaces[wi].kept.is_some()
            && !self.workspaces[wi].closed()
        {
            return Mode::confirm(
                "The workspace has not finished closing. Retry Close to continue.",
                super::ui::ConfirmAction::RetryCloseWorkspace(ws),
            );
        }
        let Some(p) = self.client_focus(id) else {
            if k.code == KeyCode::Enter && !k.mods.ctrl && !k.mods.alt {
                return self.empty_workspace_enter(id);
            }
            return Mode::Normal;
        };
        let Some(pane) = self.panes.get_mut(&p) else { return Mode::Normal };
        if self.pane_edits.contains_key(&p) || self.switches.blocks_restart(p) {
            return Mode::Normal;
        }
        if matches!(pane.content, crate::pane::Content::Files(_)) {
            return self.files_key(id, p, k);
        }
        pane.clear_selection();
        if let Some(login) = &mut pane.spec.login {
            if pane.exit_code.is_some() {
                match login.state.action(k) {
                    Some(crate::pane::LoginAction::Close) => self.close_pane(p),
                    Some(action @ (crate::pane::LoginAction::Retry | crate::pane::LoginAction::Check)) => {
                        let result = if action == crate::pane::LoginAction::Retry {
                            self.retry_login(p)
                        } else {
                            self.check_login(p)
                        };
                        if let Err(e) = result {
                            self.toast(id, NoticeKind::Error, format!("{e:#}"));
                        }
                    }
                    None => {}
                }
                return Mode::Normal;
            }
            if k.code == KeyCode::Char('c') && k.mods.ctrl {
                login.state = crate::pane::LoginState::Cancelled;
                for c in self.clients.values_mut() {
                    if c.adding_account == Some(p) {
                        c.adding_account = None;
                    }
                }
            }
        }
        if pane.exit_code.is_some() {
            match (k.code, k.mods.ctrl) {
                (KeyCode::Enter, _) | (KeyCode::Char('r'), false) if pane.spec.pending_agent.is_some() => {
                    // An agent still waiting for its account: ask again rather than start it.
                    self.resolve_pending_agent(p);
                }
                (KeyCode::Enter, _) | (KeyCode::Char('r'), false) => {
                    // A pane that waits shows what Enter runs: that, as shown. Otherwise
                    // restarting an agent resumes its conversation instead of starting over; a
                    // command that may send a prompt again is shown first.
                    if !pane.suspended
                        && let Some((spec, exact)) = self.restart_spec(p)
                    {
                        if !exact {
                            if let Err(e) = self.suspend_pane(p, spec, super::agents::AMBIGUOUS) {
                                self.toast(id, NoticeKind::Error, format!("{e:#}"));
                            }
                            self.relayout();
                            return Mode::Normal;
                        }
                        self.panes.get_mut(&p).unwrap().spec = spec;
                    }
                    let pane = self.panes.get_mut(&p).unwrap();
                    match pane.respawn(&self.env, &self.tx) {
                        Ok(()) => self.runbook_pane_started(p),
                        Err(e) => self.toast(id, NoticeKind::Error, format!("{e:#}")),
                    }
                    self.relayout();
                }
                (KeyCode::Char('x'), false)
                | (KeyCode::Char('q'), false)
                | (KeyCode::Char('c'), true)
                | (KeyCode::Esc, _) => self.close_pane(p),
                _ => {}
            }
            return Mode::Normal;
        }
        if pane.is_preview() {
            let half = (pane.rows / 2).max(1) as i32;
            match k.code {
                KeyCode::Down | KeyCode::Char('j') => pane.scroll(-1),
                KeyCode::Up | KeyCode::Char('k') => pane.scroll(1),
                KeyCode::PageDown | KeyCode::Char(' ') => pane.scroll_page(false),
                KeyCode::PageUp => pane.scroll_page(true),
                KeyCode::Char('d') if k.mods.ctrl => pane.scroll(-half),
                KeyCode::Char('u') if k.mods.ctrl => pane.scroll(half),
                KeyCode::Char('g') | KeyCode::Home => pane.scroll_to_top(),
                KeyCode::Char('G') | KeyCode::End => pane.scroll_to_bottom(),
                KeyCode::Char('r') => {
                    if let crate::pane::Content::Preview(pv) = &mut pane.content {
                        pv.reload();
                    }
                }
                KeyCode::Char('q') | KeyCode::Esc => self.close_pane(p),
                _ => {}
            }
            return Mode::Normal;
        }
        let targets = self.input_targets(id, p);
        if self.hold_input(id, p, &targets, InputItem::Key(k.clone())) {
            return Mode::Normal;
        }
        self.send_to_targets(p, &targets, vec![InputItem::Key(k.clone())])
    }

    /// Send `items` to `targets` (the focused pane and those typing is broadcast to), asking
    /// first when they would reach production panes not approved yet: then all of them wait for
    /// the answer, as one.
    fn send_to_targets(&mut self, focused: PaneId, targets: &[PaneId], items: Vec<InputItem>) -> Mode {
        // Keys typed into one's own pane alone are never asked about (see `production_guard`):
        // no context needs looking at for them.
        if targets.iter().all(|t| *t == focused) && !items.iter().any(|i| matches!(i, InputItem::Paste(_))) {
            self.send_items(targets, items);
            return Mode::Normal;
        }
        let named = targets.iter().map(|t| (*t, self.pane_production(*t))).collect();
        let pending = PendingInput { items, targets: named };
        if let Some(ask) = self.production_guard(focused, targets, &pending) {
            let action = ConfirmAction::Production { workspace: ask.workspace, approve: ask.approve, pending };
            return Mode::confirm(ask.message, action);
        }
        self.send_items(targets, pending.items);
        Mode::Normal
    }

    fn send_items(&mut self, targets: &[PaneId], items: Vec<InputItem>) {
        for item in items {
            match item {
                InputItem::Key(k) => self.send_key(targets, &k),
                InputItem::Paste(data) => self.paste_into(targets, &data),
                InputItem::Piece(data, after_cr) => self.paste_into_after(targets, &data, after_cr),
            }
        }
    }

    /// Keep input going to panes whose context is being read again (`kubectx` may have just made
    /// one production): it goes out once the read has ended, through the production check (see
    /// [`Server::release_held`]), to where it was going when it was typed. What the client types
    /// after waits behind it, in order. A key waits for the panes typing is broadcast to (typing
    /// into one's own pane is never asked about); a paste also for the focused pane (several
    /// lines pasted into production are). No file is read for this: the reads are those kraal
    /// makes anyway, and they take milliseconds; one that does not answer soon makes the context
    /// unknown, which asks.
    fn hold_input(&mut self, id: ClientId, focused: PaneId, targets: &[PaneId], item: InputItem) -> bool {
        let behind = self.held_input.iter().any(|h| h.client == id);
        let own = !matches!(item, InputItem::Key(_));
        if !behind && !targets.iter().any(|t| (own || *t != focused) && self.pane_context_reading(*t)) {
            return false;
        }
        // A pane that is gone holds nothing: what goes to it is dropped where it is sent.
        let Some((wi, _)) = self.workspace_of(focused) else { return false };
        let workspace = self.workspaces[wi].id;
        self.held_input.push(Held { client: id, workspace, focused, targets: targets.to_vec(), item, approved: None });
        true
    }

    /// Send what [`Server::hold_input`] kept, client by client and in order, once none of the
    /// panes it goes to is being read and the client waits for nothing else. Consecutive input
    /// for the same panes goes as one: when it needs a question, the question is about all of it,
    /// and a no drops all of it. Input whose panes are no longer all there does not go at all.
    pub(super) fn release_held(&mut self) {
        if self.held_input.is_empty() {
            return;
        }
        let mut queue = std::mem::take(&mut self.held_input);
        let mut clients: Vec<ClientId> = Vec::new();
        for h in &queue {
            if !clients.contains(&h.client) {
                clients.push(h.client);
            }
        }
        let mut kept: Vec<Held> = Vec::new();
        for id in clients {
            let mut mine: std::collections::VecDeque<Held> = queue.extract_if(.., |h| h.client == id).collect();
            self.release_client(id, &mut mine);
            kept.extend(mine);
        }
        // What was put back meanwhile (an answer that waits for a read) comes first.
        kept.extend(queue);
        let put_back = std::mem::replace(&mut self.held_input, kept);
        for h in put_back.into_iter().rev() {
            let at = self.held_input.iter().position(|k| k.client == h.client).unwrap_or(self.held_input.len());
            self.held_input.insert(at, h);
        }
    }

    /// Release what one client typed, from the front of `queue`; what must still wait stays.
    fn release_client(&mut self, id: ClientId, queue: &mut std::collections::VecDeque<Held>) {
        while let Some(front) = queue.front() {
            if !self.clients.contains_key(&id) {
                queue.clear();
                return;
            }
            if !self.clients.get(&id).is_some_and(|c| matches!(c.mode, Mode::Normal)) {
                return;
            }
            let there = |s: &Server, h: &Held| {
                s.workspace_of(h.focused).is_some_and(|(wi, _)| s.workspaces[wi].id == h.workspace)
                    && h.targets.iter().all(|t| s.panes.contains_key(t))
            };
            if !there(self, front) {
                self.drop_queue(id, queue);
                self.toast(id, NoticeKind::Warning, "a pane closed while what you typed waited: none of it was sent");
                return;
            }
            if front.targets.iter().any(|t| self.pane_context_reading(*t)) {
                return;
            }
            let (focused, targets, approved) = (front.focused, front.targets.clone(), front.approved.clone());
            if matches!(front.item, InputItem::Piece(..)) {
                let h = queue.pop_front().expect("front");
                if targets.iter().any(|t| self.pane_production(*t).is_some()) {
                    // The rest of a paste that never goes to production, and what was typed after
                    // it: an Enter would run the part of it that went.
                    self.stop_stream(id);
                    self.drop_queue(id, queue);
                    self.toast(
                        id, NoticeKind::Warning,
                        "a pane became production during a long paste: the rest of it and what you typed after were not sent",
                    );
                    return;
                }
                self.send_items(&targets, vec![h.item]);
                continue;
            }
            // Everything typed for the same panes, up to the next piece of a long paste.
            let same = |h: &Held| {
                h.focused == focused
                    && h.targets == targets
                    && h.approved == approved
                    && !matches!(h.item, InputItem::Piece(..))
            };
            let n = queue.iter().take_while(|h| same(h)).count();
            let items: Vec<InputItem> = queue.drain(..n).map(|h| h.item).collect();
            if let Some(named) = approved {
                if named.iter().any(|(t, why)| self.pane_production(*t) != *why) {
                    self.drop_queue(id, queue);
                    self.toast(
                        id,
                        NoticeKind::Warning,
                        "a pane changed production since the question: nothing was sent",
                    );
                    return;
                }
                self.send_items(&targets, items);
                continue;
            }
            let next = self.send_to_targets(focused, &targets, items);
            if !matches!(next, Mode::Normal) {
                // A question about it: what came after waits for the answer.
                if let Some(c) = self.clients.get_mut(&id) {
                    c.mode = next;
                    c.dirty = Dirty::Yes;
                }
                self.mark_dirty_client(id);
                return;
            }
        }
    }

    /// Drop what a client typed that still waits; a long paste among it does not go on either.
    fn drop_queue(&mut self, id: ClientId, queue: &mut std::collections::VecDeque<Held>) {
        if queue.iter().any(|h| matches!(h.item, InputItem::Piece(..))) {
            self.stop_stream(id);
        }
        queue.clear();
    }

    /// Drop all that waits for client `id` (see [`Server::hold_input`]).
    fn drop_held(&mut self, id: ClientId) {
        let mut mine: std::collections::VecDeque<Held> = self.held_input.extract_if(.., |h| h.client == id).collect();
        self.drop_queue(id, &mut mine);
    }

    /// The long paste a client is sending does not go on.
    fn stop_stream(&mut self, id: ClientId) {
        if let Some(c) = self.clients.get_mut(&id)
            && matches!(c.paste, Some(PasteFlow::Streaming { .. }))
        {
            c.paste = Some(PasteFlow::Dropped);
        }
    }

    /// Type `k` into the running terminal panes among `targets`.
    fn send_key(&mut self, targets: &[PaneId], k: &Key) {
        for t in targets {
            if let Some(pane) = self.panes.get_mut(t)
                && pane.exit_code.is_none()
                && pane.is_terminal()
            {
                let bytes = encode_key(k, pane.input_modes());
                pane.write_input(bytes);
            }
        }
    }

    /// Ask before input reaches production panes it should not reach without a thought:
    /// synced typing into production panes not approved yet, or several lines pasted into a
    /// production program that runs them as they come. `None` when the input can go.
    fn production_guard(&self, focused: PaneId, targets: &[PaneId], pending: &PendingInput) -> Option<ProductionAsk> {
        let cfg = &self.config.production;
        let (wi, _) = self.workspace_of(focused)?;
        let ws = &self.workspaces[wi];
        let live = |t: &PaneId| self.panes.get(t).is_some_and(|p| p.exit_code.is_none() && p.is_terminal());
        let prod: Vec<(PaneId, Production)> =
            targets.iter().filter(|t| live(t)).filter_map(|t| self.pane_production(*t).map(|why| (*t, why))).collect();
        if prod.is_empty() {
            return None;
        }
        let approve: Vec<(PaneId, Production)> = if cfg.confirm_sync {
            prod.iter().filter(|(t, why)| *t != focused && ws.sync_approved.get(t) != Some(why)).cloned().collect()
        } else {
            Vec::new()
        };
        // A program in bracketed paste mode (a modern shell) shows a paste before running it.
        let runs_now = prod.iter().any(|(t, _)| !self.panes[t].input_modes().bracketed_paste);
        let lines: usize = pending
            .items
            .iter()
            .filter_map(|item| match item {
                InputItem::Paste(data) if cfg.confirm_paste && runs_now => {
                    let text = String::from_utf8_lossy(data);
                    text.contains(['\n', '\r']).then(|| text.trim_end().lines().count().max(1))
                }
                _ => None,
            })
            .sum();
        let keys = pending.items.iter().filter(|i| matches!(i, InputItem::Key(_))).count();
        if approve.is_empty() && lines == 0 {
            return None;
        }
        let names = |ids: &mut dyn Iterator<Item = PaneId>| -> String {
            let all: Vec<String> = ids.map(|t| self.pane_label(t)).collect();
            let mut s = all.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
            if all.len() > 4 {
                s.push_str(&format!(" and {} more", all.len() - 4));
            }
            s
        };
        let why = prod.first().map(|(_, w)| w.why.clone()).unwrap_or_default();
        let message = if lines > 0 {
            let to = names(&mut prod.iter().map(|(t, _)| *t));
            format!(
                "Paste {lines} line{} into production ({to})? They run as they arrive.\n{why}",
                if lines == 1 { "" } else { "s" }
            )
        } else {
            format!(
                "Typing is synced to production: {}. Send keys there too, until sync stops?\n{why} · no stops syncing",
                names(&mut approve.iter().map(|(t, _)| *t))
            )
        };
        // Keys typed while a context was read go with the question, or not at all.
        let message = if keys > 1 {
            format!("{message}\nThe {keys} keys typed meanwhile go with it, or none of them.")
        } else {
            message
        };
        Some(ProductionAsk { message, workspace: ws.id, approve })
    }

    fn answer_production(
        &mut self,
        id: ClientId,
        yes: bool,
        workspace: u32,
        approve: Vec<(PaneId, Production)>,
        pending: PendingInput,
    ) -> Mode {
        let Some(wi) = self.ws_index(workspace) else { return Mode::Normal };
        if !yes {
            // What the client typed after it waited behind it: it goes too.
            self.drop_held(id);
            if approve.is_empty() {
                self.toast(id, NoticeKind::Notice, "paste cancelled: nothing was sent");
            } else {
                let ws = &mut self.workspaces[wi];
                ws.sync = super::SyncScope::Off;
                ws.marked.clear();
                ws.sync_approved.clear();
                self.toast(id, NoticeKind::Notice, "sync off · nothing was sent");
                self.mark_dirty();
            }
            return Mode::Normal;
        }
        self.workspaces[wi].sync_approved.extend(approve);
        let PendingInput { items, targets: named } = pending;
        let targets: Vec<PaneId> = named.iter().map(|(t, _)| *t).collect();
        // The answer is for the panes the question named, as they were then: a context being
        // read again may be about to change that, so it waits for the read.
        if targets.iter().any(|t| self.pane_context_reading(*t)) {
            let focused = targets[0];
            let at = self.held_input.iter().position(|h| h.client == id).unwrap_or(self.held_input.len());
            for (n, item) in items.into_iter().enumerate() {
                let held = Held {
                    client: id,
                    workspace,
                    focused,
                    targets: targets.clone(),
                    item,
                    approved: Some(named.clone()),
                };
                self.held_input.insert(at + n, held);
            }
            return Mode::Normal;
        }
        if named.iter().any(|(t, why)| self.pane_production(*t) != *why) {
            self.drop_held(id);
            self.toast(id, NoticeKind::Warning, "a pane changed production since the question: nothing was sent");
            return Mode::Normal;
        }
        self.send_items(&targets, items);
        Mode::Normal
    }

    /// Panes that receive typed input: the focused one, plus the sync targets.
    pub(super) fn input_targets(&self, id: ClientId, focused: PaneId) -> Vec<PaneId> {
        let Some(c) = self.clients.get(&id) else { return vec![focused] };
        let Some(wi) = self.ws_index(c.workspace) else { return vec![focused] };
        self.workspaces[wi].sync_targets(focused)
    }

    /// Mark or unmark a pane for typing in sync.
    /// Select or unselect a pane for broadcast. Selecting sends nothing: broadcasting starts on
    /// its own command.
    pub(super) fn toggle_mark(&mut self, id: ClientId, pane: PaneId) {
        let Some((wi, _)) = self.workspace_of(pane) else { return };
        let ws = &mut self.workspaces[wi];
        if !ws.marked.remove(&pane) {
            ws.marked.insert(pane);
        }
        let n = ws.marked.len();
        let broadcasting = ws.sync == super::SyncScope::Marked;
        if n == 0 && broadcasting {
            ws.sync = super::SyncScope::Off;
            ws.sync_approved.clear();
        }
        let msg = match (n, broadcasting) {
            (0, true) => "No pane selected: broadcast stopped.".to_string(),
            (0, false) => "No pane selected.".to_string(),
            (n, true) => format!("Broadcasting to {n} selected pane{}.", plural(n)),
            (n, false) => {
                let how = self.keymap.hint(Action::BroadcastSelected).unwrap_or_default();
                format!("{n} pane{} selected · {how} starts broadcasting to them", plural(n))
            }
        };
        self.toast(id, NoticeKind::Notice, msg);
        self.mark_dirty();
    }

    pub(super) fn reserve_paste(&mut self, id: ClientId) {
        if self.clients.get(&id).is_some_and(|c| {
            c.attached
                && c.attachments.pending.is_none()
                && c.attachments.blocked.is_none()
                && c.paste.is_none()
                && matches!(c.mode, Mode::Normal)
        }) {
            self.paste_piece(id, Vec::new());
        }
    }

    /// A paste starts: it goes to the focused pane and to those typing is broadcast to now.
    fn paste_start(&self, id: ClientId) -> PasteFlow {
        match self.client_focus(id) {
            Some(focused) => PasteFlow::Held { focused, targets: self.input_targets(id, focused), data: Vec::new() },
            None => PasteFlow::Dropped,
        }
    }

    /// A piece of a paste still coming (see [`PasteFlow`]).
    fn paste_piece(&mut self, id: ClientId, piece: Vec<u8>) {
        let flow = match self.clients.get_mut(&id).and_then(|c| c.paste.take()) {
            Some(flow) => flow,
            None => self.paste_start(id),
        };
        let next = match flow {
            PasteFlow::Held { focused, targets, mut data } if data.len() + piece.len() <= PASTE_HOLD => {
                data.extend_from_slice(&piece);
                PasteFlow::Held { focused, targets, data }
            }
            PasteFlow::Held { focused, targets, data } => self.paste_overflow(id, focused, targets, &data, &piece),
            PasteFlow::Streaming { focused, targets, after_cr } => {
                self.paste_stream(id, focused, targets, &piece, after_cr)
            }
            PasteFlow::Dropped => PasteFlow::Dropped,
        };
        if let Some(c) = self.clients.get_mut(&id) {
            c.paste = Some(next);
        }
    }

    /// The end of a paste: all of it when it came in pieces.
    fn paste_end(&mut self, id: ClientId, last: Vec<u8>) -> Mode {
        let flow = match self.clients.get_mut(&id).and_then(|c| c.paste.take()) {
            Some(flow) => flow,
            None if last.is_empty() => return Mode::Normal,
            None => self.paste_start(id),
        };
        match flow {
            PasteFlow::Held { focused, targets, mut data } if data.len() + last.len() <= PASTE_HOLD => {
                data.extend_from_slice(&last);
                if data.is_empty() { Mode::Normal } else { self.paste_to(id, focused, &targets, data) }
            }
            PasteFlow::Held { focused, targets, data } => {
                self.paste_overflow(id, focused, targets, &data, &last);
                Mode::Normal
            }
            PasteFlow::Streaming { focused, targets, after_cr } => {
                self.paste_stream(id, focused, targets, &last, after_cr);
                Mode::Normal
            }
            PasteFlow::Dropped => Mode::Normal,
        }
    }

    /// A paste too big to hold: it goes out as it comes, unless it goes to production, where it
    /// would have been asked about as a whole: then none of it goes.
    fn paste_overflow(
        &mut self,
        id: ClientId,
        focused: PaneId,
        targets: Vec<PaneId>,
        held: &[u8],
        more: &[u8],
    ) -> PasteFlow {
        // A paste this big cannot wait for a context being read: it counts as production.
        if targets.iter().any(|t| self.pane_production(*t).is_some() || self.pane_context_reading(*t)) {
            self.toast(
                id,
                NoticeKind::Warning,
                format!("a paste of more than {} MiB into production: nothing was pasted", PASTE_HOLD >> 20),
            );
            return PasteFlow::Dropped;
        }
        match self.paste_stream(id, focused, targets, held, false) {
            PasteFlow::Streaming { focused, targets, after_cr } => {
                self.paste_stream(id, focused, targets, more, after_cr)
            }
            other => other,
        }
    }

    /// Send a piece of a paste to the panes it goes to, unless one of them has become
    /// production since it started: then the rest does not go. While a context of one of them is
    /// read again, pieces wait for the read (see [`Server::hold_input`]); when more than a paste
    /// held whole would be waiting, the rest does not go either.
    fn paste_stream(
        &mut self,
        id: ClientId,
        focused: PaneId,
        targets: Vec<PaneId>,
        data: &[u8],
        after_cr: bool,
    ) -> PasteFlow {
        if targets.iter().any(|t| self.pane_production(*t).is_some()) {
            self.held_input.retain(|h| !(h.client == id && matches!(h.item, InputItem::Piece(..))));
            self.toast(
                id,
                NoticeKind::Warning,
                "a pane became production during a long paste: the rest was not pasted",
            );
            return PasteFlow::Dropped;
        }
        let next_cr = data.last().map_or(after_cr, |b| *b == b'\r');
        let waiting: usize = self
            .held_input
            .iter()
            .filter(|h| h.client == id)
            .map(|h| match &h.item {
                InputItem::Piece(d, _) | InputItem::Paste(d) => d.len(),
                InputItem::Key(_) => 0,
            })
            .sum();
        if self.hold_input(id, focused, &targets, InputItem::Piece(data.to_vec(), after_cr)) {
            if waiting + data.len() > PASTE_HOLD {
                self.held_input.retain(|h| !(h.client == id && matches!(h.item, InputItem::Piece(..))));
                self.toast(
                    id,
                    NoticeKind::Warning,
                    "a long paste waited too long for a context read: the rest was not pasted",
                );
                return PasteFlow::Dropped;
            }
            return PasteFlow::Streaming { focused, targets, after_cr: next_cr };
        }
        self.paste_into_after(&targets, data, after_cr);
        PasteFlow::Streaming { focused, targets, after_cr: next_cr }
    }

    /// Paste `data` as the terminal would: into the focused pane and those typing is broadcast
    /// to, through the production guard.
    fn paste(&mut self, id: ClientId, data: Vec<u8>) -> Mode {
        let Some(p) = self.client_focus(id) else { return Mode::Normal };
        let targets = self.input_targets(id, p);
        self.paste_to(id, p, &targets, data)
    }

    /// Paste `data` into `targets`, `focused` among them, through the production guard: none
    /// of it when one of them closed since the paste started.
    pub(super) fn paste_to(&mut self, id: ClientId, focused: PaneId, targets: &[PaneId], data: Vec<u8>) -> Mode {
        if !targets.iter().all(|t| self.panes.contains_key(t)) {
            self.toast(id, NoticeKind::Warning, "a pane closed during the paste: nothing was pasted");
            return Mode::Normal;
        }
        if self.hold_input(id, focused, targets, InputItem::Paste(data.clone())) {
            return Mode::Normal;
        }
        self.send_to_targets(focused, targets, vec![InputItem::Paste(data)])
    }

    /// Paste a buffer chosen in the list, as if the terminal pasted it.
    fn paste_buffer(&mut self, id: ClientId, buffer: u64) -> Mode {
        let Some(text) = self.picked_buffer(id, buffer) else { return Mode::Normal };
        let Some(focused) = self.client_focus(id) else { return Mode::Normal };
        if let Err(e) = self.buffer_paste(focused, &text) {
            self.toast(id, NoticeKind::Error, e);
            return Mode::Normal;
        }
        self.paste(id, text.into_bytes())
    }

    fn paste_into(&mut self, targets: &[PaneId], data: &[u8]) {
        self.paste_into_after(targets, data, false);
    }

    fn paste_into_after(&mut self, targets: &[PaneId], data: &[u8], after_cr: bool) {
        for &t in targets {
            if let Some(pane) = self.panes.get_mut(&t)
                && pane.exit_code.is_none()
                && pane.is_terminal()
            {
                let bytes = encode_paste_after(data, pane.input_modes(), after_cr);
                pane.write_input(bytes);
            }
        }
    }

    /// Copy through the initiating client, and keep the newest paste buffer.
    pub(super) fn copy_to_client(&mut self, id: ClientId, text: &str) {
        self.keep_copy(text);
        self.send_clipboard(id, text);
    }

    /// Copy through the client policy, without changing paste buffers.
    pub(super) fn send_clipboard(&mut self, id: ClientId, text: &str) {
        self.send_copy(id, crate::protocol::ClipboardOrigin::User, text);
    }
}

pub(super) fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::PasteFlow;
    use crate::server::{
        queue,
        tests::{client, server},
    };

    #[test]
    fn a_paste_reserves_its_destination_before_the_parser_flushes() {
        let (tx, _events) = queue::channel(4096);
        let mut server = server(tx);
        let (id, _peer) = client(&mut server);
        server.workspaces[0].tabs[0].focus = Some(100);
        server.client_input(id, b"\x1b[200~\x1b[201~");
        assert!(server.clients[&id].paste.is_none());
        server.client_input(id, b"\x1b[200~first");
        assert!(matches!(
            &server.clients[&id].paste,
            Some(PasteFlow::Held { focused: 100, targets, data }) if targets == &[100] && data.is_empty()
        ));
        server.workspaces[0].tabs[0].focus = Some(200);
        server.client_input(id, b" second");
        server.flush_client_input(id);
        assert!(matches!(
            &server.clients[&id].paste,
            Some(PasteFlow::Held { focused: 100, targets, data }) if targets == &[100] && data == b"first second"
        ));
        server.client_input(id, b"\x1b[201~");
        assert!(server.clients[&id].paste.is_none());
    }

    #[test]
    fn clipboard_message_reports_sending_without_claiming_a_clipboard_write() {
        let (tx, _events) = queue::channel(4096);
        let mut server = server(tx);
        let (id, peer) = client(&mut server);
        server.copy_to_client(id, "hello");
        assert!(server.clients[&id].toast.is_none());
        let (copy, _, _) = server.clients[&id].clipboard.pending.unwrap();
        server.clipboard_result(id, copy, crate::protocol::ClipboardOutcome::Terminal, false);
        assert_eq!(server.clients[&id].toast.as_ref().unwrap().0, "copy sent to terminal via OSC 52");
        assert_eq!(server.buffers.get(None).unwrap().text, "hello");
        drop(peer);
        // The writer learns the peer is gone asynchronously on macOS.
        let start = std::time::Instant::now();
        loop {
            std::thread::sleep(crate::clipboard::INTERVAL);
            server.copy_to_client(id, "next");
            server.clients.get_mut(&id).unwrap().clipboard.pending = None;
            if server.clients[&id].toast.as_ref().unwrap().0 == "copy not sent: try again" {
                break;
            }
            assert!(start.elapsed() < crate::testing::PATIENCE, "the closed client still accepts copies");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(server.buffers.get(None).unwrap().text, "next");
    }
}
