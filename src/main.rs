// On Windows, build as a GUI-subsystem exe so there is no terminal on startup;
// the daemon shows a tray icon and an on-demand log console instead (see tray.rs).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod config;
mod deploy;
mod envfile;
#[cfg(windows)]
mod tray;

use anyhow::{bail, Context, Result};
use config::{Binding, Config, DeployRecord};
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
struct DeployReport {
    #[serde(rename = "deploymentId")]
    deployment_id: String,
    state: String,
    version: String,
    error: String,
}

#[derive(Serialize)]
struct PollReq {
    version: String,
    arch: String,
    #[serde(rename = "serverUrl")]
    server_url: String,
    snapshots: Vec<SnapshotReport>,
    deployments: Vec<DeployReport>,
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
struct DeployJobIn {
    action: String,
    #[serde(default)]
    version: String,
    #[serde(default, rename = "signedUrl")]
    signed_url: String,
    #[serde(default)]
    kind: String,
    #[serde(default, rename = "runCmd")]
    run_cmd: String,
    #[serde(default, rename = "prepareCmd")]
    prepare_cmd: String,
    #[serde(default, rename = "deployRoot")]
    deploy_root: String,
    #[serde(default, rename = "unitName")]
    unit_name: String,
    #[serde(default, rename = "envFile")]
    env_file: String,
    #[serde(default, rename = "envValues")]
    env_values: Vec<EnvVar>,
}

#[derive(Deserialize)]
struct DeploymentJobIn {
    #[serde(rename = "deploymentId")]
    deployment_id: String,
    #[serde(default, rename = "unitName")]
    unit_name: String,
    #[serde(default)]
    job: Option<DeployJobIn>,
}

#[derive(Deserialize)]
struct LogReqIn {
    #[serde(rename = "requestId")]
    request_id: String,
    #[serde(default, rename = "unitName")]
    unit_name: String,
    #[serde(default)]
    lines: u32,
}

#[derive(Serialize)]
struct LogResultOut {
    #[serde(rename = "requestId")]
    request_id: String,
    text: String,
    error: String,
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
    deployments: Vec<DeploymentJobIn>,
    #[serde(default)]
    logs: Option<LogReqIn>,
    #[serde(default, rename = "serverUrl")]
    server_url: String,
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
        deployments: Vec::new(),
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

    // Report each known deployment's live service state + running version.
    let deploy_reports: Vec<DeployReport> = cfg
        .deployments
        .iter()
        .map(|d| DeployReport {
            deployment_id: d.deployment_id.clone(),
            state: deploy::state(&d.unit_name),
            version: d.version.clone(),
            error: String::new(),
        })
        .collect();

    let resp = http
        .post(format!("{}/api/client/poll", cfg.server_url))
        .bearer_auth(&cfg.token)
        .json(&PollReq {
            version: VERSION.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            server_url: cfg.server_url.clone(),
            snapshots,
            deployments: deploy_reports,
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

    // Apply deployment actions (deploy / restart / stop) and remember versions.
    // deployed_versions carries a freshly-deployed version onto the persisted
    // record so the next poll reports it.
    let mut deployed_versions: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for dj in &pr.deployments {
        let Some(job) = &dj.job else { continue };
        match job.action.as_str() {
            "deploy" => {
                let spec = deploy::DeploySpec {
                    version: job.version.clone(),
                    signed_url: job.signed_url.clone(),
                    kind: job.kind.clone(),
                    run_cmd: job.run_cmd.clone(),
                    prepare_cmd: job.prepare_cmd.clone(),
                    deploy_root: job.deploy_root.clone(),
                    unit_name: job.unit_name.clone(),
                    env_file: job.env_file.clone(),
                    env_values: job.env_values.clone(),
                };
                match deploy::deploy(http, &spec) {
                    Ok(()) => {
                        deployed_versions.insert(dj.deployment_id.clone(), job.version.clone());
                    }
                    Err(e) => eprintln!("sglaz: deploy {} failed: {:#}", dj.deployment_id, e),
                }
            }
            "restart" => {
                if let Err(e) = deploy::restart(&job.unit_name) {
                    eprintln!("sglaz: restart {} failed: {:#}", dj.deployment_id, e);
                }
            }
            "stop" => {
                if let Err(e) = deploy::stop(&job.unit_name) {
                    eprintln!("sglaz: stop {} failed: {:#}", dj.deployment_id, e);
                }
            }
            other => eprintln!("sglaz: unknown deploy action {:?}", other),
        }
    }

    // Answer a log request: fetch the unit's journal and post it back.
    if let Some(lr) = &pr.logs {
        let lines = if lr.lines == 0 { 200 } else { lr.lines };
        let (text, error) = match deploy::logs(&lr.unit_name, lines) {
            Ok(t) => (t, String::new()),
            Err(e) => (String::new(), format!("{:#}", e)),
        };
        let _ = http
            .post(format!("{}/api/client/log-result", cfg.server_url))
            .bearer_auth(&cfg.token)
            .json(&LogResultOut {
                request_id: lr.request_id.clone(),
                text,
                error,
            })
            .send();
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
    // Rebuild the deployment records from the server (authoritative), carrying
    // over each one's known running version (or the just-deployed version).
    let old_versions: std::collections::HashMap<&str, &str> = cfg
        .deployments
        .iter()
        .map(|d| (d.deployment_id.as_str(), d.version.as_str()))
        .collect();
    let new_deployments: Vec<DeployRecord> = pr
        .deployments
        .iter()
        .map(|dj| {
            let version = deployed_versions
                .get(&dj.deployment_id)
                .cloned()
                .or_else(|| old_versions.get(dj.deployment_id.as_str()).map(|s| s.to_string()))
                .unwrap_or_default();
            DeployRecord {
                deployment_id: dj.deployment_id.clone(),
                unit_name: dj.unit_name.clone(),
                version,
            }
        })
        .collect();

    let bindings_changed = bindings_differ(&cfg.bindings, &new_bindings);
    let deployments_changed = deployments_differ(&cfg.deployments, &new_deployments);
    if bindings_changed || deployments_changed || !deployed_versions.is_empty() {
        cfg.bindings = new_bindings;
        cfg.deployments = new_deployments;
        cfg.save().ok();
    }

    // Adopt a server-pushed base-URL switch. Both URLs must point at the same
    // server (token stays valid); the next poll goes to the new URL.
    let new_url = pr.server_url.trim().trim_end_matches('/').to_string();
    if !new_url.is_empty() && new_url != cfg.server_url {
        println!("sglaz: switching server URL {} -> {}", cfg.server_url, new_url);
        cfg.server_url = new_url;
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

fn deployments_differ(a: &[DeployRecord], b: &[DeployRecord]) -> bool {
    if a.len() != b.len() {
        return true;
    }
    for (x, y) in a.iter().zip(b.iter()) {
        if x.deployment_id != y.deployment_id
            || x.unit_name != y.unit_name
            || x.version != y.version
        {
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
