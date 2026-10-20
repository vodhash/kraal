//! `kraal update`: replace this binary with a release from GitHub, the way install.sh installs
//! one: the archive for this machine, checked against its checksum, tried, then renamed into
//! place.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

const REPO: &str = "vodhash/kraal";
const DOWNLOAD: Duration = Duration::from_secs(300);

/// `restart`: what to do with the sessions left on an older kraal (`None`: ask).
pub fn update(check: bool, to: Option<String>, force: bool, restart: Option<bool>) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    if check {
        let latest = latest_tag()?;
        if version_of(&latest) == current {
            println!("kraal {current} is the latest release");
        } else {
            println!("kraal {current}; the latest release is {latest}: kraal update");
        }
        return Ok(());
    }
    let exe = std::env::current_exe().context("cannot tell where this kraal is")?;
    if let Some(manager) = package_manager(&exe) {
        bail!("this kraal was installed by {manager}");
    }
    let Some(asset) = asset_name(std::env::consts::OS, std::env::consts::ARCH) else {
        bail!("no release is built for this machine: update with cargo install kraal");
    };
    let base = match std::env::var("KRAAL_DOWNLOAD_URL").ok().filter(|u| !u.is_empty()) {
        Some(url) => url,
        None => release_url(to.as_deref()),
    };
    let tmp = tempdir()?;
    let result = (|| -> Result<Option<String>> {
        let archive = tmp.join(&asset);
        let sum = tmp.join(format!("{asset}.sha256"));
        fetch(&format!("{base}/{asset}"), &archive)?;
        fetch(&format!("{base}/{asset}.sha256"), &sum)?;
        let want = std::fs::read_to_string(&sum)?;
        let want = want.split_whitespace().next().unwrap_or_default();
        if want.is_empty() || sha256(&archive)? != want {
            bail!("{asset} does not match its checksum: nothing was changed");
        }
        run(Command::new("tar").arg("-xzf").arg(&archive).arg("-C").arg(&tmp), "tar")?;
        let new = tmp.join(asset.trim_end_matches(".tar.gz")).join("kraal");
        let out = crate::exec::run(Command::new(&new).arg("--version"), Duration::from_secs(10), 4096)
            .ok()
            .filter(|o| o.success())
            .context("the downloaded kraal does not run here: nothing was changed")?;
        let version = String::from_utf8_lossy(&out.stdout).split_whitespace().nth(1).unwrap_or_default().to_string();
        if version == current && !force {
            println!("kraal {current} is already installed");
            return Ok(None);
        }
        replace(&new, &exe)?;
        println!("kraal {current} → {version} ({})", exe.display());
        Ok(Some(version))
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    match result? {
        Some(version) => super::restart::after_update(&exe, &version, restart),
        None => Ok(()),
    }
}

/// The archive name of a release for this OS and architecture (`std::env::consts`).
fn asset_name(os: &str, arch: &str) -> Option<String> {
    let os = match os {
        "linux" => "unknown-linux-musl",
        "macos" => "apple-darwin",
        _ => return None,
    };
    let arch = match arch {
        "x86_64" | "aarch64" => arch,
        _ => return None,
    };
    Some(format!("kraal-{arch}-{os}.tar.gz"))
}

fn release_url(tag: Option<&str>) -> String {
    match tag {
        Some(tag) => format!("https://github.com/{REPO}/releases/download/{tag}"),
        None => format!("https://github.com/{REPO}/releases/latest/download"),
    }
}

/// The tag of the latest release: GitHub redirects `releases/latest` to `releases/tag/<tag>`.
fn latest_tag() -> Result<String> {
    let mut cmd = Command::new("curl");
    cmd.args(["-fsSIL", "-o", "/dev/null", "-w", "%{url_effective}"])
        .arg(format!("https://github.com/{REPO}/releases/latest"));
    let url = run(&mut cmd, "curl")?;
    tag_in(&url).with_context(|| format!("no release found at {url}"))
}

fn tag_in(url: &str) -> Option<String> {
    let tag = url.trim().rsplit_once("/releases/tag/")?.1;
    (!tag.is_empty()).then(|| tag.to_string())
}

/// `v0.2.0` → `0.2.0`.
fn version_of(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

/// The package manager that owns `exe`, if any: it would put its own copy back. Homebrew links
/// its `bin` into its Cellar (`/usr/local/bin/kraal` on an Intel Mac): where a link leads counts.
fn package_manager(exe: &Path) -> Option<&'static str> {
    let real = std::fs::canonicalize(exe).ok();
    std::iter::once(exe).chain(real.as_deref()).find_map(owner)
}

fn owner(exe: &Path) -> Option<&'static str> {
    let path = exe.to_string_lossy();
    if path.contains("/Cellar/") || path.contains("/homebrew/") {
        Some("Homebrew: update it there")
    } else if path.starts_with("/nix/store/") {
        Some("Nix: update it there")
    } else if path.starts_with("/usr/bin/") {
        Some("your system's package manager: update it there")
    } else if installed_by_cargo(exe) {
        Some("Cargo: update it with cargo install kraal")
    } else {
        None
    }
}

/// Whether Cargo installed `exe`: the root it installs into (the folder above `bin`) records it
/// in `.crates.toml`, whatever that root (`cargo install --root`) and however it is reached.
fn installed_by_cargo(exe: &Path) -> bool {
    let Some(name) = exe.file_name().and_then(|n| n.to_str()) else { return false };
    let Some(root) = exe.parent().filter(|bin| bin.ends_with("bin")).and_then(Path::parent) else { return false };
    let Ok(records) = std::fs::read_to_string(root.join(".crates.toml")) else { return false };
    let Ok(records) = records.parse::<toml::Table>() else { return false };
    // `[v1]`: each package installed, with the programs it put in `bin`.
    let Some(v1) = records.get("v1").and_then(toml::Value::as_table) else { return false };
    v1.values().filter_map(toml::Value::as_array).flatten().any(|program| program.as_str() == Some(name))
}

fn fetch(url: &str, to: &Path) -> Result<()> {
    let mut cmd = Command::new("curl");
    cmd.args(["-fsSL", "--proto", "=https,file", "-o"]).arg(to).arg(url);
    let out = crate::exec::run(&mut cmd, DOWNLOAD, 64 * 1024).context("curl is needed to download kraal")?;
    if !out.success() {
        bail!("could not download {url}: {}", crate::exec::last_stderr_line(&out));
    }
    Ok(())
}

fn sha256(file: &Path) -> Result<String> {
    for (tool, args) in [("sha256sum", &[][..]), ("shasum", &["-a", "256"][..])] {
        let Ok(out) = crate::exec::run(Command::new(tool).args(args).arg(file), Duration::from_secs(60), 4096) else {
            continue;
        };
        if out.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap_or_default().to_string());
        }
    }
    bail!("sha256sum or shasum is needed to check the download")
}

fn run(cmd: &mut Command, what: &str) -> Result<String> {
    let out = crate::exec::run(cmd, DOWNLOAD, 64 * 1024).with_context(|| format!("{what} is needed"))?;
    if !out.success() {
        bail!("{what} failed: {}", crate::exec::last_stderr_line(&out));
    }
    Ok(out.stdout_text())
}

/// Copy `new` next to `exe`, synced to the disk, then rename it over `exe`: a kraal running from
/// the old file keeps it, and nothing is left half written, even by a crash of the machine.
fn replace(new: &Path, exe: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = exe.parent().context("this kraal has no folder")?;
    let staged = dir.join(format!(".kraal.new.{}", std::process::id()));
    let copy = std::fs::copy(new, &staged)
        .and_then(|_| std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)))
        .and_then(|()| std::fs::File::open(&staged)?.sync_all())
        .and_then(|()| std::fs::rename(&staged, exe));
    if let Err(e) = copy {
        let _ = std::fs::remove_file(&staged);
        bail!("cannot replace {}: {e} (install.sh can put kraal in another folder)", exe.display());
    }
    Ok(())
}

fn tempdir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("kraal-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archives_by_machine() {
        assert_eq!(asset_name("linux", "x86_64").as_deref(), Some("kraal-x86_64-unknown-linux-musl.tar.gz"));
        assert_eq!(asset_name("linux", "aarch64").as_deref(), Some("kraal-aarch64-unknown-linux-musl.tar.gz"));
        assert_eq!(asset_name("macos", "aarch64").as_deref(), Some("kraal-aarch64-apple-darwin.tar.gz"));
        assert_eq!(asset_name("windows", "x86_64"), None);
        assert_eq!(asset_name("linux", "riscv64"), None);
    }

    #[test]
    fn releases_and_tags() {
        assert_eq!(tag_in("https://github.com/vodhash/kraal/releases/tag/v0.2.0\n").as_deref(), Some("v0.2.0"));
        assert_eq!(tag_in("https://github.com/vodhash/kraal/releases"), None);
        assert_eq!(version_of("v0.2.0"), "0.2.0");
        assert!(release_url(Some("v0.2.0")).ends_with("/releases/download/v0.2.0"));
        assert!(release_url(None).ends_with("/releases/latest/download"));
        assert_eq!(
            package_manager(Path::new("/opt/homebrew/Cellar/kraal/0.1.0/bin/kraal")),
            Some("Homebrew: update it there")
        );
        assert_eq!(package_manager(Path::new("/home/me/.local/bin/kraal")), None);
    }

    #[test]
    fn a_kraal_cargo_installed_is_left_to_cargo_wherever_it_is() {
        let root = std::env::temp_dir().join(format!("kraal-cargo-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // `cargo install --root <root>/local kraal`, beside another program Cargo installed.
        let local = root.join("local");
        std::fs::create_dir_all(local.join("bin")).unwrap();
        let exe = local.join("bin/kraal");
        std::fs::write(&exe, "").unwrap();
        assert_eq!(package_manager(&exe), None, "nothing says Cargo installed it");
        let records = |more: &str| {
            let rg = "\"ripgrep 14.1.0 (registry+https://github.com/rust-lang/crates.io-index)\" = [\"rg\"]";
            std::fs::write(local.join(".crates.toml"), format!("[v1]\n{rg}\n{more}")).unwrap();
        };
        records("");
        assert_eq!(package_manager(&exe), None, "Cargo installed another program there");
        records("\"kraal 0.2.0 (registry+https://github.com/rust-lang/crates.io-index)\" = [\"kraal\"]\n");
        let cargo = Some("Cargo: update it with cargo install kraal");
        assert_eq!(package_manager(&exe), cargo);
        // Reached through a link to that root, as a Cargo home that is a link may be.
        std::os::unix::fs::symlink(&local, root.join("link")).unwrap();
        assert_eq!(package_manager(&root.join("link/bin/kraal")), cargo);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_link_into_homebrews_cellar_is_left_to_homebrew() {
        let root = std::env::temp_dir().join(format!("kraal-brew-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cellar = root.join("Cellar/kraal/0.6.0/bin");
        std::fs::create_dir_all(&cellar).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(cellar.join("kraal"), "").unwrap();
        std::os::unix::fs::symlink(cellar.join("kraal"), root.join("bin/kraal")).unwrap();
        assert_eq!(package_manager(&root.join("bin/kraal")), Some("Homebrew: update it there"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
