# Mentat — Personal AI Assistant

A personal AI assistant you run on your own devices. Written in Rust. Single binary, no runtime dependencies.

Mentat answers you on messaging channels (currently Telegram and Slack) and has a web dashboard for real-time control. The gateway is the control plane — the product is the assistant.

## Install

Rust stable toolchain required. Single binary, no runtime dependencies.

```bash
git clone https://github.com/nshi/mentat.git
cd mentat
cargo build --release --locked
cargo install --path . --force --locked
mentat onboard
```

## Quick start

```bash
# Install + onboard
./install.sh --api-key "sk-..." --provider openrouter

# Start the gateway (webhook server + web dashboard)
mentat gateway                # default: 127.0.0.1:42617

# Talk to the assistant
mentat agent -m "Hello, Mentat!"

# Interactive mode
mentat agent

# Start full autonomous runtime (gateway + channels + cron + hands)
mentat daemon

# Check status
mentat status

# Run diagnostics
mentat doctor
```

## Configuration

Minimal `~/.mentat/config.toml`:

```toml
default_provider = "anthropic"
api_key = "sk-ant-..."
```

### Channels

```toml
[channels.telegram]
bot_token = "123456:ABC-DEF..."

[channels.slack]
bot_token = "xoxb-..."
app_token = "xapp-..."
```

### Tunnels

```toml
[tunnel]
kind = "cloudflare"  # or "tailscale", "ngrok", "openvpn", "custom", "none"
```

Full config reference: [docs/reference/api/config-reference.md](docs/reference/api/config-reference.md)

## CLI commands

```bash
# Workspace management
mentat onboard              # Guided setup wizard
mentat status               # Show daemon/agent status
mentat doctor               # Run system diagnostics

# Gateway + daemon
mentat gateway              # Start gateway server (127.0.0.1:42617)
mentat daemon               # Start full autonomous runtime

# Agent
mentat agent                # Interactive chat mode
mentat agent -m "message"   # Single message mode

# Service management
mentat service install      # Install as OS service (launchd/systemd)
mentat service start|stop|restart|status

# Channels
mentat channel list         # List configured channels
mentat channel doctor       # Check channel health

# Cron + scheduling
mentat cron list            # List scheduled jobs
mentat cron add "*/5 * * * *" --prompt "Check system health"
mentat cron remove <id>

# Memory
mentat memory list          # List memory entries
mentat memory get <key>     # Retrieve a memory

# Auth profiles
mentat auth login --provider <name>
mentat auth status

# Shell completions
source <(mentat completions bash)
```

Full commands reference: [docs/reference/cli/commands-reference.md](docs/reference/cli/commands-reference.md)

## Security

Mentat connects to real messaging surfaces. Treat inbound DMs as untrusted input.

- **DM pairing** (default): unknown senders receive a pairing code; bot does not process their message until approved.
- **Autonomy levels**: `ReadOnly` (observe only), `Supervised` (default, approval for risky ops), `Full` (autonomous within policy).
- **Sandboxing**: workspace isolation, path traversal blocking, command allowlists, forbidden paths, rate limiting.

Details: [SECURITY.md](SECURITY.md)

## Architecture

- **Gateway**: HTTP/WS/SSE control plane with sessions, config, cron, webhooks, web dashboard, and pairing.
- **Agent loop**: tool dispatch, prompt construction, message classification, memory loading.
- **Providers**: resilient wrapper with failover, retry, and model routing across 20+ LLM backends.
- **Channels**: Telegram and Slack (more planned).
- **Tools**: shell, file I/O, browser, git, web fetch/search, MCP, and 70+ more.
- **Web dashboard**: React 19 + Vite with real-time chat, memory browser, config editor, cron manager.

Architecture diagrams: [docs/assets/architecture-diagrams.md](docs/assets/architecture-diagrams.md)

## Workspace + skills

Workspace root: `~/.mentat/workspace/`

Prompt files: `IDENTITY.md`, `USER.md`, `MEMORY.md`, `AGENTS.md`, `SOUL.md`

```bash
mentat skills list
mentat skills install https://github.com/user/my-skill.git
mentat skills audit https://github.com/user/my-skill.git
mentat skills remove my-skill
```

## Prerequisites

- **Debian/Ubuntu:** `sudo apt install build-essential pkg-config`
- **Fedora/RHEL:** `sudo dnf group install development-tools && sudo dnf install pkg-config`
- **macOS:** `xcode-select --install`
- **Rust toolchain:** `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`

Building from source needs ~2GB RAM (4GB+ recommended) and ~6GB disk (10GB+ recommended).

## Docs

- [Documentation hub](docs/README.md)
- [Architecture diagrams](docs/assets/architecture-diagrams.md)
- [Config reference](docs/reference/api/config-reference.md)
- [Commands reference](docs/reference/cli/commands-reference.md)
- [Providers reference](docs/reference/api/providers-reference.md)
- [Channels reference](docs/reference/api/channels-reference.md)
- [Operations runbook](docs/ops/operations-runbook.md)
- [Troubleshooting](docs/ops/troubleshooting.md)
- [Security](docs/security/README.md)
- [Contributing](CONTRIBUTING.md)

## Attribution

Mentat is based on [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw), an open-source personal AI assistant. Credit to the original ZeroClaw authors and contributors.

## License

Dual-licensed under [MIT](LICENSE-MIT) and [Apache 2.0](LICENSE-APACHE). You may choose either license.
