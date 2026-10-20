//! SSH-side features of the server: the known servers, SSH panes, whole groups in a grid,
//! discovered server suggestions, and the SSH picker.

use std::path::Path;

use anyhow::{Context, Result};

use super::ui::{PickAction, PickItem, Picker, PickerKind};
use super::{Server, Tab};
use crate::layout::{PaneId, grid};
use crate::pane::PaneSpec;
use crate::paths;
use crate::protocol::Placement;
use crate::servers::{self, Inventory, Source};
use crate::workspace::WorkspaceDef;

impl Server {
    pub(super) fn refresh_known(&mut self, defs: &[WorkspaceDef]) {
        self.config_generation += 1;
        let hosts = if self.config.ssh_config_hosts { super::ssh_config_hosts() } else { Vec::new() };
        self.apply_known(defs, Inventory::load(), &hosts);
    }

    pub(super) fn apply_known(&mut self, defs: &[WorkspaceDef], inventory: Result<Inventory>, hosts: &[String]) {
        let inv = match inventory {
            Ok(inv) => {
                self.inventory_read = true;
                inv
            }
            Err(e) => {
                self.warnings.push(format!("{e:#}"));
                // The servers last read stay: an alias must not become a bare host name.
                let mut inv = Inventory::empty();
                let last = self.known_servers.iter().filter(|k| k.source == Source::Inventory);
                inv.servers = last.map(|k| k.server.clone()).collect();
                inv
            }
        };
        self.known_servers = servers::known(&inv, defs, hosts);
    }

    /// Resolve a name or `user@host[:port]` against the known servers.
    pub fn resolve_server(&self, spec: &str) -> Option<servers::Server> {
        servers::resolve(&self.known_servers, spec)
    }

    /// `ssh` options to reach a server, connection sharing included, without the destination.
    pub fn ssh_options(&self, s: &servers::Server) -> Vec<String> {
        servers::ssh_args(s, &self.known_servers, self.config.servers.multiplex)
    }

    /// Full `ssh …` command line for a pane connected to `s`.
    pub fn ssh_command(&self, s: &servers::Server) -> Vec<String> {
        self.ssh_command_running(s, &[])
    }

    /// `ssh … host` running `remote` (with a terminal, for interactive or following commands).
    pub fn ssh_command_running(&self, s: &servers::Server, remote: &[String]) -> Vec<String> {
        let mut cmd = vec!["ssh".to_string()];
        cmd.extend(self.ssh_options(s));
        if !remote.is_empty() {
            cmd.push("-t".into());
        }
        cmd.push(s.destination());
        if !remote.is_empty() {
            cmd.push(remote_command(remote));
        }
        cmd
    }

    /// The server a pane spec connects to, if any: the connection kraal made, else the entry
    /// it is named after, else what its `ssh` command line says.
    pub(super) fn ssh_target_of(&self, spec: &PaneSpec) -> Option<servers::Server> {
        if let Some(c) = &spec.connection {
            return Some(c.clone());
        }
        if let Some(name) = &spec.server {
            return self.known_servers.iter().find(|k| &k.server.name == name).map(|k| k.server.clone());
        }
        let cmd = spec.command.first()?;
        if cmd.rsplit('/').next() != Some("ssh") {
            return None;
        }
        let mut s = servers::parse_ssh_args(&spec.command[1..])?;
        if let Some(k) = self.known_servers.iter().find(|k| servers::is_known(std::slice::from_ref(k), &s)) {
            s.name = k.server.name.clone();
        } else {
            s.name = servers::short_name(&s.host);
        }
        Some(s)
    }

    /// The server a pane is connected to right now.
    pub fn pane_server(&self, pane: PaneId) -> Option<servers::Server> {
        self.panes.get(&pane)?.ssh.clone()
    }

    /// Open an SSH pane to `s` in workspace `ws`.
    pub fn open_ssh(
        &mut self,
        ws: u32,
        target: Option<PaneId>,
        s: &servers::Server,
        placement: Placement,
    ) -> Result<PaneId> {
        self.open_ssh_running(ws, target, s, placement, &[])
    }

    /// Open an SSH pane to `s` running `remote` (a shell when empty).
    pub fn open_ssh_running(
        &mut self,
        ws: u32,
        target: Option<PaneId>,
        s: &servers::Server,
        placement: Placement,
        remote: &[String],
    ) -> Result<PaneId> {
        let spec = PaneSpec {
            command: self.ssh_command_running(s, remote),
            server: (!s.name.is_empty()).then(|| s.name.clone()),
            connection: Some(s.clone()),
            ..Default::default()
        };
        let name = if s.name.is_empty() { servers::short_name(&s.host) } else { s.name.clone() };
        self.spawn_pane_in(ws, target, spec, placement, Some(name))
    }

    /// The servers of `group`, in the inventory's order.
    pub(super) fn group_members(&self, group: &str) -> Vec<servers::Server> {
        self.known_servers
            .iter()
            .filter(|k| k.server.groups.iter().any(|g| g == group))
            .map(|k| k.server.clone())
            .collect()
    }

    /// Open every server of a group in a grid, `max_panes_per_tab` per tab, in a workspace named
    /// after the group (switching to it if it is already open).
    pub fn open_group(&mut self, group: &str, remote: &[String]) -> Result<u32> {
        let ws_name = group_workspace_name(group, remote);
        if let Some(ws) = self.workspaces.iter().find(|w| w.name == ws_name).map(|w| w.id) {
            self.reopen_workspace(ws)?;
            return Ok(ws);
        }
        let members = self.group_members(group);
        anyhow::ensure!(!members.is_empty(), "no server in group {group:?}");
        let per_tab = self.config.servers.max_panes_per_tab.max(1);
        let ws = self.new_workspace(ws_name, None);
        let chunks: Vec<&[servers::Server]> = members.chunks(per_tab).collect();
        let many = chunks.len() > 1;
        for (i, chunk) in chunks.into_iter().enumerate() {
            let mut ids = Vec::new();
            for s in chunk {
                let spec = PaneSpec {
                    command: self.ssh_command_running(s, remote),
                    server: Some(s.name.clone()),
                    connection: Some(s.clone()),
                    ..Default::default()
                };
                ids.push(self.spawn_pane(Some(s.name.clone()), spec)?);
            }
            let tab_id = self.alloc_id();
            let name = if many { format!("{group} {}", i + 1) } else { group.to_string() };
            let mut tab = Tab::new(tab_id, name);
            tab.root = Some(grid(&ids));
            tab.set_focus(ids[0]);
            let wi = self.ws_index(ws).context("workspace vanished")?;
            self.workspaces[wi].tabs.push(tab);
        }
        self.relayout();
        Ok(ws)
    }

    /// Remember a pane's SSH connection and offer unknown servers for explicit addition.
    pub(super) fn observe_ssh(&mut self, pane: PaneId, args: &[String]) {
        // The ssh kraal started in this pane is the connection it made, with what its command
        // line only spells (a jump host with its own key is a ProxyCommand there).
        let own = self.panes.get(&pane).filter(|p| p.spec.command.get(1..) == Some(args)).map(|p| p.spec.clone());
        let Some(mut s) = own.and_then(|spec| self.ssh_target_of(&spec)).or_else(|| servers::parse_ssh_args(args))
        else {
            return;
        };
        let known = servers::is_known(&self.known_servers, &s);
        if let Some(k) = self.known_servers.iter().find(|k| servers::is_known(std::slice::from_ref(k), &s)) {
            s.name = k.server.name.clone();
        } else if s.name.is_empty() {
            // Named as the inventory would name it, even when it is not added there.
            s.name = servers::short_name(&s.host);
        }
        if let Some(p) = self.panes.get_mut(&pane)
            && p.ssh.as_ref().is_none_or(|old| !old.same_connection(&s))
        {
            p.ssh = Some(s.clone());
        }
        if known || !self.config.servers.auto_add || self.discovered.iter().any(|old| old.same_connection(&s)) {
            return;
        }
        s.groups = vec!["discovered".into()];
        self.discovered.push(s);
    }

    pub(super) fn ssh_failure(&self, pane: PaneId, reason: &str) -> super::ui::Mode {
        let Some(s) = self.panes.get(&pane).and_then(|p| p.spec.connection.as_ref()) else {
            return super::ui::Mode::Normal;
        };
        let saved = self.known_servers.iter().any(|k| k.source == Source::Inventory && k.server.same_connection(s));
        let message =
            format!("Couldn't connect to {}.\n{reason}{}", s.name, if saved { "\nThe server is saved." } else { "" });
        let super::ui::Mode::Confirm(mut q) =
            super::ui::Mode::confirm(message, super::ui::ConfirmAction::RetrySsh(pane))
        else {
            unreachable!()
        };
        q.title = Some(format!("Connect · {}", s.name));
        super::ui::Mode::Confirm(q)
    }

    pub(super) fn show_ssh_failure(&mut self, pane: PaneId) {
        let Some(p) = self.panes.get(&pane) else { return };
        if p.exit_code != Some(Some(255))
            || p.spec.connection.as_ref().is_none_or(|s| p.spec.command.last() != Some(&s.destination()))
        {
            return;
        }
        let clients: Vec<_> = self
            .clients
            .iter()
            .filter_map(|(&id, c)| {
                (matches!(c.mode, super::ui::Mode::Normal) && self.client_focus(id) == Some(pane)).then_some(id)
            })
            .collect();
        for id in clients {
            let mode = self.ssh_failure(pane, "See the SSH output below.");
            self.clients.get_mut(&id).unwrap().mode = mode;
        }
    }

    pub(super) fn ssh_picker(&self) -> Picker {
        let mut items = Vec::new();
        for (g, n) in servers::groups(&self.known_servers) {
            items.push(PickItem {
                label: format!("▦ {g}"),
                detail: format!("group · {n} server{} · enter opens them all", if n == 1 { "" } else { "s" }),
                action: PickAction::SshGroup(g),
                state: None,
                dim: false,
            });
        }
        for k in &self.known_servers {
            let s = &k.server;
            let source = match &k.source {
                Source::Inventory => String::new(),
                Source::Workspace(w) => format!(" · from {w}"),
                Source::SshConfig => " · ~/.ssh/config".to_string(),
            };
            let groups = if s.groups.is_empty() { String::new() } else { format!(" · {}", s.groups.join(", ")) };
            let mut detail = if s.name == s.host {
                format!("{}{source}", groups.trim_start_matches(" · "))
            } else {
                format!("{}{groups}{source}", s.address())
            };
            if crate::context::server_production(s, &self.config.production.words) {
                detail = format!("PROD · {detail}").trim_end_matches(" · ").to_string();
            }
            items.push(PickItem {
                label: s.name.clone(),
                detail,
                action: PickAction::Ssh(s.name.clone()),
                state: None,
                dim: k.source == Source::SshConfig,
            });
        }
        if self.config.servers.auto_add {
            for s in &self.discovered {
                if servers::is_known(&self.known_servers, s) {
                    continue;
                }
                items.push(PickItem::new(
                    format!("Add {}", s.name),
                    format!("{} · discovered · not saved", s.address()),
                    PickAction::AddServer(Some(Box::new(s.clone()))),
                ));
            }
        }
        items.push(PickItem::new("+ Add server", "", PickAction::AddServer(None)));
        Picker::new(PickerKind::Ssh, "Servers", items)
    }

    /// The production servers among `target` (a server name or `@group`).
    pub(super) fn production_servers(&self, target: &str) -> Vec<String> {
        let words = &self.config.production.words;
        self.known_servers
            .iter()
            .filter(|k| match target.strip_prefix('@') {
                Some(g) => k.server.groups.iter().any(|x| x == g),
                None => k.server.name == target,
            })
            .filter(|k| crate::context::server_production(&k.server, words))
            .map(|k| k.server.name.clone())
            .collect()
    }

    /// Remove a server from the inventory file.
    pub(super) fn forget_server(&mut self, name: &str) -> Result<bool> {
        let removed = Inventory::update(|inv| Ok(inv.remove(name)))?;
        if removed {
            let defs: Vec<WorkspaceDef> = self.definitions().into_iter().filter_map(|(_, d)| d.ok()).collect();
            self.refresh_known(&defs);
        }
        Ok(removed)
    }
}

/// Whether a command line runs `ssh`.
pub(super) fn runs_ssh(argv: &[String]) -> bool {
    argv.first().is_some_and(|c| c.rsplit('/').next() == Some("ssh"))
}

/// The workspace of a group running `remote`: `journalctl -fu nginx` on @prod gets its own,
/// next to the plain shells. The whole command names it (where it is shown, it is cut to fit):
/// two commands that differ only at their end are two workspaces.
fn group_workspace_name(group: &str, remote: &[String]) -> String {
    if remote.is_empty() { group.to_string() } else { format!("{group}: {}", remote.join(" ")) }
}

/// Hosts from `~/.ssh/config` (no wildcards).
pub fn ssh_config_hosts() -> Vec<String> {
    ssh_config_hosts_observed(&mut |_, _| {}).0
}

pub(super) fn ssh_config_hosts_observed(observe: &mut impl FnMut(&Path, bool)) -> (Vec<String>, bool) {
    let Some(home) = paths::home_dir() else { return (Vec::new(), false) };
    let mut out = Vec::new();
    let failed = read_ssh_config(&home.join(".ssh/config"), &mut out, 0, observe);
    out.sort();
    out.dedup();
    (out, failed)
}

fn read_ssh_config(path: &Path, out: &mut Vec<String>, depth: u8, observe: &mut impl FnMut(&Path, bool)) -> bool {
    observe(path, false);
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => return e.kind() != std::io::ErrorKind::NotFound,
    };
    let mut failed = false;
    for line in text.lines() {
        let line = line.trim();
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else { continue };
        if key.eq_ignore_ascii_case("host") {
            out.extend(parts.filter(|h| !h.contains(['*', '?', '!'])).map(String::from));
        } else if key.eq_ignore_ascii_case("include") && depth < 3 {
            for pat in parts {
                let p = paths::expand_tilde(pat);
                let p = if p.is_relative() { paths::home_dir().unwrap_or_default().join(".ssh").join(p) } else { p };
                // Support a trailing `*` glob, the common case (`Include config.d/*`).
                if let Some(dir) = p.to_str().and_then(|s| s.strip_suffix("/*")) {
                    observe(Path::new(dir), true);
                    let entries = match std::fs::read_dir(dir) {
                        Ok(entries) => entries,
                        Err(e) => {
                            failed |= e.kind() != std::io::ErrorKind::NotFound;
                            continue;
                        }
                    };
                    let mut files = Vec::new();
                    for entry in entries {
                        match entry {
                            Ok(entry) => files.push(entry.path()),
                            Err(_) => failed = true,
                        }
                    }
                    files.sort();
                    for f in files {
                        failed |= read_ssh_config(&f, out, depth + 1, observe);
                    }
                } else {
                    failed |= read_ssh_config(&p, out, depth + 1, observe);
                }
            }
        }
    }
    failed
}

/// The group `kraal ssh --group` names, with or without its `@`.
pub(super) fn ssh_group(target: &str) -> &str {
    target.strip_prefix('@').unwrap_or(target)
}

/// The line an `ssh` pane runs on the server, as ssh sends it (it joins words with spaces):
/// what a question about it shows.
pub(super) fn remote_command(remote: &[String]) -> String {
    remote.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn include_traversal_failures_are_reported_but_missing_paths_are_optional() {
        let dir = crate::testing::TempDir::new("ssh-includes");
        let included = dir.write("included", "Host included\n");
        let missing = dir.0.join("missing");
        let config = dir.write("config", format!("Include {} {}/*\n", missing.display(), missing.display()));
        let mut hosts = Vec::new();
        assert!(!read_ssh_config(&config, &mut hosts, 0, &mut |_, _| {}));
        assert!(hosts.is_empty());
        assert!(!read_ssh_config(&missing, &mut hosts, 0, &mut |_, _| {}));
        std::fs::write(&config, format!("Include {}/* {}\n", included.display(), included.display())).unwrap();
        let mut directories = Vec::new();
        assert!(read_ssh_config(&config, &mut hosts, 0, &mut |path, directory| {
            if directory {
                directories.push(path.to_path_buf());
            }
        }));
        assert_eq!(directories, [included]);
        assert_eq!(hosts, ["included"], "a failed include does not discard hosts from readable files");
    }

    #[test]
    fn an_unreadable_inventory_keeps_the_servers_last_read() {
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = crate::server::tests::server(tx);
        let mut inv = Inventory::empty();
        inv.servers.push(servers::Server {
            name: "db".into(),
            host: "db.lan".into(),
            port: Some(2222),
            ..Default::default()
        });
        server.apply_known(&[], Ok(inv), &[]);
        let before = server.resolve_server("db");
        server.apply_known(&[], Err(anyhow::anyhow!("servers.toml: expected `=`")), &[]);
        assert_eq!(server.resolve_server("db"), before);
        assert_eq!(before.unwrap().host, "db.lan");
    }

    #[test]
    fn discovery_offers_a_form_without_a_write_or_a_connection() {
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = crate::server::tests::server(tx);
        let args: Vec<String> = ["-i", "~/.ssh/db", "-J", "bastion", "admin@db.lan"].map(String::from).into();
        server.config.servers.auto_add = false;
        server.observe_ssh(0, &args);
        assert!(server.discovered.is_empty());
        server.config.servers.auto_add = true;
        server.observe_ssh(0, &args);
        server.observe_ssh(0, &args);
        assert_eq!(server.discovered.len(), 1);
        assert!(server.jobs.is_empty());
        assert!(server.panes.is_empty());
        let mut picker = server.ssh_picker();
        assert_eq!(picker.title, "Servers");
        assert_eq!(picker.action_keys(), [("enter", "Add")]);
        assert!(picker.current().unwrap().detail.contains("not saved"));
        let PickAction::AddServer(Some(s)) = &picker.current().unwrap().action else { panic!() };
        assert_eq!(s.identity.as_deref(), Some("~/.ssh/db"));
        assert_eq!(s.jump.as_deref(), Some("bastion"));
        server.known_servers.push(servers::Known { server: *s.clone(), source: Source::Inventory });
        picker = server.ssh_picker();
        assert!(!picker.items.iter().any(|i| matches!(i.action, PickAction::AddServer(Some(_)))));
        picker.select(|i| matches!(i.action, PickAction::Ssh(_)));
        assert_eq!(picker.action_keys()[0], ("enter", "Connect"));
    }

    #[test]
    fn failed_ssh_defaults_to_close_keeps_the_server_and_retry_failure_stays_actionable() {
        use super::super::ui::Mode;
        for size in [(80, 24), (124, 36)] {
            let (tx, _rx) = crate::server::event_channel(128);
            let mut server = crate::server::tests::server(tx);
            let (client, _peer) = crate::server::tests::client(&mut server);
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = size;
            let ws = c.workspace;
            let s = servers::resolve(&[], "admin@db.lan").unwrap();
            server.known_servers.push(servers::Known { server: s.clone(), source: Source::Inventory });
            let pane = server
                .spawn_pane_in(
                    ws,
                    None,
                    PaneSpec { command: vec!["sh".into(), "-c".into(), "exit 0".into()], ..Default::default() },
                    Placement::Tab,
                    None,
                )
                .unwrap();
            let p = server.panes.get_mut(&pane).unwrap();
            p.spec.command = vec!["ssh".into(), s.destination()];
            p.spec.connection = Some(s);
            p.exit_code = Some(Some(255));
            server.show_ssh_failure(pane);
            let Mode::Confirm(q) = &server.clients[&client].mode else { panic!("failure dialog") };
            assert!(!q.yes);
            assert_eq!(q.action.cancel_label(), "Close");
            assert_eq!(q.action.verb(), "Try again");
            let screen = server.client_screen(client);
            let mut buf = ratatui::buffer::Buffer::empty(screen);
            super::super::overlays::draw_confirm(q, screen, &mut buf);
            let text = crate::testing::buffer_text(&buf);
            assert!(text.contains("The server is saved."));
            assert!(text.contains("Close") && text.contains("Try again"));
            let retry = super::super::overlays::confirm_layout(q, screen).ok.unwrap();
            // A missing SSH executable also keeps a retryable error, without running a network command.
            server.panes.get_mut(&pane).unwrap().spec.command = vec!["/missing/ssh".into()];
            if size.0 == 80 {
                server.client_input(client, b"\t\r");
            } else {
                server.client_input(client, format!("\x1b[<0;{};{}M", retry.x + 1, retry.y + 1).as_bytes());
            }
            assert!(matches!(&server.clients[&client].mode, Mode::Confirm(q) if !q.yes));
            server.client_input(client, b"\r");
            assert!(!server.panes.contains_key(&pane));
            assert!(server.ws_index(ws).is_some());
            assert_eq!(server.known_servers.len(), 1);
        }
    }

    #[test]
    fn reopening_a_group_reports_restore_failure_and_keeps_its_identity() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = crate::server::tests::server(tx);
        let ws = server.new_workspace("prod".into(), None);
        server.spawn_pane_in(ws, None, PaneSpec::default(), Placement::Tab, None).unwrap();
        crate::server::persistence::tests::writer(&mut server, vec![]);
        server.keep_and_close_workspace(ws).unwrap();
        server.env.shell = "/missing/shell".into();
        assert!(server.open_group("prod", &[]).unwrap_err().to_string().contains("Its layout is kept"));
        assert!(server.workspaces[0].closed());
        assert!(server.panes.is_empty());
        server.env.shell = "sh".into();
        assert_eq!(server.open_group("prod", &[]).unwrap(), ws);
        assert_eq!(server.workspaces.len(), 1);
        assert!(!server.workspaces[0].closed());
        assert_eq!(server.workspaces[0].panes().len(), 1);
        for pane in server.workspaces[0].panes() {
            server.close_pane(pane);
        }
    }

    #[test]
    fn group_commands_get_a_workspace_each() {
        let name = |cmd: &str| group_workspace_name("prod", &[cmd.to_string()]);
        let march = name("journalctl -fu nginx --since=2020-03-01");
        assert_eq!(march, "prod: journalctl -fu nginx --since=2020-03-01");
        assert_ne!(march, name("journalctl -fu nginx --since=2020-04-01"));
        assert_eq!(group_workspace_name("prod", &[]), "prod");
    }
}
