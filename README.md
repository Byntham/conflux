<img src="assets/icon.svg" width="64" height="64" alt="">

# conflux

conflux lets all of your computers share a pool of Claude and ChatGPT subscriptions.

One always-on machine, the **hub**, holds your account logins. Claude Code and Codex on your other machines send their requests through it.

## What you get

- **Less wasted quota.** New sessions start on the available account whose weekly limit resets soonest.
- **Automatic failover.** When an account runs out, conflux moves the session to one that still has room.
- **Stable sessions.** A session stays on one account, so its prompt cache keeps working.
- **A dashboard.** It shows how much quota each provider's pool of accounts has left and when it refills, plus each account's limits, your machines, and recent requests. It works from any device on your tailnet.
- **Optional thread history.** Collect Claude Code and Codex threads from the machines you choose, to read and search in one place.

> [!WARNING]
> Pooling subscriptions this way may break your providers' terms and could get your accounts banned. conflux is unofficial and logs in through Claude Code's and Codex's own OAuth clients, so either provider can break it at any time. It's for one person's own work on their own machines. Don't use it to serve other people, and only use Claude subscriptions from Claude Code.

## Install

### Requirements

- Every machine runs [Tailscale](https://tailscale.com) with MagicDNS on, which is the default. Use a tailnet that holds only your own devices, because anyone who can reach the hub can use your accounts.
- The hub stays on and stays at home. Linux is best, because a macOS or Windows hub runs only while you're signed in. Don't route its traffic through another VPN or an exit node, so the providers see one home IP address.
- Each machine you work from has Claude Code or Codex. The hub doesn't need them.
- The installer supports Linux x64 and ARM64 with systemd, macOS on Apple silicon, and Windows x64. On other architectures, [build from source](#other-ways-to-install).

### 1. Install on the hub

`self` makes this machine the hub.

macOS and Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/Byntham/conflux/main/install.sh | sh -s -- self
```

If the installer says it changed your PATH, open a new terminal. On a Mac, it also installs the [menu bar app](#the-mac-menu-bar-app).

Windows, in PowerShell. Run both commands. The first says this machine is waiting to be linked. The second makes it the hub.

```powershell
irm https://raw.githubusercontent.com/Byntham/conflux/main/install.ps1 | iex
conflux join self
```

### 2. Add your accounts

On the hub, run the matching command once for each subscription. On Windows, run them in PowerShell.

A Claude subscription:

```sh
conflux login claude
```

A ChatGPT subscription:

```sh
conflux login codex
```

Each command prints a link and the steps to follow. Open the link in a browser signed in to that account. The browser can be on any machine.

Your accounts now show on the dashboard. Its address is the "Dashboard:" line that step 1 printed, and it works from any of your devices. Run `conflux` on the hub to print it again.

### 3. Add your other machines

If the hub's user can ssh to a Mac or Linux machine without a password or passphrase prompt, add it from the hub. On the hub's dashboard, open Machines, click "Find machines", and click "Install" next to the machine. Or run `conflux add` on the hub to list the machines it finds, then `conflux add <machine>`. The hub installs conflux there over ssh and links the machine.

Otherwise, install conflux on the machine yourself.

macOS and Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/Byntham/conflux/main/install.sh | sh
```

Windows, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/Byntham/conflux/main/install.ps1 | iex
```

The machine now waits to be linked. Link it from either side. conflux then points Claude Code and Codex at the hub.

- On the new machine, run `conflux join`. conflux finds the hub on your tailnet and links to it. On a Mac, the menu bar app shows a "Link to" button with the hub's name instead.
- On the hub's dashboard, open Machines, click "Find machines", and click "Link" next to the machine. Or run `conflux add <machine>` on the hub.

Both ways find only machines that are online and owned by the same Tailscale user.

To skip linking, or for a machine that isn't found, end the install command with the hub's name: `sh -s -- <hub>` instead of `sh`. On Windows, run `conflux join <hub>` after the installer. The "Add a machine" panel on the same page shows these commands with your hub's name filled in.

### 4. Restart and check

Restart open Claude Code and Codex sessions on every machine, including the hub. Send a message from one. It shows up on the dashboard's Requests tab.

If it doesn't, see [Troubleshooting](docs/troubleshooting.md).

### Other ways to install

**Mac without the menu bar app.** These commands install only `conflux`, without Conflux.app. Use them in place of the command in step 1 or step 3.

On the hub:

```sh
curl -fsSL https://raw.githubusercontent.com/Byntham/conflux/main/install.sh | sh -s -- --no-app self
```

On other Macs:

```sh
curl -fsSL https://raw.githubusercontent.com/Byntham/conflux/main/install.sh | sh -s -- --no-app
```

**Add the menu bar app to an existing Mac install.** Run the install command from step 3. It keeps this machine's role.

**Build from source.** On a system without a prebuilt binary, install with Rust:

```sh
cargo install --git https://github.com/Byntham/conflux
```

Then, on the hub, run this and add your accounts as in step 2:

```sh
conflux join self
```

On other machines, run this, then link them as in step 3:

```sh
conflux wait
```

## The Mac menu bar app

Conflux.app adds an icon to the menu bar. It shows every account's limits, and lets you update machines, turn routing on or off for this Mac, and open the dashboard. A dot on the icon means something needs you, such as an account that needs a new login.

Quitting the app doesn't stop conflux. The app opens again at your next login.

If you set `CLAUDE_CONFIG_DIR` or `CODEX_HOME`, use `conflux on` and `conflux off` instead of the app's routing switch. The app edits only the default folders.

## Update

When a machine is behind, the dashboard shows a round button with an up arrow at the bottom left. Click it, or run:

```sh
conflux update              # every machine that is online
conflux update <machine>    # one machine
```

Requests already running finish on the old version. A machine that is off stays on its old version, and the dashboard marks it until you update it. A system without a prebuilt binary can't update this way. Run `cargo install` again.

## Everyday commands

| Command | What it does |
| --- | --- |
| `conflux status` | Show accounts, usage, and cooldowns |
| `conflux login claude` or `codex` | Add an account, or log one in again |
| `conflux disable <account>` | Stop using an account. `enable` brings it back |
| `conflux remove <account>` | Delete an account from the hub |
| `conflux off` | Point this machine back at its own logins. `on` undoes it |
| `conflux leave` | Stop conflux and restore Claude Code's and Codex's settings. The binary and account data stay |

If the hub is down, run `conflux off` on the machine you're using and restart your tool. If you've never logged in to Claude Code or Codex on that machine, log in directly first.

To turn routing on or off for another machine, open Machines on the dashboard, click the ⋯ button on that machine's row, and pick Turn routing off or Turn routing on. New sessions on that machine pick up the change. The dashboard edits the default folders, so if you set `CLAUDE_CONFIG_DIR` or `CODEX_HOME` there, run `conflux on` or `conflux off` on that machine instead.

To remove conflux from another machine, pick Remove conflux from the same menu. It does what `conflux leave` does on that machine. Claude Code and Codex get their old settings back, and conflux stops and no longer starts at login. An offline machine's menu has Remove from list instead. The machine comes back if conflux starts on it again.

## Move the hub

Your logins move with the hub, and your other machines follow on their own. The new machine needs conflux installed.

```sh
conflux hub move-code                               # on the current hub
conflux hub take-over <old-hub> --code <code>       # on the new machine, within 10 minutes
conflux join <new-hub>                              # back on the old machine, a minute later
```

See [Moving the hub](docs/commands.md#moving-the-hub) for details.

## Optional features

**Thread history.** Run `conflux sync on` on each machine you want to collect from. It copies Claude Code and Codex transcripts to `~/conflux/transcripts` on the hub, and the dashboard's Threads tab lists and searches them. It's off by default because transcripts contain everything your agents have read. `conflux sync off` stops it. Copies already on the hub stay until you delete them.

**Extra usage.** conflux uses a Claude account's paid extra usage only if you run `conflux overage <account> on`, and then only when no other account has room.

**T3 Code.** Run `conflux t3-key` on the hub, then add the URL and key under Settings > Providers > Usage providers in [T3 Code](https://t3.codes) to see your pooled limits there.

## Security

Anyone who can reach the hub can use and manage your accounts and read synced transcripts. The hub stores account tokens unencrypted, so protect it like a password manager. `conflux leave` keeps the tokens, so run `conflux remove <account>` first if you're done with a hub. After a hub move, the old hub keeps a copy in a `retired-*` folder under `~/.local/share/conflux`. Delete it once the move has worked.

## More docs

- [Commands](docs/commands.md) explains every command and option.
- [Configuration](docs/configuration.md) lists the files conflux keeps, `config.toml`, and environment variables.
- [Troubleshooting](docs/troubleshooting.md) covers logs, the background service, and common problems.
- [Architecture](docs/architecture.md) gives contributors a map of the code.

## License

[MIT](LICENSE)
