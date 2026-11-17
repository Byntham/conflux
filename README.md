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

One always-on machine, the **hub**, holds your account logins. Claude Code and Codex on your other machines send their requests through it, so every machine draws from the same pool of accounts.

## What you get

- **Less wasted quota.** New sessions start on the account whose weekly limit resets soonest.
- **Automatic failover.** When an account runs out, conflux moves the session to one that still has room.
- **Stable sessions.** A session stays on one account, so its prompt cache keeps working.
- **One dashboard.** See how much quota each pool has left, when it refills, your machines, and recent requests. Open it from any device on your tailnet.
- **Thread history, if you want it.** Collect Claude Code and Codex threads from chosen machines and search them in one place.

> [!WARNING]
> Pooling subscriptions may break your providers' terms and could get your accounts banned. conflux is unofficial and can break at any time. Use it only for your own work on your own machines, and use Claude subscriptions only from Claude Code.

## Quick start

You need [Tailscale](https://tailscale.com) on every machine, and one machine that stays on at home to be the hub. Linux is best for the hub. See [requirements](docs/install.md#requirements).

**1. Install on the hub.**

```sh
curl -fsSL https://raw.githubusercontent.com/Byntham/conflux/main/install.sh | sh -s -- self
```

On Windows, see [the install guide](docs/install.md#1-install-on-the-hub).

**2. Add your accounts.** Run one command per subscription, then open the link it prints.

```sh
conflux login claude    # a Claude subscription
conflux login codex     # a ChatGPT subscription
```

**3. Add your other machines.** On the hub's dashboard, open Machines and click "Find machines". Or install on each machine yourself, then run `conflux join`:

```sh
curl -fsSL https://raw.githubusercontent.com/Byntham/conflux/main/install.sh | sh
conflux join
```

**4. Restart your sessions.** Restart Claude Code and Codex on every machine. Your next message shows up on the dashboard's Requests tab.

The [install guide](docs/install.md) covers Windows, guests, the Mac menu bar app, updates, and building from source.

## Everyday commands

| Command | What it does |
| --- | --- |
| `conflux` | Print the dashboard's address and the command list |
| `conflux status` | Show accounts, usage, and cooldowns |
| `conflux login claude` or `codex` | Add an account, or log one in again |
| `conflux disable <account>` | Stop using an account. `enable` brings it back |
| `conflux off` | Use this machine's own logins, for example when the hub is down. `on` undoes it |
| `conflux update` | Update every online machine to the latest release |
| `conflux hub move <machine>` | Make another of your machines the hub. Your logins move with it |

[Commands](docs/commands.md) explains every command, including thread sync, extra usage, and the T3 Code key.

## Security

Anyone who can reach the hub can use your accounts and read synced transcripts. Keep your tailnet to your own devices. The hub stores account tokens unencrypted, so protect it like a password manager.

## Documentation

- [Install guide](docs/install.md)
- [Commands](docs/commands.md)
- [Configuration](docs/configuration.md)
- [Troubleshooting](docs/troubleshooting.md)
- [Architecture](docs/architecture.md), for contributors

## License

[MIT](LICENSE)
