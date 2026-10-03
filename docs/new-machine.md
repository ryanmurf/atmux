# Add a new computer to the atmux fleet

This runbook turns a new Linux or macOS computer into an atmux **owner node**: it runs its own
tmux server and agents, appears in the dashboard at <https://atmux.murphytek.com>, and can be
driven by other agents. It is written so an agent can follow it step by step.

There are two roles:

- **NEW**: an agent (or person) with a shell on the new computer.
- **TRON**: an agent on Tron, which holds the private CA key, the atmux source, and `kubectl`/`helm`
  access to the coordinator. Steps marked TRON cannot be done from the new computer.

Never print, log, or paste a token or private key. Copy them only with `scp`/`ssh` pipes as shown.

## 0. Decide the identity

Pick these values and use them everywhere below:

| Variable | Meaning | Example |
| --- | --- | --- |
| `ID` | lowercase machine id, unique in the fleet | `flynn` |
| `LABEL` | display name | `Flynn` |
| `ADDR` | IP the coordinator and Tron use to reach this machine (LAN IP, or public IP for a cloud host) | `192.168.0.140` |
| `HOMEDIR` | the account's home directory | `/home/ryan` or `/Users/ryan` |

Existing ids: `tron`, `max`, `midnight`, `clue`, and the coordinator `home`. TCP port `7345` on
`ADDR` must be reachable from Tron (192.168.0.109). Open it in the host firewall or cloud security
list for that source only.

## 1. NEW: install prerequisites

Linux (Debian/Ubuntu):

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config git tmux curl ca-certificates openssl
sudo loginctl enable-linger "$USER"   # keeps the user service running without a login
```

macOS:

```bash
xcode-select --install          # if the command line tools are missing
brew install tmux git openssl
```

Rust (both): install with rustup, then confirm `rustc --version` is 1.88 or newer.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
```

Agent CLIs:

```bash
curl -fsSL https://claude.ai/install.sh | bash            # Claude Code -> ~/.local/bin/claude
curl -fsSL https://chatgpt.com/codex/install.sh | sh        # Codex standalone installer
```

**Logins need the human.** Ask Ryan to run these interactively on the new computer (they open a
browser or show a code):

```bash
CLAUDE_CONFIG_DIR="$HOME/.claude-max" claude   # then /login with the personal Max account
CLAUDE_CONFIG_DIR="$HOME/.claude-hd"  claude   # then /login with the HeroDevs account
codex login
```

Match the fleet's Codex defaults so unattended agents never stop at a prompt. In
`~/.codex/config.toml`:

```toml
approval_policy = "never"
sandbox_mode = "danger-full-access"

[projects."HOMEDIR/IdeaProjects"]
trust_level = "trusted"
```

On macOS, Claude Code keeps its credentials in the login Keychain, which is only reachable from
processes started in the Aqua (logged-in desktop) session. That is why the macOS service in step 6
is a LaunchAgent and why atmux must never be started from an SSH shell on a Mac.

## 2. TRON: send the source

The fleet runs the `dashboard-improvements-20260907` branch, which is ahead of GitHub. Stream it from
Tron (the new computer must accept SSH from Tron; add Tron's public key to its
`~/.ssh/authorized_keys` if needed):

```bash
ssh NEW 'mkdir -p ~/IdeaProjects/atmux'
git -C ~/IdeaProjects/atmux archive --format=tar dashboard-improvements-20260907 \
  | ssh NEW 'tar -x -C ~/IdeaProjects/atmux'
```

For a Linux x86_64 machine you can instead copy Tron's built binary
(`~/IdeaProjects/atmux/target/release/atmux`) to the same path and skip step 3.

## 3. NEW: build

```bash
cd ~/IdeaProjects/atmux
cargo build --release --locked --all-features
./target/release/atmux --version
```

## 4. NEW: key, certificate request, and node token

```bash
ID=flynn                                   # your value from step 0
mkdir -p ~/.config/atmux/tls && chmod 700 ~/.config/atmux ~/.config/atmux/tls
openssl ecparam -name prime256v1 -genkey -noout \
  | openssl pkcs8 -topk8 -nocrypt -out ~/.config/atmux/tls/$ID.key
chmod 600 ~/.config/atmux/tls/$ID.key
openssl req -new -key ~/.config/atmux/tls/$ID.key -subj "/CN=$ID" -out ~/.config/atmux/tls/$ID.csr
openssl rand -hex 32 > ~/.config/atmux/node.token && chmod 600 ~/.config/atmux/node.token
```

The token is the bearer every caller must present to this node. It stays on this machine and in
the coordinator's secret only.

## 5. TRON: sign the certificate

```bash
ID=flynn; ADDR=192.168.0.140; NEW=user@$ADDR
cd ~/.config/atmux/tls
scp "$NEW:.config/atmux/tls/$ID.csr" .
cat > "/tmp/$ID.ext" <<EOF
basicConstraints=critical,CA:FALSE
extendedKeyUsage=serverAuth,clientAuth
subjectAltName=IP:$ADDR,DNS:$ID
EOF
openssl x509 -req -in "$ID.csr" -CA ca.crt -CAkey ca.key -CAserial ca.srl \
  -days 1095 -sha256 -extfile "/tmp/$ID.ext" -out "$ID.crt"
openssl x509 -in "$ID.crt" -noout -subject -ext subjectAltName   # check CN and IP
scp "$ID.crt" ca.crt "$NEW:.config/atmux/tls/"
```

If the machine is reachable at more than one address, list every IP in `subjectAltName`
(`IP:a,IP:b,DNS:id`). The coordinator verifies the IP it dials.

## 6. NEW: configuration and service

Create `~/.config/atmux/config.toml` (mode 600). Replace `ID`, `LABEL`, and `HOMEDIR`:

```toml
[general]
project_roots = ["~/IdeaProjects"]
favorite_dirs = []

# Codex. Add the model modes you use; ids are what launchers and the intake select.
[[profiles]]
name = "Default"
harness = "codex"
command = "codex"
args = []
[[profiles.modes]]
id = "sol61-xhigh"
label = "Sol 6.1 · xhigh"
model = "gpt-6.1-sol"
effort = "xhigh"

# Claude profiles bind each config store explicitly.
[[profiles]]
name = "max"
harness = "claude"
command = "claude"
args = []
inherit_discovered = true
claude_relaunch_permissions = "launcher_provides"
[profiles.env]
CLAUDE_CONFIG_DIR = "HOMEDIR/.claude-max"
[[profiles.modes]]
id = "opus"
model = "opus"
[[profiles.modes]]
id = "fable"
model = "fable"

[[profiles]]
name = "hd"
harness = "claude"
command = "claude"
args = []
inherit_discovered = true
claude_relaunch_permissions = "launcher_provides"
[profiles.env]
CLAUDE_CONFIG_DIR = "HOMEDIR/.claude-hd"
[[profiles.modes]]
id = "opus"
model = "opus"
[[profiles.modes]]
id = "fable"
model = "fable"

[node]
id = "ID"
label = "LABEL"
token_file = "~/.config/atmux/node.token"

[node.tls]
cert_file = "HOMEDIR/.config/atmux/tls/ID.crt"
key_file = "HOMEDIR/.config/atmux/tls/ID.key"
ca_file = "HOMEDIR/.config/atmux/tls/ca.crt"

[discovery]
enabled = false

# Agent control plane: lifecycle events and native CLI hooks, the session registry with
# archive-on-close, and automatic answers to known startup prompts.
[events]
inject_hooks = true

[registry]
enabled = true

[startup_prompts]
auto_answer = true

# Keep Claude and Codex current and resume idle agents after an update.
[maintenance]
enabled = true
interval_minutes = 30
update_timeout_seconds = 180
relaunch_limit = 4
```

Validate before starting anything. Look for any line containing `invalid`:

```bash
~/IdeaProjects/atmux/target/release/atmux --config ~/.config/atmux/config.toml doctor
```

### Linux service (systemd user unit)

`~/.config/systemd/user/atmux-web.service`:

```ini
[Unit]
Description=atmux owner node
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
Environment=PATH=%h/.local/bin:%h/.cargo/bin:/usr/local/bin:/usr/bin:/bin
WorkingDirectory=%h
ExecStart=%h/IdeaProjects/atmux/target/release/atmux --config %h/.config/atmux/config.toml web --bind 0.0.0.0:7345 --allow-remote --allowed-host ADDR:7345
Restart=on-failure
RestartSec=3
# The first agent launch can create the tmux server inside this unit's cgroup.
# Stop only atmux itself so a restart never kills tmux and every agent.
KillMode=process
UMask=0077
NoNewPrivileges=true

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now atmux-web.service
systemctl --user status atmux-web.service --no-pager
```

Restart later with `systemctl --user restart atmux-web.service`. Never remove `KillMode=process`.

### macOS service (LaunchAgent in the Aqua session)

`~/Library/LaunchAgents/dev.herodevs.atmux-web.plist` runs atmux inside a tmux session named
`atmux-web` so it inherits Keychain access:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.herodevs.atmux-web</string>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><false/>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>/opt/homebrew/bin:HOMEDIR/.local/bin:HOMEDIR/.cargo/bin:/usr/local/bin:/usr/bin:/bin</string></dict>
  <key>WorkingDirectory</key><string>HOMEDIR/IdeaProjects/atmux</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/sh</string><string>-c</string>
    <string>CMD="exec $HOME/IdeaProjects/atmux/target/release/atmux --config $HOME/.config/atmux/config.toml web --bind 0.0.0.0:7345 --allow-remote --allowed-host ADDR:7345"; if /opt/homebrew/bin/tmux has-session -t atmux-web 2&gt;/dev/null; then /opt/homebrew/bin/tmux respawn-pane -k -c "$HOME/IdeaProjects/atmux" -t atmux-web:0.0 "$CMD"; else /opt/homebrew/bin/tmux new-session -d -s atmux-web -c "$HOME/IdeaProjects/atmux" "$CMD"; fi; /opt/homebrew/bin/tmux set -g exit-empty off</string>
  </array>
  <key>StandardOutPath</key><string>/tmp/atmux-web.log</string>
  <key>StandardErrorPath</key><string>/tmp/atmux-web.log</string>
</dict>
</plist>
```

Load it **from the logged-in desktop session** (Terminal.app, not SSH):

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.herodevs.atmux-web.plist
```

Restart later only with `launchctl kickstart -k gui/$(id -u)/dev.herodevs.atmux-web`. Never kill
or rebuild that machine's default tmux server and never create a replacement tmux socket.

### Local check

```bash
TLS=~/.config/atmux/tls
curl -s --cacert $TLS/ca.crt --cert $TLS/ID.crt --key $TLS/ID.key \
  -H "Authorization: Bearer $(cat ~/.config/atmux/node.token)" \
  https://ADDR:7345/api/v1/health
```

Expect a healthy JSON response. The token is read by `curl` from the file; do not echo it.

## 7. TRON: register the node with the coordinator

Add the node's token to the coordinator's secret without printing it:

```bash
ID=flynn; NEW=user@192.168.0.140
TOKEN_B64=$(ssh "$NEW" 'cat ~/.config/atmux/node.token' | tr -d '\n' | base64 -w0)
kubectl -n murphytek patch secret atmux-node-tokens --type merge \
  -p "{\"data\":{\"$ID\":\"$TOKEN_B64\"}}"
unset TOKEN_B64
```

Add the machine to the Helm values, bump the secret revision so the Pod remounts the token, and
upgrade with the chart from Tron's atmux checkout (the image is unchanged):

```bash
helm -n murphytek get values atmux-web -o yaml > /tmp/atmux-values.yaml
# Edit /tmp/atmux-values.yaml:
#   server.machines: append {id: ID, label: LABEL, address: ADDR, port: 7345, tokenKey: ID}
#   server.secretRevision: bump it (for example v1 -> v2)
helm -n murphytek upgrade atmux-web ~/IdeaProjects/atmux/deploy/helm/atmux-web -f /tmp/atmux-values.yaml --wait
```

The chart adds the matching NetworkPolicy egress rule for `ADDR:7345` automatically. Diff the
render against the live manifest first (`helm template … | diff - <(helm get manifest …)`) and
confirm only the new machine, its egress rule, and the revision changed.

Optionally let Tron's own node federate it too: copy the token to Tron as
`~/.config/atmux/tokens/ID.token` (mode 600) and add to Tron's `~/.config/atmux/config.toml`:

```toml
[[machines]]
id = "ID"
label = "LABEL"
url = "https://ADDR:7345"
token_file = "/home/ryan/.config/atmux/tokens/ID.token"
```

Validate with `atmux doctor` before restarting Tron's web pane (see the fleet rollout notes).

## 8. Verify

- The dashboard at <https://atmux.murphytek.com> lists `LABEL` as online with its metrics.
- Launch a test agent on it from **New agent**, confirm Conversation shows its messages and Raw
  pane fills the view, then kill it and confirm it appears as archived in **Sessions**.
- `atmux --config ~/.config/atmux/config.toml doctor` on NEW is clean apart from known warnings.

## 9. Optional: Quick Resume roster

To restore this machine's sessions after a reboot from the dashboard, copy
`deploy/quick-resume/quick-resume.example.sh` to `~/.config/atmux/quick-resume.sh` (mode 700) and
list its sessions. See `deploy/quick-resume/README.md`. With the registry enabled, the coordinator
can also restore sessions automatically when the node comes back (`restore_machines` on the
coordinator).

## 10. Letting agents use atmux

- **On the node itself:** atmux serves MCP at `http://127.0.0.1:7345/mcp` with the node token as a
  bearer. It sees this machine's sessions. For example, for Claude Code:
  `claude mcp add --transport http atmux http://127.0.0.1:7345/mcp --header "Authorization: Bearer $(cat ~/.config/atmux/node.token)"`
  (this stores the token in that Claude config; use a dedicated profile if that matters). The
  endpoint is stateless and modern-only: the client must speak MCP 2026-07-28 Streamable HTTP. A
  plain JSON-RPC `tools/list` without the protocol metadata is rejected with 400, while a wrong or
  missing token gets 401.
- **Fleet-wide:** the coordinator's MCP covers every machine (`agents_list`, `agent_events`,
  `agent_conversation`, `agent_summary`, `agent_send`, `agent_launch`, `sessions_search`,
  `session_resume`, …). It sits behind Google login at atmux.murphytek.com, so agents normally use
  Tron's node, which federates Midnight and Max, or are driven through the herodevs channels.
