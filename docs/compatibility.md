# Compatibility between versions

## Saved files and sessions

Kraal 0.5 reads the session and account formats written by 0.4.x. Sessions use
`sessions-v2/<session>.json` and `scrollback-v2/<session>/` in the kraal state directory
(`~/.local/state/kraal`, or `$XDG_STATE_HOME/kraal` when set). Existing workspace KDL and
configuration files keep working. Each save writes only the current format. Kraal 0.4.0 saved
some workspaces without the contents of their KDL source, which made them report **File
changed**; the first start records that source as it is then, once, without touching the
personal layout. Inspecting a stopped session never writes.

Sessions from **0.2.0** and **0.3.0** in `sessions/<session>.json`, without a current
`sessions-v2/<session>.json`, are left untouched and refused by start, attach and restore:
`run kraal 0.4 once first`. Account commands likewise refuse an `accounts.toml` containing
accounts without identities, with the same next step. Run 0.4 to convert those files before
returning to 0.5. Kraal keeps migrations for one version only.

A saved session from a newer format is left untouched and refused with `use a newer kraal`.
Invalid or unreadable state also prevents restoration; fix the reported file and retry.
Kraal never falls back from an unreadable or unsupported current session to an older copy.

Current session files take precedence over old files. Kraal does not import, rewrite or remove
`sessions/<session>.json` or `scrollback/<session>/`. The older binaries never open the current
paths, so their saves and Forget cannot replace the current state or scrollback. Changes made
in either format do not synchronize to the other.

`kraal forget` removes current state, scrollback, attachments and saved paste buffers. If an old
session remains, a later start refuses it with `run kraal 0.4 once first`. Forget creates no import marker.

Account changes preserve comments, order and unknown keys without making a migration backup.
Usage is read from identity-based caches in `usage/by-id`; old name-based caches are untouched.
Account changes do not rewrite saved sessions or external workspace KDL files. Identity
resolution during ordinary account and workspace operations remains, including retained
references and homes after renaming or removing an account.
See [Workspace identities](workspaces.md#workspace-identities) and [Accounts](agents.md#accounts).

## Clients and servers

This version uses protocol 6, which lets a command run in an agent's pane wait for you to allow a
production action, the client saying when it has drawn the question
([Production](production.md#agents)). Kraal 0.5.x used protocol 5, which binds a
pasted clipboard text to the agent pane that received Ctrl+V; Kraal 0.4.1 used protocol 4, Kraal
0.4.0 protocol 3, and Kraal 0.2.0 and 0.3.0 protocol 2.
Different protocol versions refuse attachment and ordinary commands, and production commands
that ask the session first ([Production](production.md#agents)). There are no older-peer
adapters or optional feature negotiation. A refusal names both protocol versions and one next step:

- A newer local client with an old server: `kraal -s main restart`.
- A newer client using `attach --host box`: `kraal remote-install box` (with `-s NAME` for a named session).
- An older client with a newer server: update this machine with `kraal update`.

The older client must be updated, since restarting through that binary would downgrade the server.
The current client refuses before changing the terminal, and a mismatched attachment never
changes the session. Updating the binary does not
stop a running session automatically. Existing attached terminals keep using their server until
it stops; a newly installed client needs the matching protocol before attaching.

The upgrade control path still identifies running releases in `kraal ls` and the update notice.
`kraal kill` saves and stops a 0.2.0 or 0.3.0 server. Old socket locations remain discoverable
for this purpose and to avoid starting a second server for the same session.
Restarting those releases requires the removed disk adapters: `kraal restart` now refuses
before stopping them, with `run kraal 0.4 once first`. Use 0.4 for that upgrade first.
Restarting a 0.4.x server still saves, stops and restores it with the installed binary.
Restart lists the programs it will stop and asks before stopping them.
See [Persistence](workspaces.md#persistence).

Workspaces are saved automatically. KDL is imported and exported explicitly with `kraal import`
and `kraal export`; `kraal save` and its historical `SaveWorkspace` request are removed.
Clipboard, file attachments, pasted paths and clipboard images introduced in protocol 3 remain
part of protocol 6.
The client applies its clipboard policy, including the terminal OSC 52 fallback where configured.
The server no longer emits OSC 52 as an adapter for old clients.

## Laptop files on Linux and macOS

Laptop browsing uses the same attachment connection on both systems, with consent drawn by
the laptop client. Each directory component is opened without following symbolic links.
On macOS, the system may also ask for access when listing Documents or Desktop. Declining
system access leaves a listing error; grant access on the laptop and press `r` to retry.
The default scope excludes `~/Library`. Copies from the laptop use the same scoped component
walk and a dedicated client reader thread on both systems. The server routes bounded messages
between that client and a job authorized by a one-use token for the operation and paths.
The server loop does no file I/O for the transfer. Copies between the laptop and an SSH server
use the session machine's private spool and the existing rsync or tar SSH copy engine on both
Linux and macOS. Both directions preserve the source originals, check the listed connection
with `--expect` and ask before accessing a production server. Moves on this route are refused.
Downloads wait for laptop permission before the SSH copy starts; its progress keeps the
attachment transfer alive. Cancellation and disconnection remove the spool.

For copies from the laptop to the session machine, the job receives into a private spool, then
uses the existing local copy engine with move and no-replace semantics. Linux uses `renameat2(RENAME_NOREPLACE)` and macOS uses
`renamex_np(RENAME_EXCL)` for exclusive installation. Filesystems that cannot support the
copy engine's exclusive installation fail without silently replacing an unapproved name.
Copies into the laptop use a private `.kraal-stage-*` directory in the destination, in
`kraal-stage 2` format. The client probes exclusive rename support before receiving contents;
an unsupported filesystem refuses the copy. On macOS the descriptor-relative
`renameatx_np(RENAME_EXCL)` form of `renamex_np` anchors installation to the opened directory.
The client announces directory case sensitivity using `_PC_CASE_SENSITIVE` on macOS so
replacement names compare without case where needed. Replacement approval is always drawn
and checked locally, even with Always. A name taken meanwhile stays staged and is named.
Modes without setuid/setgid and modification times are preserved; extended attributes and ACLs are not copied.

Moves wait for installation before removing any source. Both directions recheck device,
inode, size and modification time, including nanoseconds, and keep changed sources.
Deletion and move-away confirmation is drawn by the laptop client even with Always.
Removal uses descriptor-relative operations on Linux and macOS and never follows links.
These operations do not change the saved session, workspace or configuration formats.

Laptop panels save as the session machine's home directory, with no laptop paths or listings
in the saved session. The session format and its downgrade isolation remain unchanged.

## For contributors: how the protocol changes

The client and server exchange JSON messages in `src/protocol.rs`. Change `PROTOCOL_VERSION`
when the wire contract changes. Peers must agree on that generation before interactive use.
Do not add capabilities or adapters for older peers. Keep the protocol 2 ping, pane listing and
stop requests used to upgrade released servers, and their response readers, until their upgrade
path is removed. The 0.4 saved-file migration is removed in 0.5.

## The cross-version test

`tests/compat.sh` downloads **0.2.0**, **0.3.0**, **0.4.0**, **0.4.1** and **0.5.3** and checks their
release checksums before running `tests/compat.rs` against each real binary. The tests cover
protocol refusal in both directions when generations differ, production commands refused by an
older session, saving and stopping old servers, refusal of sessions older than 0.4 without
changing their files, and restoring and restarting 0.4.x and 0.5.x sessions with their layout and
scrollback. They also check downgrade isolation and that existing
workspace and configuration files keep working. CI runs these tests on Linux and macOS.

| Command | Releases |
|---|---|
| `tests/compat.sh` | 0.2.0, 0.3.0, 0.4.0, 0.4.1 and 0.5.3 |
| `tests/compat.sh v0.2.0` | 0.2.0 only |
| `tests/compat.sh --build v0.2.0` | Built from its tag (`git fetch --tags` first) |

The releases in `RELEASES` in `tests/compat.sh` are checked against this page by the unit tests.
