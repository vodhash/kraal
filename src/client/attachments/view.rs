use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::connection::send;
use super::{Active, Controller, PROGRESS_INTERVAL, State, TransferProgress};
use crate::client::clipboard_image::Diagnostic;
use crate::protocol::ClientMsg;
use crate::server::overlays;
use crate::theme::NoticeKind;

impl Active {
    fn spinner(&self, theme: crate::theme::Theme, now: Instant) -> char {
        let frames = if theme.ascii {
            &['|', '/', '-', '\\'][..]
        } else {
            &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'][..]
        };
        let frame = (now.saturating_duration_since(self.started).as_millis() / PROGRESS_INTERVAL.as_millis()) as usize;
        frames[frame % frames.len()]
    }

    pub(super) fn status(&self, theme: crate::theme::Theme, now: Instant) -> String {
        if self.progress.is_none() {
            format!("{} Preparing attachment.\nTyping waits until it finishes.", self.spinner(theme, now))
        } else {
            "Typing waits until it finishes.".into()
        }
    }

    pub(super) fn transfer(&self, theme: crate::theme::Theme, now: Instant, done: bool) -> Option<TransferProgress> {
        let (received, total) = self.progress?;
        Some(TransferProgress {
            label: self.label.chars().filter(|c| !c.is_control()).collect(),
            received: if done { total } else { received },
            total,
            marker: if done { if theme.ascii { 'v' } else { '✓' } } else { self.spinner(theme, now) },
            ascii: theme.ascii,
        })
    }
}

impl TransferProgress {
    pub(super) fn message(&self, width: usize) -> String {
        let percent = (self.received.saturating_mul(100)).checked_div(self.total).unwrap_or(100).min(100);
        let bar_width = width.saturating_sub(2);
        let filled = (percent as usize * bar_width + 50) / 100;
        let (on, off) = if self.ascii { ("#", "-") } else { ("▓", "░") };
        format!(
            "{}\n{} / {} ({percent}%)\n{} {}{}",
            crate::text::truncate(&self.label, 64),
            crate::text::human_size(self.received),
            crate::text::human_size(self.total),
            self.marker,
            on.repeat(filled),
            off.repeat(bar_width - filled),
        )
    }
}

pub(in crate::client) struct View {
    pub(in crate::client) title: String,
    pub(in crate::client) kind: NoticeKind,
    pub(in crate::client) error_start: Option<usize>,
    pub(in crate::client) message: String,
    pub(in crate::client) transfer: Option<TransferProgress>,
    pub(in crate::client) files: String,
    pub(in crate::client) scroll: usize,
    pub(in crate::client) buttons: Vec<String>,
    pub(in crate::client) selected: usize,
    pub(in crate::client) keys: Vec<(&'static str, &'static str)>,
}

impl State {
    pub(super) fn view(&self, host: &str) -> Option<View> {
        let mut view = View {
            title: String::new(),
            kind: NoticeKind::Notice,
            error_start: None,
            message: String::new(),
            transfer: None,
            files: String::new(),
            scroll: 0,
            buttons: Vec::new(),
            selected: 0,
            keys: Vec::new(),
        };
        if self.active.is_some() {
            view.transfer = self.displayed.clone();
            view.title = format!("Sending to {host}");
            view.message = self.status.clone()?;
            view.buttons = vec!["Cancel".into()];
            view.keys = vec![("Ctrl+C", "Cancel")];
        } else if self.uncertain.is_some() {
            view.title = "Finishing attachment".into();
            view.message = self.status.clone()?;
        } else if let Some(d) = &self.dropped {
            view.selected = d.selected;
            if d.result {
                view.title = "Attachment failed".into();
                view.kind = if d.error.is_some() { NoticeKind::Error } else { self.status_kind };
                view.message =
                    self.status.clone().unwrap_or_else(|| "Transfer interrupted. Choose Retry to try again.".into());
                view.buttons = if d.image {
                    vec!["Retry".into(), "Close".into()]
                } else {
                    vec!["Retry".into(), "Paste as text".into(), "Close".into()]
                };
                view.selected = view.selected.min(view.buttons.len() - 1);
                view.keys = vec![("Enter", "Choose"), ("Tab", "Choose"), ("Esc", "Close")];
            } else if !d.ready {
                view.title = "Preparing attachment".into();
                view.message = "Waiting for the destination pane.".into();
                view.buttons = vec!["Cancel".into()];
                view.keys = vec![("Ctrl+C", "Cancel")];
            } else {
                view.title = if d.image {
                    format!("Send the clipboard image to {host}?")
                } else {
                    format!("Send {} {} to {host}?", d.paths.len(), if d.paths.len() == 1 { "file" } else { "files" })
                };
                view.files = d
                    .paths
                    .iter()
                    .map(|p| p.file_name().unwrap_or_default().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("\n");
                view.scroll = d.scroll;
                view.message = if d.image {
                    format!("Its path on {host} goes to {}.", pane_name(&d.target))
                } else {
                    format!(
                        "{:.1} MiB. Their paths on {host} go to {}.",
                        d.validate().unwrap_or(0) as f64 / 1048576.0,
                        pane_name(&d.target)
                    )
                };
                if d.broadcast {
                    view.message.push_str("\nBroadcasting is on. Paths go only to this pane.");
                }
                if let Some(error) = &d.error {
                    view.error_start = Some(view.message.len());
                    view.message.push_str(&format!("\n{error}"));
                }
                view.buttons = if d.image && d.shortcut {
                    vec!["Send".into(), format!("Always for {}", crate::text::truncate(host, 24)), "Cancel".into()]
                } else if d.image {
                    vec!["Send".into(), "Cancel".into()]
                } else {
                    vec![
                        "Send".into(),
                        format!("Always for {}", crate::text::truncate(host, 24)),
                        "Paste as text".into(),
                        "Cancel".into(),
                    ]
                };
                view.keys = vec![("Enter", "Choose"), ("Tab", "Choose"), ("Esc", "Cancel")];
            }
        } else if let Some(p) = &self.prompt {
            view.title = if p.image {
                format!("Send the clipboard image to {host}?")
            } else {
                format!("Attach files to {host}")
            };
            view.message = if p.image {
                format!("Its path on {host} goes to {}.\nSend reads the local clipboard once.", pane_name(&p.target))
            } else {
                format!(
                    "Enter absolute local paths, quoting spaces.\nPaths: {}\nTheir paths on {host} go to {}.",
                    crate::text::truncate_start(&p.text.chars().take(p.cursor).collect::<String>(), 64),
                    pane_name(&p.target)
                )
            };
            if let Some(error) = &p.error {
                view.error_start = Some(view.message.len());
                view.message.push_str(&format!("\n{error}"));
            }
            view.buttons = vec!["Send".into(), "Cancel".into()];
            view.selected = if p.image { p.cursor } else { 0 };
            view.keys = if p.image {
                vec![("Enter", "Choose"), ("Tab", "Choose"), ("Esc", "Cancel")]
            } else {
                vec![("Enter", "Send"), ("Ctrl+U", "Clear"), ("Esc", "Cancel")]
            };
        } else if let Some(status) = &self.status {
            view.kind = self.status_kind;
            view.title = if self.success_until.is_some() {
                if self.completed.is_some() { format!("Sent to {host}") } else { "Attachment complete".into() }
            } else {
                "Attachment".into()
            };
            if self.success_until.is_some() {
                view.transfer = self.completed.clone();
                view.message =
                    if let Some((title, body)) = status.split_once('\n').filter(|(_, body)| !body.trim().is_empty()) {
                        view.title = title.into();
                        body.into()
                    } else {
                        status.clone()
                    };
            }
            if self.success_until.is_none() {
                view.message = status.clone();
                view.buttons = vec!["Close".into()];
                view.keys = vec![("Enter", "Close"), ("Esc", "Close")];
            }
        } else {
            return None;
        }
        Some(view)
    }
}

pub(super) fn friendly_result(message: &str, retry: bool) -> String {
    let message = message
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect::<String>()
        .replace("the reserved pane", "the destination pane");
    if !retry {
        return message;
    }
    message
        .replace("Press Ctrl+C, then use Attach clipboard image to retry.", "Choose Retry to read the clipboard again.")
        .replace("Press Ctrl+C, then use Attach files to retry.", "Choose Retry to send again.")
        .replace(
            "Close this result, then use Attach clipboard image to retry.",
            "Choose Retry to read the clipboard again.",
        )
        .replace("Close this result, then use Attach files to retry.", "Choose Retry to send again.")
}

pub(super) fn pane_name(target: &str) -> &str {
    target.rsplit(" › ").next().unwrap_or(target).rsplit(" / ").next().unwrap_or(target)
}

impl View {
    pub(in crate::client) fn scroll_limits(&self, screen: Rect) -> (usize, usize) {
        let layout = self.layout(screen);
        (layout.max_scroll, layout.file_rows.max(1))
    }

    pub(super) fn layout(&self, screen: Rect) -> ViewLayout {
        let width = screen.width.saturating_sub(4).min(100);
        let text_width = width.saturating_sub(4) as usize;
        let content = if let Some(transfer) = &self.transfer {
            format!("{}\n{}", transfer.message(text_width), self.message)
        } else {
            self.message.clone()
        };
        let message = crate::text::wrap(&content, text_width);
        let files = self.files.lines().flat_map(|line| crate::text::fold(line, text_width)).collect::<Vec<_>>();
        let mut button_rows = 0;
        let mut used = 0;
        for label in &self.buttons {
            let width = crate::text::columns(label).min(text_width.saturating_sub(2)) + 2;
            if button_rows == 0 || used + width > text_width {
                button_rows += 1;
                used = 0;
            }
            used += width + 3;
        }
        let base = if button_rows == 0 { 2 } else { 4 + button_rows };
        let height = (message.len() + files.len() + base).min(screen.height.saturating_sub(2) as usize) as u16;
        let area = overlays::centered(screen, width, height);
        let body = area.height.saturating_sub(base as u16) as usize;
        let mut file_rows = body.saturating_sub(message.len());
        if files.len() > file_rows {
            file_rows = file_rows.saturating_sub(1);
        }
        let max_scroll = files.len().saturating_sub(file_rows);
        let error_start = self.error_start.map(|start| crate::text::wrap(&self.message[..start], text_width).len());
        ViewLayout { area, message, error_start, files, file_rows, max_scroll, body, button_rows: button_rows as u16 }
    }

    pub(in crate::client) fn render(&self, screen: Rect) -> (Buffer, Vec<Rect>) {
        let layout = self.layout(screen);
        let area = layout.area;
        let mut buf = Buffer::empty(area);
        let theme = crate::theme::current();
        let title_colour = if self.buttons.is_empty() { self.kind.colour(theme) } else { theme.accent };
        let inner = overlays::frame(area, &self.title, title_colour, &mut buf);
        let mut buttons = Vec::new();
        if inner.width < 3 || inner.height == 0 {
            return (buf, buttons);
        }
        if !self.buttons.is_empty() && inner.height < 4 {
            return (buf, buttons);
        }
        let width = inner.width.saturating_sub(2);
        let x = inner.x + 1;
        let mut bx = x;
        let mut by = inner.bottom().saturating_sub(1 + layout.button_rows);
        for (index, label) in self.buttons.iter().enumerate() {
            let label = crate::text::truncate(label, width.saturating_sub(2) as usize);
            let w = crate::text::columns(&label) as u16 + 2;
            if bx + w > x + width {
                bx = x;
                by += 1;
            }
            if by >= inner.bottom() - 1 {
                break;
            }
            let rect = Rect::new(bx, by, w, 1);
            buf.set_stringn(
                bx,
                by,
                format!(" {label} "),
                w as usize,
                overlays::button_style(index == self.selected, false),
            );
            buttons.push(rect);
            bx += w + 3;
        }
        let body = layout.body;
        let scroll = self.scroll.min(layout.max_scroll);
        for (row, line) in
            layout.message.iter().chain(layout.files.iter().skip(scroll).take(layout.file_rows)).take(body).enumerate()
        {
            let kind = if row >= layout.message.len() {
                NoticeKind::Notice
            } else if layout.error_start.is_some_and(|start| row >= start) {
                NoticeKind::Error
            } else {
                self.kind
            };
            buf.set_stringn(x, inner.y + row as u16, line, width as usize, Style::default().fg(kind.colour(theme)));
        }
        if layout.max_scroll > 0 && layout.message.len() < body {
            buf.set_stringn(
                x,
                inner.y + body as u16 - 1,
                format!("PgUp/PgDn: filenames ({}/{})", scroll + 1, layout.max_scroll + 1),
                width as usize,
                Style::default().fg(crate::theme::current().text),
            );
        }
        let keys: Vec<_> = self
            .keys
            .iter()
            .map(|&(key, what)| {
                if key == "Enter" && what == "Choose" {
                    (key, self.buttons.get(self.selected).map_or(what, String::as_str))
                } else {
                    (key, what)
                }
            })
            .collect();
        if !keys.is_empty() {
            buf.set_line(x, inner.bottom() - 1, &overlays::key_line(&keys, "", width), width);
        }
        (buf, buttons)
    }
}

pub(super) struct ViewLayout {
    area: Rect,
    message: Vec<String>,
    error_start: Option<usize>,
    files: Vec<String>,
    pub(super) file_rows: usize,
    pub(super) max_scroll: usize,
    body: usize,
    button_rows: u16,
}

impl Controller {
    pub(super) fn host(&self) -> String {
        self.destination
            .as_ref()
            .map_or("this machine", |(_, label)| label)
            .chars()
            .filter(|c| !c.is_control())
            .collect()
    }

    fn output_local(&self, bytes: &[u8]) -> std::io::Result<bool> {
        match &self.laptop {
            Some(laptop) => laptop.output(bytes),
            None => super::super::terminal().write(bytes),
        }
    }

    pub fn draw(&self) {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        self.draw_with(cols, rows, |bytes| {
            let _ = self.output_local(bytes);
        });
    }

    pub(super) fn draw_with(&self, cols: u16, rows: u16, write: impl FnOnce(&[u8])) {
        // Keep the snapshot and its terminal write ordered with state changes and other draws.
        let mut state = self.state.lock().unwrap();
        if let Some(view) = state.view(&self.host()) {
            let (buf, _) = view.render(Rect::new(0, 0, cols, rows.saturating_sub(1)));
            write(&self.overlay(&mut state, &buf));
        } else {
            state.drawn_area = None;
            write(&state.image_notices.render(cols, rows));
        }
    }

    fn overlay(&self, state: &mut State, buf: &Buffer) -> Vec<u8> {
        if state.drawn_area.replace(buf.area).is_some_and(|previous| previous.intersection(buf.area) != previous) {
            self.redraw();
        }
        crate::ansi::overlay(buf)
    }

    pub fn output(&self, bytes: &[u8]) -> std::io::Result<bool> {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        self.output_with(bytes, cols, rows, |bytes| self.output_local(bytes))
    }

    /// A local question, this one's or the laptop's, covers the server's frames until it
    /// closes and asks for a repaint.
    pub fn covers(&self) -> bool {
        self.state.lock().unwrap().view(&self.host()).is_some() || self.laptop.as_ref().is_some_and(|l| l.covers())
    }

    pub(super) fn output_with(
        &self,
        bytes: &[u8],
        cols: u16,
        rows: u16,
        write: impl FnOnce(&[u8]) -> std::io::Result<bool>,
    ) -> std::io::Result<bool> {
        // A server frame and the local question are one write, so incoming output cannot
        // briefly replace the question while a local key is authorizing a transfer.
        let mut state = self.state.lock().unwrap();
        let view = state.view(&self.host());
        let notice = state.image_notices.render(cols, rows);
        if view.is_none() {
            state.drawn_area = None;
        }
        if view.is_none() && notice.is_empty() {
            return write(bytes);
        }
        let mut out = b"\x1b[?2026h".to_vec();
        // The server ends its own synchronized frame. Keep the local box in that frame.
        for part in bytes.split_inclusive(|b| *b == b'l') {
            out.extend_from_slice(part.strip_suffix(b"\x1b[?2026l").unwrap_or(part));
        }
        if let Some(view) = view {
            let (buf, _) = view.render(Rect::new(0, 0, cols, rows.saturating_sub(1)));
            out.extend(self.overlay(&mut state, &buf));
        } else {
            out.extend(notice);
        }
        out.extend_from_slice(b"\x1b[?2026l");
        write(&out)
    }

    pub(super) fn button_at(&self, state: &State, screen: Rect, col: u16, row: u16) -> Option<usize> {
        let (_, buttons) = state.view(&self.host())?.render(screen);
        buttons.iter().position(|area| area.contains((col, row).into()))
    }

    pub(super) fn image_notice(self: &Arc<Self>, diagnostic: Diagnostic) {
        let mut state = self.state.lock().unwrap();
        self.show_image_notice(&mut state, diagnostic);
    }

    pub(super) fn show_image_notice(self: &Arc<Self>, state: &mut State, diagnostic: Diagnostic) {
        let replacing = state.image_notices.is_active();
        if !state.image_notices.show(diagnostic) {
            return;
        }
        // A full server frame restores every row covered by the previous notice.
        if replacing {
            self.redraw();
        }
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        if state.view(&self.host()).is_none() {
            let _ = self.output_local(&state.image_notices.render(cols, rows));
        }
        let this = Arc::downgrade(self);
        let _ = crate::threads::named("clipboard-notice").spawn(move || {
            std::thread::sleep(Duration::from_secs(4));
            if let Some(this) = this.upgrade() {
                this.expire_image_notice(Instant::now());
            }
        });
    }

    pub(super) fn expire_image_notice(&self, now: Instant) {
        let mut state = self.state.lock().unwrap();
        if state.image_notices.expire(now) {
            self.redraw();
        }
    }

    pub(super) fn expire_success(&self, now: Instant) {
        let mut state = self.state.lock().unwrap();
        if state.success_until.is_none_or(|until| now < until) {
            return;
        }
        state.success_until = None;
        state.completed = None;
        state.status = None;
        state.result_token = None;
        drop(state);
        self.redraw();
    }

    pub(super) fn redraw(&self) {
        let _ = send(&self.writer, &ClientMsg::Repaint);
    }

    pub(super) fn start_progress(self: &Arc<Self>, token: String) {
        let this = Arc::downgrade(self);
        let _ = crate::threads::named("attachment-progress").spawn(move || {
            loop {
                std::thread::sleep(PROGRESS_INTERVAL);
                let Some(this) = this.upgrade() else { return };
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                if !this.draw_progress_with(&token, Instant::now(), cols, rows, |bytes| {
                    let _ = this.output_local(bytes);
                }) {
                    return;
                }
            }
        });
    }

    pub(super) fn draw_progress_with(
        &self,
        token: &str,
        now: Instant,
        cols: u16,
        rows: u16,
        write: impl FnOnce(&[u8]),
    ) -> bool {
        let mut state = self.state.lock().unwrap();
        let Some(active) = state.active.as_mut().filter(|active| active.token == token) else { return false };
        if now.saturating_duration_since(active.last_draw.unwrap_or(active.started)) < PROGRESS_INTERVAL {
            return true;
        }
        active.last_draw = Some(now);
        let status = active.status(crate::theme::current(), now);
        state.displayed = active.transfer(crate::theme::current(), now, false);
        state.status = Some(status);
        state.status_kind = NoticeKind::Notice;
        if let Some(view) = state.view(&self.host()) {
            let (buf, _) = view.render(Rect::new(0, 0, cols, rows.saturating_sub(1)));
            write(&self.overlay(&mut state, &buf));
        }
        true
    }
}
