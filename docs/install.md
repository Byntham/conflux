# Install

Everything the [quick start](../README.md#quick-start) leaves out: [installing over ssh](#install-from-the-hub-over-ssh), [linking from the hub](#link-a-machine-from-the-hub), [linking by name](#link-a-machine-by-name), [guests](#join-as-a-guest), [other ways to install](#other-ways-to-install), [the Mac menu bar app](#the-mac-menu-bar-app), [updates](#update), and [uninstalling](#uninstall).

## Requirements

- Every machine runs [Tailscale](https://tailscale.com) with MagicDNS on, which is the default. Every machine signed in to Tailscale as you can use and manage your accounts. Machines of other users, and tagged machines, need your approval first. See [Security](../README.md#security).
- The hub stays on and stays at home. Linux is best, because a macOS or Windows hub runs only while you're signed in. Don't route its traffic through another VPN or an exit node, so the providers see one home IP address.
- Each machine you work from has Claude Code, Codex, or Grok Build. The hub needs them only if you also work on it.
- On Linux, conflux keeps running after you log out only if lingering is on for your user. `conflux join` tries to turn it on. See [Troubleshooting](troubleshooting.md#conflux-stops-when-you-log-out-of-linux).
- The installer supports Linux x64 and ARM64 with systemd, macOS on Apple silicon, and Windows x64. On other systems, [build from source](#build-from-source).

## Install from the hub, over ssh

The hub can install conflux on a Mac or Linux machine and link it for you. This works if the hub's user can ssh to the machine without a password or passphrase prompt. On the dashboard, open Machines, click "Find machines", and click "Install" next to the machine. Or run `conflux add` on the hub to list the machines it finds, then `conflux add <machine>`.

See [`conflux add`](commands.md#conflux-add-machine) for the ssh rules.

## Link a machine from the hub

A machine where you ran the installer waits to be linked. Instead of running `conflux join` on it, you can link it from the hub. On the dashboard, open Machines, click "Find machines", and click "Link" next to the machine. Or run `conflux add <machine>` on the hub. On a Mac, you can also click the "Link to" button in the menu bar app.

The hub finds only machines that are online and owned by the same Tailscale user.

## Link a machine by name

If `conflux join` lists more than one hub, or finds none, name the hub yourself: `conflux join <hub>`. You can also end the install command with the hub's name, `sh -s -- <hub>` instead of `sh`, to install and link in one step. The "Add a machine" panel on the dashboard's Machines page shows these commands with your hub's name filled in.

## Join as a guest

A machine signed in to Tailscale as another user can join as a guest. On that machine, run `conflux join` to list the hubs, then `conflux join <hub>`. On a Mac, click "Join <owner>'s hub as a guest". The hub's owner then approves it on the dashboard or with [`conflux guest approve`](commands.md#conflux-guest-approvedenyremove-machine). Until then, the guest keeps using its own logins.

## Other ways to install

### Mac without the menu bar app

This command installs only `conflux`, without Conflux.app. Use it in place of the install command in the quick start.

```sh
curl -fsSL https://raw.githubusercontent.com/Byntham/conflux/main/install.sh | sh -s -- --no-app
```

To add the menu bar app to an existing Mac install later, run the install command from the quick start. It keeps the machine's role.

### Build from source

On a system without a prebuilt binary, install with Rust:

```sh
cargo install --git https://github.com/Byntham/conflux
```

Then run `conflux join self` on the hub, or `conflux wait` on other machines, and continue with the quick start. The background service needs systemd on Linux.

## The Mac menu bar app

Conflux.app adds an icon to the menu bar. It shows every account's limits, and lets you update machines, turn routing on or off for this Mac, and open the dashboard. A dot on the icon means something needs you, such as an account that needs a new login.

Quitting the app doesn't stop conflux. The app opens again at your next login.

If you set `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, or `GROK_HOME`, use `conflux on` and `conflux off` instead of the app's routing switch. The app edits only the default folders.

## Update

When a machine is behind, the dashboard shows a round button with an up arrow at the bottom left. Click it, or run:

```sh
conflux update              # every machine that is online
conflux update <machine>    # one machine
```

Requests already running finish on the old version. A machine that is off stays on its old version, and the dashboard marks it until you update it. A system without a prebuilt binary can't update this way. Run `cargo install` again.

## Uninstall

`conflux leave` stops conflux on a machine and gives the coding tools their old settings back. It keeps the binary and any account tokens. On the hub, run `conflux remove <account>` for each account first. See [`conflux leave`](commands.md#conflux-leave).

After a hub move, the old hub keeps a copy of the account tokens in a `retired-*` folder under `~/.local/share/conflux`. Delete it once the move has worked.
