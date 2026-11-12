# Commands

Every `conflux` command, what it does, and where to run it. `conflux help <command>` prints a short version in the terminal.

`conflux` on its own prints the command list and a link to the hub's dashboard. On a node, the link points at the hub.

Some commands change only the machine you run them on. The account commands send their request to the conflux service on this machine. On a node, that service passes the request on to the hub. So you can manage accounts from any machine, as long as conflux runs there and the hub is up.

An account ID looks like `claude-1a2b3c4d5e6f` or `codex-1a2b3c4d5e6f`. `conflux status` lists them.

- [Accounts](#accounts): `status`, `login`, `usage`, `enable`, `disable`, `overage`, `remove`
- [This machine](#this-machine): `join`, `wait`, `off`, `on`, `leave`, `sync`, `serve`
- [Every machine](#every-machine): `update`
- [Moving the hub](#moving-the-hub): `hub move-code`, `hub take-over`, `hub resume`
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

### `conflux login claude`, `conflux login codex`

Adds an account to the pool. If the account is already in the pool, this logs it in again and keeps its settings.

For `claude`, conflux prints a link. Open it in a browser signed in to the Claude account and approve access. The browser then shows a connection error, which is expected. Paste the whole URL from the address bar into the terminal.

For `codex`, conflux prints a link and a code. Open the link in a browser signed in to the ChatGPT account and enter the code. conflux waits and finishes on its own.

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

### `conflux join <hub>`

Makes this machine the hub, or a node of a hub.

- `conflux join self` makes this machine the hub. `localhost`, `127.0.0.1`, and this machine's own name also work.
- `conflux join <hub>` makes this machine a node. `<hub>` is the hub's name in `tailscale status`. conflux checks that it can reach the hub and warns you if it can't.

`join` then does three things:

1. It installs conflux as a background service and starts it. See [Logs and the background service](troubleshooting.md#logs-and-the-background-service) for what it installs on each system.
2. It points Claude Code at conflux. It sets `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` under `env` in `~/.claude/settings.json`.
3. It points Codex at conflux. It sets `model_provider = "conflux"` and adds a `[model_providers.conflux]` table in `~/.codex/config.toml`.

Before it changes either file, conflux saves the values it replaces. `conflux off` and `conflux leave` put them back. If `CLAUDE_CONFIG_DIR` or `CODEX_HOME` is set, conflux edits the files there instead.

conflux warns you if your shell sets a variable that can bypass these settings, such as `ANTHROPIC_API_KEY` or `OPENAI_BASE_URL`.

Open Claude Code and Codex sessions keep their old settings. Restart them to go through conflux.

Options:

- `--no-service` skips the background service. Run `conflux serve` yourself.
- `--force` joins another hub even though this machine holds logged-in accounts. Without it, `join` refuses, because those accounts would be stranded. To move accounts to a new hub, use [`conflux hub take-over`](#conflux-hub-take-over-old-hub---code-code) instead.

`join` refuses while this machine is in the middle of a hub move. Finish the move, or run `conflux hub resume`.

### `conflux wait`

Installs and starts the background service, then waits for a hub to link this machine. The installer runs it when you don't name a hub.

To link a waiting machine, open Machines on the hub's dashboard, click "Find machines", and click "Link" next to it. The machine accepts a link only from a machine owned by the same Tailscale user. When linked, it edits Claude Code's and Codex's settings the same way `join` does.

If this machine has already joined a hub or holds accounts, `wait` changes nothing. It only restarts the service if one is installed.

### `conflux off`

Points Claude Code and Codex back at this machine's own logins, using the values `join` saved. The conflux service keeps running. Restart open sessions to pick up the change.

Use it when the hub is down. If you have never logged in to Claude Code or Codex on this machine, log in directly first.

### `conflux on`

Points Claude Code and Codex at conflux again after `conflux off`. It saves the current values first, so a later `off` restores what was there just before `on`.

### `conflux leave`

Stops conflux on this machine and gives Claude Code and Codex their old settings back. It removes the background service.

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

### `conflux update [machine...]`

Updates machines to the latest conflux release. With no names, it updates every online machine that is behind. With names, it updates only those. Use the names the dashboard shows.

Each machine downloads the release and starts it next to the running version. The new version takes every new request. The old one finishes the requests it already has, then exits. A Codex WebSocket closes when its current turn ends, and Codex opens a new one for the next turn.

The command waits up to 5 minutes and prints each machine's result. A machine that is offline or already updating is skipped. If a machine fails, or is still updating after 5 minutes, the command exits with an error. Check the dashboard for details.

A system without a prebuilt binary can't update this way. Run `cargo install` on it again.

## Moving the hub

Your logins move with the hub, and your nodes follow on their own. The new machine needs conflux installed. Any node is ready.

```sh
conflux hub move-code                               # on the current hub
conflux hub take-over <old-hub> --code <code>       # on the new machine, within 10 minutes
conflux join <new-hub>                              # back on the old machine
```

### `conflux hub move-code`

Run it on the current hub. It prints a one-time code and the exact `take-over` command to run on the new machine. The code works for 10 minutes.

### `conflux hub take-over <old-hub> --code <code>`

Run it on the new machine. It asks the old hub to stop taking requests and send its accounts, then makes this machine the hub. Codex WebSocket sessions close when the move starts.

When it finishes, your nodes switch to the new hub on their own. Wait about a minute, with the old hub still running, then run `conflux join <new-hub>` on the old machine. A node that was off during the move can run `conflux join <new-hub>` itself.

If the move stops after the accounts reached the new machine, run `conflux hub take-over <old-hub>` again without `--code` to finish it. If it stops before that, `take-over` asks for a code. Run `conflux hub resume` on the old hub, then start again with a new code.

Options:

- `--code <code>` is the code from `conflux hub move-code`. You need it to start a move, but not to finish one that already has the accounts.
- `--force` lets a machine with a battery become the hub. Without it, `take-over` refuses on a laptop, because a laptop may leave home and change IP address.

`take-over` refuses if this machine already holds accounts.

After a move, the old hub keeps a copy of the accounts in a `retired-*` folder under its data directory. Delete it once the move has worked.

### `conflux hub resume`

Run it on the old hub when a move got stuck before it finished. The old hub goes back to serving requests as if the move never started.

## Other

### `conflux t3-key`

Run it on the hub. It prints a URL and a management key. Add them in [T3 Code](https://t3.codes) under Settings > Providers > Usage providers to see your pooled limits there. Running it again prints the same key.
