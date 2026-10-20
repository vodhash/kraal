use std::time::Instant;

use ratatui::layout::Rect;

use super::{Controller, Prompt, clean};
use crate::client::attachments::View;
use crate::laptop::Scope;
use crate::laptop::copy::Operation;
use crate::theme::NoticeKind;

impl Controller {
    pub(super) fn view(&self, prompt: &Prompt) -> View {
        let host = self.identity.as_ref().map_or("laptop", |i| i.host.as_str());
        let server = self.destination.as_ref().map_or("server", |(_, label)| label.as_str());
        if let Some((names, _)) = &prompt.replacement {
            let removing = !prompt.request.to_laptop && prompt.request.operation != Operation::Copy;
            let action = if !removing {
                "Replace"
            } else if prompt.request.operation == Operation::Move {
                "Move"
            } else {
                "Delete"
            };
            let message = if removing {
                if prompt.request.operation == Operation::Move {
                    format!(
                        "{server} asks to move these items to {} on {server}. Sources are removed only after installation and only if unchanged. Cancel keeps everything as it is.",
                        clean(&prompt.request.path)
                    )
                } else {
                    format!(
                        "{server} asks to delete these items for good, including folder contents. Cancel keeps everything as it is."
                    )
                }
            } else {
                format!(
                    "{server} asks to replace these items in {}. Folders are replaced in full. Cancel keeps everything as it is.",
                    clean(&prompt.request.path)
                )
            };
            return View {
                title: format!("{action} files on {}?", clean(host)),
                kind: NoticeKind::Warning,
                error_start: None,
                message,
                files: names.iter().map(|name| clean(name)).collect::<Vec<_>>().join("\n"),
                transfer: None,
                scroll: prompt.scroll,
                buttons: vec![action.into(), "Cancel".into()],
                selected: prompt.selected,
                keys: vec![("Enter", "Choose"), ("Tab", "Choose"), ("Esc", "Cancel")],
            };
        }
        let scope = if prompt.scope == Scope::All {
            "all directories your user can access"
        } else if cfg!(target_os = "macos") {
            "your home directory, without its hidden top-level entries or ~/Library"
        } else {
            "your home directory, without its hidden top-level entries (~/.ssh, ~/.config)"
        };
        View {
            title: format!("Let {server} use the files of {}?", clean(host)),
            kind: NoticeKind::Notice,
            error_start: None,
            message: format!(
                "{server}'s file manager asks to list and copy files in {scope}.\nReplacing anything, deleting or moving files away from this laptop is asked in this box each time.\nAllow lasts until this kraal attach ends. Always is kept in ~/.config/kraal/laptop-files.toml."
            ),
            transfer: None,
            files: String::new(),
            scroll: 0,
            buttons: vec!["Allow".into(), format!("Always for {}", crate::text::truncate(server, 24)), "Cancel".into()],
            selected: prompt.selected,
            keys: vec![("Enter", "Choose"), ("Tab", "Choose"), ("Esc", "Cancel")],
        }
    }

    pub(super) fn draw(&self) {
        let _ = self.output(&[]);
    }

    /// A local question covers the server's frames.
    pub fn covers(&self) -> bool {
        self.state.lock().unwrap().prompt.is_some()
    }

    pub fn output(&self, bytes: &[u8]) -> std::io::Result<bool> {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        self.output_with(bytes, cols, rows, |bytes| super::super::terminal().write(bytes))
    }

    pub(super) fn output_with(
        &self,
        bytes: &[u8],
        cols: u16,
        rows: u16,
        write: impl FnOnce(&[u8]) -> std::io::Result<bool>,
    ) -> std::io::Result<bool> {
        let state = self.state.lock().unwrap();
        let Some(prompt) = &state.prompt else {
            if let Some((until, message)) = &state.notice
                && Instant::now() < *until
            {
                let mut out = bytes.to_vec();
                let lines = crate::text::wrap(message, usize::from(cols).saturating_sub(2).max(1))
                    .into_iter()
                    .map(|line| (line, Some(NoticeKind::Notice)))
                    .collect::<Vec<_>>();
                out.extend(super::super::notice_lines(&lines, cols, rows));
                return write(&out);
            }
            return write(bytes);
        };
        let (buffer, _) = self.view(prompt).render(Rect::new(0, 0, cols, rows.saturating_sub(1)));
        let mut out = b"\x1b[?2026h".to_vec();
        for part in bytes.split_inclusive(|b| *b == b'l') {
            out.extend_from_slice(part.strip_suffix(b"\x1b[?2026l").unwrap_or(part));
        }
        out.extend(crate::ansi::overlay(&buffer));
        write(&out)
    }
}
