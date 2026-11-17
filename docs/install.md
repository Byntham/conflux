# Install

The [README](../README.md#quick-start) has the short version. This page adds the details: [installing over ssh](#from-the-hub-over-ssh), [linking a machine by name](#link-a-machine-by-name), [joining as a guest](#as-a-guest), [the Mac menu bar app](#the-mac-menu-bar-app), [updates](#update), and [uninstalling](#uninstall).

## Requirements

- Every machine runs [Tailscale](https://tailscale.com) with MagicDNS on, which is the default. Every machine signed in to Tailscale as you can use and manage your accounts. Machines of other users, and tagged machines, need your approval first. See [Security](../README.md#security).
- The hub stays on and stays at home. Linux is best, because a macOS or Windows hub runs only while you're signed in. Don't route its traffic through another VPN or an exit node, so the providers see one home IP address.
- Each machine you work from has Claude Code or Codex. The hub doesn't need them.
- The installer supports Linux x64 and ARM64 with systemd, macOS on Apple silicon, and Windows x64. On other systems, [build from source](#build-from-source).

## 1. Install conflux

Run this on the hub, then on each machine you work from.

macOS and Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/Byntham/conflux/main/install.sh | sh
```

Windows, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/Byntham/conflux/main/install.ps1 | iex
```

conflux now runs as a background service and waits to be linked. If the installer says it changed your PATH, open a new terminal. On a Mac, it also installs the [menu bar app](#the-mac-menu-bar-app).

## 2. Make one machine the hub

On the hub:

```sh
conflux join self
```

The last line it prints is the dashboard's address. The dashboard works from any device on your tailnet. Run `conflux` on the hub to print the address again.

## 3. Add your accounts

On the hub, run the command for each subscription you have. Run it again for each extra account.

```sh
conflux login claude    # a Claude subscription
conflux login codex     # a ChatGPT subscription
```

Each command prints a link. Open it in a browser signed in to that account. The browser can be on any machine.

- For Claude, the browser then shows a connection error. That is expected. Paste the whole URL from the address bar into the terminal.
- For ChatGPT, type the code from the terminal into the page. The command finishes on its own.

Your accounts now show on the dashboard.

## 4. Link your other machines

On each machine you work from:

```sh
conflux join
```

conflux finds the hub on your tailnet and links to it. On a Mac, you can click the "Link to" button in the menu bar app instead. Once linked, conflux points Claude Code and Codex at the hub, and the machine shows on the dashboard's Machines page.

You can also link a waiting machine from the hub. On the dashboard, open Machines, click "Find machines", and click "Link" next to the machine. Or run `conflux add <machine>` on the hub. The hub finds only machines that are online and owned by the same Tailscale user.

### From the hub, over ssh

The hub can do steps 1 and 4 for you. This works if the hub's user can ssh to a Mac or Linux machine without a password or passphrase prompt. On the dashboard, open Machines, click "Find machines", and click "Install" next to the machine. Or run `conflux add` on the hub to list the machines it finds, then `conflux add <machine>`.

See [`conflux add`](commands.md#conflux-add-machine) for the ssh rules.

### Link a machine by name

If `conflux join` lists more than one hub, or finds none, name the hub yourself: `conflux join <hub>`. You can also end the install command with the hub's name, `sh -s -- <hub>` instead of `sh`, to install and link in one step. The "Add a machine" panel on the dashboard's Machines page shows these commands with your hub's name filled in.

### As a guest

A machine signed in to Tailscale as another user can join as a guest. On that machine, run `conflux join` to list the hubs, then `conflux join <hub>`. On a Mac, click "Join <owner>'s hub as a guest". The hub's owner then approves it on the dashboard or with [`conflux guest approve`](commands.md#conflux-guest-approvedenyremove-machine). Until then, the guest keeps using its own logins.

## 5. Restart and check

Restart open Claude Code and Codex sessions on every machine, including the hub. Send a message from one. It shows up on the dashboard's Requests page.

If it doesn't, see [Troubleshooting](troubleshooting.md).

## Other ways to install

### Mac without the menu bar app

This command installs only `conflux`, without Conflux.app. Use it in place of the command in step 1.

```sh
curl -fsSL https://raw.githubusercontent.com/Byntham/conflux/main/install.sh | sh -s -- --no-app
```

To add the menu bar app to an existing Mac install later, run the install command from step 1. It keeps the machine's role.

### Build from source

On a system without a prebuilt binary, install with Rust:

```sh
cargo install --git https://github.com/Byntham/conflux
```

Then run `conflux join self` on the hub and add your accounts as in step 3. On other machines, run `conflux wait`, then link them as in step 4.

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

## Uninstall

`conflux leave` stops conflux on a machine and gives Claude Code and Codex their old settings back. It keeps the binary and any account tokens. On the hub, run `conflux remove <account>` for each account first. See [`conflux leave`](commands.md#conflux-leave).

After a hub move, the old hub keeps a copy of the account tokens in a `retired-*` folder under `~/.local/share/conflux`. Delete it once the move has worked.
