# sglaz client

The agent half of **sglaz**. It runs on a machine, connects to the sglaz
server, and keeps a `.env` file in sync with an environment defined on the
server. Sync is **pull-based**: the client polls the server on an interval, so
it works fine behind NAT/firewalls (it only makes outbound requests).

---

## How it works (30 seconds)

1. In the server UI, open an App → **Add client** → you get a one-time code.
2. On the target machine: `sglaz -connect <CODE> --server https://your-server`
   This enrolls the machine and writes a `config.yaml` containing a permanent
   token. The code is now used up.
3. Back in the UI, assign the client an **Environment** and a **.env file path**.
4. Run `sglaz` (usually as a service). It polls every ~15s:
   - reports the current contents of its `.env` file (so the server can show a
     diff and let you **Sync ↑**),
   - applies any **Sync ↓** the server has queued (writes the file).

---

## Commands

```
sglaz -connect <CODE> [--server <URL>]   Enroll this machine (one time)
sglaz                                    Run the sync daemon (after enrolling)
sglaz --help                             Show help
```

- `--server <URL>` is only needed at enroll time; it's saved into the config.
  Precedence: `--server` flag → `SGLAZ_SERVER` env var → built-in default
  (`http://localhost:17823`).

---

## Where is the config file?

`config.yaml` lives in the standard per-OS config directory:

| OS       | Path                                                     |
|----------|----------------------------------------------------------|
| Linux    | `~/.config/sglaz/config.yaml` (`$XDG_CONFIG_HOME/sglaz/`) |
| macOS    | `~/Library/Application Support/sglaz/config.yaml`         |
| Windows  | `%APPDATA%\sglaz\config\config.yaml`                      |

**Override with `SGLAZ_CONFIG`** — set it to an absolute path. This is what you
want for a system service, where `$HOME` may be unset or wrong:

```
SGLAZ_CONFIG=/etc/sglaz/config.yaml sglaz -connect ABCD1234 --server https://your-server
```

The file is created on first `-connect`. It contains a token — treat it like a
credential (it's in `.gitignore`).

---

## Managing multiple apps on one machine

You enroll **once per machine** — the agent has a single identity (one token).
It then manages **as many apps as you attach to it on the server**: the server
sends the list of apps (bindings) on every poll, each with its own `.env` path.
No extra `-connect` calls; just attach more apps from their App pages in the UI
and the running agent picks them up automatically.

## Config fields

One identity, plus a server-driven list of app bindings:

```yaml
server_url: https://your-server         # where to poll; set at enroll time
token: 3f9a...                          # bearer token issued on enroll (secret)
client_id: 6a5d40d941243bcd3a1b6dbc     # this agent's id on the server
poll_interval_secs: 15                  # how often to poll (server can adjust)
bindings:                               # learned from the server; do not hand-edit
  - binding_id: 6a5d40d941243bcd3a1b6dbb
    app_name: api-server
    file_path: /opt/app/.env
  - binding_id: 6a5d40d941243bcd3a1b6dbc
    app_name: worker
    file_path: /opt/worker/.env
```

- `bindings` are authoritative from the server — attach/detach apps in the UI and
  the agent reconciles on its next poll. Persisted so a restart reports files fast.
- `poll_interval_secs` is seeded from the server and re-synced each poll (min 3s).

---

## Cross-compiling (build once on your Mac, run on a Linux VPS)

The binary uses **rustls** (no OpenSSL), so a fully static **musl** build is the
easiest thing to ship — one file, no glibc/dependency surprises across distros.

From an Apple-Silicon Mac, the no-Docker path uses `cargo-zigbuild`:

```bash
brew install zig
cargo install cargo-zigbuild
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

# Intel/AMD VPS:
cargo zigbuild --release --target x86_64-unknown-linux-musl
#   -> target/x86_64-unknown-linux-musl/release/sglaz

# ARM VPS (Graviton, Ampere, etc.):
cargo zigbuild --release --target aarch64-unknown-linux-musl
```

Docker-based alternative (`cross`) works too:

```bash
cargo install cross
cross build --release --target x86_64-unknown-linux-musl
```

Windows target: `x86_64-pc-windows-gnu` (add the target, then `cargo zigbuild`).

**Hosting the binary:** upload the built file as a **GitHub Release asset** (or
any static URL — S3, or even the sglaz server itself). The server only needs a
URL it can `curl`. That URL is what the (planned) auto-install feature will use.

---

## Running as a service (Linux / systemd)

Install the binary and a service so it starts on boot and restarts on crash:

```bash
sudo install -m755 sglaz /usr/local/bin/sglaz
sudo mkdir -p /etc/sglaz

# enroll once (writes /etc/sglaz/config.yaml)
sudo SGLAZ_CONFIG=/etc/sglaz/config.yaml /usr/local/bin/sglaz \
     -connect <CODE> --server https://your-server

sudo tee /etc/systemd/system/sglaz.service >/dev/null <<'UNIT'
[Unit]
Description=sglaz client agent
After=network-online.target
Wants=network-online.target

[Service]
Environment=SGLAZ_CONFIG=/etc/sglaz/config.yaml
ExecStart=/usr/local/bin/sglaz
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT

sudo systemctl daemon-reload
sudo systemctl enable --now sglaz
sudo systemctl status sglaz     # check it's running
journalctl -u sglaz -f          # follow logs
```

`Restart=always` also enables clean self-upgrades later: replace the binary on
disk and `systemctl restart sglaz`.

---

## Is it online? / did the install work?

The client's poll doubles as a heartbeat. The server records `lastSeen` on every
poll and shows each client as **online** (seen within ~3 poll intervals) or
**offline**. So "did the install succeed?" == "did the client check in?" — watch
the client's status flip to online in the App page shortly after install.
