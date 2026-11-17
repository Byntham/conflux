<p align="center">
  <img src="assets/icon.svg" width="80" height="80" alt="conflux logo">
</p>

<h1 align="center">conflux</h1>

<p align="center">Pool your Claude Code and Codex subscriptions across all of your computers.</p>

<p align="center">
  <a href="#quick-start"><strong>Quick start</strong></a> ·
  <a href="docs/install.md">Install guide</a> ·
  <a href="docs/commands.md">Commands</a> ·
  <a href="docs/troubleshooting.md">Troubleshooting</a>
</p>

conflux is a proxy. One always-on machine, the **hub**, holds your Claude and ChatGPT account logins. Claude Code and Codex still run on each of your machines, but they send their requests through the hub. So every machine draws from the same pool of accounts.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/dashboard-dark.png">
  <img src="assets/dashboard-light.png" alt="The conflux dashboard with two Claude and two Codex accounts pooled across three machines. Each pool shows how much quota is left, the burn rate, how long it lasts, and a week of history with the coming resets.">
</picture>

<p align="center"><sub>Two Claude and two Codex accounts, pooled across three machines. Each jump is quota coming back at a reset.</sub></p>

## What you get

- **Less wasted quota.** New sessions start on the available account whose weekly limit resets soonest.
- **Automatic failover.** When an account runs out, the next request goes to one that still has room.
- **Stable sessions.** An active session stays on one account, so its prompt cache keeps working.
- **One dashboard.** See each pool's quota, your machines, and recent requests from any of your devices.
- **Optional thread sync.** `conflux sync on` copies a machine's threads to the hub, where you can search them. It's off by default.

> [!WARNING]
> Pooling subscriptions may break your providers' terms and could get your accounts banned. conflux is unofficial. It logs in through Claude Code's and Codex's own OAuth clients, so either provider can break it at any time. Use it only for your own work on your own machines, and use Claude subscriptions only from Claude Code.

## Quick start

### Before you start

- Claude or ChatGPT subscriptions to pool. Codex uses a ChatGPT subscription.
- [Tailscale](https://tailscale.com) on every machine, signed in to the same tailnet, with MagicDNS on. It's on by default.
- [Claude Code](https://claude.com/product/claude-code) or [Codex](https://developers.openai.com/codex/cli) on each machine you work from.
- A hub that stays on at home, ideally Linux. Keep it off VPNs and exit nodes.
- Linux x64 or ARM64 with systemd, macOS on Apple silicon, or Windows x64. On anything else, [build from source](docs/install.md#build-from-source).

Every machine signed in to your tailnet as you can use your accounts. See [Security](#security).

Working on one machine only? Do steps 1, 2, and 4.

### 1. Install on the hub

macOS and Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/Byntham/conflux/main/install.sh | sh -s -- self
```

Windows, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/Byntham/conflux/main/install.ps1 | iex
conflux join self
```

conflux now runs as a background service, and this machine is the hub. On a Mac, you also get a menu bar app. The last line printed is the dashboard's address. Run `conflux` to print it again. If `conflux` isn't found, open a new terminal.

### 2. Add your accounts

On the hub, run one command per subscription:

```sh
conflux login claude    # a Claude subscription
conflux login codex     # a ChatGPT subscription
```

Each one prints a link. Open it in a browser signed in to that account, on any machine.

- For Claude, the browser then shows a connection error. That is expected. Paste the whole URL from the address bar into the terminal.
- For ChatGPT, type the code from the terminal into the page. The command finishes on its own.

Your accounts now show on the dashboard.

### 3. Add your other machines

Run this on each machine you work from.

macOS and Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/Byntham/conflux/main/install.sh | sh
conflux join
```

Windows, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/Byntham/conflux/main/install.ps1 | iex
conflux join
```

If `conflux` isn't found, open a new terminal and run `conflux join` again.

`conflux join` finds your hub and links to it. The machine then shows on the dashboard's Machines page, and its Claude Code and Codex send their requests through the hub. `conflux leave` puts their settings back.

**Shortcut.** If the hub can ssh to a Mac or Linux machine without a password or passphrase, skip the commands. On the dashboard, open Machines, click "Find machines", then click "Install".

### 4. Restart and check

Restart Claude Code and Codex on every machine, including the hub. Send a message. It shows up on the dashboard's Requests tab. If it doesn't, see [Troubleshooting](docs/troubleshooting.md).

The [install guide](docs/install.md) covers guests, the Mac menu bar app, updates, and uninstalling.

## Everyday commands

| Command | What it does |
| --- | --- |
| `conflux` | Print the help and the dashboard's address |
| `conflux status` | Show accounts, usage, and cooldowns |
| `conflux disable <account>` | Stop using an account. `enable` brings it back |
| `conflux off` | Use this machine's own logins, for example when the hub is down. `on` undoes it |
| `conflux update` | Update every online machine that is behind |
| `conflux leave` | Stop conflux on this machine and restore Claude Code's and Codex's settings |

`<account>` is an ID from `conflux status`. [Commands](docs/commands.md) explains every command, including how to move the hub to another machine.

## Security

- Every machine signed in to Tailscale as you can use and manage your accounts and read synced transcripts. So can any program on the hub itself.
- Machines of other Tailscale users, and tagged machines, join as guests. They can send requests only after you approve them.
- The hub stores account tokens unencrypted. Protect it like a password manager.
- `conflux leave` keeps the tokens. Before you retire a hub, follow [Uninstall](docs/install.md#uninstall).

## Documentation

- [Install guide](docs/install.md)
- [Commands](docs/commands.md)
- [Configuration](docs/configuration.md)
- [Troubleshooting](docs/troubleshooting.md)
- [Architecture](docs/architecture.md), for contributors

## License

[MIT](LICENSE)
