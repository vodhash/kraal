//! Git: what branch a pane is on and whether it has changes, and worktrees that give each coding
//! agent its own branch and directory.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitInfo {
    pub root: PathBuf,
    /// Branch name, or a short commit id when detached.
    pub branch: String,
    /// No branch is checked out (`branch` is a commit id).
    pub detached: bool,
    /// The commit checked out (`(initial)` before the first).
    pub head: String,
    /// The branch it follows (`origin/main`), if any.
    pub upstream: Option<String>,
    /// Tracked files differ from HEAD.
    pub dirty: bool,
    /// Files git does not track yet (what agents create most), or perhaps: in a tree too big to
    /// list them in time, they are not ruled out.
    pub untracked: bool,
    pub ahead: u32,
    pub behind: u32,
    /// The number of the kraal worktree this is (see [`Worktree::index`]); 0 elsewhere.
    pub worktree_index: u32,
    /// The main checkout of the repository (`root` itself unless this is a linked worktree).
    pub main: PathBuf,
    /// On a kraal worktree's branch: the base it was made from, when recorded.
    pub base: String,
    /// The directory git was asked about, its symbolic links resolved, as git gives paths
    /// (`root`, `main`, the worktrees it lists): what a directory is compared with them as.
    pub real: PathBuf,
    /// Where git writes what changes this (HEAD, the index, the branch and its upstream): the
    /// directories to watch to know when to ask again. Empty when git could not say.
    pub watch: Vec<PathBuf>,
}

impl GitInfo {
    /// `⎇ main* ↑2` style label.
    pub fn label(&self, ascii: bool) -> String {
        let mut s = format!("{}{}", if ascii { "git:" } else { "⎇ " }, self.branch);
        if self.dirty {
            s.push('*');
        }
        if self.untracked {
            s.push('?');
        }
        if self.ahead > 0 {
            s.push_str(&format!(" {}{}", if ascii { "+" } else { "↑" }, self.ahead));
        }
        if self.behind > 0 {
            s.push_str(&format!(" {}{}", if ascii { "-" } else { "↓" }, self.behind));
        }
        s
    }
}

/// A worktree kraal created for an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// The main checkout.
    pub repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    /// What the branch started from, and what reviews compare against.
    pub base: String,
    /// Its number among the repository's kraal worktrees (1, 2…), kept while it exists: what
    /// tells its ports from those of the others. 0 when it has none (made by an older kraal).
    #[serde(default)]
    pub index: u32,
}

impl Worktree {
    pub fn name(&self) -> String {
        self.branch.strip_prefix("kraal/").unwrap_or(&self.branch).to_string()
    }
}

/// git in `dir`, never asking anything, nor taking a lock it can do without (one the user's own
/// git would then wait for).
fn git_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).env("GIT_OPTIONAL_LOCKS", "0").env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

/// Run git with a deadline; stdout on success.
fn git(dir: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    let mut cmd = git_cmd(dir);
    cmd.args(args);
    if args.first() == Some(&"config") {
        // set_config looks for git's own words about its lock (`git config` runs no hooks).
        cmd.env("LC_ALL", "C");
    }
    let out = crate::exec::run(&mut cmd, timeout, 8 << 20).context("running git")?;
    if out.timed_out {
        bail!("git {} took too long", args.join(" "));
    }
    if !out.success() {
        bail!("git {}: {}", args.join(" "), out.stderr_text().trim());
    }
    Ok(out.stdout_text())
}

/// Branch and state of the repository containing `cwd`, if any. Untracked files count; when
/// listing them is too slow (huge trees), only tracked changes are checked and untracked files
/// are not ruled out (`?` stays shown), rather than the tree looking clean.
pub fn status(cwd: &Path) -> Option<GitInfo> {
    let base = ["status", "--porcelain=v2", "--branch"];
    let (out, listed) = match git(cwd, &[&base[..], &["-unormal"]].concat(), Duration::from_secs(5)) {
        Ok(out) => (out, true),
        Err(_) => (git(cwd, &[&base[..], &["-uno"]].concat(), Duration::from_secs(2)).ok()?, false),
    };
    let dirs = git(cwd, &["rev-parse", "--show-toplevel", "--absolute-git-dir"], Duration::from_secs(1)).ok()?;
    let mut lines = dirs.lines().map(PathBuf::from);
    let (root, git_dir) = (lines.next()?, lines.next()?);
    // A linked worktree's own git directory names the one the repository's worktrees share.
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(c) => git_dir.join(c.trim()).canonicalize().unwrap_or_else(|_| git_dir.clone()),
        Err(_) => git_dir.clone(),
    };
    let mut info = parse_status(&out)?;
    info.watch = watched_dirs(&git_dir, &common, &info.branch, upstream(&out));
    info.untracked |= !listed;
    info.real = real_path(cwd);
    info.main = main_of(&root, &common);
    // Recorded with the worktree, whatever its branch is now (by older kraals, with its branch).
    let kraal_branch = info.branch.starts_with("kraal/").then_some(info.branch.as_str());
    let linked = (git_dir != common).then_some(git_dir.as_path());
    info.worktree_index = worktree_index_of(&info.main, linked, kraal_branch);
    if let Some(branch) = kraal_branch {
        info.base = worktree_base(&info.main, branch).unwrap_or_default();
    }
    info.root = root;
    Some(info)
}

/// Where `branch` is pushed, as `git push` would push it (`branch.<name>.pushRemote`,
/// `remote.pushDefault`, `push.default`), or else to `origin` under its own name: that remote's
/// URL and the branch there. `None` until it has been pushed there (its remote-tracking branch
/// exists).
pub fn pushed_to(dir: &Path, branch: &str) -> Option<(String, String)> {
    let t = Duration::from_secs(2);
    let format = "--format=%(push:remotename)%00%(push)";
    let push = git(dir, &["for-each-ref", format, &format!("refs/heads/{branch}")], t).ok()?;
    let pushed = push.trim_end().split_once('\0').and_then(|(remote, tracking)| {
        if remote.is_empty() {
            return None;
        }
        // `push.default=simple` to another remote than the upstream's (`pushRemote`): git names no
        // remote-tracking branch, the push goes to the branch of the same name there.
        let tracking = if tracking.is_empty() { format!("refs/remotes/{remote}/{branch}") } else { tracking.into() };
        let there = tracking.strip_prefix(&format!("refs/remotes/{remote}/"))?.to_string();
        Some((remote.to_string(), there, tracking))
    });
    let (remote, there, tracking) =
        pushed.unwrap_or_else(|| ("origin".into(), branch.into(), format!("refs/remotes/origin/{branch}")));
    git(dir, &["rev-parse", "--verify", "--quiet", &tracking], t).ok()?;
    let url = git(dir, &["remote", "get-url", "--push", &remote], t).ok()?;
    Some((url.trim().to_string(), there))
}

/// `path` with its symbolic links resolved (itself when it cannot be).
pub fn real_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Whether `dir` is `root` or inside it, however each is spelled: git gives paths with their
/// symbolic links resolved (`/private/var/…` on macOS), a shell or the user may not.
pub fn is_within(dir: &Path, root: &Path) -> bool {
    dir.starts_with(root) || real_path(dir).starts_with(real_path(root))
}

/// The main checkout of the repository whose working tree is `top`, and whose git directory
/// shared by its worktrees is `common` (`<repo>/.git` for normal repositories).
fn main_of(top: &Path, common: &Path) -> PathBuf {
    match common.file_name() {
        Some(n) if n == ".git" => common.parent().map(Path::to_path_buf).unwrap_or_else(|| top.to_path_buf()),
        _ => top.to_path_buf(),
    }
}

/// The upstream branch `git status --porcelain=v2 --branch` names, e.g. `origin/main`.
fn upstream(status: &str) -> Option<&str> {
    status.lines().find_map(|l| l.strip_prefix("# branch.upstream "))
}

/// The directories git writes to when what `status` shows changes: the git directory (HEAD,
/// the index), the common one (packed refs), and those holding the branch's ref and its
/// upstream's, which are replaced, never written in place. Directories not there are left out:
/// creating one changes its parent, which is watched.
fn watched_dirs(git_dir: &Path, common: &Path, branch: &str, upstream: Option<&str>) -> Vec<PathBuf> {
    let mut dirs = vec![git_dir.to_path_buf()];
    let mut add = |d: PathBuf| {
        if !dirs.contains(&d) && d.is_dir() {
            dirs.push(d);
        }
    };
    add(common.to_path_buf());
    let heads = common.join("refs/heads");
    add(heads.clone());
    // `kraal/name` lives in `refs/heads/kraal`.
    if let Some(parent) = heads.join(branch).parent().filter(|p| p.starts_with(&heads)) {
        add(parent.to_path_buf());
    }
    if let Some(up) = upstream {
        let remotes = common.join("refs/remotes");
        if let Some(parent) = remotes.join(up).parent().filter(|p| p.starts_with(&remotes)) {
            add(parent.to_path_buf());
        }
    }
    dirs
}

/// Parse `git status --porcelain=v2 --branch` output.
pub fn parse_status(text: &str) -> Option<GitInfo> {
    let mut branch = None;
    let mut oid = None;
    let (mut ahead, mut behind, mut dirty, mut untracked) = (0, 0, false, false);
    for line in text.lines() {
        if let Some(h) = line.strip_prefix("# branch.head ") {
            if h != "(detached)" {
                branch = Some(h.to_string());
            }
        } else if let Some(o) = line.strip_prefix("# branch.oid ") {
            oid = Some(o.to_string());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    behind = n.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") || line.starts_with("u ") {
            dirty = true;
        } else if line.starts_with("? ") {
            untracked = true;
        }
    }
    let head = oid?;
    let detached = branch.is_none();
    let branch = branch.unwrap_or_else(|| head.chars().take(7).collect());
    Some(GitInfo {
        root: PathBuf::new(),
        branch,
        detached,
        head,
        upstream: upstream(text).map(str::to_string),
        dirty,
        untracked,
        ahead,
        behind,
        worktree_index: 0,
        main: PathBuf::new(),
        base: String::new(),
        real: PathBuf::new(),
        watch: Vec::new(),
    })
}

/// Top of the repository containing `cwd`. For a linked worktree, the main checkout.
pub fn main_checkout(cwd: &Path) -> Option<PathBuf> {
    let common = git(cwd, &["rev-parse", "--path-format=absolute", "--git-common-dir"], Duration::from_secs(1)).ok()?;
    let common = PathBuf::from(common.trim());
    // `<repo>/.git` for normal repositories.
    if common.file_name().is_some_and(|n| n == ".git") {
        return common.parent().map(Path::to_path_buf);
    }
    git(cwd, &["rev-parse", "--show-toplevel"], Duration::from_secs(1)).ok().map(|s| PathBuf::from(s.trim()))
}

/// The path account rules should see for `cwd`: inside a linked worktree, the same place in the
/// main checkout, so rules written for a repository also apply to its worktrees. `cwd` itself
/// when git could not tell in time (it is asked again next time).
pub fn project_path(cwd: &Path) -> PathBuf {
    if let Some(hit) = known_project_path(cwd) {
        return hit;
    }
    let Some(found) = find_project_path(cwd) else { return cwd.to_path_buf() };
    if let Ok(mut c) = PROJECT_PATHS.lock() {
        c.get_or_insert_with(Default::default).insert(cwd.to_path_buf(), found.clone());
    }
    found
}

/// Answers of [`project_path`], remembered (worktrees do not move).
static PROJECT_PATHS: std::sync::Mutex<Option<ProjectPaths>> = std::sync::Mutex::new(None);

/// The answers of [`project_path`] kept: the latest ones, for the directories of the panes of a
/// server that runs for weeks, not every directory a shell ever went to. One forgotten is asked
/// again (the git thread asks about the directories of panes as they come up).
const PROJECT_PATHS_KEPT: usize = 1024;

#[derive(Default)]
struct ProjectPaths {
    map: std::collections::HashMap<PathBuf, PathBuf>,
    /// The directories in `map`, oldest first.
    order: std::collections::VecDeque<PathBuf>,
}

impl ProjectPaths {
    fn get(&self, cwd: &Path) -> Option<PathBuf> {
        self.map.get(cwd).cloned()
    }

    fn insert(&mut self, cwd: PathBuf, found: PathBuf) {
        if self.map.insert(cwd.clone(), found).is_some() {
            return;
        }
        self.order.push_back(cwd);
        while self.order.len() > PROJECT_PATHS_KEPT {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
    }
}

/// [`project_path`] without running git, for the server's event loop: what git said before
/// (the git thread asks about the directories of panes). `None` when git was not asked yet, or
/// could not tell: a worktree may live anywhere, whatever its directory's name says, so ask
/// [`project_path`] off the loop first.
pub fn known_project_path(cwd: &Path) -> Option<PathBuf> {
    PROJECT_PATHS.lock().ok().and_then(|c| c.as_ref().and_then(|m| m.get(cwd)))
}

/// Where `cwd` is in its main checkout: `cwd` itself outside a repository or in the main
/// checkout. `None` when this could not be told for sure now (git took too long, or failed and
/// the repository's own files could not be read): such an answer is never remembered, and no
/// account is picked on it.
fn find_project_path(cwd: &Path) -> Option<PathBuf> {
    let mut cmd = git_cmd(cwd);
    cmd.args(["rev-parse", "--path-format=absolute", "--git-common-dir", "--show-toplevel"]).env("LC_ALL", "C");
    let out = match crate::exec::run(&mut cmd, Duration::from_secs(5), 64 << 10) {
        Ok(out) => out,
        // No git here: the repository's own files tell.
        Err(_) => return project_path_from_files(cwd).ok(),
    };
    if out.timed_out {
        return None;
    }
    let text = out.stdout_text();
    let mut lines = text.lines();
    let (Some(common), Some(top), true) = (lines.next(), lines.next(), out.success()) else {
        // Whatever git says when it fails, the files tell: "not a git repository" is also what
        // it says in a linked worktree whose main checkout is gone, which is still a worktree.
        return project_path_from_files(cwd).ok();
    };
    let (common, top) = (PathBuf::from(common), PathBuf::from(top));
    Some(project_path_in(cwd, &top, &main_of(&top, &common)))
}

/// `cwd`, in worktree `top`, as the same place in main checkout `main`.
fn project_path_in(cwd: &Path, top: &Path, main: &Path) -> PathBuf {
    if top == main {
        return cwd.to_path_buf();
    }
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    match canonical.strip_prefix(top) {
        Ok(rest) => main.join(rest),
        Err(_) => main.to_path_buf(),
    }
}

/// [`find_project_path`] from the repository's own files, as git finds them: the nearest
/// `.git` above `cwd`; a linked worktree's is a file naming its git directory, whose `commondir`
/// leads to the main repository's. An error when one of them cannot be read.
fn project_path_from_files(cwd: &Path) -> std::io::Result<PathBuf> {
    let canonical = cwd.canonicalize()?;
    let mut dir = canonical.as_path();
    loop {
        let dot = dir.join(".git");
        match std::fs::symlink_metadata(&dot) {
            // The main checkout of a repository.
            Ok(m) if m.is_dir() => return Ok(cwd.to_path_buf()),
            Ok(_) => {
                let text = std::fs::read_to_string(&dot)?;
                let Some(gitdir) = text.lines().next().and_then(|l| l.strip_prefix("gitdir:")) else {
                    return Err(std::io::Error::other(format!("{}: not a git link", dot.display())));
                };
                let gitdir = dir.join(gitdir.trim());
                // The git directory it names must be there: a worktree whose main checkout went
                // is not "no worktree".
                if !std::fs::metadata(&gitdir)?.is_dir() {
                    return Err(std::io::Error::other(format!("{}: not a directory", gitdir.display())));
                }
                let common = match std::fs::read_to_string(gitdir.join("commondir")) {
                    Ok(c) => gitdir.join(c.trim()).canonicalize()?,
                    // A submodule: a repository of its own, not a worktree.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(cwd.to_path_buf()),
                    Err(e) => return Err(e),
                };
                return Ok(project_path_in(cwd, dir, &main_of(dir, &common)));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match dir.parent() {
                Some(up) => dir = up,
                // Not in a repository.
                None => return Ok(cwd.to_path_buf()),
            },
            Err(e) => return Err(e),
        }
    }
}

/// A name usable in a branch and a directory.
pub fn valid_worktree_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 60
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with(['.', '-'])
}

/// Where kraal puts the worktrees of `repo`: a sibling directory `<repo>.worktrees/`.
pub fn worktrees_dir(repo: &Path) -> PathBuf {
    let name = repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    repo.with_file_name(format!("{name}.worktrees"))
}

/// The main checkout of the repository containing `cwd`, and what a new worktree made there
/// starts from: `base`, by default the current branch. That is the one checked out where the
/// user is, which may be another worktree than the main checkout; a detached HEAD is its commit.
pub fn start_point(cwd: &Path, base: Option<&str>) -> Result<(PathBuf, String)> {
    let repo = main_checkout(cwd).context("not inside a git repository")?;
    let base = match base {
        Some(b) => b.to_string(),
        None => match git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"], Duration::from_secs(2))?.trim() {
            "HEAD" => git(cwd, &["rev-parse", "HEAD"], Duration::from_secs(2))?.trim().to_string(),
            branch => branch.to_string(),
        },
    };
    Ok((repo, base))
}

/// Create a worktree on a new branch `kraal/<name>` from `base` (default: the current branch).
pub fn add_worktree(cwd: &Path, name: &str, base: Option<&str>) -> Result<Worktree> {
    if !valid_worktree_name(name) {
        bail!("invalid worktree name {name:?} (letters, digits, - _ .)");
    }
    let (repo, base) = start_point(cwd, base)?;
    let branch = format!("kraal/{name}");
    let path = worktrees_dir(&repo).join(name);
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let path_s = path.display().to_string();
    let index = changing_worktrees(&repo, |common| {
        git(&repo, &["worktree", "add", "-b", &branch, &path_s, &base], Duration::from_secs(60))?;
        record_worktree(&repo, common, &path, &branch, &base)
    })?;
    Ok(Worktree { repo, path, branch, base, index })
}

/// How long a change to a repository's worktrees waits for another one to be done: as long as
/// one may take.
const WORKTREES_WAIT: Duration = Duration::from_secs(60);

/// Run `change`, which adds or removes a worktree of `repo`, while no other kraal changes that
/// repository's worktrees, in this process or in another (`kraal wt` beside a server, two
/// servers). Git does not keep them apart itself: a `git worktree add` reads the entries of the
/// other worktrees, fails on one that another is writing at that moment ("failed to read
/// .git/worktrees/<name>/commondir"), and a worktree's number is the smallest that none of the
/// others holds. `change` is given the repository's common git directory.
fn changing_worktrees<T>(repo: &Path, change: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    let common = git(repo, &["rev-parse", "--path-format=absolute", "--git-common-dir"], Duration::from_secs(2))?;
    let common = PathBuf::from(common.trim());
    crate::paths::locked_within(&common.join("kraal-worktrees"), WORKTREES_WAIT, || change(&common))
        .with_context(|| format!("waiting to change the worktrees of {}", repo.display()))?
}

/// What a kraal worktree's number is recorded in: a file of git's own directory for that
/// worktree, which goes when the worktree is removed (or pruned), whatever its branch.
const INDEX_FILE: &str = "kraal-index";

/// Remember with the branch of a new worktree at `path` its base (for reviews and merges in
/// later sessions), and with the worktree its number: the smallest no other registered worktree
/// of `repo` holds, whatever it has checked out since (another branch, a detached HEAD). Whoever
/// calls this is alone in changing the repository's worktrees ([`changing_worktrees`]).
fn record_worktree(repo: &Path, common: &Path, path: &Path, branch: &str, base: &str) -> Result<u32> {
    let own = git(path, &["rev-parse", "--absolute-git-dir"], Duration::from_secs(2))?;
    let own = PathBuf::from(own.trim());
    set_config(repo, &[&format!("branch.{branch}.kraalbase"), base])?;
    let used = used_indices(repo, common)?;
    let index = (1..).find(|n| !used.contains(n)).unwrap_or(1);
    std::fs::write(own.join(INDEX_FILE), format!("{index}\n")).context("recording the worktree's number")?;
    // Where older kraals look for it.
    set_config(repo, &[&format!("branch.{branch}.kraalindex"), &index.to_string()])?;
    Ok(index)
}

/// The numbers the registered worktrees of `repo` hold: those recorded with them, and for
/// worktrees made by an older kraal, those recorded with their branch.
fn used_indices(repo: &Path, common: &Path) -> Result<std::collections::HashSet<u32>> {
    let mut used = std::collections::HashSet::new();
    let admin = admin_dirs(common);
    for dir in admin.values() {
        if let Some(n) = read_index(dir) {
            used.insert(n);
        }
    }
    for w in list_worktrees(repo)? {
        used.insert(w.index);
    }
    Ok(used)
}

/// Git's own directory for each registered linked worktree of a repository (`common`), by the
/// worktree's path.
fn admin_dirs(common: &Path) -> std::collections::HashMap<PathBuf, PathBuf> {
    let mut found = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir(common.join("worktrees")) else { return found };
    for e in entries.flatten() {
        let Ok(gitdir) = std::fs::read_to_string(e.path().join("gitdir")) else { continue };
        // `<worktree>/.git`
        if let Some(path) = Path::new(gitdir.trim()).parent() {
            found.insert(path.to_path_buf(), e.path());
        }
    }
    found
}

fn read_index(admin: &Path) -> Option<u32> {
    std::fs::read_to_string(admin.join(INDEX_FILE)).ok()?.trim().parse().ok().filter(|n| *n > 0)
}

/// The number of a kraal worktree: recorded in its own git directory `admin` (a linked
/// worktree), else, for one made by an older kraal, with its `branch`. 0 when it has none.
fn worktree_index_of(repo: &Path, admin: Option<&Path>, branch: Option<&str>) -> u32 {
    admin.and_then(read_index).or_else(|| branch.and_then(|b| worktree_index(repo, b))).unwrap_or(0)
}

/// Change the configuration of `repo` with `git config <args>`. A git writing it at the same
/// moment (another worktree being made) holds its lock for an instant: then try again.
fn set_config(repo: &Path, args: &[&str]) -> Result<()> {
    let mut tries = 0;
    loop {
        match git(repo, &[&["config"], args].concat(), Duration::from_secs(5)) {
            Ok(_) => return Ok(()),
            Err(e) if tries < 40 && format!("{e:#}").contains("could not lock config file") => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
}

/// The number of the kraal worktree on `branch`, as recorded when it was created.
pub fn worktree_index(repo: &Path, branch: &str) -> Option<u32> {
    let out = git(repo, &["config", "--get", &format!("branch.{branch}.kraalindex")], Duration::from_secs(2)).ok()?;
    out.trim().parse().ok().filter(|n| *n > 0)
}

/// The content of `path` in commit `rev` of `repo`, when it is a regular file there; `None`
/// when nothing is at that path.
pub fn file_at(repo: &Path, rev: &str, path: &str, cap: usize) -> Result<Option<String>> {
    let entry = git(repo, &["ls-tree", "-z", rev, "--", path], Duration::from_secs(5))?;
    // `<mode> <type> <id>\t<path>`
    let Some((meta, _)) = entry.split('\0').next().and_then(|e| e.split_once('\t')) else { return Ok(None) };
    let fields: Vec<&str> = meta.split(' ').collect();
    let [mode, "blob", id] = fields[..] else { bail!("{path} in {rev} is not a file") };
    if mode != "100644" && mode != "100755" {
        bail!("{path} in {rev} is not a regular file");
    }
    let content = git(repo, &["cat-file", "blob", id], Duration::from_secs(5))?;
    if content.len() > cap {
        bail!("{path} in {rev} is larger than {} KiB", cap >> 10);
    }
    Ok(Some(content))
}

/// The branch a kraal worktree started from, as recorded when it was created.
pub fn worktree_base(repo: &Path, branch: &str) -> Option<String> {
    let out = git(repo, &["config", "--get", &format!("branch.{branch}.kraalbase")], Duration::from_secs(2)).ok()?;
    let base = out.trim().to_string();
    (!base.is_empty()).then_some(base)
}

/// What removing a worktree would lose, and what a removal checks has not moved.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorktreeState {
    /// Uncommitted changes, untracked files included.
    pub changes: usize,
    /// Commits the history of its base does not have: on its branch, and on a detached HEAD in
    /// it.
    pub unmerged: usize,
    /// Those commits change nothing their base does not have already: the branch was merged by
    /// squash (or rebased, or cherry-picked) into it.
    pub applied: bool,
    /// Ignored files and directories (`.gitignore`) that the main checkout does not have, the
    /// same, at the same place: build output, a local database, a changed `.env`. Removing the
    /// worktree deletes them.
    pub ignored: Vec<String>,
    /// Which work that is: the commits at stake, what is staged, the content of each changed
    /// file, the ignored files. A new edit to an already changed file, a change staged, or a
    /// rewritten commit changes it even when the counts stay the same.
    pub identity: u64,
    /// The commit its branch was at: a removal deletes the branch only there.
    pub tip: Option<String>,
    /// Its base, as a ref name and the commit it was at (none for a base given as a commit).
    pub base: Option<(String, String)>,
}

impl WorktreeState {
    /// Removing it loses no change and no commit (ignored files aside).
    pub fn is_clean(&self) -> bool {
        self.changes == 0 && (self.unmerged == 0 || self.applied)
    }

    /// Removing it loses nothing at all.
    pub fn loses_nothing(&self) -> bool {
        self.is_clean() && self.ignored.is_empty()
    }

    /// The same work as `other` (whatever its base did meanwhile).
    pub fn same_work(&self, other: &WorktreeState) -> bool {
        (self.changes, self.unmerged, self.applied, &self.ignored, self.identity)
            == (other.changes, other.unmerged, other.applied, &other.ignored, other.identity)
    }

    /// `3 uncommitted changes and 2 unmerged commits`.
    pub fn describe(&self) -> String {
        let n = |count: usize, what: &str| format!("{count} {what}{}", if count == 1 { "" } else { "s" });
        let mut parts = Vec::new();
        if self.changes > 0 {
            parts.push(n(self.changes, "uncommitted change"));
        }
        if self.unmerged > 0 && !self.applied {
            parts.push(n(self.unmerged, "unmerged commit"));
        }
        if !self.ignored.is_empty() {
            parts.push(format!("ignored files not in the main checkout ({})", self.ignored_names()));
        }
        match parts.split_last() {
            None => "nothing to lose".into(),
            Some((last, [])) => last.clone(),
            Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        }
    }

    /// The first of its ignored files, and how many others.
    pub fn ignored_names(&self) -> String {
        let mut names: Vec<String> = self.ignored.iter().take(3).cloned().collect();
        if self.ignored.len() > 3 {
            names.push(format!("{} more", self.ignored.len() - 3));
        }
        names.join(", ")
    }
}

pub fn worktree_state(w: &Worktree) -> Result<WorktreeState> {
    use std::hash::{Hash, Hasher};
    let mut identity = std::collections::hash_map::DefaultHasher::new();
    let status = git(&w.path, &["status", "--porcelain", "-z", "--untracked-files=all"], Duration::from_secs(10))?;
    let mut changes = 0;
    let mut fields = status.split('\0').filter(|f| !f.is_empty());
    while let Some(field) = fields.next() {
        changes += 1;
        // `XY path`; a rename or copy is followed by its original path.
        if matches!(field.as_bytes().first(), Some(b'R' | b'C')) {
            fields.next();
        }
        let path = field.get(3..).unwrap_or_default();
        path.hash(&mut identity);
        hash_content(&w.path.join(path), &mut identity);
    }
    // What is staged, by blob id (a change staged over an unchanged file shows nowhere else).
    git(&w.path, &["diff", "--cached", "--raw", "-z", "--no-abbrev", "--no-renames"], Duration::from_secs(10))?
        .hash(&mut identity);
    let ignored = ignored_files(w, &mut identity)?;
    let base = if w.base.is_empty() {
        worktree_base(&w.repo, &w.branch)
            .or_else(|| git(&w.repo, &["rev-parse", "--abbrev-ref", "HEAD"], Duration::from_secs(2)).ok())
            .map(|b| b.trim().to_string())
            .context("no base branch")?
    } else {
        w.base.clone()
    };
    // The base as it is now: what the commits are compared with.
    let base_oid =
        git(&w.repo, &["rev-parse", "--verify", "-q", &format!("{base}^{{commit}}")], Duration::from_secs(5))
            .with_context(|| format!("its base {base} is not there"))?
            .trim()
            .to_string();
    let base_ref = git(&w.repo, &["rev-parse", "--symbolic-full-name", &base], Duration::from_secs(5))
        .ok()
        .map(|r| r.trim().to_string())
        .filter(|r| r.starts_with("refs/"));
    // Commits only a detached HEAD holds are lost with the worktree too.
    let branch_ref = format!("refs/heads/{}", w.branch);
    let tip = git(&w.repo, &["rev-parse", "--verify", "-q", &branch_ref], Duration::from_secs(5))
        .ok()
        .map(|t| t.trim().to_string());
    let mut tips: Vec<String> = tip.iter().cloned().collect();
    if git(&w.path, &["symbolic-ref", "-q", "HEAD"], Duration::from_secs(5)).is_err() {
        tips.push(git(&w.path, &["rev-parse", "HEAD"], Duration::from_secs(5))?.trim().to_string());
    }
    tips.hash(&mut identity);
    let unmerged = if tips.is_empty() {
        0
    } else {
        let mut args = vec!["rev-list", "--count"];
        args.extend(tips.iter().map(String::as_str));
        args.extend(["--not", base_oid.as_str()]);
        let count = git(&w.repo, &args, Duration::from_secs(10))?;
        count.trim().parse().with_context(|| format!("git rev-list --count said {:?}, not a count", count.trim()))?
    };
    // Commits its history lacks may still have all their changes in the base: a squash merge.
    let applied = unmerged > 0 && tips.iter().all(|t| changes_in(&w.repo, t, &base_oid));
    Ok(WorktreeState {
        changes,
        unmerged,
        applied,
        ignored,
        identity: identity.finish(),
        tip,
        base: base_ref.map(|r| (r, base_oid)),
    })
}

/// Whether every change commit `tip` brings is in commit `base` already, though its history may
/// not be there (a squash merge, a rebase, a cherry-pick): merging it into `base` would change
/// nothing. A change the base made again differently, or undid since, is not in it.
fn changes_in(repo: &Path, tip: &str, base: &str) -> bool {
    if git(repo, &["merge-base", "--is-ancestor", tip, base], Duration::from_secs(10)).is_ok() {
        return true;
    }
    // A conflict ends this with an error: not in it.
    let Ok(merged) = git(repo, &["merge-tree", "--write-tree", "--no-messages", base, tip], Duration::from_secs(30))
    else {
        return false;
    };
    let Ok(tree) = git(repo, &["rev-parse", "--verify", "-q", &format!("{base}^{{tree}}")], Duration::from_secs(5))
    else {
        return false;
    };
    merged.lines().next().is_some_and(|m| !m.trim().is_empty() && m.trim() == tree.trim())
}

/// The ignored files and directories of worktree `w` that its main checkout does not have, the
/// same, at the same place (a file copied there when the worktree was made is no loss). A
/// directory counts whole, its content unread.
fn ignored_files(w: &Worktree, identity: &mut impl std::hash::Hasher) -> Result<Vec<String>> {
    use std::hash::Hash;
    let args = ["ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--directory"];
    let out = git(&w.path, &args, Duration::from_secs(10))?;
    let mut found = Vec::new();
    for entry in out.split('\0').filter(|e| !e.is_empty()) {
        let at = w.path.join(entry);
        if !entry.ends_with('/') && same_file(&at, &w.repo.join(entry)) {
            continue;
        }
        entry.hash(identity);
        if let Ok(m) = at.symlink_metadata() {
            (m.len(), m.modified().ok()).hash(identity);
        }
        found.push(entry.to_string());
    }
    Ok(found)
}

/// Whether `a` and `b` are regular files with the same content (up to 64 MiB), or links to the
/// same place.
fn same_file(a: &Path, b: &Path) -> bool {
    use std::io::Read;
    let (Ok(ma), Ok(mb)) = (a.symlink_metadata(), b.symlink_metadata()) else { return false };
    if ma.file_type().is_symlink() || mb.file_type().is_symlink() {
        return ma.file_type().is_symlink()
            && mb.file_type().is_symlink()
            && std::fs::read_link(a).ok() == std::fs::read_link(b).ok();
    }
    if !ma.is_file() || !mb.is_file() || ma.len() != mb.len() || ma.len() > 64 << 20 {
        return false;
    }
    let (Ok(mut fa), Ok(mut fb)) = (std::fs::File::open(a), std::fs::File::open(b)) else { return false };
    let (mut ba, mut bb) = (vec![0u8; 64 << 10], vec![0u8; 64 << 10]);
    loop {
        let n = match fa.read(&mut ba) {
            Ok(0) => return fb.read(&mut bb).is_ok_and(|m| m == 0),
            Ok(n) => n,
            Err(_) => return false,
        };
        if fb.read_exact(&mut bb[..n]).is_err() || ba[..n] != bb[..n] {
            return false;
        }
    }
}

/// Feed what `path` holds into `h`: a file's bytes, a link's target, or that it is missing.
fn hash_content(path: &Path, h: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    use std::io::Read;
    match path.symlink_metadata() {
        Ok(m) if m.file_type().is_symlink() => std::fs::read_link(path).ok().hash(h),
        Ok(m) if m.is_file() => {
            let Ok(mut f) = std::fs::File::open(path) else { return "unreadable".hash(h) };
            let mut buf = [0u8; 64 << 10];
            loop {
                match f.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => h.write(&buf[..n]),
                }
            }
        }
        Ok(_) => "other".hash(h),
        Err(_) => "missing".hash(h),
    }
}

/// What a removal may discard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discard {
    /// Nothing: a worktree with anything to lose stays.
    Nothing,
    /// Its ignored files (build output, local databases), but no change and no commit.
    Ignored,
    /// Everything (forced).
    Everything,
}

/// Remove a worktree and its branch, unless that would lose more than `discard` allows.
pub fn remove_worktree(w: &Worktree, discard: Discard) -> Result<()> {
    if discard == Discard::Everything {
        let state = worktree_state(w).ok();
        let path = w.path.display().to_string();
        changing_worktrees(&w.repo, |_| {
            git(&w.repo, &["worktree", "remove", "--force", &path], Duration::from_secs(30))
        })?;
        return match state.as_ref().and_then(|s| s.tip.as_deref()) {
            Some(tip) => delete_branch(w, tip, None),
            None => git(&w.repo, &["branch", "-D", &w.branch], Duration::from_secs(10)).map(|_| ()),
        };
    }
    let state = worktree_state(w)?;
    if !state.is_clean() {
        bail!("{} has {}; remove it with force to discard them", w.name(), state.describe());
    }
    if discard == Discard::Nothing && !state.ignored.is_empty() {
        bail!("{} has {}: removing it deletes them", w.name(), state.describe());
    }
    remove_checked(w, &state)
}

/// Remove worktree `w` as `state` found it (its losses, if any, accepted): its directory, then
/// its branch, only at the commit that was checked and, when the removal rests on its work being
/// in its base, only while the base is still where it was. Git's own check of the directory
/// (no change, nothing untracked) applies unless changes were accepted.
pub fn remove_checked(w: &Worktree, state: &WorktreeState) -> Result<()> {
    let path = w.path.display().to_string();
    let mut args = vec!["worktree", "remove", path.as_str()];
    if state.changes > 0 {
        args.push("--force");
    }
    changing_worktrees(&w.repo, |_| git(&w.repo, &args, Duration::from_secs(30)))?;
    match &state.tip {
        Some(tip) => delete_branch(w, tip, state.base.as_ref().filter(|_| state.is_clean())),
        None => Ok(()),
    }
}

/// Delete the branch of `w` if it is still at `tip`, and its `base` still at its commit: one
/// transaction, so that a commit made meanwhile is never deleted.
fn delete_branch(w: &Worktree, tip: &str, base: Option<&(String, String)>) -> Result<()> {
    let mut input = String::new();
    if let Some((name, oid)) = base {
        input.push_str(&format!("verify {name} {oid}\n"));
    }
    input.push_str(&format!("delete refs/heads/{} {tip}\n", w.branch));
    git_with_input(&w.repo, &["update-ref", "--stdin"], &input, Duration::from_secs(10))
        .with_context(|| format!("the branch {} was kept: it, or its base, moved since it was checked", w.branch))?;
    // What deleting a branch with git also removes: its settings (kraal's base and number).
    let _ = set_config(&w.repo, &["--remove-section", &format!("branch.{}", w.branch)]);
    Ok(())
}

/// [`git`] with `input` as its standard input.
fn git_with_input(dir: &Path, args: &[&str], input: &str, timeout: Duration) -> Result<String> {
    use std::io::Write;
    let mut cmd = git_cmd(dir);
    cmd.args(args);
    let (reader, mut writer) = std::io::pipe().context("making a pipe")?;
    // Small enough for the pipe: written before git starts.
    writer.write_all(input.as_bytes()).context("writing to git")?;
    drop(writer);
    let out = crate::exec::run_with_input(&mut cmd, reader.into(), timeout, 1 << 20).context("running git")?;
    if out.timed_out {
        bail!("git {} took too long", args.join(" "));
    }
    if !out.success() {
        bail!("git {}: {}", args.join(" "), out.stderr_text().trim());
    }
    Ok(out.stdout_text())
}

/// The kraal worktrees of the repository containing `cwd`, each with what removing it would
/// lose, or why that could not be told. One that would lose nothing is merged into its base and
/// clean: removing it is safe.
pub fn worktree_states(cwd: &Path) -> Result<Vec<(Worktree, Result<WorktreeState, String>)>> {
    let list = list_worktrees(cwd)?;
    Ok(list
        .into_iter()
        .map(|w| {
            let state = worktree_state(&w).map_err(|e| format!("{e:#}"));
            (w, state)
        })
        .collect())
}

/// The kraal worktrees of the repository containing `cwd`.
pub fn list_worktrees(cwd: &Path) -> Result<Vec<Worktree>> {
    let repo = main_checkout(cwd).context("not inside a git repository")?;
    let common = git(&repo, &["rev-parse", "--path-format=absolute", "--git-common-dir"], Duration::from_secs(2))?;
    let admin = admin_dirs(Path::new(common.trim()));
    let out = git(&repo, &["worktree", "list", "--porcelain"], Duration::from_secs(5))?;
    let mut list = Vec::new();
    let mut path: Option<PathBuf> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch refs/heads/")
            && b.starts_with("kraal/")
            && let Some(p) = path.take()
        {
            let base = worktree_base(&repo, b).unwrap_or_default();
            let index = worktree_index_of(&repo, admin.get(&p).map(PathBuf::as_path), Some(b));
            list.push(Worktree { repo: repo.clone(), path: p, branch: b.to_string(), base, index });
        }
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_v2() {
        let s = "# branch.oid 3f58e39c80027dcb\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n1 .M N... 100644 100644 100644 a b f\n? new.txt\n";
        let i = parse_status(s).unwrap();
        assert_eq!((i.branch.as_str(), i.dirty, i.untracked, i.ahead, i.behind), ("main", true, true, 2, 1));
        assert_eq!(i.label(false), "⎇ main*? ↑2 ↓1");
        assert_eq!(i.label(true), "git:main*? +2 -1");
        let d = parse_status("# branch.oid 3f58e39c80027dcb\n# branch.head (detached)\n").unwrap();
        assert_eq!((d.branch.as_str(), d.dirty), ("3f58e39", false));
        assert!(parse_status("").is_none());
    }

    #[test]
    fn only_the_latest_project_paths_are_remembered() {
        let mut paths = ProjectPaths::default();
        let dir = |i: usize| PathBuf::from(format!("/x/{i}"));
        // A shell that went through many directories over the weeks.
        for i in 0..PROJECT_PATHS_KEPT + 10 {
            paths.insert(dir(i), dir(i));
        }
        assert_eq!((paths.map.len(), paths.order.len()), (PROJECT_PATHS_KEPT, PROJECT_PATHS_KEPT));
        assert_eq!(paths.get(&dir(9)), None, "the oldest went");
        assert_eq!(paths.get(&dir(10)), Some(dir(10)));
        assert_eq!(paths.get(&dir(PROJECT_PATHS_KEPT + 9)), Some(dir(PROJECT_PATHS_KEPT + 9)));
        // Told again, a directory is counted once.
        paths.insert(dir(10), dir(10));
        assert_eq!((paths.map.len(), paths.order.len()), (PROJECT_PATHS_KEPT, PROJECT_PATHS_KEPT));
    }

    #[test]
    fn status_names_the_directories_git_writes_to() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kraal-gitdirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(ok.status.success(), "{args:?}: {}", String::from_utf8_lossy(&ok.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "one"]);
        let common = repo.join(".git");
        let info = status(&repo).unwrap();
        assert_eq!(info.watch, vec![common.clone(), common.join("refs/heads")]);
        // A linked worktree on `kraal/x`, tracking `origin/x`: its own git directory, the common
        // one, and those of both refs.
        let wt = dir.join("wt");
        git(&["worktree", "add", "-q", "-b", "kraal/x", wt.to_str().unwrap()]);
        git(&["update-ref", "refs/remotes/origin/x", "HEAD"]);
        git(&["config", "remote.origin.url", "/nowhere"]);
        git(&["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"]);
        git(&["config", "branch.kraal/x.remote", "origin"]);
        git(&["config", "branch.kraal/x.merge", "refs/heads/x"]);
        let info = status(&wt).unwrap();
        assert_eq!(
            info.watch,
            vec![
                common.join("worktrees/wt"),
                common.clone(),
                common.join("refs/heads"),
                common.join("refs/heads/kraal"),
                common.join("refs/remotes/origin"),
            ]
        );
        assert!(status(&dir).is_none(), "not a repository");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_worktree_is_found_even_when_git_cannot_say() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kraal-gitfiles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(ok.status.success(), "{args:?}: {}", String::from_utf8_lossy(&ok.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "one"]);
        let wt = dir.join("far/feature");
        git(&["worktree", "add", "-q", "-b", "feature", wt.to_str().unwrap()]);
        std::fs::create_dir_all(wt.join("src")).unwrap();
        let repo = repo.canonicalize().unwrap();
        // From the files alone: the worktree's place in the main checkout.
        assert_eq!(project_path_from_files(&wt.join("src")).unwrap(), repo.join("src"));
        assert_eq!(project_path_from_files(&repo).unwrap(), repo);
        assert_eq!(project_path_from_files(&dir).unwrap(), dir, "not in a repository");
        // git fails on a broken configuration: that is no answer, the files still tell.
        let config = repo.join(".git/config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str("\n[[broken\n");
        std::fs::write(&config, text).unwrap();
        assert_eq!(find_project_path(&wt.join("src")), Some(repo.join("src")));
        // A worktree link that cannot be read is no answer either: nothing is remembered.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(wt.join(".git"), std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_to_string(wt.join(".git")).is_err() {
            assert_eq!(find_project_path(&wt.join("src")), None);
        }
        std::fs::set_permissions(wt.join(".git"), std::fs::Permissions::from_mode(0o644)).unwrap();
        // The main checkout gone, git says "not a git repository": still a worktree, of a
        // repository that cannot be told now. No answer, and nothing remembered.
        std::fs::rename(&repo, dir.join("moved")).unwrap();
        assert_eq!(find_project_path(&wt.join("src")), None);
        assert!(project_path_from_files(&wt.join("src")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worktrees_get_numbers_and_merged_ones_are_told_apart() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kraal-gitnum-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = dir.join("app");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |dir: &Path, args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(ok.status.success(), "{args:?}: {}", String::from_utf8_lossy(&ok.stderr));
        };
        run(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "1").unwrap();
        run(&repo, &["add", "a.txt"]);
        run(&repo, &["commit", "-q", "-m", "one"]);
        // Made at the same time, each gets its own number.
        let made: Vec<Worktree> = std::thread::scope(|s| {
            let jobs: Vec<_> =
                ["a", "b", "c"].iter().map(|n| s.spawn(|| add_worktree(&repo, n, None).unwrap())).collect();
            jobs.into_iter().map(|j| j.join().unwrap()).collect()
        });
        let mut numbers: Vec<u32> = made.iter().map(|w| w.index).collect();
        numbers.sort();
        assert_eq!(numbers, vec![1, 2, 3]);
        let by_name = |n: &str| made.iter().find(|w| w.name() == n).unwrap().clone();
        let (a, b, c) = (by_name("a"), by_name("b"), by_name("c"));
        assert_eq!(status(&b.path).unwrap().worktree_index, b.index, "git tells a pane's directory");
        assert_eq!(status(&repo).unwrap().worktree_index, 0, "the main checkout has none");
        let listed = list_worktrees(&repo).unwrap();
        assert!(listed.iter().all(|w| made.contains(w)), "{listed:?}");
        // A number is free again once its worktree is gone.
        remove_worktree(&a, Discard::Nothing).unwrap();
        assert_eq!(add_worktree(&repo, "d", None).unwrap().index, a.index);
        // b has a commit main does not have, c a change: neither is merged and clean.
        std::fs::write(b.path.join("b.txt"), "b").unwrap();
        run(&b.path, &["add", "b.txt"]);
        run(&b.path, &["commit", "-q", "-m", "b"]);
        std::fs::write(c.path.join("a.txt"), "changed").unwrap();
        let clean = |name: &str| {
            let states = worktree_states(&repo).unwrap();
            let (_, state) = states.into_iter().find(|(w, _)| w.name() == name).unwrap();
            state.unwrap().is_clean()
        };
        assert!(!clean("b") && !clean("c"));
        assert!(clean("d"), "nothing done there: nothing to lose");
        run(&repo, &["merge", "-q", "--no-ff", "-m", "merge b", "kraal/b"]);
        assert!(clean("b"), "merged into its base");
        assert!(!clean("c"));
        // A repository file is read as committed, and only when it is a regular file.
        std::fs::create_dir_all(repo.join(".kraal")).unwrap();
        std::fs::write(repo.join(".kraal/worktree.toml"), "setup = []\n").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", repo.join(".kraal/link.toml")).unwrap();
        run(&repo, &["add", ".kraal"]);
        run(&repo, &["commit", "-q", "-m", "kraal"]);
        assert_eq!(file_at(&repo, "main", ".kraal/worktree.toml", 1024).unwrap().as_deref(), Some("setup = []\n"));
        assert_eq!(file_at(&repo, "main", ".kraal/none.toml", 1024).unwrap(), None);
        assert_eq!(file_at(&repo, "main~1", ".kraal/worktree.toml", 1024).unwrap(), None, "not in that commit");
        assert!(file_at(&repo, "main", ".kraal/link.toml", 1024).is_err(), "a link is not followed");
        assert!(file_at(&repo, "main", ".kraal", 1024).is_err(), "a directory is no file");
        assert!(file_at(&repo, "main", ".kraal/worktree.toml", 4).is_err(), "too large");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worktree_lifecycle() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kraal-git-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let repo = dir.join("app");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(ok.status.success(), "{args:?}: {}", String::from_utf8_lossy(&ok.stderr));
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "1").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "one"]);
        let info = status(&repo).unwrap();
        assert_eq!((info.branch.as_str(), info.dirty), ("main", false));
        let w = add_worktree(&repo, "fix-login", None).unwrap();
        assert_eq!(w.branch, "kraal/fix-login");
        assert_eq!(w.base, "main");
        assert_eq!(w.path, dir.join("app.worktrees").join("fix-login"));
        assert_eq!(main_checkout(&w.path).unwrap(), std::fs::canonicalize(&repo).unwrap());
        std::fs::create_dir_all(w.path.join("src")).unwrap();
        let canonical_repo = std::fs::canonicalize(&repo).unwrap();
        assert_eq!(project_path(&w.path.join("src")), canonical_repo.join("src"), "rules see the repository");
        assert_eq!(project_path(&repo), repo, "a main checkout is itself");
        assert_eq!(known_project_path(&w.path.join("src")), Some(canonical_repo.join("src")), "remembered");
        // A directory's name says nothing: a worktree of `app` named like another repository's.
        let decoy = dir.join("other.worktrees/fix");
        run(&["worktree", "add", "-q", "-b", "decoy", decoy.to_str().unwrap()]);
        assert_eq!(known_project_path(&decoy), None, "only git can tell");
        assert_eq!(project_path(&decoy), canonical_repo, "its repository, not `other`");
        assert_eq!(known_project_path(&decoy), Some(canonical_repo.clone()), "remembered");
        assert_eq!(known_project_path(&dir.join("anywhere")), None, "only git can tell");
        assert_eq!(project_path(&dir.join("anywhere")), dir.join("anywhere"), "outside any repository");
        let wi = status(&w.path).unwrap();
        assert_eq!(wi.branch, "kraal/fix-login");
        // Made from inside a worktree, a new one starts from that worktree's branch.
        let f = add_worktree(&w.path, "followup", None).unwrap();
        assert_eq!(f.base, "kraal/fix-login");
        remove_worktree(&f, Discard::Everything).unwrap();
        let in_tree = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&w.path)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(ok.status.success(), "{args:?}");
        };
        std::fs::write(w.path.join("b.txt"), "b").unwrap();
        assert!(status(&w.path).unwrap().untracked);
        in_tree(&["add", "b.txt"]);
        in_tree(&["commit", "-q", "-m", "two"]);
        let counts = |w: &Worktree| {
            let s = worktree_state(w).unwrap();
            (s.changes, s.unmerged)
        };
        assert_eq!(counts(&w), (0, 1));
        std::fs::write(w.path.join("a.txt"), "2").unwrap();
        assert!(status(&w.path).unwrap().dirty);
        let listed = list_worktrees(&repo).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].base, "main", "the base is remembered");
        assert!(add_worktree(&repo, "fix-login", None).is_err(), "names are unique");
        assert!(add_worktree(&repo, "../escape", None).is_err());
        assert_eq!(counts(&w), (1, 1));
        // More work in the same file: the same counts, but not the same work.
        let before = worktree_state(&w).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(w.path.join("a.txt"), "22").unwrap();
        assert_eq!(counts(&w), (1, 1));
        assert_ne!(worktree_state(&w).unwrap(), before, "a new edit changes the identity");
        // The same size and the same time, but other content.
        let before = worktree_state(&w).unwrap();
        let file = w.path.join("a.txt");
        let mtime = file.metadata().unwrap().modified().unwrap();
        std::fs::write(&file, "33").unwrap();
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        assert_ne!(worktree_state(&w).unwrap(), before, "content counts, not size and time");
        // A change staged over the same file: the counts do not move either.
        let before = worktree_state(&w).unwrap();
        in_tree(&["add", "a.txt"]);
        assert_eq!(counts(&w), (1, 1));
        assert_ne!(worktree_state(&w).unwrap(), before, "what is staged counts");
        // A commit made on a detached HEAD would be lost with the worktree.
        in_tree(&["stash", "-q"]);
        in_tree(&["checkout", "-q", "--detach"]);
        std::fs::write(w.path.join("c.txt"), "c").unwrap();
        in_tree(&["add", "c.txt"]);
        in_tree(&["commit", "-q", "-m", "detached"]);
        assert_eq!(counts(&w), (0, 2), "the branch's commit and the detached one");
        in_tree(&["checkout", "-q", "kraal/fix-login"]);
        in_tree(&["stash", "pop", "-q"]);
        let refused = remove_worktree(&w, Discard::Nothing).unwrap_err().to_string();
        assert!(refused.contains("1 uncommitted change and 1 unmerged commit"), "{refused}");
        assert!(w.path.exists(), "nothing removed without force");
        remove_worktree(&w, Discard::Everything).unwrap();
        assert!(!w.path.exists());
        assert!(list_worktrees(&repo).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A repository with one commit in a new directory, for a test; `None` without git.
    fn test_repo(name: &str) -> Option<(PathBuf, PathBuf)> {
        Command::new("git").arg("--version").output().ok()?;
        let dir = std::env::temp_dir().join(format!("kraal-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("app")).unwrap();
        let dir = dir.canonicalize().unwrap();
        let repo = dir.join("app");
        std::fs::write(repo.join(".gitignore"), ".env\n*.db\nnode_modules/\n").unwrap();
        std::fs::write(repo.join("a.txt"), "1\n2\n3\n").unwrap();
        run_git(&repo, &["init", "-q", "-b", "main"]);
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-q", "-m", "one"]);
        Some((dir, repo))
    }

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn commit_file(dir: &Path, file: &str, text: &str) {
        std::fs::write(dir.join(file), text).unwrap();
        run_git(dir, &["add", file]);
        run_git(dir, &["commit", "-q", "-m", file]);
    }

    /// A git changing the configuration at the same moment holds its lock for an instant: kraal
    /// writes once it is free, and tells git's refusal when it stays taken.
    #[test]
    fn the_configuration_is_written_once_its_lock_is_free() {
        let Some((dir, repo)) = test_repo("gitlock") else { return };
        // Without the wait of its own that a recent git has.
        run_git(&repo, &["config", "core.configLockTimeout", "0"]);
        let lock = repo.join(".git/config.lock");
        std::fs::write(&lock, "").unwrap();
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                std::fs::remove_file(&lock).unwrap();
            });
            set_config(&repo, &["kraal.test", "1"]).unwrap();
        });
        assert_eq!(run_git(&repo, &["config", "kraal.test"]), "1\n");
        std::fs::write(&lock, "").unwrap();
        let err = format!("{:#}", set_config(&repo, &["kraal.test", "2"]).unwrap_err());
        assert!(err.contains("could not lock config file"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Made at the same moment and removed at the same moment by several threads, as the
    /// server's picker and its agents do, the worktrees of a repository do not fail on each
    /// other in git, and each has a number of its own: round after round.
    #[test]
    fn worktrees_made_and_removed_at_once_each_have_their_number() {
        let Some((dir, repo)) = test_repo("gitmany") else { return };
        for round in 0..4 {
            let names: Vec<String> = (0..5).map(|n| format!("r{round}-{n}")).collect();
            let made: Vec<Worktree> = std::thread::scope(|s| {
                let adds: Vec<_> = names.iter().map(|n| s.spawn(|| add_worktree(&repo, n, None).unwrap())).collect();
                adds.into_iter().map(|add| add.join().unwrap()).collect()
            });
            let mut numbers: Vec<u32> = made.iter().map(|w| w.index).collect();
            numbers.sort();
            assert_eq!(numbers, [1, 2, 3, 4, 5], "round {round}");
            std::thread::scope(|s| {
                for w in &made {
                    s.spawn(|| remove_worktree(w, Discard::Nothing).unwrap());
                }
            });
            assert_eq!(list_worktrees(&repo).unwrap(), [], "round {round}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A directory reached through a symbolic link is inside the worktree git names with its
    /// links resolved (as `/var` is `/private/var` on macOS), and git says where it is.
    #[test]
    fn a_directory_through_a_link_is_in_its_worktree() {
        let Some((dir, repo)) = test_repo("gitlink") else { return };
        let w = add_worktree(&repo, "x", None).unwrap();
        std::fs::create_dir_all(w.path.join("src")).unwrap();
        std::os::unix::fs::symlink(&dir, dir.join("link")).unwrap();
        let through = dir.join("link/app.worktrees/x/src");
        assert!(!through.starts_with(&w.path), "spelled otherwise");
        assert!(is_within(&through, &w.path) && is_within(&w.path.join("src"), &dir.join("link/app.worktrees/x")));
        assert!(!is_within(&dir.join("link/app"), &w.path));
        let info = status(&through).unwrap();
        assert_eq!((info.real, info.root, info.worktree_index), (w.path.join("src"), w.path.clone(), 1));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A worktree keeps its number while it exists, whatever it has checked out: one made after
    /// it never gets the same.
    #[test]
    fn a_number_stays_with_its_worktree_whatever_it_checks_out() {
        let Some((dir, repo)) = test_repo("gitkeep") else { return };
        let a = add_worktree(&repo, "a", None).unwrap();
        assert_eq!(a.index, 1);
        run_git(&a.path, &["checkout", "-q", "--detach"]);
        assert_eq!(status(&a.path).unwrap().worktree_index, 1, "detached, it is still number 1");
        let b = add_worktree(&repo, "b", None).unwrap();
        assert_eq!(b.index, 2, "1 is still taken");
        // Back on its branch, or on another one: the same number, and no one else's.
        run_git(&a.path, &["checkout", "-q", "kraal/a"]);
        assert_eq!(status(&a.path).unwrap().worktree_index, 1);
        run_git(&a.path, &["checkout", "-q", "-b", "other"]);
        assert_eq!(status(&a.path).unwrap().worktree_index, 1);
        assert_eq!(add_worktree(&repo, "c", None).unwrap().index, 3);
        // Free again once the worktree is gone.
        run_git(&repo, &["worktree", "remove", a.path.to_str().unwrap()]);
        assert_eq!(add_worktree(&repo, "d", None).unwrap().index, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Ignored files are work too: an ignored `.env` changed in the worktree, or a local
    /// database, keep it, unless their loss is accepted. One the main checkout has, the same, is
    /// no loss.
    #[test]
    fn ignored_files_of_its_own_keep_a_worktree() {
        let Some((dir, repo)) = test_repo("gitignored") else { return };
        std::fs::write(repo.join(".env"), "A=1\n").unwrap();
        let w = add_worktree(&repo, "fix", None).unwrap();
        std::fs::write(w.path.join(".env"), "A=1\n").unwrap();
        let state = worktree_state(&w).unwrap();
        assert!(state.loses_nothing(), "a copy of the main checkout's .env is no loss: {state:?}");
        std::fs::write(w.path.join(".env"), "A=2\n").unwrap();
        std::fs::write(w.path.join("dev.db"), "rows").unwrap();
        std::fs::create_dir_all(w.path.join("node_modules/x")).unwrap();
        std::fs::write(w.path.join("node_modules/x/index.js"), "x").unwrap();
        let state = worktree_state(&w).unwrap();
        assert!(state.is_clean() && !state.loses_nothing(), "{state:?}");
        assert_eq!(state.ignored, vec![".env", "dev.db", "node_modules/"]);
        assert!(state.describe().contains("ignored files not in the main checkout (.env, dev.db, node_modules/)"));
        let refused = remove_worktree(&w, Discard::Nothing).unwrap_err().to_string();
        assert!(refused.contains("ignored files"), "{refused}");
        assert_eq!(std::fs::read_to_string(w.path.join(".env")).unwrap(), "A=2\n", "nothing removed");
        // A change to an ignored file changes what was confirmed.
        std::fs::write(w.path.join("dev.db"), "more rows").unwrap();
        assert!(!worktree_state(&w).unwrap().same_work(&state));
        remove_worktree(&w, Discard::Ignored).unwrap();
        assert!(!w.path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The branch goes only at the commit that was checked: a commit made after the check (in
    /// another terminal) keeps it.
    #[test]
    fn a_commit_made_after_the_check_keeps_its_branch() {
        let Some((dir, repo)) = test_repo("gitrace") else { return };
        let w = add_worktree(&repo, "fix", None).unwrap();
        let checked = worktree_state(&w).unwrap();
        assert!(checked.loses_nothing());
        commit_file(&w.path, "late.txt", "late\n");
        let err = remove_checked(&w, &checked).unwrap_err().to_string();
        assert!(err.contains("was kept"), "{err}");
        let tip = run_git(&repo, &["rev-parse", "kraal/fix"]);
        assert!(run_git(&repo, &["log", "--format=%s", tip.trim()]).contains("late.txt"), "the new commit is there");
        // Its base moved back since the check: the branch is not deleted on an old answer.
        let w2 = add_worktree(&repo, "two", None).unwrap();
        commit_file(&w2.path, "b.txt", "b\n");
        run_git(&repo, &["merge", "-q", "--ff-only", "kraal/two"]);
        let checked = worktree_state(&w2).unwrap();
        assert!(checked.loses_nothing(), "{checked:?}");
        run_git(&repo, &["reset", "-q", "--hard", "HEAD~1"]);
        assert!(remove_checked(&w2, &checked).is_err());
        assert!(!run_git(&repo, &["branch", "--list", "kraal/two"]).is_empty(), "its commit is not lost");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A branch merged by squash (or rebased) into its base has commits the base's history does
    /// not have, but its changes are all there: it is merged. One with a change the base does not
    /// have (part of it picked, or undone since) is not.
    #[test]
    fn a_branch_squashed_into_its_base_is_merged() {
        let Some((dir, repo)) = test_repo("gitsquash") else { return };
        let w = add_worktree(&repo, "sq", None).unwrap();
        commit_file(&w.path, "a.txt", "1\n2 changed\n3\n");
        commit_file(&w.path, "new.txt", "new\n");
        assert!(!worktree_state(&w).unwrap().is_clean());
        run_git(&repo, &["merge", "-q", "--squash", "kraal/sq"]);
        run_git(&repo, &["commit", "-q", "-m", "sq (#1)"]);
        // The base goes on meanwhile.
        commit_file(&repo, "later.txt", "later\n");
        let state = worktree_state(&w).unwrap();
        assert_eq!(state.unmerged, 2);
        assert!(state.applied && state.is_clean() && state.loses_nothing(), "{state:?}");
        assert_eq!(state.describe(), "nothing to lose");

        // Only part of it in the base: not merged.
        let p = add_worktree(&repo, "part", None).unwrap();
        commit_file(&p.path, "p1.txt", "1\n");
        commit_file(&p.path, "p2.txt", "2\n");
        let first = run_git(&p.path, &["rev-parse", "HEAD~1"]);
        run_git(&repo, &["cherry-pick", first.trim()]);
        let state = worktree_state(&p).unwrap();
        assert!(!state.applied && !state.is_clean(), "{state:?}");
        assert!(remove_worktree(&p, Discard::Nothing).is_err());
        // Squashed, then undone in the base: its work is no longer there.
        let u = add_worktree(&repo, "undone", None).unwrap();
        commit_file(&u.path, "u.txt", "u\n");
        run_git(&repo, &["merge", "-q", "--squash", "kraal/undone"]);
        run_git(&repo, &["commit", "-q", "-m", "undone (#2)"]);
        run_git(&repo, &["revert", "--no-edit", "HEAD"]);
        assert!(!worktree_state(&u).unwrap().is_clean());

        remove_worktree(&w, Discard::Nothing).unwrap();
        assert!(!w.path.exists());
        assert!(run_git(&repo, &["branch", "--list", "kraal/sq"]).is_empty(), "its branch is gone");
        let settings =
            Command::new("git").arg("-C").arg(&repo).args(["config", "--get-regexp", "^branch\\.kraal/sq\\."]).output();
        assert!(settings.unwrap().stdout.is_empty(), "and its settings");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_branch_is_followed_where_git_push_puts_it() {
        let dir = std::env::temp_dir().join(format!("kraal-pushed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = dir.canonicalize().unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(ok.status.success(), "{args:?}: {}", String::from_utf8_lossy(&ok.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "one"]);
        git(&["config", "push.default", "simple"]);
        for (remote, url) in [("origin", "git@github.com:o/r.git"), ("fork", "git@github.com:me/r.git")] {
            git(&["config", &format!("remote.{remote}.url"), url]);
        }
        for branch in ["fix", "solo", "local"] {
            git(&["branch", branch]);
        }
        assert_eq!(pushed_to(&repo, "local"), None, "never pushed");
        // Pushed without -u: origin's branch of the same name.
        git(&["update-ref", "refs/remotes/origin/solo", "HEAD"]);
        assert_eq!(pushed_to(&repo, "solo"), Some(("git@github.com:o/r.git".into(), "solo".into())));
        // Fetched from a mirror, pushed to GitHub: where it is pushed.
        git(&["config", "remote.origin.pushurl", "git@github.com:o/pushed.git"]);
        assert_eq!(pushed_to(&repo, "solo"), Some(("git@github.com:o/pushed.git".into(), "solo".into())));
        git(&["config", "--unset", "remote.origin.pushurl"]);
        // Following origin/fix, pushed to the fork: the fork's fix, once it is there.
        git(&["update-ref", "refs/remotes/origin/fix", "HEAD"]);
        git(&["config", "branch.fix.remote", "origin"]);
        git(&["config", "branch.fix.merge", "refs/heads/fix"]);
        git(&["config", "branch.fix.pushRemote", "fork"]);
        assert_eq!(pushed_to(&repo, "fix"), None, "not on the fork yet");
        git(&["update-ref", "refs/remotes/fork/fix", "HEAD"]);
        assert_eq!(pushed_to(&repo, "fix"), Some(("git@github.com:me/r.git".into(), "fix".into())));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
