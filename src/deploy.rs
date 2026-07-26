//! Deployment: download a GitHub release artifact into a versioned directory,
//! keep the env in <root>/shared, point <root>/current at the release, and run
//! it as a supervised systemd service. Stop/restart just drive systemctl.
//!
//! Layout maintained on the machine:
//!   <root>/releases/<version>/   the downloaded (and, for archives, extracted) build
//!   <root>/shared/<env>          the .env, written by the normal env sync
//!   <root>/current -> releases/<version>
//!
//! Service management is systemd-only (the sglaz agent already runs as root
//! under systemd on Linux hosts). On other platforms deploy actions error
//! clearly instead of pretending to work.

use crate::envfile::EnvVar;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A fully-resolved deploy action from the server.
pub struct DeploySpec {
    pub version: String,
    pub signed_url: String,
    pub kind: String, // "binary" | "archive"
    pub run_cmd: String,
    pub prepare_cmd: String,
    pub deploy_root: String,
    pub unit_name: String,
    pub env_file: String, // absolute path; systemd loads it into the process
    pub env_values: Vec<EnvVar>,
}

/// Download, place, and (re)start a release. Returns on success; the caller
/// records the running version.
pub fn deploy(http: &reqwest::blocking::Client, spec: &DeploySpec) -> Result<()> {
    require_linux()?;

    let root = PathBuf::from(&spec.deploy_root);
    let release_dir = root.join("releases").join(sanitize(&spec.version));
    std::fs::create_dir_all(&release_dir)
        .with_context(|| format!("creating {}", release_dir.display()))?;
    std::fs::create_dir_all(root.join("shared")).ok();

    // 1. Download the artifact.
    println!("sglaz-deploy[{}]: downloading {}", spec.unit_name, spec.version);
    let resp = http
        .get(&spec.signed_url)
        .send()
        .context("download request failed")?;
    if !resp.status().is_success() {
        bail!("download returned status {}", resp.status());
    }
    let bytes = resp.bytes().context("reading download body")?;

    // 2. Place it.
    if spec.kind == "archive" {
        extract_tar_gz(&bytes, &release_dir)
            .context("extracting release archive (expected .tar.gz)")?;
    } else {
        // Binary: the asset itself is the executable.
        let bin = release_dir.join("app");
        std::fs::write(&bin, &bytes).with_context(|| format!("writing {}", bin.display()))?;
        set_executable(&bin)?;
    }

    // 2b. Write the env file (in shared/) so systemd loads it into the process
    // via EnvironmentFile before start. Only when the server sent values — an
    // empty list leaves any existing file untouched.
    if !spec.env_values.is_empty() && !spec.env_file.trim().is_empty() {
        crate::envfile::write(&spec.env_file, &spec.env_values)
            .with_context(|| format!("writing env file {}", spec.env_file))?;
    }

    // 3. Optional prepare step (e.g. `pip install -r requirements.txt`).
    if !spec.prepare_cmd.trim().is_empty() {
        println!("sglaz-deploy[{}]: prepare: {}", spec.unit_name, spec.prepare_cmd);
        run_in(&release_dir, &spec.prepare_cmd).context("prepare command failed")?;
    }

    // 4. Flip the `current` symlink atomically.
    let current = root.join("current");
    symlink_swap(&release_dir, &current).context("updating current symlink")?;

    // 5. Install/refresh the systemd unit and (re)start.
    install_unit(spec, &current)?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", &spec.unit_name])?;
    systemctl(&["restart", &spec.unit_name])?;

    println!("sglaz-deploy[{}]: now running {}", spec.unit_name, spec.version);
    Ok(())
}

/// Stop (kill) a deployment's service.
pub fn stop(unit: &str) -> Result<()> {
    require_linux()?;
    systemctl(&["stop", unit])
}

/// Restart a deployment's service.
pub fn restart(unit: &str) -> Result<()> {
    require_linux()?;
    systemctl(&["restart", unit])
}

/// Report a unit's state as one of running|stopped|crashed|unknown.
pub fn state(unit: &str) -> String {
    if cfg!(not(target_os = "linux")) {
        return "unknown".to_string();
    }
    let out = Command::new("systemctl").args(["is-active", unit]).output();
    match out {
        Ok(o) => match String::from_utf8_lossy(&o.stdout).trim() {
            "active" => "running".to_string(),
            "failed" => "crashed".to_string(),
            "inactive" | "deactivating" => "stopped".to_string(),
            "activating" => "running".to_string(),
            other if other.is_empty() => "unknown".to_string(),
            _ => "unknown".to_string(),
        },
        Err(_) => "unknown".to_string(),
    }
}

/// Fetch the last `lines` of a unit's journal.
pub fn logs(unit: &str, lines: u32) -> Result<String> {
    require_linux()?;
    let out = Command::new("journalctl")
        .args([
            "-u",
            unit,
            "-n",
            &lines.to_string(),
            "--no-pager",
            "--output=short-iso",
        ])
        .output()
        .context("running journalctl")?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if !err.trim().is_empty() {
            text.push_str("\n[journalctl] ");
            text.push_str(err.trim());
        }
    }
    Ok(text)
}

// --- internals ---

fn require_linux() -> Result<()> {
    if cfg!(not(target_os = "linux")) {
        bail!("deployments are supported only on systemd Linux hosts");
    }
    Ok(())
}

fn systemctl(args: &[&str]) -> Result<()> {
    let out = Command::new("systemctl")
        .args(args)
        .output()
        .with_context(|| format!("running systemctl {:?}", args))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("systemctl {:?} failed: {}", args, err.trim());
    }
    Ok(())
}

fn run_in(dir: &Path, cmd: &str) -> Result<()> {
    let out = Command::new("/bin/sh")
        .arg("-lc")
        .arg(cmd)
        .current_dir(dir)
        .output()
        .context("spawning shell")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let so = String::from_utf8_lossy(&out.stdout);
        bail!("command exited non-zero: {}{}", so.trim(), err.trim());
    }
    Ok(())
}

/// Write the systemd unit file for a deployment. Requires root (the agent runs
/// as root under systemd, same as its own installer). EnvironmentFile makes
/// systemd load the env file into the process before ExecStart.
fn install_unit(spec: &DeploySpec, current: &Path) -> Result<()> {
    let unit = format!(
        "[Unit]\n\
         Description=sglaz deployment {name}\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         WorkingDirectory={wd}\n\
         EnvironmentFile=-{env}\n\
         ExecStart=/bin/sh -lc 'exec {run}'\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        name = spec.unit_name,
        wd = current.display(),
        env = spec.env_file,
        run = spec.run_cmd,
    );
    let path = format!("/etc/systemd/system/{}.service", spec.unit_name);
    std::fs::write(&path, unit).with_context(|| format!("writing {}", path))?;
    Ok(())
}

/// Atomically repoint `link` at `target` by creating a temp symlink and
/// renaming it over the old one (rename is atomic on the same directory).
fn symlink_swap(target: &Path, link: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let tmp = link.with_extension("new");
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(target, &tmp)
            .with_context(|| format!("symlink {} -> {}", tmp.display(), target.display()))?;
        std::fs::rename(&tmp, link)
            .with_context(|| format!("renaming symlink into place at {}", link.display()))?;
    }
    Ok(())
}

fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod +x {}", path.display()))?;
    }
    Ok(())
}

/// Extract a .tar.gz into `dest`, stripping the single top-level directory that
/// GitHub source tarballs wrap everything in (like `tar --strip-components=1`).
fn extract_tar_gz(bytes: &[u8], dest: &Path) -> Result<()> {
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries().context("reading tar entries")? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        // Strip the leading component (e.g. "owner-repo-<sha>/").
        let stripped: PathBuf = path.components().skip(1).collect();
        if stripped.as_os_str().is_empty() {
            continue;
        }
        let out_path = dest.join(&stripped);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        entry
            .unpack(&out_path)
            .with_context(|| format!("unpacking {}", out_path.display()))?;
    }
    Ok(())
}

/// Guard against path traversal / odd characters in a version string used as a
/// directory name.
fn sanitize(version: &str) -> String {
    version
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect()
}
