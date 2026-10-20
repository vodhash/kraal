# Install

kraal is a single binary for Linux (x86_64, arm64) and macOS (Apple Silicon, Intel). Linux is the
primary target; macOS passes the test suite but is not used day to day yet. Windows is not
supported.

## The install script

```sh
curl -fsSL https://raw.githubusercontent.com/vodhash/kraal/main/install.sh | sh
```

The script takes the archive for your machine from the
[releases](https://github.com/vodhash/kraal/releases) and checks it against its checksum before
installing anything, into `~/.local/bin`. Settings:

| Variable | |
|---|---|
| `KRAAL_VERSION` | a release tag such as `v0.1.0` (the latest by default) |
| `KRAAL_INSTALL_DIR` | where the binary goes (`~/.local/bin` by default) |
| `KRAAL_DOWNLOAD_URL` | where the archives are, instead of GitHub (a mirror); `kraal update` reads it too |

## Updates

```sh
kraal update
```

This replaces kraal with the latest release the same way (it needs `curl` and `tar`):

| Command | |
|---|---|
| `kraal update --check` | only says whether there is a newer release |
| `kraal update --to v0.2.0` | installs that release instead of the latest |
| `kraal update --force` | installs even when it is the version already here |

A kraal installed by Cargo or a package manager (Homebrew, Nix, your distribution) is updated
there instead: `kraal update` changes nothing and says which one installed it
(`this kraal was installed by Cargo: update it with cargo install kraal`).

A session's server keeps the version it started with. After installing, `kraal update` lists the
sessions still on an older kraal, with the programs a restart would stop in them, and offers to
[restart](workspaces.md#persistence) them: no by default when programs would stop, and without a
terminal to ask on; `--restart` or `--no-restart` answer beforehand. Later, `kraal ls` shows the
version each session runs. A different protocol version refuses attachment with a message
naming both versions. Run `kraal restart` to bring an older local server onto the installed
binary. See [Compatibility](compatibility.md#clients-and-servers).

## With Homebrew

On macOS and Linux:

```sh
brew install vodhash/tap/kraal
```

`brew upgrade kraal` updates it, to each release as it comes out; `kraal update` leaves a kraal
that Homebrew installed to Homebrew.

## With Cargo

The latest release, from crates.io, with Rust 1.88 or later:

```sh
cargo install kraal
```

The same command again updates it; `kraal update` leaves a kraal that Cargo installed to Cargo.
The main branch instead:

```sh
cargo install --git https://github.com/vodhash/kraal
```

## By hand

Each [release](https://github.com/vodhash/kraal/releases) has an archive per platform, its
`.sha256`, and `SHA256SUMS`. The Linux builds are static (musl), so they run on any
distribution.

## On a server

`kraal remote-install <server>` copies the running binary to `~/.local/bin` on a server of the
same OS and CPU, for [remote sessions](servers.md#remote-sessions). If the selected session
(default `main`, or `-s NAME`) is running, it then uses the installed binary to restart it,
showing what stops and asking for confirmation. Layout and scrollback are kept.
