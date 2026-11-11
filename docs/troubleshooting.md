# Troubleshooting

## Logs and the background service

`conflux join` and `conflux wait` install conflux as a background service. Here is what that is on each system.

| System | Service | Logs | Restart |
| --- | --- | --- | --- |
| Linux | systemd user unit `conflux.service` in `~/.config/systemd/user/` | `journalctl --user -u conflux` | `systemctl --user restart conflux` |
| macOS | launchd agent `dev.conflux` in `~/Library/LaunchAgents/` | `~/Library/Logs/conflux.log` | `launchctl kickstart -k gui/$(id -u)/dev.conflux` |
| Windows | `conflux` entry in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` | none | Sign out and back in |

On Windows, conflux runs in a hidden console and keeps no log file. To see its output, end `conflux.exe` in Task Manager and run `conflux serve` in a terminal.

To see more detail anywhere, run `conflux serve` with `CONFLUX_LOG=debug`. Stop the background service first, or `conflux serve` can't take the port.

## Common problems

### "the conflux daemon is not running"

A command like `conflux status` could not reach conflux on this machine. Start the background service with the restart command above. If that fails, check the logs.

### Claude Code or Codex says "the hub (...) is unreachable"

The node is running but can't reach the hub. Check that the hub machine is on, that conflux runs there, and that both machines show up in `tailscale status`.

To keep working until the hub is back, run `conflux off` and restart your session. Run `conflux on` when the hub is back.

### Requests don't show up under "Recent requests"

- Restart the session. Sessions that were open before `conflux join` keep their old settings.
- Check that `~/.claude/settings.json` has `ANTHROPIC_BASE_URL` set to `http://127.0.0.1:8317`, and that `~/.codex/config.toml` has `model_provider = "conflux"`.
- Check your shell for `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, or `OPENAI_BASE_URL`. These can make the tools skip conflux.

### The dashboard says "host not allowed"

conflux answers only to the names it knows. On the hub, use `http://127.0.0.1:8317`. From other machines, use the hub's tailnet name or tailnet IP address. A LAN address or another name won't work.

### "Claude subscription accounts only serve Claude Code"

The hub refuses Claude requests from anything other than Claude Code. See `claude_code_only` in [Configuration](configuration.md#configtoml).

### An account shows "needs login"

Its token stopped working. Run `conflux login claude` or `conflux login codex` again with the same account. It keeps its ID and settings.

### conflux stops when you log out of Linux

conflux runs as a systemd user service. It keeps running after you log out only if your user has lingering turned on. `conflux join` tries to turn it on and warns you if it can't. Ask an admin to run `sudo loginctl enable-linger <user>`.

### `conflux join` refuses because the machine holds accounts

Joining another hub would strand this machine's accounts. To move them, run [`conflux hub take-over`](commands.md#conflux-hub-take-over-old-hub---code-code) on the new hub instead. To join anyway, pass `--force`.

### A hub move got stuck

On the old hub, run `conflux hub resume` to put it back in service. Or run `conflux hub take-over <old-hub>` again on the new machine to finish the move.
