<img src="assets/icon.svg" width="64" height="64" alt="">

# conflux

conflux lets all of your computers share a pool of Claude and ChatGPT subscriptions.

You log in to your accounts once, on one always-on machine at home. Claude Code and Codex on your other machines then send their requests through it. conflux picks which account handles each request and switches accounts when one hits its limit, so you stop thinking about which subscription has room left.

## What you get

- **The best account for each request.** conflux uses the account whose weekly limit resets soonest, so less of your quota goes to waste.
- **Automatic failover.** When an account runs out, the next request moves to one that still has room.
- **Stable sessions.** A session stays on one account, which keeps its prompt cache working.
- **A dashboard.** See every account's limits, your machines, and recent requests from any device on your network.
- **Simple setup.** One install command per machine, and no config files to edit.
- **A single place for your history, if you want it.** Optionally collect every Claude Code and Codex thread from every machine, to read and search together.

It runs on Linux, macOS (Apple silicon), and Windows, and works with [T3 Code](https://t3.codes).

> [!WARNING]
> Pooling subscriptions this way may break your providers' terms and could get your accounts banned. conflux is for one person's own work on their own machines. Don't use it to serve other people, and only use Claude subscriptions from Claude Code.

## Before you start

You need:

- [Tailscale](https://tailscale.com) on every machine, all on the same tailnet.
- One machine to be the **hub**. It should stay on, stay at home, and not use a VPN, so that all your traffic comes from one home IP address.

## Set up the hub

Download the build for your system from [Releases](https://github.com/Byntham/conflux/releases), rename it `conflux`, and put it on your `PATH`. Or build it with Rust:

```sh
cargo install --git https://github.com/Byntham/conflux
```

Then, on the hub:

```sh
conflux join self
conflux login claude
conflux login codex
```

Repeat the login commands for each account. Claude login asks you to approve access in a browser. The browser then shows a connection error, which is expected: paste the URL from its address bar back into the terminal.

The dashboard is now at `http://<hub>:8317`, where `<hub>` is the machine's Tailscale name.

## Add your other machines

Run this on each one, replacing `<hub>`:

```sh
curl -fsSL http://<hub>:8317/conflux/install.sh | sh
```

On Windows, in PowerShell:

```powershell
irm http://<hub>:8317/conflux/install.ps1 | iex
```

That's all. conflux runs in the background and points Claude Code and Codex at the hub. Sessions that are already open need a restart to pick it up. The dashboard shows these commands with your hub's name filled in.

## Everyday commands

| Command | What it does |
| --- | --- |
| `conflux status` | Show accounts, usage, and cooldowns |
| `conflux login claude` or `codex` | Add an account, or log one in again |
| `conflux disable <account>` | Stop using an account. `enable` brings it back |
| `conflux remove <account>` | Delete an account from the hub |
| `conflux off` | Point this machine back at its own logins. `on` undoes it |
| `conflux leave` | Remove conflux from this machine |

If the hub is ever down, run `conflux off` on the machine you're using and keep working.

## Move the hub to another machine

Your logins move with the hub, and your other machines follow within a minute.

```sh
conflux hub move-code                            # on the current hub
conflux hub take-over <old hub> --code <code>    # on the new machine
conflux join <new hub>                           # back on the old machine
```

If a move gets stuck, `conflux hub resume` on the old hub puts it back in service.

## Optional features

**Thread history.** Run `conflux sync on` on any machine to copy its Claude Code and Codex transcripts to `~/conflux/transcripts` on the hub. The dashboard's Threads tab lists and searches them, and you can point an agent at the folder to look for patterns. It's off by default because transcripts contain everything your agents have read.

**Extra usage.** conflux won't use a Claude account's paid extra usage unless you allow it with `conflux overage <account> on`, and then only when no other account has room.

**T3 Code.** Run `conflux t3-key` on the hub, then add the URL and key under Settings > Providers > Usage providers in T3 Code to see your pooled limits there.

## Security

Being on your tailnet is the login. Anyone who can reach the hub can use your accounts, so only run conflux on a tailnet that holds your own devices. Account tokens stay on the hub, in files only your user can read.
