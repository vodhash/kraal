# Configuration

`~/.config/kraal/config.toml`. A config only needs the settings it changes; this page shows them
all with their defaults, and so does the program:

| Command | |
|---|---|
| `kraal config default` | prints every setting with its default, commented |
| `kraal config path` | prints where the file is |
| `kraal config check` | validates the config, every workspace file, `accounts.toml` and `servers.toml`; it warns about what kraal would leave out (a key bound twice, a tool that does not exist) and fails when a file cannot be read |
| `kraal config keys` | prints every key ([Keys](keys.md#your-own-keys)) |
| `kraal config reload` | makes the running session read the config again |

A reload applies keys, theme, sidebar, bottom bar, mouse, pane frames, notifications, tools,
hints, hooks, production words, contexts, saving scrollback and the paste buffers' settings at
once; panes already open keep their shell and scrollback length, and `[graphics]` waits for the
session's next start. Client clipboard policy is read on attach; the server copy size limit
updates on reload. When the file cannot be read, nothing changes and the command says why.
If another reload is requested before the first finishes, the older request is refused; only
the newest request can apply its configuration.

`config.toml`, `accounts.toml` and `servers.toml` must be regular UTF-8 files of at most
1 MiB (1,048,576 bytes) each. Symlinks to regular files are supported; directories, FIFOs and
devices are refused. Missing files still use defaults. Config reloads and automatic account
and server inventory reads run in the background, so slow storage does not hold up terminal
input while these files are read.

Automatic scans check file metadata and directory listings on ticks at least five seconds
apart, without opening or reading file contents on the server loop. Unchanged inputs do not
start a background scan. Changes, pending scan requests and failed reads trigger another
read; a file that cannot be parsed is read again once it changes. A change during a scan is
checked again so it is not lost. Idle sessions keep their
slower tick rate. This covers the configuration files, workspace definition directories and
linked sources, and SSH host files and included directories when `ssh_config_hosts` is on.
Changes to `config.toml` still need `kraal config reload` to apply settings.

The check also reads your runbooks and, run in a repository, its `.kraal/worktree.toml` and its
runbooks. A key kraal does not know in `config.toml`, `servers.toml` or `accounts.toml` (a typo,
or a setting of another version of kraal) is a warning that names it, `bar.kyes: unknown key (did
you mean keys?)`, and the rest of the file applies, a tool or a hint pattern that has such a key
included; a session says the same of `config.toml` when it starts or reloads it. A runbook and
`.kraal/worktree.toml` refuse a key they do not know: they say what runs, and running part of it
would be worse than refusing it.

## Panes and the screen

```toml config
shell = "/bin/zsh"       # for new panes; $SHELL by default
scrollback = 10000       # lines kept per pane
save_scrollback = true   # panes show their text again after a restart; it is written to disk
pane_frames = true       # false: panes touch, and their details go in the top bar
mouse = true             # false leaves the mouse to your terminal
workspace_dirs = []      # more directories of workspace files, after ~/.config/kraal/workspaces
ssh_config_hosts = true  # offer the hosts of ~/.ssh/config in the server picker

[sidebar]
visible = true           # false folds to the four-column strip
width = 28               # in terminals at least three times as wide

[bar]
keys = "context"         # the focused pane's actions on the last row; "off": none

[notifications]
mode = "terminal"        # terminal (OSC 777 and OSC 9), system (notify-send), both, off
on_blocked = true        # an agent that waits for you
on_done = true           # an agent that finished
on_pull_request = true   # a pull request's checks fail or pass, or its review changes (gh)
long_command = 30        # seconds a command runs before its end is announced; 0: never
```

The full sidebar needs at least three times `sidebar.width`, 84 columns by default.
At 40 to 83 columns it becomes a four-column workspace and agent strip; below 40 it disappears.
`visible = false` also folds to this strip, including on wide terminals. `ctrl+b V l` toggles
full sidebar and strip for the attached terminal. When too narrow for the full sidebar, it opens
Workspaces and leaves that preference unchanged. See [the strip](workspaces.md) for its controls.

`save_scrollback` saves what each pane shows, its scrollback included, in
`~/.local/state/kraal/scrollback-v2`, so that panes show it again after a restart (see
[Persistence](workspaces.md#persistence)). Scrollback can hold secrets: `false` saves none and
deletes what was saved. Account login panes and their text are never saved, even when this
setting is `true`. `long_command` needs a shell that marks its commands
([Shell commands](terminal.md#shell-commands)). A program in a pane can send its own
notifications (OSC 9, OSC 777): kraal passes on three per pane a second at most and drops the rest.

## Clipboard

```toml config
[clipboard]
mode = "auto"          # native user copies with terminal fallback; "terminal" or "off" too
max_bytes = 1048576     # UTF-8 bytes per external copy, from 1 through 8388608
```

These are the attaching machine's settings. The server also enforces its own `max_bytes`.
An external copy must fit both limits and is never truncated. `off` preserves internal paste
buffers subject to their limits. Programs use the terminal unless the attaching machine's
saved server has `clipboard_native = true`. See [Clipboard](terminal.md#clipboard) for
backends, messages, intermediate machines and how to revoke permission.

## Keys

```toml config
[keys]
preset = "kraal"         # or "classic" for single-key commands
prefix = ["ctrl+b"]

[keys.leader]            # keys after the prefix; a space separates a sequence
split_right = ["%", "P right"]

[keys.direct]            # keys without the prefix
toggle_floating = ["alt+f"]
```

Every name these two tables take is in `kraal config keys` ([Keys](keys.md#your-own-keys)).
A whole example, on the classic preset: [keys-classic.toml](../examples/config/keys-classic.toml).

## Themes

```toml config
[theme]
preset = "default"       # or "high-contrast", or "ascii" for terminals or fonts without box drawing
accent = "green"         # colours: a name, "#rrggbb" or 0-255; each one is optional
dim = "darkgray"
sync = "magenta"         # panes typing is broadcast to
production = "red"
frames = "rounded"       # or "plain", "thick", "double", "ascii"
```

A look to copy, with the sidebar and the bottom bar: [theme.toml](../examples/config/theme.toml).

## Servers, production and contexts

```toml config
[servers]
auto_add = true                # offer temporary suggestions until restart; only Add saves them
multiplex = true               # one SSH connection per server for kraal's panes, transfers and tools
max_panes_per_tab = 9          # opening a whole group puts at most this many servers in a tab
download_dir = "~/Downloads"   # where downloads go, and the versions of a server's file you keep

[production]
words = ["prod", "production", "prd"]   # [] turns the guard off, red frames included
confirm_sync = true            # ask before broadcast typing reaches a production pane
confirm_paste = true           # ask before a paste of several lines runs in a production pane

[contexts]
kubernetes = true              # show, and count for production, the pane's Kubernetes context
terraform = true               # its Terraform workspace
aws = true                     # its AWS profile
```

See [Servers](servers.md), [Production](production.md) and [Contexts](tools.md#contexts).

## Images

```toml config
[graphics]
enabled = true           # false: no image in panes, and previews draw theirs with half blocks
memory_mb = 256          # what all panes keep for images
```

See [Images](terminal.md#images).

## Attachments

The receiving server applies these limits before accepting a batch and checks every chunk:

```toml config
[attachments]
file_bytes = 67108864         # 64 MiB per file
batch_bytes = 134217728       # 128 MiB per batch
files = 32                   # files per batch, configurable from 1 to 1024
session_bytes = 1073741824    # 1 GiB of retained files and result records
```

Byte limits must be positive. Changes apply to new batches after config reload. Existing
configurations use these defaults. Use **Attach files** in the command palette to select local
files explicitly. **Attach clipboard image** reads a PNG image with a fixed 16 MiB limit,
also subject to these file, batch and session quotas. Bind `attach_clipboard_image` in
`[keys.leader]` or `[keys.direct]` if desired; no shortcut is assigned by default.
Larger or recursive transfers use `kraal cp`. See
[Attachments](terminal.md#attachments) for retention, cancellation and quota recovery.

## Laptop files

This setting belongs to the laptop running `attach --host`. It is read again for each file
manager request. The local client asks for permission before listing or
reading or writing anything. Permission to copy works between the laptop and the session
machine or an SSH server in both directions. SSH copies relay through the session machine's
private spool, with a separate confirmation for production servers. Moves between the laptop
and an SSH server are refused. Deleting, moving files away or replacing an item on the laptop
always asks in the local client box, even with Always. Moving to the laptop asks locally
if it replaces existing items.

```toml config
[laptop_files]
scope = "home"
```

`laptop_files.scope` accepts `"home"` (the default), `"all"` or `"off"`. Home excludes hidden
entries directly inside your home, and `~/Library` on macOS, even when hidden files are shown.
Hidden files in deeper directories follow the file manager's `.` key. All allows directories
anywhere your user can access; off refuses every request, including remembered permissions.
Neither enabled scope follows symbolic links when opening directories or files. Copies preserve
links whose targets resolve inside the scope and refuse links that escape it. An unreadable
or invalid config refuses listing and all file operations. A selected folder is copied recursively,
including its nested hidden files.

**Allow** lasts for the current attach, including reconnects. **Always** stores a `[[laptop_copy]]`
entry with `name`, `ssh` and `write = true` in `~/.config/kraal/laptop-files.toml` (or under
`$XDG_CONFIG_HOME/kraal`). Removing the matching entry revokes Always at the next request;
remove the file to revoke all remembered servers. Older grants without `write = true`
permit reading and ask again before the first write. This is separate from `paste-files.toml`.
See the [file manager](servers.md#the-file-manager) and [example](../examples/config/laptop-files.toml).

## Paste buffers

`[buffers]` sets what the [paste buffers](keys.md#paste-buffers) keep:

```toml config
[buffers]
limit = 50               # numbered buffers kept, the oldest going first (0 keeps none)
max_mb = 32              # what all buffers hold together; past it, the oldest numbered ones go
persist = false          # true keeps them across restarts, in a private file
```

Buffers live in the server's memory only and go when it stops: they can hold secrets (a password
you copied, a token). `persist = true` keeps them across restarts in
`~/.local/state/kraal/buffers/<session>.json`, a file only you can read (0600); `false` writes
nothing and deletes that file. A file kraal cannot read back (damaged, written by a newer kraal,
or over 8 × `max_mb`) is moved aside to `<session>.json.bad` (`.bad.1` to `.bad.9` when that one
is taken), and the start says so, rather than written over; when it cannot be moved, the server
leaves it alone and saves no buffers until it restarts. `kraal forget` deletes them all.

## Worktrees

`[worktrees]` prepares the worktrees kraal makes for agents, in every repository, and numbers
their ports (see [worktrees](agents.md#worktrees)):

```toml config
[worktrees]
copy = [".env*"]         # copied from the main checkout into each new worktree
setup = []               # run there before the agent starts, after the repository's own
port_step = 10           # $KRAAL_PORT_OFFSET = the worktree's number × port_step
port_base = 3000         # also $PORT = port_base + $KRAAL_PORT_OFFSET (unset: no $PORT)
```

## Tools, hints and hooks

```toml config
[tools.pods]             # a tool of your own, or a change to one kraal ships
command = "kubectl get pods -o wide"

[hints]
alphabet = "asdfqwerzxcvjklmiuopghtybn"   # the letters labels are made of, the easiest first

[hints.patterns.ticket]  # a pattern of your own to label; with `tool`, it runs that tool
pattern = 'OPS-\d+'

[hooks]                  # a command per event
"agent.blocked" = 'notify-send "$KRAAL_AGENT needs you"'
```

See [Tools](tools.md#tools) for a tool's fields, [Hints](tools.md#hints) for patterns of your
own, and [Scripting](scripting.md#events) for hooks.
