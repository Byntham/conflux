# Commands

Every `conflux` command, what it does, and where to run it. `conflux help <command>` prints a short version in the terminal.

`conflux` on its own prints the command list and a link to the hub's dashboard. On a node, the link points at the hub.

Some commands change only the machine you run them on. The account commands send their request to the conflux service on this machine. On a node, that service passes the request on to the hub. So you can manage accounts from any machine, as long as conflux runs there and the hub is up.

An account ID looks like `claude-1a2b3c4d5e6f` or `codex-1a2b3c4d5e6f`. `conflux status` lists them.

- [Accounts](#accounts): `status`, `login`, `usage`, `enable`, `disable`, `overage`, `remove`
- [This machine](#this-machine): `join`, `wait`, `off`, `on`, `leave`, `sync`, `serve`
- [Every machine](#every-machine): `add`, `update`, `guest`
- [Moving the hub](#moving-the-hub): `hub move`, `hub resume`
- [Other](#other): `t3-key`

## Accounts

### `conflux status`

Lists every account in the pool:

```
ACCOUNT              TYPE    EMAIL                            SHORT  LONG   STATUS
claude-1a2b3c4d5e6f  claude  you@example.com                  12%    40%    ok
codex-6f5e4d3c2b1a   codex   you@example.com                  -      -      cooling account until 2026-01-01T12:00:00Z
```

SHORT and LONG are the percentage used in the shortest and longest usage windows, for example 5 hours and 7 days. A `-` means conflux has no usage data for that account yet.

STATUS is `ok`, or one or more of these:

- `disabled`. You turned the account off with `conflux disable`.
- `needs login (...)`. The token stopped working. Run `conflux login` for that provider again.
- `billing blocked (...)`. The provider refused the account for a billing reason.
- `cooling <scope> until <time>`. The account hit a limit, and conflux skips it until that time. The scope is `account` for the whole account, or `model:<name>` for one model.
- `overage allowed`. You turned on paid extra usage with `conflux overage`.

On a machine that is waiting to be linked, `status` says so and tells you how to link it.

On the hub's owner's machines, `status` also lists machines that wait for your approval. See [`conflux guest`](#conflux-guest-approvedenyremove-machine). On a guest machine, it says whether the hub's owner has approved it yet.

### `conflux login claude`, `conflux login codex`, `conflux login grok`, `conflux login muse`

Adds an account to the pool. If the account is already in the pool, this logs it in again and keeps its settings.

For `claude`, conflux prints a link. Open it in a browser signed in to the Claude account and approve access. The browser then shows a connection error, which is expected. Paste the whole URL from the address bar into the terminal.

For `codex`, `grok`, and `muse`, conflux prints a link and a code. Open the link in a browser signed in to the ChatGPT, xAI, or Meta account and enter the code. conflux waits and finishes on its own.

The browser does not have to be on the hub or on the machine where you run the command.

### `conflux usage <account>`

Asks the provider for the account's current usage, then prints it in the same table as `status`. Use it when `status` shows old numbers.

### `conflux disable <account>`, `conflux enable <account>`

`disable` stops conflux from choosing the account for new requests, including requests from sessions that were using it. A Codex WebSocket that is already open on the account keeps going until it closes. Its login and settings stay. `enable` lets conflux use it again.

### `conflux overage <account> on|off`

Allows or forbids a Claude account's paid extra usage. It is off by default.

When it is on, conflux still prefers every account that has room. It uses this account's extra usage only when no other account can take the request.

### `conflux remove <account>`

Deletes the account, its tokens, and its settings from the hub. To add it back, run `conflux login` again.

## This machine

These commands change only the machine you run them on.

### `conflux join [hub]`

Makes this machine the hub, or a node of a hub.

- `conflux join self` makes this machine the hub. `localhost`, `127.0.0.1`, and this machine's own name also work.
- `conflux join <hub>` makes this machine a node. `<hub>` is the hub's name in `tailscale status`, or its Tailscale address. conflux checks that it can reach the hub and warns you if it can't.
- `conflux join` with no name finds the hub for you. It works on a machine that is waiting to be linked. conflux asks each online machine on your tailnet whether it is a hub. If it finds one hub, and that hub belongs to your Tailscale user, it links to it. Otherwise it lists the hubs it found, and you name one yourself. A hub that belongs to another Tailscale user shows as "join <owner>'s hub as a guest". The background service is already running, so `--no-service` and `--force` do not apply.

`join` with a name then does these things:

1. It installs conflux as a background service and starts it. See [Logs and the background service](troubleshooting.md#logs-and-the-background-service) for what it installs on each system.
2. It points Claude Code at conflux. It sets `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` under `env` in `~/.claude/settings.json`. It also sets `CLAUDE_CODE_PROMPT_CACHE_TTL` to `1h`, unless you already set it. Claude Code treats the token as an API key, so without it the main conversation caches for 5 minutes instead of a subscription's 1 hour.
3. It points Codex at conflux. It sets `model_provider = "conflux"` and adds a `[model_providers.conflux]` table in `~/.codex/config.toml`.
4. If `~/.grok` exists, it points Grok Build at conflux. It sets `cli_chat_proxy_base_url` under `[endpoints]` in `~/.grok/config.toml`. If Grok Build has no login of its own on this machine, it also sets `auth_provider_command` under `[auth]`, so Grok Build starts without one. A machine where you ran `grok login` keeps that login, and conflux replaces its token with a pooled account's. If you run `grok login` on a machine that conflux set up without one, run `conflux on` right after it, before you start Grok Build, so conflux removes its `auth_provider_command` and Grok Build keeps the new login.
5. If `~/.config/muse` exists, it points Muse Code at conflux. It sets `endpoint_transport` in `~/.config/muse/settings.json` to conflux's address with `auth` set to `none`, so Muse Code sends no key of its own and needs no login on this machine.

Before it changes these files, conflux saves the values it replaces. `conflux off` and `conflux leave` put them back. If `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GROK_HOME`, or `XDG_CONFIG_HOME` is set, conflux edits the files there instead. If you install Grok Build or Muse Code after `join`, run `conflux on` again.

conflux warns you if your shell sets a variable that can bypass these settings, such as `ANTHROPIC_API_KEY` or `OPENAI_BASE_URL`.

Open coding tool sessions keep their old settings. Restart them to go through conflux.

Options:

- `--no-service` skips the background service. Run `conflux serve` yourself.
- `--force` joins another hub even though this machine holds logged-in accounts. Without it, `join` refuses, because those accounts would be stranded. To move accounts to a new hub, use [`conflux hub move`](#conflux-hub-move-machine) on the hub instead.

`join` refuses while this machine is in the middle of a hub move. Wait for the move to finish, or run `conflux hub resume`.

If the hub belongs to another Tailscale user, this machine is a guest, and the hub's owner must approve it first. `join` installs the service but leaves the coding tools alone. Once the owner approves the machine, conflux points them at the hub. Until then they keep using this machine's own logins. If the owner turns the machine down, run `join` again to ask again.

### `conflux wait`

Installs and starts the background service, then waits for a hub to link this machine. The installer runs it when you don't name a hub.

Link a waiting machine from either side:

- On the waiting machine, run [`conflux join`](#conflux-join-hub) with no name, or click "Link to" in the Mac menu bar app.
- On the hub's dashboard, open Machines, click "Find machines", and click "Link" next to it. Or run [`conflux add <machine>`](#conflux-add-machine) on the hub.

The machine links only to a hub owned by the same Tailscale user. When linked, it edits the coding tools' settings the same way `join` does.

If this machine has already joined a hub or holds accounts, `wait` changes nothing. It only restarts the service if one is installed.

### `conflux off`

Points the coding tools back at this machine's own logins, using the values `join` saved. The conflux service keeps running. Restart open sessions to pick up the change.

Use it when the hub is down. If you have never logged in to a coding tool on this machine, log in to it directly first.

To turn routing off or on for another machine, open Machines on the dashboard, click the ⋯ button on that machine's row, and pick Turn routing off or Turn routing on. New sessions on that machine pick up the change. The dashboard edits the default folders, so if you set `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, or `GROK_HOME` there, run `conflux off` or `conflux on` on that machine instead.

### `conflux on`

Points the coding tools at conflux again after `conflux off`. It saves the current values first, so a later `off` restores what was there just before `on`.

### `conflux leave`

Stops conflux on this machine and gives the coding tools their old settings back. It removes the background service.

`leave` keeps the conflux binary, its config, and any account tokens on this machine. If this machine is the hub and you are done with it, run `conflux remove` for each account first. To start again later, run `conflux join`.

### `conflux sync on|off|status`

Controls whether this machine copies its Claude Code and Codex transcripts to the hub.

- `on` starts copying. Transcripts contain everything your agents have read, so it is off by default.
- `off` stops copying. Copies already on the hub stay until you delete them.
- `status` shows whether sync is on.

The hub keeps the copies in `~/conflux/transcripts`. The dashboard's Threads tab lists and searches them.

### `conflux serve`

Runs conflux in the foreground until you stop it. The background service runs this command for you. Run it yourself after `conflux join <hub> --no-service`, or to watch logs while you debug. To watch logs, stop the background service first. See [Troubleshooting](troubleshooting.md#logs-and-the-background-service). Set `CONFLUX_LOG` to change the log level. See [Configuration](configuration.md).

## Every machine

### `conflux add [machine...]`

Adds machines on your tailnet to the hub. With no names, it lists the machines it finds that are not linked yet. They must be online and owned by the same Tailscale user as the hub.

```sh
conflux add
conflux add studio laptop
```

What happens to a named machine depends on its state:

- It waits to be linked. The hub links it.
- conflux is not running on it, and it runs macOS or Linux. The hub logs in over ssh, runs the installer, waits for conflux to start, and links it. If conflux already runs there, for example as a node that lost its hub, the hub leaves it alone. Run [`conflux join <hub>`](#conflux-join-hub) there to move it to this hub.
- conflux is not running on it, and it runs Windows. `add` prints the install command to run there.
- It runs conflux but does not wait to be linked. `add` refuses. Run [`conflux join <hub>`](#conflux-join-hub) there.

The ssh login runs as the user that runs the hub's service, without a terminal. The hub runs `ssh <machine>` with the machine's tailnet name, so a `Host` entry for that name in `~/.ssh/config` applies. The login must need no password, passphrase, or browser approval. Tailscale SSH works, and so does a key without a passphrase. A Mac must have its user logged in at the desktop. The hub and the machine must both use the default port. If the login fails, `add` prints the install command to run on the machine.

The dashboard does the same from Machines, with "Find machines", then "Install" or "Link".

### `conflux update [machine...]`

Updates machines to the latest conflux release. With no names, it updates every online machine that is behind. With names, it updates only those. Use the names the dashboard shows.

Each machine downloads the release and starts it next to the running version. The new version takes every new request. The old one finishes the requests it already has, then exits. A Codex WebSocket closes when its current turn ends, and Codex opens a new one for the next turn.

The command waits up to 5 minutes and prints each machine's result. A machine that is offline or already updating is skipped. If a machine fails, or is still updating after 5 minutes, the command exits with an error. Check the dashboard for details.

A system without a prebuilt binary can't update this way. Run `cargo install` on it again.

### `conflux guest approve|deny|remove <machine>`

Decides whether a guest may send requests through the hub. A guest is a machine signed in to Tailscale as another user, or a tagged machine. Run it on any of the hub owner's machines.

```sh
conflux guest approve studio
```

- `approve` lets the machine send requests through the hub, starting with its next request.
- `deny` turns down the machine's request.
- `remove` takes away an approval. Requests already running finish, and a Codex WebSocket closes after its current turn.

`<machine>` is the name the dashboard shows. If two guests share a name, use the Tailscale ID that the error lists.

While a guest is not approved, conflux points its coding tools back at its own logins, and points them at the hub again once it is approved. A guest asks when it joins, and the dashboard shows it under Machines as "Wants to join". A guest that was turned down or removed asks again only when someone runs `conflux join` on it, or clicks "Ask again" in its Mac menu bar app. Reconnecting does not ask again.

The hub keeps its approvals in `guests.json` in its config folder. They move with the hub.

## Moving the hub

Your logins move with the hub, and your nodes follow on their own. Any node of yours that is online and on the hub's version can become the hub. Start the move on the dashboard: open Machines, click the ⋯ button on the machine's row, and pick Make hub. Or run this on the hub:

```sh
conflux hub move <machine>
```

### `conflux hub move <machine>`

Run it on the current hub. `<machine>` is the name the dashboard shows for that node. The hub checks the move first, then stops taking requests and sends its accounts to that machine over the node's connection. Codex WebSocket sessions close when the move starts.

The new machine stores the accounts, confirms the move with the old hub, and restarts as the hub. Requests already running on it keep running. The old hub then waits until the new hub answers, and restarts as its node. Your other nodes switch to the new hub on their own. A node that was off during the move must run `conflux join <new-hub>` itself.

The hub refuses the move, and changes nothing, if:

- the hub or the machine is updating,
- the machine is offline, a guest machine, or on another version,
- the machine has a battery and you did not pass `--force`.

If the machine already holds accounts, it refuses after the move starts, and the hub goes back to serving requests. If the new machine does not confirm the move within 2 minutes, the old hub goes back to serving requests. While the move runs, the Overview page shows a Cancel button.

Options:

- `--force` lets a machine with a battery become the hub. Without it, the hub refuses a laptop, because a laptop may leave home and change IP address.

After a move, the old hub keeps a copy of the accounts in a `retired-*` folder under its data directory. Delete it once the move has worked.

### `conflux hub resume`

Run it on the hub to cancel a move that has not finished. The hub goes back to serving requests as if the move never started.

## Other

### `conflux t3-key`

Run it on the hub. It prints a URL and a management key. Add them in [T3 Code](https://t3.codes) under Settings > Providers > Usage providers to see your pooled limits there. Running it again prints the same key.
