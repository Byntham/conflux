# Troubleshooting

## Logs and the background service

`conflux join` and `conflux wait` install conflux as a background service. Here is what that is on each system.

| System | Service | Logs | Restart |
| --- | --- | --- | --- |
| Linux | systemd user unit `conflux.service` in `~/.config/systemd/user/` | `journalctl --user -u conflux` | `systemctl --user restart conflux` |
| macOS | launchd agent `dev.conflux` in `~/Library/LaunchAgents/` | `~/Library/Logs/conflux.log` | `launchctl kickstart -k gui/$(id -u)/dev.conflux` |
| Windows | `conflux` entry in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` | none | `conflux wait`, or sign out and back in |

On Windows, conflux runs in a hidden console and keeps no log file.

To watch logs in a terminal, stop the background service, then run `conflux serve`. Set `CONFLUX_LOG=debug` for more detail. If you only kill conflux, the service or its supervisor starts it again, so stop it this way:

- Linux: `systemctl --user stop conflux`. Start it again with `systemctl --user start conflux`.
- macOS: `launchctl bootout gui/$(id -u)/dev.conflux`. Start it again with `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.conflux.plist`.
- Windows: end every `conflux.exe` in Task Manager. Start it again with `conflux wait`.

## Common problems

### "the conflux daemon is not running"

A command like `conflux status` could not reach conflux on this machine. Start the background service with the restart command above. If that fails, check the logs.

### A coding tool says "the hub (...) is unreachable"

The node is running but can't reach the hub. Check that the hub machine is on, that conflux runs there, and that both machines show up in `tailscale status`.

To keep working until the hub is back, run `conflux off` and restart your session. Run `conflux on` when the hub is back. If `off` fails because it can't read `~/.grok/config.toml` or `~/.config/muse/settings.json`, fix or move that file and run `conflux off` again. conflux changes no tool's settings until it can read all of them.

### Requests don't show up on the Requests tab

- Restart the session. Sessions that were open before `conflux join` keep their old settings.
- Check that `~/.claude/settings.json` has `ANTHROPIC_BASE_URL` set to `http://127.0.0.1:<port>`, that `~/.codex/config.toml` has `model_provider = "conflux"`, that `~/.grok/config.toml` has `cli_chat_proxy_base_url` set to `http://127.0.0.1:<port>/grok/v1`, and that `~/.config/muse/settings.json` has `endpoint_transport.base_url` set to `http://127.0.0.1:<port>/muse/v1`. `<port>` is `port` in `config.toml`, which is 8317 by default.
- Check your shell for `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, `OPENAI_BASE_URL`, or `GROK_CLI_CHAT_PROXY_BASE_URL`. These can make the tools skip conflux.

### The dashboard says "host not allowed"

conflux answers only to the names it knows. On the hub, use `http://127.0.0.1:8317`. From other machines, use the hub's tailnet name or tailnet IP address. A LAN address or another name won't work.

### "Claude subscription accounts only serve Claude Code"

The hub refuses Claude requests from anything other than Claude Code. See `claude_code_only` in [Configuration](configuration.md#configtoml).

### An account shows "needs login"

Its token stopped working. Run `conflux login claude`, `conflux login codex`, `conflux login grok`, or `conflux login muse` again with the same account. It keeps its ID and settings.

### conflux stops when you log out of Linux

conflux runs as a systemd user service. It keeps running after you log out only if your user has lingering turned on. `conflux join` tries to turn it on and warns you if it can't. Ask an admin to run `sudo loginctl enable-linger <user>`.

### `conflux join` refuses because the machine holds accounts

Joining another hub would strand this machine's accounts. To move them, run [`conflux hub move <machine>`](commands.md#conflux-hub-move-machine) on this machine. The other machine must be one of its nodes, and it becomes the hub. To join anyway, pass `--force`.

### A hub move got stuck

If the new machine stops before it confirms the move, the old hub goes back into service after 2 minutes. To cancel sooner, click Cancel on the dashboard or run `conflux hub resume` on the old hub.

If the new machine already has the accounts, restart conflux there. It confirms the move with the old hub again and finishes it, or drops the accounts if the move was cancelled.

If the old hub confirmed the move but the new hub never comes up, the old hub keeps refusing requests and waits for it. Restart conflux on the new machine. If that machine is gone, the old hub still has a copy of the accounts in the `retired-*` folder under its data directory.
