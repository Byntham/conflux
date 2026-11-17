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

conflux is a proxy. One always-on machine, the **hub**, holds your Claude and ChatGPT logins. Claude Code and Codex on each of your machines send their requests to it over [Tailscale](https://tailscale.com), so every machine draws from the same pool of accounts.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/dashboard-dark.png">
  <img src="assets/dashboard-light.png" alt="The conflux dashboard with two Claude and two Codex accounts pooled across three machines. Each pool shows how much quota is left, the burn rate, how long it lasts, and a week of history with the coming resets.">
</picture>

<p align="center"><sub>Two Claude and two Codex accounts, pooled across three machines. Left of Now is measured use. Right of it is a forecast, and each jump is quota coming back at a reset.</sub></p>

## What you get

- **Less wasted quota.** New sessions start on the available account whose weekly limit resets soonest.
- **Automatic failover.** When an account hits its limit before a reply starts, conflux retries the request on one that still has room.
- **Stable sessions.** An active session stays on one account while it has room, so its prompt cache keeps working.
- **One dashboard.** See each pool's quota, your machines, and recent requests from any device on your tailnet.
- **Optional thread sync.** `conflux sync on` copies a machine's transcripts to the hub, where you can search them. It's off by default.

> [!WARNING]
> Pooling subscriptions may break your providers' terms and could get your accounts banned. conflux is unofficial. It logs in through Claude Code's and Codex's own OAuth clients, so either provider can break it at any time. Use it only for your own work on your own machines, and use Claude subscriptions only from Claude Code.

## Quick start

Working on one machine only? Skip step 4.

### Before you start

- One or more Claude or ChatGPT subscriptions. Codex uses a ChatGPT subscription.
- [Tailscale](https://tailscale.com) on every machine, signed in to the same tailnet. MagicDNS must be on, which is the default.
- [Claude Code](https://claude.com/product/claude-code) or [Codex](https://developers.openai.com/codex/cli) on each machine you work from.
- A hub that stays on at home. Linux is best, because a macOS or Windows hub runs only while you're signed in.
- Keep the hub off VPNs and exit nodes, so the providers see one home IP address.
- Linux x64 or ARM64 with systemd, macOS on Apple silicon, or Windows x64. On anything else, [build from source](docs/install.md#build-from-source).

### 1. Install conflux

Run this on the hub, and on each other machine you work from. Step 4 has a shortcut for Mac and Linux machines.

macOS and Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/Byntham/conflux/main/install.sh | sh
```

Windows, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/Byntham/conflux/main/install.ps1 | iex
```

conflux now runs as a background service. On a Mac, you also get a [menu bar app](docs/install.md#the-mac-menu-bar-app). If the installer says it changed your PATH, open a new terminal.

### 2. Make one machine the hub

On the hub:

```sh
conflux join self
```

Ignore the installer's hint to run `conflux join`. That one is for your other machines.

The last line it prints is the dashboard's address. The last line of `conflux` shows it again.

### 3. Add your accounts

On the hub, run the command for each subscription you have. Run it again for each extra account.

```sh
conflux login claude    # a Claude subscription
conflux login codex     # a ChatGPT subscription
```

Each one prints a link. Open it in a browser signed in to that account, on any machine.

- For Claude, the browser then shows a connection error. That is expected. Paste the whole URL from the address bar into the terminal.
- For ChatGPT, type the code from the terminal into the page. The command finishes on its own.

Your accounts now show on the dashboard. Its "Add Claude" and "Add Codex" buttons do the same as these commands.

### 4. Link your other machines

**From the dashboard.** Open Machines, click "Find machines", then click "Install". The hub installs conflux on the machine over ssh and links it. This works for Mac and Linux machines the hub can ssh to without a password or passphrase.

**From the machine.** On each other machine where you ran step 1:

```sh
conflux join
```

It finds your hub and links to it. If it lists more than one hub, or finds none, run `conflux join <hub>` with your hub's name.

Either way, the machine then shows on the dashboard's Machines page, and its Claude Code and Codex send their requests through the hub. `conflux leave` puts their settings back.

### 5. Restart and check

Restart Claude Code and Codex on every machine, including the hub. Send a message. It shows up on the dashboard's Requests page. If it doesn't, see [Troubleshooting](docs/troubleshooting.md) or [open an issue](https://github.com/Byntham/conflux/issues).

The [install guide](docs/install.md) covers guests, linking from the hub, the Mac menu bar app, updates, and uninstalling.

## Everyday commands

| Command | What it does |
| --- | --- |
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
