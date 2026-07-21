// On Windows, build as a GUI-subsystem exe so there is no terminal on startup;
// the daemon shows a tray icon and an on-demand log console instead (see tray.rs).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod config;
mod envfile;
#[cfg(windows)]
mod tray;

use anyhow::{bail, Context, Result};
use config::{Binding, Config};
use envfile::EnvVar;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Default server if none is given via --server or SGLAZ_SERVER.
const DEFAULT_SERVER: &str = "http://localhost:17823";

/// This binary's version, from Cargo.
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Serialize)]
struct EnrollReq {
    code: String,
    hostname: String,
    os: String,
    arch: String,
    version: String,
}

#[derive(Deserialize)]
struct EnrollResp {
    token: String,
    #[serde(rename = "clientId")]
    client_id: String,
    name: String,
    #[serde(rename = "pollIntervalSecs")]
    poll_interval_secs: u64,
}

#[derive(Serialize)]
struct SnapshotReport {
    #[serde(rename = "bindingId")]
    binding_id: String,
    snapshot: Vec<EnvVar>,
}

#[derive(Serialize)]
struct PollReq {
    version: String,
    arch: String,
    snapshots: Vec<SnapshotReport>,
}

#[derive(Deserialize)]
struct Job {
    #[serde(rename = "type")]
    job_type: String,
    #[serde(rename = "filePath")]
    file_path: String,
    values: Vec<EnvVar>,
}

#[derive(Deserialize)]
struct BindingJob {
    #[serde(rename = "bindingId")]
    binding_id: String,
    #[serde(default, rename = "appName")]
    app_name: String,
    #[serde(default, rename = "filePath")]
    file_path: String,
    #[serde(default)]
    job: Option<Job>,
}

#[derive(Deserialize)]
struct Upgrade {
    version: String,
    url: String,
}

#[derive(Deserialize)]
struct BrowseReq {
    #[serde(rename = "requestId")]
    request_id: String,
    path: String,
    /// Optional create-then-list action: "mkdir" or "touch".
    #[serde(default)]
    action: Option<String>,
    /// Bare name of the folder/file to create (no path separators).
    #[serde(default)]
    name: Option<String>,
}

#[derive(Serialize)]
struct BrowseEntryOut {
    name: String,
    dir: bool,
    path: String,
}

#[derive(Serialize)]
struct BrowseResultOut {
    #[serde(rename = "requestId")]
    request_id: String,
    path: String,
    parent: String,
    entries: Vec<BrowseEntryOut>,
    error: String,
}

#[derive(Deserialize)]
struct PollResp {
    #[serde(rename = "pollIntervalSecs")]
    poll_interval_secs: u64,
    #[serde(default)]
    bindings: Vec<BindingJob>,
    #[serde(default)]
    upgrade: Option<Upgrade>,
    #[serde(default)]
    browse: Option<BrowseReq>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let want_help = args.iter().any(|a| a == "-h" || a == "--help");
    let connect_code = flag_value(&args, &["-connect", "--connect"]);

    // On Windows the exe has no console of its own. For the one-shot commands
    // (`--help`, `-connect`) attach to the launching terminal so output shows.
    #[cfg(windows)]
    if want_help || connect_code.is_some() {
        tray::attach_parent_console();
    }

    let result = if want_help {
        print_help();
        Ok(())
    } else if let Some(code) = connect_code {
        let server = flag_value(&args, &["-server", "--server"])
            .or_else(|| std::env::var("SGLAZ_SERVER").ok())
            .unwrap_or_else(|| DEFAULT_SERVER.to_string());
        enroll(&server, &code)
    } else {
        // The sync daemon. On Windows it runs under a tray icon with an
        // on-demand log console; elsewhere it's a plain foreground loop.
        #[cfg(windows)]
        {
            tray::run(run)
        }
        #[cfg(not(windows))]
        {
            run()
        }
    };

    if let Err(e) = result {
        eprintln!("sglaz: error: {:#}", e);
        std::process::exit(1);
    }
}

/// Enroll with the server using a one-time code; persist the returned token.
fn enroll(server: &str, code: &str) -> Result<()> {
    let server = server.trim_end_matches('/').to_string();
    let hostname = gethostname::gethostname().to_string_lossy().to_string();
    let os = std::env::consts::OS.to_string();

    let http = http_client()?;
    let resp = http
        .post(format!("{}/api/client/enroll", server))
        .json(&EnrollReq {
            code: code.trim().to_uppercase(),
            hostname,
            os,
            arch: std::env::consts::ARCH.to_string(),
            version: VERSION.to_string(),
        })
        .send()
        .with_context(|| format!("could not reach server at {}", server))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("enrollment rejected ({}): {}", status, body.trim());
    }

    let er: EnrollResp = resp.json().context("unexpected enroll response")?;

    // One identity per machine — overwrite. Bindings are learned from the server.
    let cfg = Config {
        server_url: server,
        token: er.token,
        client_id: er.client_id,
        poll_interval_secs: er.poll_interval_secs.max(3),
        bindings: Vec::new(),
    };
    cfg.save()?;

    println!("sglaz: connected as '{}'.", er.name);
    println!("sglaz: config saved to {}", config::config_path()?.display());
    println!("sglaz: now run `sglaz` (or restart the service) to sync.");
    Ok(())
}

/// Run the poll loop: report each attached app's file, apply sync-down jobs.
/// One process, one identity, many apps (bindings) driven by the server.
fn run() -> Result<()> {
    let mut cfg = Config::load()?;
    println!(
        "sglaz: 👁 watching. server={} interval={}s",
        cfg.server_url, cfg.poll_interval_secs
    );
    let http = http_client()?;
    let mut interval = cfg.poll_interval_secs.max(1);
    loop {
        match poll_once(&http, &mut cfg) {
            Ok(next) => interval = next.max(1),
            Err(e) => eprintln!("sglaz: poll error: {:#}", e),
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
}

/// Poll once; returns the interval (seconds) the server wants before the next poll.
fn poll_once(http: &reqwest::blocking::Client, cfg: &mut Config) -> Result<u64> {
    // Report the current contents of each known binding's file.
    let snapshots: Vec<SnapshotReport> = cfg
        .bindings
        .iter()
        .filter(|b| !b.file_path.is_empty())
        .map(|b| SnapshotReport {
            binding_id: b.binding_id.clone(),
            snapshot: envfile::read(&b.file_path),
        })
        .collect();

    let resp = http
        .post(format!("{}/api/client/poll", cfg.server_url))
        .bearer_auth(&cfg.token)
        .json(&PollReq {
            version: VERSION.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            snapshots,
        })
        .send()
        .context("poll request failed")?;

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        bail!("server rejected token — re-enroll with `sglaz -connect <code>`");
    }
    if !resp.status().is_success() {
        bail!("poll returned status {}", resp.status());
    }

    let pr: PollResp = resp.json().context("unexpected poll response")?;

    // Apply sync-down write jobs.
    for bj in &pr.bindings {
        if let Some(job) = &bj.job {
            if job.job_type == "write" {
                let path = if job.file_path.is_empty() {
                    bj.file_path.clone()
                } else {
                    job.file_path.clone()
                };
                if path.is_empty() {
                    eprintln!("sglaz[{}]: write job has no file path", bj.app_name);
                    continue;
                }
                envfile::write(&path, &job.values)?;
                println!("sglaz[{}]: wrote {} value(s) to {}", bj.app_name, job.values.len(), path);
            }
        }
    }

    // Answer a file-browse request if the server asked. It may first ask us to
    // create a folder or an empty file, then list the (now-updated) directory.
    if let Some(b) = &pr.browse {
        let mut action_err = String::new();
        if let (Some(action), Some(name)) = (b.action.as_deref(), b.name.as_deref()) {
            if let Err(e) = create_item(&b.path, action, name) {
                action_err = e;
            }
        }
        let mut out = list_dir(&b.path, &b.request_id);
        if !action_err.is_empty() && out.error.is_empty() {
            out.error = action_err;
        }
        let _ = http
            .post(format!("{}/api/client/browse-result", cfg.server_url))
            .bearer_auth(&cfg.token)
            .json(&out)
            .send();
    }

    // Rebuild the local binding list from the server (authoritative). Persist
    // if it changed so a restart can report snapshots immediately.
    let new_bindings: Vec<Binding> = pr
        .bindings
        .iter()
        .map(|bj| Binding {
            binding_id: bj.binding_id.clone(),
            app_name: bj.app_name.clone(),
            file_path: bj.file_path.clone(),
        })
        .collect();
    if bindings_differ(&cfg.bindings, &new_bindings) {
        cfg.bindings = new_bindings;
        cfg.save().ok();
    }

    // Apply a self-upgrade last (replaces the binary and exits; systemd restarts).
    if let Some(up) = pr.upgrade {
        if up.version != VERSION {
            self_upgrade(http, &up.url, &up.version)?;
        }
    }
    Ok(pr.poll_interval_secs)
}

/// The available drive roots on Windows (C:\, D:\, …), used as the "/" listing.
#[cfg(windows)]
fn windows_drives() -> Vec<BrowseEntryOut> {
    let mut out = Vec::new();
    for c in b'A'..=b'Z' {
        let letter = c as char;
        let root = format!("{}:\\", letter);
        if std::path::Path::new(&root).exists() {
            out.push(BrowseEntryOut {
                name: format!("{}:", letter),
                dir: true,
                path: root,
            });
        }
    }
    out
}

/// Create a folder ("mkdir") or an empty file ("touch") named `name` inside
/// `dir`, for the picker's "new folder / new .env" controls. `name` is a bare
/// filename; the server rejects path separators before this ever runs.
fn create_item(dir: &str, action: &str, name: &str) -> Result<(), String> {
    let target = std::path::Path::new(dir).join(name);
    match action {
        "mkdir" => std::fs::create_dir_all(&target).map_err(|e| e.to_string()),
        "touch" => {
            if target.exists() {
                return Ok(()); // already there — just let the user select it
            }
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(&target)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        _ => Err(format!("unknown action: {}", action)),
    }
}

/// List a directory for the file picker. Returns entries (dirs first), the
/// canonical path, and its parent ("" if none).
fn list_dir(req_path: &str, request_id: &str) -> BrowseResultOut {
    let mut path = req_path.to_string();
    if path.is_empty() {
        path = "/".to_string();
    }

    // On Windows, "/" is the drive chooser (there is no single filesystem root).
    #[cfg(windows)]
    if path == "/" {
        return BrowseResultOut {
            request_id: request_id.to_string(),
            path: "/".to_string(),
            parent: String::new(),
            entries: windows_drives(),
            error: String::new(),
        };
    }

    let p = std::path::Path::new(&path);
    #[allow(unused_mut)]
    let mut parent = p
        .parent()
        .map(|x| x.to_string_lossy().to_string())
        .unwrap_or_default();
    // On Windows, "up" from a drive root (C:\) returns to the drive chooser.
    #[cfg(windows)]
    if parent.is_empty() && path != "/" {
        parent = "/".to_string();
    }

    match std::fs::read_dir(p) {
        Ok(rd) => {
            let mut entries: Vec<BrowseEntryOut> = rd
                .flatten()
                .map(|e| BrowseEntryOut {
                    name: e.file_name().to_string_lossy().to_string(),
                    dir: e.file_type().map(|t| t.is_dir()).unwrap_or(false),
                    path: e.path().to_string_lossy().to_string(),
                })
                .collect();
            entries.sort_by(|a, b| {
                b.dir
                    .cmp(&a.dir)
                    .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            BrowseResultOut {
                request_id: request_id.to_string(),
                path,
                parent,
                entries,
                error: String::new(),
            }
        }
        Err(err) => BrowseResultOut {
            request_id: request_id.to_string(),
            path,
            parent,
            entries: Vec::new(),
            error: format!("{}", err),
        },
    }
}

fn bindings_differ(a: &[Binding], b: &[Binding]) -> bool {
    if a.len() != b.len() {
        return true;
    }
    for (x, y) in a.iter().zip(b.iter()) {
        if x.binding_id != y.binding_id || x.file_path != y.file_path {
            return true;
        }
    }
    false
}

/// Download a new binary, replace this executable in place, and exit so the
/// service manager restarts us on the new version.
fn self_upgrade(http: &reqwest::blocking::Client, url: &str, version: &str) -> Result<()> {
    println!("sglaz: upgrading {} -> {} from {}", VERSION, version, url);
    let exe = std::env::current_exe().context("cannot locate current executable")?;
    // Stage the download next to the current exe (same filesystem for an atomic rename).
    let staged = exe.with_extension("new");

    let resp = http.get(url).send().context("download failed")?;
    if !resp.status().is_success() {
        bail!("download returned status {}", resp.status());
    }
    let bytes = resp.bytes().context("reading download body")?;
    std::fs::write(&staged, &bytes).with_context(|| format!("writing {}", staged.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        // On Linux/macOS a running binary can be replaced via rename.
        std::fs::rename(&staged, &exe).context("replacing binary")?;
    }
    #[cfg(windows)]
    {
        // Windows can't overwrite a running exe: move it aside first.
        let old = exe.with_extension("old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&exe, &old).context("moving old binary aside")?;
        std::fs::rename(&staged, &exe).context("installing new binary")?;
        // No service manager on Windows (tray/Startup-folder setup), so relaunch
        // the new binary ourselves — by absolute path, no args (daemon mode) —
        // before this old process exits.
        std::process::Command::new(&exe)
            .spawn()
            .context("relaunching upgraded binary")?;
    }

    println!("sglaz: upgraded to {}. restarting.", version);
    std::process::exit(0);
}

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build http client")
}

/// Find the value following any of `names` in the argument list.
fn flag_value(args: &[String], names: &[&str]) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        if names.contains(&a.as_str()) {
            return args.get(i + 1).cloned();
        }
    }
    None
}

fn print_help() {
    println!(
        "sglaz client agent

USAGE:
    sglaz -connect <CODE> [--server <URL>]   Enroll this machine with the server
    sglaz                                    Run the sync daemon (after enrolling)

OPTIONS:
    --server <URL>   Server base URL for enrollment (default: {default},
                     or set SGLAZ_SERVER). Saved after enrolling.
    -h, --help       Show this help

CONFIG:
    Stored at the OS config dir, override with SGLAZ_CONFIG.",
        default = DEFAULT_SERVER
    );
}
