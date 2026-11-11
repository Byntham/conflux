# Configuration

Most people never need this page. `conflux join` and the installer set everything up. This page lists the files conflux keeps, the settings you can change by hand, and the environment variables it reads.

## Files

conflux uses the same paths on Linux, macOS, and Windows. On Windows, `~` is your user profile folder.

| Path | What it holds |
| --- | --- |
| `~/.config/conflux/config.toml` | This machine's settings. See below. |
| `~/.config/conflux/joined.json` | The Claude Code and Codex settings that `join` replaced, so `off` and `leave` can restore them. |
| `~/.config/conflux/management-key` | The key that `conflux t3-key` prints. Hub only. |
| `~/.local/share/conflux/accounts/` | One set of files per account, with its OAuth tokens in plain text. Hub only. |
| `~/.local/share/conflux/retired-*/` | The accounts an old hub kept after a hub move. Delete it once the move has worked. |
| `~/conflux/transcripts/` | Transcripts that machines copied with `conflux sync on`. Hub only. |

If `XDG_CONFIG_HOME` or `XDG_DATA_HOME` is set, conflux uses it in place of `~/.config` or `~/.local/share`.

conflux also edits two files that belong to the coding tools. See [`conflux join`](commands.md#conflux-join-hub).

| Path | Changed by |
| --- | --- |
| `~/.claude/settings.json`, or `$CLAUDE_CONFIG_DIR/settings.json` | `join`, `on`, `off`, `leave`, and linking a waiting machine |
| `~/.codex/config.toml`, or `$CODEX_HOME/config.toml` | the same commands |

## config.toml

conflux writes this file itself. You can edit it, then restart the background service to apply the change. See [Logs and the background service](troubleshooting.md#logs-and-the-background-service).

```toml
role = "node"
hub = "<hub>"
port = 8317
claude_code_only = true
transcript_sync = false
```

| Key | Default | Meaning |
| --- | --- | --- |
| `role` | `"hub"` | `"hub"`, `"node"`, or `"waiting"`. Use `conflux join` or `conflux wait` to change it. |
| `hub` | none | The hub's tailnet name, on a node. A node updates it on its own after a hub move. |
| `port` | `8317` | The port conflux listens on. A node expects the hub on the same port, so every machine must use the same value. After you change it, run `conflux on` on that machine so Claude Code and Codex use the new port, then restart their sessions. |
| `claude_code_only` | `true` | When true, the hub refuses requests for Claude accounts that don't come from Claude Code. |
| `transcript_sync` | `false` | Whether this machine copies its transcripts to the hub. Use `conflux sync` to change it. |

## Environment variables

### For everyone

| Variable | Effect |
| --- | --- |
| `CONFLUX_LOG` | The log level, such as `debug` or `conflux=debug`. It uses the `tracing` filter syntax. The default is `info`. |
| `CLAUDE_CONFIG_DIR` | Where conflux finds Claude Code's `settings.json`. |
| `CODEX_HOME` | Where conflux finds Codex's `config.toml`. |

`conflux join` warns you if your shell sets `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, `CODEX_HOME`, or `OPENAI_BASE_URL`. These can make Claude Code or Codex skip the settings conflux wrote.

### For development and tests

| Variable | Effect |
| --- | --- |
| `CONFLUX_HOME` | Puts the config in `$CONFLUX_HOME/config` and the data, including transcripts, in `$CONFLUX_HOME/data`. |
| `CONFLUX_ANTHROPIC_API` | The Claude API. Default `https://api.anthropic.com`. |
| `CONFLUX_CLAUDE_AUTHORIZE` | The Claude login page. |
| `CONFLUX_CLAUDE_TOKEN` | The Claude OAuth token endpoint. |
| `CONFLUX_CODEX_API` | The Codex API. Default `https://chatgpt.com/backend-api/codex`. |
| `CONFLUX_CHATGPT_BACKEND` | The ChatGPT backend that conflux reads usage from. |
| `CONFLUX_OPENAI_AUTH` | The OpenAI login server. |
| `CONFLUX_RELEASES` | Where `conflux update` looks for releases. |

Only the hub reads the endpoint variables, because a node sends everything to the hub. The exception is `CONFLUX_RELEASES`, which every machine reads when it updates.

`CONFLUX_HOME` does not isolate everything. `join`, `on`, `off`, `leave`, and `wait` still edit the real Claude Code and Codex settings and the real background service. Test that code through `ClientPaths` with temporary paths, as `src/setup.rs` does.

conflux sets `CONFLUX_HANDOFF` itself when it hands over to a new version. Don't set it.
