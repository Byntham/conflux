<p align="center">
  <img src="assets/icon.svg" width="80" height="80" alt="conflux logo">
</p>

<h1 align="center">conflux</h1>

<p align="center">Share your Claude and ChatGPT subscriptions across all of your computers.</p>

<p align="center">
  <a href="#quick-start"><strong>Quick start</strong></a> ·
  <a href="docs/install.md">Install guide</a> ·
  <a href="docs/commands.md">Commands</a> ·
  <a href="docs/troubleshooting.md">Troubleshooting</a>
</p>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/dashboard-dark.png">
  <img src="assets/dashboard-light.png" alt="The conflux dashboard. Each provider's pool shows how much quota is left, the burn rate, how long it lasts, and a week of history with the coming resets.">
</picture>

One always-on machine, the **hub**, holds your account logins. Claude Code and Codex on your other machines send their requests through it, so every machine draws from the same pool of accounts.

## What you get

- **Less wasted quota.** New sessions start on the available account whose weekly limit resets soonest.
- **Automatic failover.** When an account runs out, the next request goes to one that still has room.
- **Stable sessions.** A session stays on one account, so its prompt cache keeps working.
- **One dashboard.** See how much quota each pool has left, when it refills, your machines, and recent requests. Open it from any device on your tailnet.
- **Thread history, if you want it.** Run `conflux sync on` on a machine to copy its Claude Code and Codex threads to the hub, where you can search them. It's off by default, because transcripts hold everything your agents have read.

> [!WARNING]
> Pooling subscriptions may break your providers' terms and could get your accounts banned. conflux is unofficial. It logs in through Claude Code's and Codex's own OAuth clients, so either provider can break it at any time. Use it only for your own work on your own machines, and use Claude subscriptions only from Claude Code.

## Quick start

You need:

- [Tailscale](https://tailscale.com) on every machine.
- Claude Code or Codex on each machine you work from.
- One machine that stays on at home to be the hub. Linux is best. Don't send its traffic through another VPN or an exit node.

The installer supports Linux x64 and ARM64 with systemd, macOS on Apple silicon, and Windows x64. The commands below are for macOS and Linux. For Windows, follow the [install guide](docs/install.md).

**1. Install on the hub.**

```sh
curl -fsSL https://raw.githubusercontent.com/Byntham/conflux/main/install.sh | sh -s -- self
```

The last line it prints is the dashboard's address. Run `conflux` to print it again. If the installer changed your PATH, open a new terminal.

**2. Add your accounts.** Run one command per subscription, then open the link it prints.

```sh
conflux login claude    # a Claude subscription
conflux login codex     # a ChatGPT subscription
```

**3. Add your other machines.** On the dashboard, open Machines, click "Find machines", then click "Install". This needs ssh from the hub to the machine without a password. Otherwise, install on the machine yourself and link it:

```sh
curl -fsSL https://raw.githubusercontent.com/Byntham/conflux/main/install.sh | sh
conflux join
```

**4. Restart your sessions.** Restart Claude Code and Codex on every machine, including the hub. Your next message shows up on the dashboard's Requests tab. If it doesn't, see [Troubleshooting](docs/troubleshooting.md).

The [install guide](docs/install.md) covers Windows, guests, the Mac menu bar app, updates, building from source, and uninstalling.

## Everyday commands

| Command | What it does |
| --- | --- |
| `conflux` | Print the dashboard's address and the command list |
| `conflux status` | Show accounts, usage, and cooldowns |
| `conflux login claude` or `codex` | Add an account, or log one in again |
| `conflux disable <account>` | Stop using an account. `enable` brings it back |
| `conflux remove <account>` | Delete an account from the hub |
| `conflux off` | Use this machine's own logins, for example when the hub is down. `on` undoes it |
| `conflux update` | Update every online machine that is behind |
| `conflux hub move <machine>` | Make another of your machines the hub. Your logins move with it |
| `conflux leave` | Stop conflux on this machine and restore Claude Code's and Codex's settings |

[Commands](docs/commands.md) explains every command, including thread sync, extra usage, and the T3 Code key.

## Security

- Every machine signed in to Tailscale as you can use and manage your accounts and read synced transcripts. So can any program on the hub itself.
- Machines of other Tailscale users, and tagged machines, join as guests. They can send requests only after you approve them.
- The hub stores account tokens unencrypted. Protect it like a password manager.
- `conflux leave` keeps the tokens. If you're done with a hub, run `conflux remove <account>` for each account first.
- After a hub move, the old hub keeps a copy of the tokens in a `retired-*` folder. Delete it once the move has worked.

## Documentation

- [Install guide](docs/install.md)
- [Commands](docs/commands.md)
- [Configuration](docs/configuration.md)
- [Troubleshooting](docs/troubleshooting.md)
- [Architecture](docs/architecture.md), for contributors

## License

[MIT](LICENSE)
