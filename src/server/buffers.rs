//! Paste buffers in the session: every copy is kept (see [`crate::buffers`]), listed in a picker
//! to paste again, and reached from the command line (`kraal buffer`).

use super::persistence::Persist;
use super::ui::{PickAction, PickItem, Picker, PickerKind};
use super::{ClientId, Server};
use crate::buffers;
use crate::layout::PaneId;
use crate::pane::{PaneSpec, ShownText};
use crate::protocol::{BufferInfo, Placement, Response};
use crate::text::human_size;
use crate::theme::NoticeKind;

impl Server {
    /// Keep what was just copied as the newest buffer.
    pub(super) fn keep_copy(&mut self, text: &str) {
        if self.buffers.copy(text, crate::accounts::now()) {
            self.buffers_dirty = true;
        }
    }

    /// The buffers to choose from, newest first; Enter pastes one into the focused pane.
    pub(super) fn buffer_picker(&self) -> Picker {
        let now = crate::accounts::now();
        let sep = if crate::theme::current().ascii { " | " } else { " ⏎ " };
        let items = (0..self.buffers.list().len())
            .map(|i| {
                let b = &self.buffers.list()[i];
                let mut detail = format!("{} · {}", self.buffers.key(i), human_size(b.text.len() as u64));
                let lines = buffers::lines(&b.text);
                if lines > 1 {
                    detail.push_str(&format!(" · {lines} lines"));
                }
                detail.push_str(&format!(" · {}", buffers::age(now - b.at)));
                PickItem::new(buffers::one_line(&b.text, sep, 56), detail, PickAction::Buffer(b.id))
            })
            .collect();
        Picker::new(PickerKind::Buffers, "Paste buffers · newest first", items)
    }

    /// The text of the buffer a picker's line points at; when it went meanwhile, the client is
    /// told.
    pub(super) fn picked_buffer(&mut self, client: ClientId, buffer: u64) -> Option<String> {
        let text = self.buffers.by_id(buffer).map(|b| b.text.clone());
        if text.is_none() {
            self.toast(client, NoticeKind::Warning, "that buffer is gone");
        }
        text
    }

    /// Show a buffer in a floating preview pane of the client's workspace (never saved with the
    /// session).
    pub(super) fn show_buffer(&mut self, client: ClientId, buffer: u64) {
        let Some(text) = self.picked_buffer(client, buffer) else { return };
        let Some(ws) = self.clients.get(&client).map(|c| c.workspace) else { return };
        let title = format!("buffer {}", self.buffers.key_of(buffer).unwrap_or_default());
        let spec = PaneSpec { text: Some(ShownText { title: title.clone(), text }), ..Default::default() };
        if let Err(e) = self.spawn_pane_in(ws, None, spec, Placement::Floating, Some(title)) {
            self.toast(client, NoticeKind::Error, format!("{e:#}"));
        }
    }

    pub(super) fn delete_picked_buffer(&mut self, client: ClientId, buffer: u64) {
        let key = self.buffers.key_of(buffer);
        match self.buffers.delete_id(buffer) {
            Some(_) => {
                self.buffers_dirty = true;
                self.toast(client, NoticeKind::Notice, format!("deleted buffer {}", key.unwrap_or_default()));
            }
            None => self.toast(client, NoticeKind::Warning, "that buffer is gone"),
        }
    }

    /// The buffers as `kraal buffer list` shows them, newest first.
    pub(super) fn buffer_infos(&self) -> Vec<BufferInfo> {
        (0..self.buffers.list().len())
            .map(|i| {
                let b = &self.buffers.list()[i];
                BufferInfo {
                    buffer: self.buffers.key(i),
                    named: b.name.is_some(),
                    bytes: b.text.len(),
                    lines: buffers::lines(&b.text),
                    at: b.at,
                    preview: buffers::one_line(&b.text, " | ", 72),
                }
            })
            .collect()
    }

    /// `kraal buffer set`.
    pub(super) fn set_buffer(&mut self, name: Option<&str>, text: String) -> Result<Response, String> {
        if let Some(name) = name {
            buffers::check_name(name)?;
        }
        if text.is_empty() {
            return Err("nothing to keep: the text is empty".into());
        }
        self.buffers.set(name, text, crate::accounts::now())?;
        self.buffers_dirty = true;
        Ok(Response::Ok)
    }

    /// `kraal buffer delete`.
    pub(super) fn delete_buffer(&mut self, key: Option<&str>, all: bool) -> Result<Response, String> {
        if all {
            self.buffers.clear();
        } else {
            self.buffers.delete(key)?;
        }
        self.buffers_dirty = true;
        Ok(Response::Ok)
    }

    /// `kraal buffer paste`: the buffer goes to the pane as a paste, bracketed when its program
    /// asked for that. Whether the pane may get it (production) is the caller's to check.
    pub(super) fn paste_buffer_to(&mut self, pane: PaneId, key: Option<&str>) -> Result<Response, String> {
        let text = self.buffers.get(key)?.text.clone();
        self.paste_text(pane, &text)
    }

    /// `text` pasted into `pane`, as a buffer is.
    pub(super) fn paste_text(&mut self, pane: PaneId, text: &str) -> Result<Response, String> {
        let bytes = self.buffer_paste(pane, text)?;
        self.panes.get_mut(&pane).ok_or("no such pane")?.write_input(bytes);
        Ok(Response::Ok)
    }

    /// The paste of `text` that `pane` gets, bracketed when its program asked for that. Refused
    /// when it runs no program, or when its program cannot be sent that much at once now: the
    /// paste would be dropped whole.
    pub(super) fn buffer_paste(&self, pane: PaneId, text: &str) -> Result<Vec<u8>, String> {
        let label = self.pane_label(pane);
        let pn = self.panes.get(&pane).ok_or("no such pane")?;
        if !pn.is_terminal() {
            return Err(format!("{label} runs no program to paste into"));
        }
        if pn.exit_code.is_some() {
            return Err("the pane's program has exited".into());
        }
        let bytes = crate::input::encode_paste(text.as_bytes(), pn.input_modes());
        let room = pn.input_room();
        if bytes.len() > room {
            let (most, size) = (human_size(room as u64), human_size(bytes.len() as u64));
            return Err(format!("{label} takes a paste of {most} at most now: this one is {size}"));
        }
        Ok(bytes)
    }

    /// Take back the buffers kept by the last server, when they are kept; otherwise make sure no
    /// file holds them any more.
    pub(super) fn load_buffers(&mut self) {
        let path = crate::paths::buffers_path(&self.session);
        if !self.config.buffers.persist {
            if path.exists() {
                self.write_buffers(None);
            }
            return;
        }
        let loaded = match crate::paths::read_regular(&path, (self.config.buffers.max_bytes() as u64).saturating_mul(8))
        {
            Ok(data) => self.buffers.load_json(&data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => Err(e.to_string()),
        };
        let Err(e) = loaded else { return };
        // The next copy would write over buffers that could not be taken back: they are kept aside.
        let mut moved = Err(std::io::ErrorKind::AlreadyExists.into());
        for aside in crate::paths::buffers_aside_paths(&self.session) {
            moved = crate::paths::rename_no_replace(&path, &aside).map(|()| aside);
            if !moved.as_ref().is_err_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) {
                break;
            }
        }
        let kept = match moved {
            Ok(aside) => format!("kept as {}", crate::paths::shorten_home(&aside)),
            Err(why) => {
                // Rather than written over, it is left alone until the server stops, settings or not.
                self.buffers_file_stuck = true;
                format!("could not move it aside ({why}): buffers are not saved until the server restarts")
            }
        };
        self.warnings.push(format!("paste buffers: {}: {e}; {kept}", crate::paths::shorten_home(&path)));
    }

    /// Write the buffers that changed to their file, when they are kept across restarts. A file
    /// whose last write failed is written (or deleted, when they are not kept any more) again a
    /// while later, or at once when the server is `stopping`.
    pub(super) fn save_buffers(&mut self, stopping: bool) {
        let retry = self.persisting.lock().is_ok_and(|mut p| p.retry_buffers(stopping));
        let changed = std::mem::take(&mut self.buffers_dirty);
        let persist = self.config.buffers.persist;
        if retry || (changed && persist) {
            self.write_buffers(persist.then(|| self.buffers.to_json()));
        }
    }

    /// Have the writer put `data` in the buffers' file, or delete it (`None`): only the latest
    /// waits for it.
    fn write_buffers(&self, data: Option<Vec<u8>>) {
        if self.buffers_file_stuck {
            return;
        }
        let file = (crate::paths::buffers_path(&self.session), data);
        if self.persisting.lock().is_ok_and(|mut p| p.offer_buffers(file)) {
            let _ = self.persister.send(Persist::Buffers);
        }
    }

    /// The `[buffers]` settings changed: new limits apply at once, and the file is written or
    /// deleted as `persist` now says.
    pub(super) fn reconfigure_buffers(&mut self, persisted: bool) {
        let cfg = self.config.buffers.clone();
        self.buffers.set_limits(cfg.limit, cfg.max_bytes());
        match (persisted, cfg.persist) {
            (true, false) => self.write_buffers(None),
            // Written again: the new limits may have dropped some.
            (_, true) => self.buffers_dirty = true,
            _ => {}
        }
        self.save_buffers(false);
    }
}

/// Write the buffers' file, readable by its owner only, or delete it (`None`).
pub(super) fn write_file(path: &std::path::Path, data: Option<&[u8]>) -> std::io::Result<()> {
    match data {
        Some(data) => {
            if let Some(dir) = path.parent() {
                crate::paths::ensure_private_dir(dir)?;
            }
            crate::paths::write_private(path, data)
        }
        None => match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::server::persistence::{Persist, spawn_persister};

    #[test]
    fn buffers_that_cannot_be_taken_back_are_kept_aside_from_the_next_copy() {
        let (tx, _events) = crate::server::event_channel(16);
        let mut server = crate::server::tests::server(tx.clone());
        server.session = format!("buffers-aside-{}", crate::identity::new_uuid());
        server.config.buffers.persist = true;
        server.persister = spawn_persister(tx, server.persisting.clone(), ()).unwrap();
        let path = crate::paths::buffers_path(&server.session);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json").unwrap();
        server.load_buffers();
        let aside = path.with_extension("json.bad");
        assert!(server.warnings[0].contains(&crate::paths::shorten_home(&aside)), "{:?}", server.warnings);
        server.keep_copy("hello");
        server.save_buffers(false);
        let (done, written) = crossbeam_channel::bounded(1);
        server.persister.send(Persist::Flush(done)).unwrap();
        assert_eq!(written.recv_timeout(crate::testing::PATIENCE).unwrap(), Ok(()));
        assert!(std::fs::read_to_string(&path).unwrap().contains("hello"));
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), "not json");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(aside).unwrap();
    }

    #[test]
    fn buffers_that_cannot_be_kept_aside_are_not_written_over_after_a_reload() {
        let (tx, _events) = crate::server::event_channel(16);
        let mut server = crate::server::tests::server(tx.clone());
        server.session = format!("buffers-stuck-{}", crate::identity::new_uuid());
        server.config.buffers.persist = true;
        server.persister = spawn_persister(tx, server.persisting.clone(), ()).unwrap();
        let path = crate::paths::buffers_path(&server.session);
        let asides: Vec<_> = crate::paths::buffers_aside_paths(&server.session).collect();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&asides[0], "earlier").unwrap();
        std::fs::write(&path, "not json").unwrap();
        server.load_buffers();
        assert_eq!(std::fs::read_to_string(&asides[1]).unwrap(), "not json", "{:?}", server.warnings);
        // No name left to keep it aside: the file stays, and a reload keeping `persist` leaves it.
        for aside in &asides[2..] {
            std::fs::write(aside, "earlier").unwrap();
        }
        std::fs::write(&path, "not json").unwrap();
        server.load_buffers();
        server.keep_copy("hello");
        let persisted = server.config.buffers.persist;
        server.config.buffers.persist = true;
        server.reconfigure_buffers(persisted);
        let (done, written) = crossbeam_channel::bounded(1);
        server.persister.send(Persist::Flush(done)).unwrap();
        assert_eq!(written.recv_timeout(crate::testing::PATIENCE).unwrap(), Ok(()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
        for p in asides.iter().chain([&path]) {
            std::fs::remove_file(p).unwrap();
        }
    }
}
