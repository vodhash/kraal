# Coding agents

kraal follows recognized agents in panes. It manages named accounts, usage and account changes
for Claude Code and Codex. Other agents use their own sign-in and account settings.

## Launch, add, change and return

| Task | Path and result |
|---|---|
| Start | `ctrl+b a`, choose an installed agent (or a Claude or Codex account), Enter. Tab also offers **Start in new tab** and, in a local repository, **Start in worktree**. |
| Add an account | `ctrl+b A l` opens **Accounts**. Choose **+ Add account**, select **Agent**, enter **Name** and submit. Complete the agent's sign-in; kraal verifies it before confirming success. `ctrl+b A n` opens the same form directly. |
| Use an existing sign-in | Open Accounts. A verified standard home is adopted with a name; an uncertain match asks **Which account is ~/.claude** (or ~/.codex). Choose which account it is without logging in again. |
| Change this pane's account | `ctrl+b A c`, or click its account name. Check **Continue conversation** or **New conversation**, then choose the destination. Sign-in is checked before anything stops. |
| Return | Click **Undo** after a change, or use `ctrl+b p`, select the pane, Tab, **Undo account change**. The previous account is checked again. Undo does not revert project files. |

Search filters these lists. **+ Add account** stays in Accounts, separate from launching.
If sign-in fails, Enter closes its pane; **Try again** (`r`) explicitly starts another login.
**Check again** (`c`) verifies the existing sign-in without a new login. An account awaiting
verification stays marked **Sign-in required**. See [Accounts](#accounts) for recovery and review.

| Capability | Claude Code | Codex | Other recognized agents |
|---|---|---|---|
| Launch from Start an agent | Yes | Yes | Yes, when installed on PATH |
| Named accounts and reported usage | Yes | Yes | Managed by the agent |
| Change account | Continue a readable, compatible transcript | Start a new conversation | Not supported by kraal |
| Undo account change | Resume the latest conversation on the previous account | Reopen the original account's saved conversation | Not supported by kraal |
| Resume after Close or restart | Supported launch and known conversation | Supported launch and known conversation | No kraal conversation resume |

A requested change can wait for a reliable end-of-turn hook; otherwise it asks before stopping
the agent. Missing sign-in or an unverifiable conversation leaves the current pane unchanged.
The command `kraal agent codex --account work` starts a new agent. There is currently no CLI
command to change the account of an existing pane; use **Change account** in kraal.

## Registered capabilities

These are kraal's validated capabilities, not a claim about everything the agent itself can do.
Launch commands are tried in the order shown; executable aliases produce one picker entry.
Native sign-in, checks and isolation below mean kraal can manage named accounts through the
agent's own commands. Conversation identification means kraal can identify a conversation for
its supported resume flow. Reliable turn end requires the installed integration to cover the
whole lifecycle; partial hooks and screen heuristics do not qualify for deferred switching.

| Agent | Launch on PATH | Native sign-in | Reliable check | Account isolation | Conversation identification | Cross-account resume | Reliable turn end |
|---|---|---|---|---|---|---|---|
| `claude` | `claude` | Yes | Yes | Yes | Yes | Yes | Yes |
| `codex` | `codex` | Yes | Yes | Yes | Yes | No | No |
| `opencode` | `opencode`, `opencode2` | No | No | No | No | No | Yes |
| `gemini` | `gemini` | No | No | No | No | No | No |
| `aider` | `aider` | No | No | No | No | No | No |
| `cursor` | `cursor-agent` | No | No | No | No | No | No |
| `amp` | `amp` | No | No | No | No | No | No |
| `pi` | `pi` | No | No | No | No | No | Yes |
| `grok` | `grok` | No | No | No | No | No | No |
| `copilot` | `copilot` | No | No | No | No | No | Yes |
| `goose` | `goose` | No | No | No | No | No | No |
| `crush` | `crush` | No | No | No | No | No | No |
| `qwen` | `qwen` | No | No | No | No | No | Yes |
| `kimi` | `kimi` | No | No | No | No | No | Yes |
| `droid` | `droid` | No | No | No | No | No | Yes |
| `auggie` | `auggie` | No | No | No | No | No | No |
| `kiro` | `kiro-cli`, `q chat` | No | No | No | No | No | No |
| `kilo` | `kilo`, `kilocode` | No | No | No | No | No | Yes |
| `cline` | `cline` | No | No | No | No | No | No |
| `codebuff` | `codebuff`, `freebuff` | No | No | No | No | No | No |
| `qoder` | `qoder`, `qodercli` | No | No | No | No | No | No |
| `codebuddy` | `codebuddy` | No | No | No | No | No | Yes |
| `junie` | `junie` | No | No | No | No | No | No |
| `llxprt` | `llxprt` | No | No | No | No | No | No |
| `kode` | `kode` | No | No | No | No | No | No |
| `openhands` | `openhands` | No | No | No | No | No | No |
| `vibe` | `vibe-rs`, `vibe` | No | No | No | No | No | No |
| `continue` | `cn` | No | No | No | No | No | No |
| `letta` | `letta` | No | No | No | No | No | No |
| `rovodev` | `acli rovodev run` | No | No | No | No | No | No |

Only Claude and Codex have account adapters. Other agents keep their own account settings;
kraal supplies no account-specific environment overrides or conversation resume arguments for them.
Continue, Letta and Python Vibe are detected through their runtime scripts; broad command names
alone do not identify a running agent. Cursor's generic `agent` process also needs its bundled
script path for detection; the launch picker looks for `cursor-agent`.

`kraal agent gemini` also launches the registered command with its native account. The same
applies to `kraal wt new fix --agent gemini`. An explicit `--account` for an agent without
account support is refused with **Accounts are managed in <agent>**.

## States

kraal recognises 30 coding agents by their process, run directly or through Node or Python as
npm and pip install them: Claude Code (`claude`), Codex (`codex`), OpenCode (`opencode`), Gemini
CLI (`gemini`), Qwen Code (`qwen`), GitHub Copilot CLI (`copilot`), Cursor CLI (`cursor-agent`
or `agent`), Factory's droid (`droid`), Augment (`auggie`), Kiro CLI (`kiro-cli`, `q chat`),
Kimi Code (`kimi`), Kilo (`kilo`), Cline (`cline`), Continue (`cn`), Amp (`amp`), Aider
(`aider`), Goose (`goose`), Crush (`crush`), pi (`pi`), Grok (`grok`), Codebuff (`codebuff`),
Qoder (`qoder`), CodeBuddy (`codebuddy`), Junie (`junie`), LLxprt Code (`llxprt`), Kode
(`kode`), OpenHands (`openhands`), Mistral Vibe (`vibe`), Letta Code (`letta`) and Rovo Dev
(`acli rovodev run`). It infers their state from the screen: the interrupt hint an agent shows
while it works, the permission prompt or question it shows while it waits for you.

For exact states, install kraal's hooks in the agent. For Claude Code, this adds hooks to
`~/.claude/settings.json` and keeps a backup:

```sh
kraal integration install claude
```

| Command | |
|---|---|
| `kraal integration install gemini` | the same for another agent of the table below |
| `kraal integration install all` | Claude Code, Codex, and every agent below used here |
| `kraal integration status` | says what is installed, for the agents and for [your shells](terminal.md#shell-commands) |
| `kraal integration uninstall claude` | takes kraal's entries out again |

| Agent | `install …` | What kraal changes |
|---|---|---|
| Claude Code | `claude` | hooks and status line in `~/.claude/settings.json` |
| Gemini CLI | `gemini` | hooks in `~/.gemini/settings.json` |
| Qwen Code | `qwen` | hooks in `~/.qwen/settings.json` |
| GitHub Copilot CLI | `copilot` | a file of its own, `~/.copilot/hooks/kraal.json` |
| OpenCode 1 | `opencode` | a plugin of its own, `~/.config/opencode/plugins/kraal.js` |
| Kilo | `kilo` | a plugin of its own, `~/.config/kilo/plugin/kraal.js` |
| droid | `droid` | `~/.factory/hooks.json` (the hooks of `settings.json` when they are kept there) |
| CodeBuddy | `codebuddy` | hooks in `~/.codebuddy/settings.json` |
| Kimi Code | `kimi` | `[[hooks]]` tables in `~/.kimi-code/config.toml` |
| pi (0.84.4 or later) | `pi` | an extension of its own, `~/.pi/agent/extensions/kraal.ts` |
| Codex | `codex` | `notify` in `~/.codex/config.toml`: the end of each turn |
| Cursor CLI | `cursor` | `~/.cursor/hooks.json`: the start and end of each turn |
| Augment | `auggie` | hooks in `~/.augment/settings.json` that run a script of kraal's own, `~/.augment/kraal-hook.sh`: tool calls and the end of each turn |

Codex's, Cursor's and Augment's hooks say nothing when the agent asks you something, and Gemini
CLI's nothing when you decline a tool or cancel one: that still comes from the screen. The other
agents have no hook that tells their state; theirs always comes from the screen. A turn that
finished stays done until you look at the pane, even when the agent then says it waits for a
prompt. A file kraal edits is copied next to it first (`.kraal-bak`), only kraal's entries are
added, and `kraal integration uninstall <agent>` takes out those alone. A file of kraal's own is
written and removed whole; one of the same name that kraal cannot read, or that is not kraal's,
is left alone, and one you edited is copied next to it before it changes or goes. Agents read
their hooks when a session starts, and Gemini CLI runs them only in folders you trust. `all`
covers Claude Code and Codex, and each other agent whose directory exists (`~/.gemini`,
`~/.copilot`, …).

The homes of your other Claude and Codex [accounts](#accounts) get them too: a Claude home
without `settings.json` is linked to the hooked one (a `settings.json` of its own is left alone,
and said), and each Codex home's `config.toml` gets the same `notify`, which `uninstall` takes out
again. `status` says **partially installed** while one of them lacks the hooks.

The hooks do nothing outside kraal. Some agents run another agent's hooks (Cursor and Continue
run Claude Code's): kraal ignores a hook for an agent other than the one running in its pane,
and one from an agent run that one started (`claude -p` from its Bash tool, which inherits the
pane and its hooks): it neither replaces the conversation nor ends the session. An
agent whose hooks tell every state (those of the table but Gemini CLI, Codex, Cursor CLI and
Augment) is followed even under a wrapper kraal does not recognise (`bash -c 'opencode; …'`):
until its hooks say the session ended, or, for OpenCode and Kilo, whose plugins cannot say it,
until the process the plugin runs in ends.
States are shown as coloured dots, with the state written
next to them in the sidebar and in pane frames: yellow working, red blocked (waiting for you), green
done (not seen yet), `○` idle. A blocked or finished agent you are not
looking at triggers a desktop notification through your terminal (OSC 777/9) or `notify-send`.

In the four-column sidebar strip, **AG** lists local agents then remote agents with the same
state icons and colours, including the ASCII and high-contrast themes. Click an agent to reveal
its pane, or **AG** to find it by name. Overflow keeps blocked then done agents visible and
provides a list row. Showing a done icon does not acknowledge the result; visiting the pane
uses the usual acknowledgement. Accounts stay in `ctrl+b A l` and the full sidebar.

The agents of kraal sessions on your servers are listed too, after the server's name, and
`alt+a` goes to them as well: see [Agents on your servers](servers.md#agents-on-your-servers).
`kraal agents` lists them with their server, and their changes of state are `agent.*` events with
a `server` field, for scripts and hooks: see [Scripting](scripting.md).

## Resume limits

Close and a server restart stop the processes, including any turn in progress. Kraal can request
that Claude Code or Codex reopen a known conversation (`claude --resume <id>`, `codex resume <id>`)
with supported launch options. The history must still exist in the agent's home, and the agent
must be able to sign in. Kraal's saved scrollback is not a substitute for that history.
A pane already stopped stays stopped. Without a known conversation, a managed launch may run
its original command; it cannot promise to recover the previous conversation.

For a supported resume, kraal removes a recognized command-line prompt so it is not sent again;
the pane shows the resume command and waits for Enter. A one-shot run (`claude -p`, `codex exec`) is not
resumed: it waits for Enter as any other command does. When kraal cannot tell an option's value
from a prompt (an option it does not know), the pane also shows the command and waits: Enter runs
it as shown. Enter on an ended agent uses the same argument checks. Account changes apply
additional transcript and sign-in checks before stopping anything.

A recognized Claude or Codex run inside a shell can receive a basic resume command after
restoration; kraal does not reconstruct every option or environment variable typed into that
shell. Other agents have no conversation resume in kraal. A shell comes back as a new shell;
a command pane waits for Enter. Duplicate removes managed conversation selections and keeps
commands waiting for Enter. Import also keeps commands waiting, but uses the arguments written
in the file: review any explicit resume arguments before launching. Neither operation copies
the agent's conversation history.

## Claude Code agent teams

`kraal agent claude --teams` starts Claude Code with its
[agent teams](https://code.claude.com/docs/en/agent-teams) on. Each teammate Claude starts opens
as a kraal pane next to it, named after the teammate, and kraal follows it like any other agent:
its state in the sidebar and on its pane, `alt+a`, notifications, `kraal agents`. Teammates run
on the lead's account. A teammate that fails keeps its pane open, with its error. When the lead
exits, Claude closes its teammates' panes.

For this, Claude runs as if inside tmux: kraal sets `TMUX` and puts itself first in Claude's
`PATH` under the name `tmux`, and passes `--teammate-mode auto` unless you give your own
`--teammate-mode`. Claude's calls to split, name, start and close its teammates' panes go to
kraal, which refuses any other call made to its pretend tmux and acts only on the panes of the
team. Any other tmux command, from Claude or the tools it runs, goes to the real tmux when one
is installed.

- `--teams` works from a kraal pane, for Claude Code only, and opens the lead in a new pane.
- Claude Code does not resume teammates: after a restart, a teammate's pane waits for Enter.

## From your phone

Claude Code's [Remote Control](https://code.claude.com/docs/en/remote-control) puts a Claude
session on claude.ai/code and in the Claude app: you follow it, answer its questions and approve
its permission prompts from your phone. kraal keeps its agents running when you close the
terminal or detach, so they stay there for you.

- `kraal agent claude --remote`, run in a kraal pane, starts Claude on Remote Control in a new
  pane. kraal keeps it on when it resumes the agent after a restart or moves it to another
  account. Run outside kraal, it starts Claude in that terminal, which must stay open.
- To put every Claude session on Remote Control, including the agents you start from kraal's
  menu or a workspace file, turn on **Enable Remote Control for all sessions** in Claude's
  `/config`.
- To be told on your phone when Claude needs you, turn on **Push when actions required** in
  Claude's `/config`, and sign in to the Claude app with the same account.

Remote Control needs a Claude subscription sign-in (Pro, Max, Team or Enterprise), not an API
key, and covers Claude Code only. kraal's own questions, such as a production approval, are
answered in kraal.

## Worktrees

Panes in a git repository show their branch in their frame (`⎇ main*? ↑2`: `*` changes, `?`
untracked files, commits ahead).

In the agent picker (`ctrl+b a`), select an agent or account and press Tab, then **Start in worktree**: kraal creates a branch
`kraal/<name>` and a directory next to the repository (`<repo>.worktrees/<name>`), and starts the
agent there in its own tab. Several agents can work on the same repository without stepping on each
other. `ctrl+b R g` in that tab reviews the changes made since it left its base (your pager, delta
included), commits them, opens a pull request (`gh`), merges into the base branch (only a local
branch: not a base given as a commit or a remote branch) or removes the worktree. Removing says
first what would be lost (uncommitted changes, commits the base does not have, ignored files of
its own) and which panes it closes. Account rules written for the repository also apply in its
worktrees.

| Command | |
|---|---|
| `kraal wt new fix-login --agent claude` | the same from the command line (without `--agent`, the worktree alone); its last line is the id of the agent's pane |
| `kraal wt new fix-login --base main` | from that branch instead of the one checked out |
| `kraal wt setup` | shows the repository's `.kraal/worktree.toml` as a new worktree would get it (`--base main`: as that branch has it) |
| `kraal wt trust` | approves that file as it is now, for new worktrees to be prepared from it (see Preparation below) |
| `kraal wt list` | this repository's worktrees, with each one's number |
| `kraal wt rm fix-login` | removes one with its branch; refuses if it would lose changes, unmerged commits or ignored files, or while panes work in it |
| `kraal wt rm fix-login --discard-ignored` | removes it with ignored files of its own |
| `kraal wt rm fix-login --force` | removes anyway, closing the panes that work in it and deleting its changes, unmerged commits and ignored files (stopped by those panes, `rm` says which it has) |
| `kraal wt prune` | removes those merged into their base and clean |
| `kraal wt prune --dry-run` | only says which ones would go |
| `kraal wt prune --discard-ignored` | also those whose only leftovers are ignored files |

A worktree's name takes letters, digits, `-`, `_` and `.`. The agent kraal starts in one is
Claude Code or Codex, the two it has accounts for; any other runs there once you start it.

**Preparation.** A new worktree has no `.env` and no `node_modules`. A repository says how to
prepare its worktrees in `.kraal/worktree.toml`, read from the commit the worktree starts from:

```toml worktree
copy = [".env*", "config/master.key"]   # from the main checkout, when they are there
setup = ["npm ci", "npm run db:migrate"]
```

`copy` takes paths in the repository, with `*` and `?` in the last part; a directory is copied
whole, without the links in it. A file already in the worktree stays as it is, and nothing is
copied through a link out of the repository, nor written through one. `setup` runs in the new
worktree, in the agent's pane, before the agent starts; the first command that fails stops the
setup, and the agent then waits for Enter. With `kraal wt new`, the setup runs in the terminal
you typed it in: a command that fails there fails `kraal wt new`, the worktree stays, and the
agent is not started.

This file comes with the repository, and someone else may have written it. Like a repository's
[runbooks](runbooks.md#repository-runbooks), kraal acts on it only once you have approved that
version of it: it copies none of the files it names and runs none of its commands before, and it
needs your approval again when the file changes. Approving is an act of its own:

- The agent picker shows what the file copies and every line of every command it runs, and asks
  whether to trust it, before the worktree is made. A long one
  [scrolls](keys.md#lists-fields-and-questions), and Cancel is the button selected.
- In a terminal, `kraal wt new fix-login` shows the same and asks, before anything is made.
- `kraal wt trust` approves the file as it is now, without a question: for scripts, and once you
  have read it (`kraal wt setup`).

Whatever kraal shows of the file has its control and invisible characters written out (`\u{1b}`,
`\r`), so what you approve is what runs. `--yes` asks nothing and approves nothing: with it, or
without a terminal, a file that is new or changed is refused, nothing is made, and the message
names the commands above. `--no-setup` makes the worktree without any preparation (the file is
not even read). Your own `copy` and `setup`, for every repository, go in `[worktrees]` in the
config; they need no approval, and run after the repository's.

To copy: a repository's [worktree.toml](../examples/repository/.kraal/worktree.toml), your own
[`[worktrees]`](../examples/config/worktrees.toml), and
[agent-in-worktree.sh](../examples/scripts/agent-in-worktree.sh), which starts an agent in a new
worktree and tells you when it needs you.

**Ports.** Each worktree has a number in its repository (1, 2…), kept with the worktree whatever
it checks out (another branch, a detached HEAD) and reused once the worktree is gone. Panes opened
in it get `$KRAAL_WORKTREE` (its name), `$KRAAL_WORKTREE_INDEX` and `$KRAAL_PORT_OFFSET` (the
number times `port_step`, 10 by default), and `$PORT` when `[worktrees]` sets `port_base`, even
when `kraal run` opens them with `--cwd` in a worktree the session has not seen yet, so the same
dev server can run in several worktrees at once:
`npm run dev -- --port $((5173 + KRAAL_PORT_OFFSET))`. The sidebar's PORTS section names the
worktree a port is served from.

**Cleanup.** `ctrl+b R g` › Remove merged worktrees, from any checkout of the repository, removes
in one go the worktrees whose work is all in their base, after saying which panes it closes and
why the others stay. `kraal wt prune` does the same, keeping the worktrees panes of the session
work in. A worktree is merged when all of these hold:

- it has no uncommitted change and no untracked file;
- each commit of its branch (and of a detached HEAD in it) is in the history of its base, or, for
  a branch merged by squash, rebased or cherry-picked, merging the branch into its base would
  change nothing: every change the branch brings is in the base, the same. A change the base made
  differently since, or undid, means the branch is not merged, and stays. Update the base first
  (`git pull` on it) for a pull request merged on the server to count. This takes git 2.38 or
  later; with an older git, only commits in the base's history count;
- it has no ignored file (`.gitignore`) of its own: build output, a local database, a `.env` you
  changed. One the main checkout has, the same, at the same place (a file `copy` put there) is
  no loss. Remove merged worktrees lists such files and deletes them once you agree; `kraal wt
  prune` keeps such a worktree unless given `--discard-ignored`.

The branch is then deleted only at the commit that was checked, and only while its base is
where it was: a commit made meanwhile (in another terminal) keeps the branch. Neither ever forces
a removal.

## Accounts

### Add and manage

Each account has a name and a sign-in home for an agent (`CLAUDE_CONFIG_DIR` for Claude Code,
`CODEX_HOME` for Codex). kraal never reads or stores
credentials; it chooses which directory a pane starts with and reads the usage the agents report.

Accounts are for Claude Code and Codex. A second Claude subscription, named `pro`, in a new
directory where kraal then runs `claude auth login`:

```sh
kraal accounts add claude pro
```

| Command | |
|---|---|
| `kraal accounts add codex work` | the same for Codex, with `codex login` |
| `kraal accounts add claude team --home ~/claude-team --no-login --threshold 80` | in a directory you choose (`~/.local/share/kraal/accounts/claude-team` otherwise), without logging in now, with a threshold of its own |
| `kraal accounts` | every account: sign-in status, 5-hour and weekly usage (`kraal accounts list --json` for scripts) |
| `kraal accounts review claude` | answers which account an existing sign-in belongs to, without another login |
| `kraal accounts login claude:pro` | logs an account in again |
| `kraal accounts rename claude:pro work` | renames it: rules and usage follow, the directory stays |
| `kraal accounts rm claude:work` | forgets it; its directory is kept, and adding the name again reuses its sign-in (`kraal accounts login` signs in again) |
| `kraal accounts limits --threshold 85 --mode ask` | sets the threshold and what happens there (below) |
| `kraal agent claude` | starts Claude Code on the account the rules pick, in a new pane whose id it prints (in this terminal outside kraal) |
| `kraal agent codex --account work --right` | on that account, in a split on the right (`--down`, `--floating` and `--tab` place it too; `--here` runs it in this terminal even inside kraal) |
| `kraal agent claude -- --model opus` | what follows `--` goes to the agent |
| `kraal agent claude --remote` | Claude Code on Remote Control, to answer it from your phone ([From your phone](#from-your-phone)) |
| `kraal agent claude --teams` | Claude Code's agent teams, each teammate in a kraal pane ([Claude Code agent teams](#claude-code-agent-teams)) |

When there are no named accounts for the agent, a verified sign-in in `~/.claude` or
`~/.codex` is adopted as **Personal**, or with a public identity returned by the agent, such
as Claude's email. You can rename it like any other account. No login is started and no
credentials are copied. An absent agent or an unsigned home adds no account.
**Could not verify** offers **Check again**, without starting a login; on the command line,
run `kraal accounts list` again.

Accounts opens immediately. Discovery checks Claude and Codex independently in the background.
When named accounts already exist, kraal also asks the agent for their public identity in each
account's own home and caches the results. If exactly one named account has the same reported
email and organization or plan fields, kraal joins the existing sign-in to it automatically.
An email can match when neither sign-in reports additional context. A different or missing
organization or plan does not match. Account names and usage percentages are never identity
proof. The saved answer produces one notice, for example: `~/.claude is Claude · patron.bzh`.

An unresolved sign-in never appears as an extra account. No match, several matches, or an
unreadable identity leave one dim question just above **+ Add account**:
**? Which account is ~/.claude** (or ~/.codex). A narrow sidebar shortens this to
**? Account for ~/.claude**. Its second line shows that home's measured usage, such as
**77% used**, or **No usage yet** before any measurement. Open the question in Accounts
(`ctrl+b A l`), click it in the sidebar, or run `kraal accounts review claude`.
The existing **Which account is this?**
question lists your named accounts and **A different account**. Choose one, then confirm
**Use account**. Cancel is selected first. Opening the question does not run another status
command or start a login. Reopening Accounts checks unsigned standard homes again, so a
sign-in completed outside kraal can be found.

Automatic joins and answers to the question have the same effect. Running agents keep their
process and home. New launches use the selected named account's home. Panes count once under
that account, and its latest usage observation is used without adding usage from two homes.
No home or credential is changed. `kraal accounts list` shows joined sign-in homes under their
account; `--json` includes them in that account's `joined_homes` array. Choosing
**A different account** asks for its name and keeps the existing home as a separate named
account. Command-line review keeps its proposed name. The answer is saved, so the question
does not return. Two names for the same canonical home are also joined automatically.

`kraal accounts add` also adopts an unlisted verified sign-in when there are no named accounts.
An explicit `--home` with a verified sign-in is adopted without login, even without `--no-login`.
Adding another name for a listed home resolves to that same account. Use Rename to change its
visible name. Old `default` references in commands, rules and saved sessions continue to resolve.

New Claude account homes link shared settings, skills, plugins and project conversations from
`~/.claude` when available; existing entries in a chosen home are left alone. A conversation can
continue on another account only after its transcript is checked. Codex homes share no live
files or databases; a new home receives a copy of `~/.codex/config.toml` when available.
An agent kraal starts on an account gets `$KRAAL_ACCOUNT` (`claude:pro`) and an internal
`$KRAAL_ACCOUNT_ID` that stays the same when renamed. It does not inherit the API keys of
your environment (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`…), which would bill another account.
Removing an account does not let a replacement with the same name or home take over its
panes. Refreshing a pane's account label keeps the home it launched with, including after
accounts are joined.
Agent panes kraal starts, including workspace agents and restored sessions, run without
`DISPLAY` and `WAYLAND_DISPLAY` so agents such as Codex use terminal clipboard copying.
Ctrl+V in an agent pane reads images or text on the attaching client, locally or over `--host`.
See [Clipboard images](terminal.md#clipboard-images). Shell panes keep their display environment.
Account sign-in panes also keep it because sign-in may open a browser.

Command-line logins and sign-in checks use the same account environment. Logins use your
terminal; checks run with no terminal input and keep the agent's output private.

Open **Accounts** from the Agents menu (`ctrl+b A l`), or click the sidebar's **ACCOUNTS**
heading. In the sidebar, accounts use the same two-line rows as workspaces: a bold
`Claude · work` label after `●` when panes use the account, or `○` when none do, then an
indented, dimmed `57% used · 2 panes`. Only the usage turns red at or above the account's
threshold. Zero panes are omitted. Before any measurement, the sidebar and the Accounts list say
**No usage yet**. A login still to verify, or an account whose last sign-in check found it
unsigned, says **Sign-in required** (in red in the sidebar). A check that found the agent
missing says **Not installed**. These statuses take precedence over saved usage. An
inconclusive check, or no check yet, leaves the usual usage text. Expired observations say
**Last checked ...**. Rows use the checks already collected by discovery at startup and
when opening Accounts. Discovery checks named homes when it needs them to identify an
existing sign-in. Signing in through kraal invalidates the old checks, verifies the login,
and refreshes the rows.
A narrow sidebar omits whole status parts that do not fit. Clicking either line opens details
with the usage windows, when they were checked and their resets. The **⋮** marker on a selected
or hovered account opens its actions, as does a right-click. Enter in the list opens details.
Tab opens **Rename**, **Sign in**, and **Remove** (`ctrl+r`, `ctrl+l`, `ctrl+x`). Remove asks first, with Cancel selected, and explains that conversations and the
agent's own sign-in are kept.
Enter or Esc in details returns to the originating list with its search and selection, or to
the panes when opened from the sidebar or directly. Rename opened through details keeps the
same return path. Background account checks keep the list's search and selected row while
updating its entries.
The bottom-right count includes only accounts, showing matches over the total while searching.
**+ Add account** is excluded from both counts.

**+ Add account** opens one form, also reached directly with `ctrl+b A n`. It stays visible when
filtering the list. **Agent** starts with the focused pane's agent, otherwise the last choice;
Left/Right changes it. When an existing sign-in needs review, a one-line explanation and the
buttons **Use existing account**, **Sign in to another account**, and **Cancel** come before
any name field. **Use existing account** opens Review; it does not adopt the sign-in. Choosing
**Sign in to another account** opens the name field for a new sign-in. A name for an existing
sign-in uses the public identity reported by the agent when available and not already taken.
Otherwise **Name** is empty with the hint **e.g. Work or Personal**. Enter on an empty name asks
for a name and changes nothing. No numbered Personal name is proposed by the form.
Tab moves between fields and buttons; arrows choose a button. Buttons stay together, with a
border and a `>` marker on the selected action. A missing agent says **Not installed** and explains
how to retry. An error keeps both fields; long errors wrap and scroll with PageUp/PageDown or
the wheel. Enter or one click on a button submits it. Successful addition returns to Accounts,
selects the new account and says **Account added**. These screens also work at 80x24 without
the sidebar.
Esc or **Cancel** returns to Accounts with its search and selection when the form was opened
from that list. From the sidebar, command palette or a direct shortcut, it closes to the panes.
Closing the **Account added** result follows the same return path. Rename follows this rule too.
While **Checking sign-in...** is shown,
the form accepts cancellation but no second submission. A cancelled check cannot later replace
the screen you are using with its result.

The launch picker (`ctrl+b a`) lists registered agents with an executable on PATH, with Claude
and Codex first. For these two, it lists accounts that do not require sign-in, as before. Other
agents appear once and use their native sign-in and environment. Missing agents are omitted.
Enter or one click starts the selected agent or account. Tab offers **Start in new tab** (`ctrl+t`),
**Start in worktree** (`ctrl+w`) when the focused pane is in a local repository, and **Account
details** (`ctrl+d`). Worktree launches keep the selected account, or the native account for other agents.
For agents without account support, **Accounts** (`ctrl+d`) explains **Accounts are managed in
<agent>**. **Change account** and the direct **Add account** shortcut on their pane give the same
explanation. Manage Claude and Codex accounts in Accounts.

The name is a label: renaming keeps the same account, rules and usage. Its directory stays where
it is, and running agents go on under the new name. Removing an account keeps the label and
launch environment of panes that use it. Adding another account with that name does not attach
those panes, saved sessions, workspace bindings, rules or cached usage to the replacement.
A workspace bound to a removed account asks you to choose an account again.

### Change account and Undo

Click the account name in a pane's frame, the bottom bar's **switch account**, or press
`ctrl+b A c` to open **Change account** for that pane. It lists accounts of the same agent.
In that picker, Enter switches to the selected account; launch, login,
rename and remove actions are not offered. The pane's account is marked `current`: selecting
it closes the picker and leaves the agent running as it is, without restarting it. An exited
pane stays exited too. Another name for the same account directory also leaves it unchanged.

Each destination says **Continue conversation**, **New conversation**, **Sign-in required**, or
**Could not verify**. Sign-in checks run while the picker is open and again when you choose an
account. An account that needs sign-in offers **Sign in**; the current agent keeps running.
After signing in, choose the account again. If verification fails, the current pane is unchanged;
**Check again** retries it. A failure shown while another dialog is open can be retried from
**Change account**.

With reliable hooks, a change requested during work says **Switch after this turn**. Open
`ctrl+b A c` again and choose **Cancel account change** to cancel it. Without a reliable turn
boundary, kraal asks **Stop and switch?**, with **Cancel** selected. Only one change can be
pending per pane: every attached client sees it and can cancel it before the agent stops.
Sign-in and the conversation are checked again after waiting, before stopping anything.

A Claude conversation must have a readable transcript on the current account. If it is missing,
empty, unreadable, or has diverged from the destination's copy, kraal refuses and says why, leaving the
agent running. This also applies before Claude has written its first transcript: let it finish
a turn before changing accounts. Transcript searches, checks and copies run in the background.
Kraal refuses a destination whose account home is in use by another process or by another
pending change, or whose separate transcript is open in another process. Stop that agent before
trying again. Shared transcripts need no copy. Transfers of the same conversation lock both account
stores; another transfer must finish before retrying. Finishing or failing a transfer releases its
locks even while a child process is still starting. The lock files remain in `projects`; their
presence alone does not mean a transfer is running.

When replacing an older destination transcript, kraal checks the revision it actually moved
and keeps that file at `projects/<project>/.kraal-transfer-<id>-<unique>/previous.jsonl` in the
destination account home. Writes through an already open file remain recoverable there.
A concurrent change refuses the transfer and reports this recovery path; a newly created
destination is never overwritten. These recovery copies are kept until you remove them.
Process checks cannot prevent an external agent from starting afterward, so avoid running the
same conversation on two accounts during a change. A failure after stopping follows the
recovery procedure below.

Codex changes start a **New conversation**, stated in both the
picker and the confirmation; the original history stays in the original account's directory.
Kraal needs its conversation id and saved history before stopping Codex. If the id is not yet
known, finish a turn with kraal's integration enabled, then try again.

After the replacement starts, **Using <name> · Undo** offers a clickable return. The previous
account is also kept in the pane's actions: open **Find a pane** (`ctrl+b p`), select the pane,
press Tab and choose **Undo account change**. This remains available after the message expires
and after restoring a saved session. Claude returns with the latest conversation, subject to
the same transcript, sign-in and interruption checks. Codex reopens the original conversation
in its account and keeps the newer conversation's files in the other account. Undo does not
revert changes the agent made to your working files. A successful Undo clears that return action;
a later account change replaces it with the most recent source.

A failure after stopping triggers one attempt to resume the previous account. Kraal confirms
success only after observing the replacement agent running, and says **Resumed on <name>** if
recovery succeeds. If recovery also fails, the pane keeps the previous command and Enter retries
it. The message only says the conversation is saved when its history can be verified.

Claude and Codex started by hand in a shell can also change accounts when kraal can identify
the foreground process, account home, saved conversation and exact resume arguments. A session
selector or a hook from that process supplies the conversation id. An opaque wrapper, credential
override, unknown conversation or ambiguous command instead shows **Can't switch this process.**
and offers **Open with another account**. That opens another pane and leaves the original running.
Its saved conversation is checked in the background: the picker opens at once and turns into
that message if the conversation cannot be resumed.
Shells with other jobs are not stopped for an account change.
If process information is temporarily unavailable, kraal keeps the last identified account
label. It still requires a fresh check before stopping the agent. A failed check leaves the
pane unchanged; open **Change account** to try again.

### Sign-in and recovery

Sign-in from Accounts opens a dedicated login pane. Opening the same account's login again
in that session focuses the existing pane, including through another account name for the same
directory. After the login exits successfully, kraal checks with `claude auth status` or
`codex login status`, in that account's environment. A confirmed sign-in closes the pane and
says that the account is signed in. When adding, it returns to the new row in Accounts.
A failed or cancelled sign-in stays open: Enter closes it,
and `r` is the separate **Try again** action. While a check runs, Enter also closes the pane;
its late result is ignored. `c`, **Check again**, retries an unsuccessful check without running
the login again. A check waits at most ten seconds and discards the agent's output.

If sign-in is verified but the account cannot be saved, the pane stays open with
**signed in, could not save account**. Fix the file error, then press `c` to check and save
again without another login. Background warnings cannot replace the sign-in confirmation
while it is displayed; their details remain in **Messages** (`ctrl+b V j`).

A lock in the account directory also prevents another kraal session or `kraal accounts login`
from starting the same login while it is running or being checked.

The account entry is kept in `accounts.toml` if sign-in fails, is cancelled, or cannot be
verified. Until verification succeeds it shows **Sign-in required** in Accounts and the sidebar,
even if it has usage from before. Login panes and their scrollback are never saved, including
when saving a workspace layout. Their output, titles, terminal notifications and clipboard
requests are not copied into kraal's messages, notifications or logs.

`kraal accounts` uses the same check: **signed in**, **sign-in required**, **not installed**,
or **could not check sign-in**. A timeout or unexpected failure is **could not check sign-in**,
not a missing agent. JSON keeps `logged_in` (`true`, `false`, or `null`) and adds `status`:
`signed_in`, `not_signed_in`, `not_installed`, or `could_not_check`.
The command-line add and login commands also verify sign-in after the login succeeds.

### Usage and rules

Claude Code passes the 5-hour and weekly usage to its status line;
`kraal integration install claude` puts a small status line in front of yours that records it,
then runs yours unchanged. Codex accounts are asked through Codex's own app-server every few
minutes (`kraal accounts refresh` asks now). Limit messages the agent prints on screen ("You've
hit your session limit") count too; a line that only quotes one (a prompt, a diff) does not.

The CLI usage bars, account summaries and thresholds share the same expiry rule.
A reported reset ends a window at that time. Without a reset time, it counts for five hours
or seven days after the observation. CLI bars show zero for expired windows. Account summaries
and thresholds use the highest percentage among windows that still count. When all reported
windows have expired, account summaries show **Last checked ...** and thresholds use zero.
No reported windows means usage is unknown. Account details and `kraal accounts list --json`
keep the reported percentages. Details show **Reset unavailable** when no reset time was supplied.

**Rules and thresholds** live in `~/.config/kraal/accounts.toml` (`kraal accounts edit`).
The file must be regular UTF-8 text of at most 1 MiB; symlinks to regular files are supported.
See [Configuration](configuration.md) for file checks and background reads.

```toml accounts
[limits]
threshold = 90        # percent of the 5-hour or weekly window
mode = "ask"          # notify | ask | auto

[[rule]]
workspace = "pro"     # or: path = "~/Dev/client"
agent = "claude"
accounts = ["pro", "Personal"]  # in order of preference
```

The file also holds the accounts themselves, one `[[account]]` each (`name`, `agent`, `home`,
an internal `id`, and a `threshold` of its own if it has one), which `kraal accounts add` writes. A login that has
not been verified also has `sign_in_required = true`; successful verification removes that flag.
Pending reviews have `review = true` and may keep a `public_identity` supplied by the agent.
Former names, identities and homes are stored in `[[account.references]]` under their owner,
including notes and unknown keys. Separate `linked_to` entries are compacted on their next
rewrite. Once a review is resolved there is one `[[account]]` per account in the list.
References whose account was removed have no owner; they are kept in `[[forgotten]]` to prevent
rediscovery and identity reuse, not as account entries. Keep these generated fields.
No home or credential is removed. See [Compatibility](compatibility.md) for saved formats and
clients of different versions.

Kraal edits what it changes and leaves the rest as you wrote it, including comments, order and
unknown keys. A new file includes an explanatory header and omits empty rules and an empty
`[claude]` table. A file that cannot be parsed is left untouched. Account commands refuse files
with accounts without identities, as written by 0.2.0 and 0.3.0, with
`run kraal 0.4 once first`. Kraal 0.5 does not convert them or create a backup.
Usage is read from `usage/by-id`; old name-based caches are left untouched.
Account changes do not rewrite saved sessions or external workspace definitions.
Identity resolution during ordinary account and workspace operations keeps retained references
and homes associated with their account after renames or removals.

Rules accept bare names and `agent:name`; kraal writes resolved
references as `agent:@id`. Workspace files keep the readable `agent="claude:name"` and add
`account_id` when bound or saved. These are generated details: keep them when editing a label;
use names when writing a new rule or choosing a different account in a workspace, removing that
pane's old `account_id`. No command or picker requires an identifier.
[examples/accounts.toml](../examples/accounts.toml) has limits and two rules, by workspace and by
directory.

The first rule that matches decides. A new agent starts on the first account of that rule whose
known usage is under its threshold. Otherwise it tries the first account with unknown usage,
or, when all have known usage over the threshold, the one whose usage resets first. Accounts
marked as needing sign-in are left out, unless all of them are. When
the matching rule names no account that exists any more, no account is used there (rather than
any). Without a matching rule, every confirmed account of the agent may be used, in list order. When
the account of a running agent reaches its threshold, kraal notifies you (`notify`) with
**<name> is at NN% used · Change account**. Click **Change account** in kraal's notification to
open that pane's account picker, even when another pane has focus. In `ask` mode it offers to
continue the conversation on another account once the agent finished its turn. In `auto` mode
it does so by itself at the end of the turn. `auto` only switches when the agent's own hooks said
its turn ended, which Claude Code's do (`kraal integration install claude`) and Codex's cannot;
otherwise kraal asks instead, since restarting an agent mid-turn would lose its work. It also
asks once you have looked at the finished pane or sent it input (typed, pasted, broadcast or
`kraal send`), since its prompt may hold a draft, until the agent takes a prompt (its hook says
so; for an agent without such a hook, once it goes from waiting to working); this is checked
again just before stopping. Only an agent that exited or whose finished turn is still unseen is switched by
itself. Two agents never change to the same Claude account
at once: `auto` picks another account, and a manual change to it is refused. Limits are
watched for agents kraal started on an account and for shell launches it can associate safely. A Claude conversation continues on the other account; a
Codex one cannot (its history stays in its account's directory), so switching starts a new one.
Threshold alternatives need known usage below their threshold; unknown usage is not zero.
Accounts marked as needing sign-in are excluded. An automatic change also checks sign-in and
the conversation before stopping anything, and needs confirmation for a new Codex conversation.
A failed automatic attempt is not repeated on each check: choose an account manually to retry,
or wait until usage falls below the threshold and reaches it again.
Workspace files can say `agent="claude"` (rules apply) or `agent="claude:pro"`.

`auto` is off by default. Providers set usage limits per subscription; check their terms (for
Anthropic, the Usage Policy's section on bypassing product restrictions; for OpenAI, the Terms of
Use on circumventing rate limits or restrictions) before letting a tool move your work between
subscriptions automatically. Accounts are for the subscriptions you hold yourself (work and
personal ones, say): both providers forbid sharing an account's sign-in with someone else.
