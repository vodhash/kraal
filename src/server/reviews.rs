//! Pull requests in the server: those of the branches local panes are on, asked of `gh` in the
//! background, shown on frames and in the sidebar, announced when their checks or their review
//! change, and sent to an agent when you ask (see [`crate::review`]).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::ui::{ConfirmAction, Mode, PickAction, PickItem};
use super::{ClientId, Server};
use crate::git::GitInfo;
use crate::layout::PaneId;
use crate::review::{News, Pr, Rollup, State, Tone};
use crate::theme::NoticeKind;

/// Branches of a repository (by its main checkout), and what is known of each one's pull request.
pub type PrWatches = HashMap<PathBuf, HashMap<String, PrWatch>>;

pub struct PrWatch {
    /// A checkout of the repository, to run `gh` in: the last one a pane was seen in.
    dir: PathBuf,
    pub pr: Option<Pr>,
    asked: Option<Instant>,
    next: Instant,
    /// The question being answered: an answer to an older one is dropped.
    asking: Option<u32>,
    /// The checkout changed while a question was out: ask again soon after its answer.
    changed: bool,
    /// After a failure, no question before this, whatever changes.
    floor: Option<Instant>,
    /// The ids of the comments seen so far.
    seen: HashSet<String>,
}

impl PrWatch {
    fn new(dir: PathBuf, now: Instant) -> Self {
        PrWatch {
            dir,
            pr: None,
            asked: None,
            next: now,
            asking: None,
            changed: false,
            floor: None,
            seen: HashSet::new(),
        }
    }

    /// Asked again only after `wait`, even when the checkout changes.
    fn retry_after(&mut self, now: Instant, wait: Duration) {
        self.next = now + wait;
        self.floor = Some(self.next);
    }
}

/// The soonest a branch is asked about again after its checkout changed (a commit, a push).
const SOON: Duration = Duration::from_secs(15);

/// How long until a branch is asked about again: often while its checks run, rarely once its
/// pull request is closed, when there is none, or when `gh` could not tell.
fn wait(pr: Option<&Pr>, failed: bool) -> Duration {
    Duration::from_secs(match pr {
        _ if failed => 300,
        None => 300,
        Some(p) if p.state != State::Open => 900,
        Some(p) if p.checks.state == Rollup::Pending => 30,
        Some(_) => 120,
    })
}

impl Server {
    /// Ask `gh` about the branches of local panes that are due, and forget those no pane is on.
    pub(super) fn refresh_prs(&mut self, now: Instant) {
        let mut shown: HashSet<(&PathBuf, &String)> = HashSet::new();
        for info in self.git.values().flatten().filter(|i| !i.detached) {
            shown.insert((&info.main, &info.branch));
            let branches = self.prs.entry(info.main.clone()).or_default();
            match branches.get_mut(&info.branch) {
                Some(w) => w.dir.clone_from(&info.root),
                None => {
                    branches.insert(info.branch.clone(), PrWatch::new(info.root.clone(), now));
                }
            }
        }
        for (main, branches) in self.prs.iter_mut() {
            branches.retain(|branch, _| shown.contains(&(main, branch)));
        }
        self.prs.retain(|_, branches| !branches.is_empty());
        let due: Vec<(PathBuf, String, PathBuf)> = self
            .prs
            .iter()
            .flat_map(|(main, branches)| branches.iter().map(move |(b, w)| (main, b, w)))
            .filter(|(_, _, w)| w.asking.is_none() && w.next <= now)
            .map(|(main, b, w)| (main.clone(), b.clone(), w.dir.clone()))
            .collect();
        if due.is_empty() {
            return;
        }
        let gh = crate::exec::have_program("gh");
        for (main, branch, dir) in due {
            let question = self.alloc_id();
            let Some(w) = self.prs.get_mut(&main).and_then(|b| b.get_mut(&branch)) else { continue };
            if !gh {
                w.retry_after(now, wait(None, true));
                continue;
            }
            w.asking = Some(question);
            let asked = branch.clone();
            let (m, b) = (main.clone(), branch.clone());
            let started = self.background(
                move || crate::review::fetch(&dir, &asked).map_err(|e| format!("{e:#}")),
                move |server, answer| server.pr_answer(m, b, question, answer),
            );
            if !started && let Some(w) = self.prs.get_mut(&main).and_then(|b| b.get_mut(&branch)) {
                w.asking = None;
                w.retry_after(now, wait(None, true));
            }
        }
    }

    /// Something changed in a checkout (a commit, a push): ask about its branch's pull request
    /// soon, not before the wait a failure set.
    pub(super) fn pr_soon(&mut self, info: &GitInfo) {
        if let Some(w) = self.prs.get_mut(&info.main).and_then(|b| b.get_mut(&info.branch)) {
            if w.asking.is_some() {
                w.changed = true;
            }
            let soon = w.asked.map_or_else(Instant::now, |a| a + SOON);
            w.next = w.next.min(soon).max(w.floor.unwrap_or(soon));
        }
    }

    /// When the next branch is due to be asked about: the server wakes for it.
    pub(super) fn next_pr_due(&self) -> Option<Instant> {
        self.prs.values().flat_map(|b| b.values()).filter(|w| w.asking.is_none()).map(|w| w.next).min()
    }

    fn pr_answer(&mut self, main: PathBuf, branch: String, question: u32, answer: Result<Option<Pr>, String>) {
        let now = Instant::now();
        let Some(w) = self.prs.get_mut(&main).and_then(|b| b.get_mut(&branch)) else { return };
        if w.asking != Some(question) {
            return;
        }
        w.asking = None;
        w.asked = Some(now);
        let changed = std::mem::take(&mut w.changed);
        // A failure (no network, gh signed out, no GitHub remote) keeps what was known, quietly.
        let Ok(new) = answer else {
            w.retry_after(now, wait(w.pr.as_ref(), true));
            return;
        };
        w.floor = None;
        w.next = now + wait(new.as_ref(), false);
        if changed {
            w.next = w.next.min(now + SOON);
        }
        let news = match (&w.pr, &new) {
            (Some(old), Some(new)) => new.news(old, &w.seen),
            _ => Vec::new(),
        };
        if let Some(new) = &new {
            w.seen.extend(new.comment_ids().cloned());
        }
        if w.pr != new {
            w.pr = new;
            self.mark_dirty();
        }
        if !news.is_empty() {
            self.announce_pr(&main, &branch, &news);
        }
    }

    /// Events, and a notification, for what changed in a branch's pull request.
    fn announce_pr(&mut self, main: &PathBuf, branch: &str, news: &[News]) {
        let Some(pr) = self.prs.get(main).and_then(|b| b.get(branch)).and_then(|w| w.pr.clone()) else { return };
        let agent = self.branch_agent(main, branch, None);
        let mut base = agent.map(|p| self.pane_fields(p)).unwrap_or_default();
        base.insert("repo".into(), json!(main.display().to_string()));
        base.insert("branch".into(), json!(branch));
        base.insert("number".into(), json!(pr.number));
        base.insert("url".into(), json!(pr.url));
        let failed: Vec<&str> = pr.checks.failed.iter().map(|c| c.name.as_str()).collect();
        let mut parts = Vec::new();
        let mut review = serde_json::Map::new();
        for n in news {
            match n {
                News::ChecksFailed => {
                    let mut fields = base.clone();
                    fields.insert("checks".into(), json!(failed));
                    self.emit("pr.checks.failed", Value::Object(fields));
                    parts.push(if failed.is_empty() {
                        "CI failed".into()
                    } else {
                        format!("CI failed: {}", failed.join(", "))
                    });
                }
                News::ChecksPassed => {
                    self.emit("pr.checks.passed", Value::Object(base.clone()));
                    parts.push("CI passed".into());
                }
                News::ChangesRequested | News::Approved => {
                    let (decision, said) = match n {
                        News::Approved => ("approved", "approved"),
                        _ => ("changes_requested", "changes requested"),
                    };
                    review.insert("decision".into(), json!(decision));
                    parts.push(said.into());
                }
                News::Comments(count) => {
                    review.insert("comments".into(), json!(count));
                    parts.push(if *count == 1 { "1 new comment".into() } else { format!("{count} new comments") });
                }
            }
        }
        if !review.is_empty() {
            let mut fields = base;
            fields.extend(review);
            self.emit("pr.review", Value::Object(fields));
        }
        if self.config.notifications.on_pull_request {
            let warn = news.iter().any(|n| matches!(n, News::ChecksFailed | News::ChangesRequested));
            let kind = if warn { NoticeKind::Warning } else { NoticeKind::Notice };
            self.notify(kind, &format!("#{} · {branch}", pr.number), &parts.join(" · "), agent);
        }
    }

    /// The pull request of the branch a local pane is on, once `gh` has said.
    pub fn pane_pr(&self, pane: PaneId) -> Option<&Pr> {
        let g = self.pane_git(pane).filter(|g| !g.detached)?;
        self.prs.get(&g.main)?.get(&g.branch)?.pr.as_ref()
    }

    /// The agent to send a branch's review to: `prefer` when it is one, else the first agent
    /// pane on that branch. Only one whose program in front is that agent: not a shell it was
    /// suspended for, nor anything started from there, which would run what it is sent. Never one
    /// on production.
    fn branch_agent(&self, main: &PathBuf, branch: &str, prefer: Option<PaneId>) -> Option<PaneId> {
        let fits = |p: PaneId| {
            self.panes.get(&p).is_some_and(|pane| pane.agent.kind.is_some() && pane.exit_code.is_none())
                && self.pane_production(p).is_none()
                && self.pane_git(p).is_some_and(|g| {
                    // The agent works in that checkout, not only the pane's shell (`cd ../b && claude`).
                    !g.detached
                        && &g.main == main
                        && g.branch == branch
                        && self.agent_in_front(p).is_some_and(|(_, dir)| crate::git::is_within(&dir, &g.root))
                })
        };
        prefer.filter(|p| fits(*p)).or_else(|| self.panes.keys().copied().filter(|p| fits(*p)).min())
    }

    /// The process running in front in pane `p`, and the directory it works in, when it is the
    /// agent the pane is known for.
    fn agent_in_front(&self, p: PaneId) -> Option<(u32, PathBuf)> {
        let pane = self.panes.get(&p)?;
        let pid = pane.foreground_pid()?;
        let agent = crate::process::argv(pid).as_deref().and_then(crate::agent::detect_agent)?;
        (pane.agent.kind.as_deref() == Some(agent)).then_some((pid, crate::process::cwd(pid)?))
    }

    fn review_agent(&self, pane: PaneId) -> Option<PaneId> {
        let g = self.pane_git(pane)?;
        self.branch_agent(&g.main, &g.branch, Some(pane))
    }

    /// Frame colour of a pull request label.
    pub(super) fn pr_color(tone: Tone) -> ratatui::style::Color {
        let t = crate::theme::current();
        match tone {
            Tone::Failed => t.blocked,
            Tone::Pending => t.working,
            Tone::Passed => t.done,
            Tone::Plain => t.dim,
        }
    }

    /// The actions on the pull request of a pane's branch, for the git menu.
    pub(super) fn pr_items(&self, pane: PaneId) -> Vec<PickItem> {
        let Some(pr) = self.pane_pr(pane) else { return Vec::new() };
        let item = |label: String, detail: String, op: &str| PickItem {
            label,
            detail,
            action: PickAction::Worktree { pane, op: op.into() },
            state: None,
            dim: false,
        };
        let mut items = vec![item(format!("Open pull request #{}", pr.number), pr.url.clone(), "pr-view")];
        if pr.state != State::Open {
            return items;
        }
        let Some(agent) = self.review_agent(pane) else { return items };
        let to = self.pane_label(agent);
        if pr.checks.state == Rollup::Failed {
            let names: Vec<&str> = pr.checks.failed.iter().map(|c| c.name.as_str()).collect();
            let detail = match names.is_empty() {
                true => format!("CI failed · to {to}"),
                false => format!("{} failed · to {to}", names.join(", ")),
            };
            items.push(item("Send failing checks to the agent".into(), detail, "pr-checks"));
        }
        if !pr.comments.is_empty() {
            let n = pr.comments.len();
            let mut detail = format!("{n} to address · to {to}");
            if !pr.others.is_empty() {
                detail.push_str(&format!(" · {} from others left out", pr.others.len()));
            }
            items.push(item("Send review comments to the agent".into(), detail, "pr-comments"));
        }
        items
    }

    pub(super) fn pr_op(&mut self, client: ClientId, pane: PaneId, op: &str) -> Mode {
        let Some(pr) = self.pane_pr(pane).cloned() else { return Mode::Normal };
        if op == "pr-view" {
            self.open_for(client, &pr.url);
            return Mode::Normal;
        }
        let Some(agent) = self.review_agent(pane) else {
            self.toast(client, NoticeKind::Warning, "no agent on this branch to send it to");
            return Mode::Normal;
        };
        let text = match op {
            "pr-checks" => pr.checks_prompt(),
            "pr-comments" => pr.comments_prompt(),
            _ => None,
        };
        let (Some(text), Some(g)) = (text, self.pane_git(agent).cloned()) else { return Mode::Normal };
        // Quoted, the text is shown as it is written: every space, each line its own.
        let quoted: Vec<String> = text.lines().map(|l| format!("  {l}")).collect();
        let mut message = format!("Send this to {}?\n\n{}", self.pane_label(agent), quoted.join("\n"));
        if op == "pr-comments" && !pr.others.is_empty() {
            message.push_str(&format!(
                "\n\nLeft out, from people who are not the repository's owners, collaborators or organization members: {}",
                pr.others.join(", ")
            ));
        }
        Mode::confirm(message, ConfirmAction::SendToAgent { pane: agent, main: g.main, branch: g.branch, text })
    }

    /// Confirmed: what the pane's checkout is on is read again (off the event loop: a checkout in
    /// another terminal may not be seen yet), then the text is sent if it is still about it.
    pub(super) fn confirm_send(&mut self, client: ClientId, pane: PaneId, main: PathBuf, branch: String, text: String) {
        let Some((pid, dir)) = self.agent_in_front(pane) else {
            let label = self.pane_label(pane);
            self.toast(
                client,
                NoticeKind::Error,
                format!("{label} does not run its agent in front now: nothing was sent"),
            );
            return;
        };
        self.background(
            move || crate::git::status(&dir),
            move |server, now| server.send_to_agent(client, pane, pid, &main, &branch, &text, now),
        );
    }

    /// The confirmed text goes to the agent as a paste, then Enter: only while that pane is still
    /// the agent of the branch the text is about (`now`: its checkout as just read, where agent
    /// process `pid` works), runs that same process in front, and takes pastes whole
    /// (bracketed): line by line, each line would be sent on its own.
    #[expect(clippy::too_many_arguments, reason = "what was asked about, and what was read since")]
    fn send_to_agent(
        &mut self,
        client: ClientId,
        pane: PaneId,
        pid: u32,
        main: &PathBuf,
        branch: &str,
        text: &str,
        now: Option<GitInfo>,
    ) {
        let label = self.pane_label(pane);
        let still = now.is_some_and(|g| !g.detached && &g.main == main && g.branch == branch)
            && self.agent_in_front(pane).is_some_and(|(front, _)| front == pid);
        if !still || self.branch_agent(main, branch, Some(pane)) != Some(pane) {
            let why = format!("{label} is no longer the agent of {branch} here: nothing was sent");
            self.toast(client, NoticeKind::Error, why);
            return;
        }
        if !self.panes[&pane].input_modes().bracketed_paste {
            self.toast(client, NoticeKind::Error, format!("{label} does not take a paste now: nothing was sent"));
            return;
        }
        match self.buffer_paste(pane, text) {
            Ok(bytes) => {
                let p = self.panes.get_mut(&pane).expect("checked above");
                p.write_input(bytes);
                p.write_input(b"\r".to_vec());
                self.toast(client, NoticeKind::Notice, format!("sent to {label}"));
            }
            Err(e) => self.toast(client, NoticeKind::Error, format!("{e}: nothing was sent")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::PaneSpec;
    use crate::review::{Check, Checks, Comment, Decision};

    fn open() -> Pr {
        Pr {
            number: 12,
            url: "https://github.com/o/r/pull/12".into(),
            state: State::Open,
            draft: false,
            head: "abc".into(),
            checks: Checks { state: Rollup::Pending, ..Checks::default() },
            decision: None,
            comments: Vec::new(),
            more_threads: false,
            others: Vec::new(),
        }
    }

    fn failing() -> Pr {
        let failed = vec![Check { name: "linux".into(), url: "https://github.com/o/r/actions/runs/7/job/1".into() }];
        Pr { checks: Checks { state: Rollup::Failed, failed, more: false }, ..open() }
    }

    /// Follow `kraal/fix` of the repository in `dir`, as if a question about it were out.
    fn watch(server: &mut Server, dir: &std::path::Path, pr: Option<Pr>) {
        let mut w = PrWatch::new(dir.into(), Instant::now() + Duration::from_secs(3600));
        w.pr = pr;
        w.asking = Some(1);
        server.prs.entry(dir.into()).or_default().insert("kraal/fix".into(), w);
    }

    fn pr_events(server: &Server) -> Vec<Value> {
        let kept = server.events.kept.iter().filter(|e| e["type"].as_str().is_some_and(|t| t.starts_with("pr.")));
        kept.map(|e| Value::Object(e.clone())).collect()
    }

    #[test]
    fn a_change_of_checks_or_review_is_an_event_and_a_notification_once() {
        let (tx, _rx) = crate::server::event_channel(64);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let dir = PathBuf::from("/repo");
        let answer = |server: &mut Server, pr: Result<Option<Pr>, String>| {
            server.pr_answer(dir.clone(), "kraal/fix".into(), 1, pr);
            server.prs.get_mut(&dir).unwrap().get_mut("kraal/fix").unwrap().asking = Some(1);
        };
        watch(&mut server, &dir, None);
        answer(&mut server, Ok(Some(open())));
        assert!(pr_events(&server).is_empty(), "the first sight is no news");

        answer(&mut server, Ok(Some(failing())));
        let events = pr_events(&server);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["type"], "pr.checks.failed");
        assert_eq!((events[0]["branch"].clone(), events[0]["number"].clone()), (json!("kraal/fix"), json!(12)));
        assert_eq!(events[0]["checks"], json!(["linux"]));
        let toast = server.clients[&client].toast.clone().unwrap();
        assert_eq!(toast.0, "#12 · kraal/fix: CI failed: linux");

        answer(&mut server, Ok(Some(failing())));
        answer(&mut server, Err("gh: offline".into()));
        assert_eq!(pr_events(&server).len(), 1, "the same failure, or no answer, says nothing new");
        assert_eq!(server.prs[&dir]["kraal/fix"].pr, Some(failing()), "a failed answer keeps what was known");

        // An answer to an older question (the branch was left, then followed again) is dropped.
        server.pr_answer(dir.clone(), "kraal/fix".into(), 0, Ok(None));
        assert_eq!(server.prs[&dir]["kraal/fix"].pr, Some(failing()));

        server.config.notifications.on_pull_request = false;
        server.clients.get_mut(&client).unwrap().toast = None;
        let comment = Comment {
            place: Some("a.rs:1".into()),
            messages: vec![("bob".into(), "Add a test.".into())],
            ids: vec!["c1".into()],
            more: 0,
        };
        let reviewed = Pr { decision: Some(Decision::ChangesRequested), comments: vec![comment], ..failing() };
        answer(&mut server, Ok(Some(reviewed)));
        let events = pr_events(&server);
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[1]["type"], "pr.review");
        assert_eq!(
            (events[1]["decision"].clone(), events[1]["comments"].clone()),
            (json!("changes_requested"), json!(1))
        );
        assert!(server.clients[&client].toast.is_none(), "on_pull_request = false: events only");
    }

    #[test]
    fn failing_checks_go_to_the_agent_as_shown_in_one_paste() {
        let dir = std::env::temp_dir().join(format!("kraal-review-{}", crate::identity::new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git").arg("-C").arg(&dir).args(args).status().unwrap().success());
        };
        git(&["init", "-q"]);
        git(&["symbolic-ref", "HEAD", "refs/heads/kraal/fix"]);
        let out = dir.join(".git/received");
        let (tx, rx) = crate::server::event_channel(4096);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let ws = server.clients[&client].workspace;
        // An agent that takes pastes whole and keeps what it is sent.
        let quoted = crate::shell::quote(&out.display().to_string());
        let agent = format!("printf '\\033[?2004h'; stty raw -echo; exec -a claude cat > {quoted}");
        let command = vec!["/bin/bash".into(), "-c".into(), agent];
        let spec = PaneSpec { command, cwd: Some(dir.clone()), ..Default::default() };
        let pane =
            server.spawn_pane_in(ws, None, spec, crate::protocol::Placement::Auto, Some("claude".into())).unwrap();
        server.panes.get_mut(&pane).unwrap().agent.kind = Some("claude".into());
        let pump = |server: &mut Server, done: &dyn Fn(&Server) -> bool| {
            let started = Instant::now();
            while !done(server) {
                assert!(started.elapsed() < crate::testing::PATIENCE, "gave up waiting");
                while let Ok(event) = rx.try_recv() {
                    server.handle(event);
                }
                let mut data = Vec::new();
                while server.take_output(pane, &mut data, false) {
                    server.pane_output(pane, &data);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        pump(&mut server, &|s| s.panes[&pane].input_modes().bracketed_paste && s.agent_in_front(pane).is_some());
        let real = crate::git::status(&dir).unwrap();
        assert_eq!((real.branch.as_str(), real.main.as_path()), ("kraal/fix", dir.as_path()));
        server.git.insert(dir.clone(), Some(real));
        let reviewed = Pr { others: vec!["mallory".into()], ..failing() };
        watch(&mut server, &dir, Some(reviewed));

        // A pane known for an agent where something else runs in front now is no target.
        let command = vec!["/bin/sh".into(), "-c".into(), "exec sleep 60".into()];
        let spec = PaneSpec { command, cwd: Some(dir.clone()), ..Default::default() };
        let other =
            server.spawn_pane_in(ws, None, spec, crate::protocol::Placement::Auto, Some("other".into())).unwrap();
        server.panes.get_mut(&other).unwrap().agent.kind = Some("claude".into());
        pump(&mut server, &|s| s.panes[&other].foreground_argv().is_some_and(|a| a[0] == "sleep"));
        assert_eq!(server.review_agent(other), Some(pane), "the agent really in front gets it");

        let items: Vec<String> =
            server.pr_items(pane).into_iter().map(|i| format!("{} | {}", i.label, i.detail)).collect();
        assert_eq!(
            items,
            [
                "Open pull request #12 | https://github.com/o/r/pull/12",
                "Send failing checks to the agent | linux failed · to test › claude",
            ]
        );
        let answer = |server: &mut Server| {
            let Mode::Confirm(mut question) = server.pr_op(client, pane, "pr-checks") else {
                panic!("a question first")
            };
            question.yes = true;
            let message = question.message.clone();
            server.clients.get_mut(&client).unwrap().toast = None;
            server.clients.get_mut(&client).unwrap().mode = Mode::Confirm(question);
            server.client_input(client, b"\r");
            pump(server, &|s| s.clients[&client].toast.is_some());
            (message, server.clients[&client].toast.clone().unwrap().0)
        };
        let prompt = failing().checks_prompt().unwrap();

        // Asked about kraal/fix, the checkout is on another branch by the time of the answer,
        // before kraal has seen it.
        git(&["symbolic-ref", "HEAD", "refs/heads/other"]);
        let (message, toast) = answer(&mut server);
        let quoted: Vec<String> = prompt.lines().map(|l| format!("  {l}")).collect();
        assert_eq!(message, format!("Send this to test › claude?\n\n{}", quoted.join("\n")));
        assert_eq!(toast, "test › claude is no longer the agent of kraal/fix here: nothing was sent");

        git(&["symbolic-ref", "HEAD", "refs/heads/kraal/fix"]);
        let (_, toast) = answer(&mut server);
        assert_eq!(toast, "sent to test › claude");
        let want = format!("\x1b[200~{prompt}\x1b[201~\r");
        let started = Instant::now();
        while std::fs::read_to_string(&out).unwrap_or_default() != want {
            assert!(started.elapsed() < crate::testing::PATIENCE, "{:?}", std::fs::read_to_string(&out));
            std::thread::sleep(Duration::from_millis(5));
        }
        for p in server.panes.values() {
            if let Some(stop) = crate::server::panes::ProgramStop::new(p) {
                stop.signal(libc::SIGKILL);
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
