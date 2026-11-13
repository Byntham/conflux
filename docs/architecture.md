# Architecture

> [!IMPORTANT]
> For agents: do not rely on this document for architecture decisions. It is a map, and it can be out of date. Read the code before you change anything.

conflux is one Rust binary. One machine, the hub, holds the Claude and Codex subscription accounts and talks to the providers. Every other machine runs conflux as a node and forwards its traffic to the hub. Machines find each other over Tailscale.

## Roles

`config.toml` has a `role` field with one of three values. The worker reads it at start and runs the matching server.

- A hub runs `app::serve` (`src/app.rs`). It holds the accounts, routes requests, and serves the dashboard. It listens on `127.0.0.1:<port>` and on each tailnet IP it finds. The default port is 8317.
- A node runs `node::serve` (`src/node.rs`). It listens only on loopback and forwards requests to `<hub>`.
- A waiting machine runs `link::serve` (`src/link.rs`). It listens on loopback and the tailnet. It accepts a link from a hub, or finds a hub and links to it when asked on loopback.

A machine gets its role in one of these ways:

- `conflux join <hub>` sets the role to node. If `<hub>` names this machine, it sets the role to hub instead.
- `conflux wait` sets the role to waiting. It leaves the role alone if the machine has already joined or holds accounts.
- A hub links a waiting machine, or a waiting machine links itself to a hub. It then becomes a node. See "Linking waiting machines".
- `conflux hub take-over` makes the new machine a hub. See "Hub move".
- With no `config.toml`, the role defaults to hub.

## Process model

`conflux serve` is a supervisor (`src/supervisor.rs`). It starts the hidden `conflux worker` subcommand and gives it a pipe on stdin as a lifeline. Every worker inherits that pipe. When the last worker exits, the pipe closes, and the supervisor starts a new worker 3 seconds later. The installed service (`src/service.rs`) runs `conflux serve` as a systemd user unit, a launchd agent, or a Windows Run key entry.

A hub takes an exclusive file lock on `hub.lock` in the data directory, so two hubs cannot share one account store.

### Zero-downtime update

`src/update.rs` and `src/lifecycle.rs` replace a running worker without dropping requests.

1. The updater downloads the release asset next to the binary and checks that it answers `--version`.
2. It moves the old binary aside and puts the new one in its place.
3. The old worker starts the new binary as `conflux worker`. The `CONFLUX_HANDOFF` variable tells it a loopback port, a token, and the listening sockets it may inherit.
4. The new worker connects back and proves the token. The old worker pauses token refresh and usage polling, then sends its in-memory state. That state is the routing state, usage snapshots and their history, the request log, and the machine list.
5. The new worker imports the state, serves on the inherited sockets, and replies that it is ready.
6. The old worker stops accepting connections and finishes the requests it already has, for up to an hour. It forwards late changes, such as new response owners, to the new worker.

If any step fails, the old worker resumes and puts the old binary back. A node runs the same handoff with no state to send. Routing state, usage, and the request log live only in memory. An update keeps them. A plain restart loses them.

The hub and a waiting machine ask the release page for the latest version when they start and every 5 minutes after. The refresh button at the bottom of the dashboard's sidebar and Check for updates in the Mac menu call `/conflux/api/update/check` to ask at once.

`conflux update` calls `/conflux/api/update` on the hub. The hub sends an update command to each outdated node over its control socket, then updates itself.

## Request flow

Claude Code and Codex on every machine point at `http://127.0.0.1:<port>`. On the hub, that is the hub itself. On a node, it is the node's own conflux.

```
Claude Code / Codex
        |
        v
  node (127.0.0.1)  --- adds x-conflux-node header --->  hub (tailnet IP)
                                                           |
                                         access guard, hub state guard
                                                           |
                                          router picks an account
                                                           |
                                                           v
                                        Anthropic API / ChatGPT Codex API
```

A node does not route or hold credentials. `src/node.rs` copies each HTTP request and WebSocket session to the hub, minus hop-by-hop headers, and streams the reply back. It adds an `x-conflux-node` header so the hub can log which machine sent the request. If the hub is unreachable, the node returns an error in the provider's format. `GET /` on a node redirects to the hub's dashboard.

On the hub, `src/proxy.rs` handles `/v1/*` and `/backend-api/*`.

- `/v1/messages` is Claude. `/backend-api/codex/*` is Codex. Other `/v1/*` paths are Claude when the user agent is Claude Code, and Codex otherwise.
- By default, the hub only serves Claude requests that come from Claude Code. The `claude_code_only` setting controls this.
- The hub reads the model and a session id from the request. It asks the router for an account, removes the client's credentials, and adds the account's OAuth token.
- It streams the response back and scans it for token usage. The in-memory request log (`src/requests.rs`) keeps recent requests for the dashboard. It does not record every failure.

Codex WebSocket sessions arrive on the same paths with an `Upgrade` header and go to `src/ws.rs`. The hub reads the first client frame to get the model, session, and `previous_response_id`. It then opens the upstream socket with the chosen account and relays frames both ways. Claude does not use WebSockets.

## Routing and failover

`src/router.rs` picks an account for each request. It skips accounts that are disabled, need a new login, are cooling down, or are billing-blocked.

- Stickiness. A session id stays pinned to its account for an hour after its last request. A Codex `previous_response_id` goes to the account that created that response.
- Ranking. Without a pin, the router prefers accounts with fresh usage data, then the account whose longest usage window resets soonest.
- Overage. The router picks an account that has used up its usage only if overage is on for it and no other account is free.

The proxy fails over to another account before any response bytes reach the client:

- A 429 sets a cooldown. `src/proxy.rs` reads the provider's headers or error body to decide whether it applies to the whole account or one model, and when it ends. A reset more than about a week away becomes a billing block.
- A 401 triggers one token refresh and a retry. If that fails, the account cools down for a minute.
- A failed connection moves on to the next account.

When no account is left, the client gets the last upstream rejection, or a 429 or 503 from conflux. Once a response starts streaming, there is no failover. For WebSockets, failover happens only during the handshake. If a quota error arrives mid-session, the hub forwards it and closes the socket.

A cooldown or billing block lifts early when a usage reading taken after it shows room again.

## Usage and token refresh

`src/usage.rs` keeps a usage snapshot per account. It updates the snapshot from the rate-limit headers on each response. It also polls each provider's usage endpoint when an account has no data, or has been idle and its data is old. It keeps up to 3 days of readings per account, at most one every 15 minutes, for the dashboard's pool chart. Rate-limit headers carry no model-scoped window, so a model-scoped window such as Fable weekly enters this history only from a poll.

`src/refresh.rs` refreshes OAuth tokens before they expire. Claude tokens refresh an hour ahead, and Codex tokens a few days ahead. Refresh tokens are single use, so each refresh writes a claim and then a receipt to a per-account journal before and after the call. After a crash, the store replays a finished receipt. If the outcome is unknown, conflux does not retry with a token that may already be spent. The account needs a new login once its access token expires. Logins (`src/oauth.rs`, `src/api.rs`) use a pasted redirect URL for Claude and a device code for Codex.

## Hub move

`src/hubmove.rs` moves the accounts to a new hub.

1. On the old hub, `conflux hub move-code` prints a one-time code. It works only from loopback and expires in 10 minutes.
2. On the new machine, `conflux hub take-over <old-hub> --code <code>` calls `/conflux/move/prepare`. The old hub enters the moving state. It stops refresh and usage polling, closes WebSocket sessions, and returns its account files and management key.
3. The new machine stores the files under `incoming/` and calls `/conflux/move/commit`. The old hub becomes retired, moves its account store aside, and tells every connected node about the new hub.
4. The new machine moves the accounts into place, sets its role to hub, restarts its service, and points its own clients at conflux.

The hub state lives in `hub-state.json` (`src/hubstate.rs`), so a restart in the middle of a move resumes in the right state. While moving or retired, the hub refuses proxy traffic with a clear error. A node follows a new hub address only if it resolves to a tailnet IP. `conflux hub resume` cancels a move on the old hub.

## Linking waiting machines

1. On the hub dashboard, Find machines calls `/conflux/api/discover`. The hub lists peers owned by the same Tailscale user and probes `GET /conflux/link` on each one.
2. Link calls `/conflux/api/link`. The hub sends `POST /conflux/link` to the peer.
3. The waiting machine checks with Tailscale that the caller belongs to the same user. It then points its clients at conflux and saves its role as node, with the caller's address as the hub.
4. The worker loop reads the new config and starts the node server.

The hub can also install conflux first. `POST /conflux/api/install` takes a peer's address. If nothing answers on the peer, the hub runs `ssh` to the peer's tailnet name, pinned to that address, as the user that runs the hub's service and without prompts. A node answers only on loopback, so the remote command first checks loopback and stops if conflux runs there. Otherwise it runs the installer, which ends with `conflux wait`. `wait` never changes the role of a machine that already has one. The hub takes one install per machine at a time. The hub then probes the peer until it answers, and links it as in step 2. The dashboard's Install button and `conflux add` both call this route.

A waiting machine can also start the link itself. `conflux join` with no name and the Mac menu bar app both use these two routes, which answer only on loopback.

1. `GET /conflux/api/hubs` lists peers owned by the same Tailscale user and probes `GET /conflux/api/status` on each one. A peer that answers with the hub role is a hub. Nodes listen only on loopback, so they never answer.
2. `POST /conflux/api/join` takes a hub's address. The waiting machine checks again that the address belongs to the same user and answers as a hub. It then becomes a node the same way as in step 3 above, with that address as the hub.

## Control channel

Each node keeps a WebSocket open to `/conflux/control` on the hub (`src/machines.rs`). The node reports its hostname, version, sync setting, and update status every 20 seconds. The hub uses the socket to send two commands, "the hub moved" and "update to version X". The dashboard's machine list comes from these reports.

## Transcript sync

`src/transcript_sync.rs` runs on hubs and nodes when `conflux sync on` has set `transcript_sync`. Every 15 seconds it reads new lines from Claude Code's `projects/` and Codex's `sessions/` JSONL files. It drops noisy line types and posts batches to `/conflux/transcripts` on the hub. The hub sends its own transcripts to itself over loopback.

Each upload carries a byte offset. The hub (`src/transcripts.rs`) appends only when the offset matches its own, and returns the offset it expects next. It stores files as `<machine>/<tool>/<path>`, keeps a thread index, and serves `/conflux/api/threads` and `/conflux/api/transcripts/search`.

## HTTP API and dashboard

- `/conflux/api/*` is the API that the CLI and the dashboard use (`src/api.rs`, `src/link.rs`, `src/hubmove.rs`). The CLI calls it on `127.0.0.1`. `conflux status` reads `/conflux/api/status`.
- `/` serves `src/dashboard.html`, a single page with no external assets. It polls `/conflux/api/dashboard` for accounts, machines, and recent requests.
- `/v0/management/*` is a small API for T3 Code (`src/management.rs`). It needs the management key as a bearer token. It only allows a fixed list of usage and reset-credit calls.

`src/access.rs` guards every route. The `Host` header must be loopback or one of the hub's tailnet names or addresses. Proxy and machine routes refuse browser requests. Other state-changing requests must be same-origin JSON.

## Files on disk

| Location | Contents |
| --- | --- |
| Config dir | `config.toml`, `management-key`, `joined.json`, `hub-state.json` |
| Data dir | `accounts/`, `hub.lock`, `transcript-offsets.json`, and `incoming/` or `retired-<id>/` during a hub move |
| `accounts/` | `<id>.json` credentials, `<id>.settings.json`, and a `<id>.journal/` directory for refresh claims and receipts |
| Transcripts dir | `<machine>/<tool>/<path>` copies of each JSONL file, and a `.state/` directory with offsets and the index |

The config dir is `$XDG_CONFIG_HOME/conflux`, or `~/.config/conflux`. The data dir is `$XDG_DATA_HOME/conflux`, or `~/.local/share/conflux`. The transcripts dir is `~/conflux/transcripts`. `CONFLUX_HOME` moves all three under `<home>/config` and `<home>/data`. `src/config.rs` has the details.

`src/setup.rs` points the clients at conflux. In Claude Code's `settings.json`, it sets `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, and `CLAUDE_CODE_PROMPT_CACHE_TTL=1h` under `env`. The last one keeps the main conversation on the 1 hour cache a subscription gets, because Claude Code treats the token as an API key. In Codex's `config.toml`, it adds a `conflux` model provider and selects it. It records the old values in `joined.json`, and `conflux off` or `conflux leave` puts them back. `CLAUDE_CONFIG_DIR` and `CODEX_HOME` change where it looks.

## Module map

| File | Purpose |
| --- | --- |
| `src/cli.rs` | Command definitions and the client side of each command |
| `src/supervisor.rs` | `serve`, which keeps a worker running |
| `src/app.rs` | Hub server, routes, background tasks, update state export |
| `src/node.rs` | Node server, forwarding, control socket client |
| `src/link.rs` | Waiting role, discovery, and linking |
| `src/proxy.rs` | HTTP proxy, header handling, 429 classification |
| `src/ws.rs` | Codex WebSocket relay |
| `src/router.rs` | Account choice, pins, cooldowns, billing blocks |
| `src/usage.rs` | Usage snapshots from headers and polling |
| `src/accounts.rs` | Account store, credentials, refresh journal |
| `src/refresh.rs` | Token refresh scheduling and crash-safe refresh |
| `src/oauth.rs` | Login and refresh calls to the providers |
| `src/requests.rs` | Request log and token usage scanning |
| `src/api.rs` | `/conflux/api` handlers for status, logins, accounts, update |
| `src/dashboard.rs`, `src/dashboard.html` | Dashboard page and its data |
| `src/management.rs` | Key-protected `/v0/management` API |
| `src/machines.rs` | Node registry and the hub side of the control socket |
| `src/hubmove.rs`, `src/hubstate.rs` | Hub move protocol and its saved state |
| `src/update.rs` | Release checks, download, binary swap |
| `src/lifecycle.rs` | Listener handoff and draining between workers |
| `src/transcript_sync.rs` | Transcript upload from each machine |
| `src/transcripts.rs` | Transcript storage, index, and search on the hub |
| `src/setup.rs` | Edits to Claude Code and Codex config |
| `src/service.rs` | systemd, launchd, and Windows service install |
| `src/tailnet.rs` | Tailscale status, peers, and addresses |
| `src/access.rs` | Host and origin checks |
| `src/config.rs` | Paths, `config.toml`, upstream endpoints |
| `src/fsutil.rs` | Private directories and durable writes |

## Tests

Integration tests live in `tests/`, one file per area: `proxy.rs`, `ws.rs`, `node.rs`, `hubmove.rs`, `link.rs`, `management.rs`, `transcripts.rs`, and `update.rs`. Unit tests sit in `#[cfg(test)]` modules next to parsers and other logic that is hard to reach from outside.

`tests/common/mod.rs` has the shared test setup, the `Harness` type. It starts a fake upstream on a random loopback port and a hub `App` in the same process. The hub uses a temp directory for its paths and `Endpoints::all_at` to send every provider call to the fake. The fake picks its reply by the bearer token in the request, so each test queues replies per account. It also answers token refreshes, scripts WebSocket sessions, and records what it received. `tests/update.rs` runs the built binary with `CONFLUX_HOME` and the `CONFLUX_*` endpoint variables set to the fake.
