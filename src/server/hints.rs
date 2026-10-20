//! Hints mode: labels over what can be acted on in the focused pane; a label copies it, or,
//! after Tab, acts on it where the pane is (opens a link, edits a file, shows a commit…). A
//! file of an SSH pane's server opens in the local editor, and each save goes back there.

use std::collections::HashSet;

use anyhow::Result;

use super::edits::EditTarget;
use super::ui::{HintsMode, Mode};
use super::{ClientId, Server};
use crate::hints::{self, Kind, Match};
use crate::input::{InputEvent, KeyCode, MouseKind};
use crate::layout::PaneId;
use crate::protocol::Placement;
use crate::shell;
use crate::theme::NoticeKind;
use crate::tools::{Open, ToolDef, Where};

/// A path seen in a pane on a server, as that server reads it: in the pane's directory there
/// when the shell reported it (OSC 7), else in the home directory.
fn remote_path(file: &str, cwd: Option<&std::path::Path>) -> String {
    match cwd {
        Some(dir) if !file.starts_with('/') && !file.starts_with('~') => {
            let dir = dir.display().to_string();
            format!("{}/{}", dir.trim_end_matches('/'), file.strip_prefix("./").unwrap_or(file))
        }
        _ => file.to_string(),
    }
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0" | "::1" | "::" | "")
}

impl Server {
    pub(super) fn start_hints(&mut self, id: ClientId) -> Mode {
        let Some(pane) = self.client_focus(id) else { return Mode::Normal };
        let Some(rows) = self.panes.get(&pane).filter(|p| p.is_terminal()).map(|p| p.screen_rows()) else {
            self.toast(id, NoticeKind::Notice, "hints work in terminal panes");
            return Mode::Normal;
        };
        let mut servers = HashSet::new();
        for k in &self.known_servers {
            servers.insert(k.server.name.clone());
            servers.insert(k.server.host.clone());
        }
        // A local pane's files can be looked at: on a server, they cannot.
        let dir = self.panes.get(&pane).filter(|p| p.ssh.is_none()).and_then(|p| p.current_cwd());
        let found = hints::find(&rows, &servers, &self.hint_patterns, dir.as_deref());
        let hints = hints::label(found, &self.config.hints.alphabet);
        if hints.is_empty() {
            self.toast(id, NoticeKind::Notice, "nothing to act on in this pane");
            return Mode::Normal;
        }
        // What is labelled stays in view as it was, while the pane's program goes on.
        let snapshot = self.pane_snapshot(id, pane);
        Mode::Hints(HintsMode { pane, hints, typed: String::new(), act: false, snapshot })
    }

    pub(super) fn on_hints(&mut self, id: ClientId, mut h: HintsMode, ev: InputEvent) -> Mode {
        let k = match ev {
            InputEvent::Key(k) => k,
            InputEvent::Mouse(m) if matches!(m.kind, MouseKind::Press(_)) => return Mode::Normal,
            _ => return Mode::Hints(h),
        };
        match k.code {
            KeyCode::Backspace => {
                h.typed.pop();
            }
            // Tab switches between copying a target and acting on it.
            KeyCode::Tab => h.act = !h.act,
            KeyCode::Char(c) if c.is_ascii_alphabetic() && !k.mods.ctrl && !k.mods.alt => {
                if h.snapshot.as_ref().is_some_and(|s| self.pane_inner_rect(id, h.pane) != Some(s.area)) {
                    self.toast(
                        id,
                        NoticeKind::Warning,
                        "The pane moved since the labels were made: ask for hints again.",
                    );
                    return Mode::Normal;
                }
                h.typed.push(c.to_ascii_lowercase());
                if let Some(found) = h.hints.iter().find(|x| x.label == h.typed) {
                    let m = found.m.clone();
                    return match self.hint_chosen(id, h.pane, m, h.act) {
                        Ok(next) => next,
                        Err(e) => {
                            self.toast(id, NoticeKind::Error, format!("{e:#}"));
                            Mode::Normal
                        }
                    };
                }
                if !h.hints.iter().any(|x| x.label.starts_with(&h.typed)) {
                    h.typed.pop();
                }
            }
            _ => return Mode::Normal,
        }
        Mode::Hints(h)
    }

    /// What a client sees of a pane right now, at its place on screen.
    fn pane_snapshot(&mut self, client: ClientId, pane: PaneId) -> Option<Box<ratatui::buffer::Buffer>> {
        let inner = self.pane_inner_rect(client, pane)?;
        let mut buf = ratatui::buffer::Buffer::empty(inner);
        let mut links = crate::ansi::Links::new();
        self.panes.get_mut(&pane)?.render(inner, &mut buf, &mut links);
        Some(Box::new(buf))
    }

    fn hint_chosen(&mut self, id: ClientId, pane: PaneId, m: Match, act: bool) -> Result<Mode> {
        if !act {
            self.copy_to_client(id, &m.text);
            return Ok(Mode::Normal);
        }
        match m.kind {
            Kind::Url => self.open_url_from(id, pane, &m.text)?,
            Kind::Address => self.open_url_from(id, pane, &format!("http://{}", m.text))?,
            Kind::Ip | Kind::Server => {
                let (user, host) = match m.text.rsplit_once('@') {
                    Some((u, h)) => (Some(u.to_string()), h),
                    None => (None, m.text.as_str()),
                };
                let mut server = self
                    .known_servers
                    .iter()
                    .find(|k| k.server.name == host || k.server.host == host)
                    .map(|k| k.server.clone())
                    .or_else(|| self.resolve_server(host))
                    .ok_or_else(|| anyhow::anyhow!("not a server: {host}"))?;
                if user.is_some() {
                    server.user = user;
                }
                let ws = self.clients.get(&id).map(|c| c.workspace).unwrap_or(0);
                self.open_ssh(ws, Some(pane), &server, Placement::Auto)?;
            }
            Kind::Path => {
                // A file of an SSH pane's server opens here, and is saved back there.
                if let Some(server) = self.pane_server_entry(pane) {
                    return Ok(self.edit_hinted(id, pane, server, &m.text));
                }
                let (file, line) = hints::split_line(&m.text);
                let editor = crate::edit::editor_argv(&crate::paths::expand_tilde(file), line);
                self.run_here(id, pane, "edit", shell::join(&editor), &m.text)?;
            }
            Kind::Hash => {
                let h = shell::quote(&m.text);
                let cmd = format!(
                    "if git rev-parse -q --verify {h}^{{commit}} >/dev/null 2>&1; then git show {h}; \
                     else docker logs -f --tail 200 {h}; fi"
                );
                self.run_here(id, pane, &m.text.chars().take(9).collect::<String>(), cmd, &m.text)?;
            }
            Kind::Pod => {
                let cmd = format!(
                    "kubectl logs -f --tail=200 ${{KRAAL_KUBE_CONTEXT:+--context \"$KRAAL_KUBE_CONTEXT\"}} \
                     ${{KRAAL_KUBE_NAMESPACE:+-n \"$KRAAL_KUBE_NAMESPACE\"}} {}",
                    shell::quote(&m.text)
                );
                self.run_here(id, pane, &m.text, cmd, &m.text)?;
            }
            Kind::Uuid => {
                if let Some(p) = self.panes.get_mut(&pane)
                    && p.exit_code.is_none()
                {
                    p.write_input(m.text.into_bytes());
                }
            }
            Kind::Custom(i) => match self.hint_patterns.patterns.get(i).and_then(|(_, _, tool)| tool.clone()) {
                Some(tool) => return Ok(self.tool_request(id, Some(pane), &tool, Some(m.text))),
                None => self.copy_to_client(id, &m.text),
            },
        }
        Ok(Mode::Normal)
    }

    /// Edit `text` (`path[:line]`), a path seen in `pane` on `server`, in the local editor.
    fn edit_hinted(&mut self, id: ClientId, pane: PaneId, server: crate::servers::Server, text: &str) -> Mode {
        let (file, line) = hints::split_line(text);
        let cwd = self.panes.get(&pane).and_then(|p| p.remote_cwd());
        let mut ssh = self.ssh_options(&server);
        ssh.push(server.destination());
        let target = EditTarget {
            server: server.name.clone(),
            ssh,
            path: remote_path(file, cwd.as_deref()),
            line,
            ws: self.clients.get(&id).map(|c| c.workspace),
            near: Some(pane),
            sudo: false,
            production: None,
        };
        let production = self.pane_production(pane);
        self.edit_request(id, target, production)
    }

    /// Open a URL on the client's machine. A URL to the loopback of the pane's server
    /// (`http://localhost:5173/app`) goes through a tunnel, reused when one already reaches
    /// that port there.
    fn open_url_from(&mut self, id: ClientId, pane: PaneId, url: &str) -> Result<()> {
        let (Some((port, rest)), Some(server)) = (hints::loopback_port(url), self.pane_server(pane)) else {
            self.open_for(id, url);
            return Ok(());
        };
        // A tunnel to that very port over that very connection, if there is one.
        let existing = self
            .tunnels
            .iter()
            .find(|t| {
                t.spec.remote_port == port
                    && is_loopback(&t.spec.remote_host)
                    && match &t.spec.via {
                        Some(via) => via.same_connection(&server),
                        None => t.spec.server == server.name,
                    }
            })
            .map(|t| (t.id, t.spec.local));
        let (tunnel, local) = match existing {
            Some(found) => found,
            None => {
                let info = self.add_tunnel_to(server.clone(), &port.to_string(), None)?;
                self.toast(
                    id,
                    NoticeKind::Notice,
                    format!("tunnel localhost:{} → {}:{port}…", info.spec.local, server.name),
                );
                (info.id, info.spec.local)
            }
        };
        let scheme = url.split("://").next().unwrap_or("http");
        // Opened once the tunnel is up (now, if it already is).
        self.pending_opens.push((id, tunnel, format!("{scheme}://localhost:{local}{rest}")));
        self.open_pending();
        Ok(())
    }

    /// Run a command where `pane` is (its server, or here in its directory), in a floating pane.
    fn run_here(&mut self, id: ClientId, pane: PaneId, label: &str, command: String, selection: &str) -> Result<()> {
        let tool = ToolDef {
            command,
            description: String::new(),
            key: None,
            on: Where::Pane,
            open: Open::Floating,
            hold: false,
            when: Vec::new(),
            enabled: true,
            read_only: true,
        };
        let mut ctx = self.tool_context(pane);
        ctx.selection = Some(selection.to_string());
        let spec = tool.spec(&ctx, &|s, remote| self.ssh_command_running(s, remote));
        ToolDef::check_cwd(&spec)?;
        let ws = self.clients.get(&id).map(|c| c.workspace).unwrap_or(0);
        self.spawn_pane_in(ws, Some(pane), spec, Placement::Floating, Some(label.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::remote_path;
    use std::path::Path;

    #[test]
    fn a_path_on_a_server_is_read_where_its_pane_is() {
        let cwd = Some(Path::new("/srv/app/"));
        assert_eq!(remote_path("src/main.rs", cwd), "/srv/app/src/main.rs");
        assert_eq!(remote_path("./conf/x.yml", cwd), "/srv/app/conf/x.yml");
        assert_eq!(remote_path("../shared/.env", cwd), "/srv/app/../shared/.env");
        assert_eq!(remote_path("/etc/hosts", cwd), "/etc/hosts");
        assert_eq!(remote_path("~/notes", cwd), "~/notes");
        // Without a reported directory, the home directory there.
        assert_eq!(remote_path("src/main.rs", None), "src/main.rs");
    }
}
