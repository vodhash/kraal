# Packaging

What the release workflow (`.github/workflows/release.yml`) publishes besides the release and the
crate, on each tag `v<version>`, from that release's `SHA256SUMS`:

| Where | Template | Published by | Needs |
|---|---|---|---|
| Homebrew tap (`brew install vodhash/tap/kraal`) | `homebrew/kraal.rb.in` | `homebrew/publish.sh` | secret `TAP_DEPLOY_KEY`: a deploy key of the tap repository with write access; `vars.HOMEBREW_TAP` if the tap is not `<owner>/homebrew-tap` |
| AUR (`kraal-bin`) | `aur/PKGBUILD.in` | `aur/publish.sh`, in an Arch container | secret `AUR_SSH_PRIVATE_KEY`: a key whose public half is in the AUR account; `vars.AUR_MAINTAINER` for the PKGBUILD's maintainer line |

Without its secret, a job publishes nothing and says so; an AUR key the AUR does not know (an
account not made yet) is a warning, not a failure. `fill.sh` fills a template; each `publish.sh`
takes `DRY_RUN=1` to stop before reaching the tap or the AUR (the AUR one builds the package with
makepkg first, so it needs Arch):

```sh
gh release download v0.5.3 --pattern SHA256SUMS
DRY_RUN=1 packaging/homebrew/publish.sh 0.5.3 SHA256SUMS vodhash/homebrew-tap
DRY_RUN=1 packaging/aur/publish.sh 0.5.3 SHA256SUMS
```
