# conflux

Rust proxy that pools Claude Code and Codex subscription accounts on one hub machine.

## Approach

- Don't keep complexity just because it exists, or add machinery to look thorough. Find the real constraint, then make the smallest change that makes the right behavior obvious. Fight scope creep.
- If a rule here fights the task, say so and get the developer's sign-off before breaking it.

## Don't break the developer's setup

- Never run `conflux` against the real install. Set `CONFLUX_HOME` to a temp dir. The real install holds OAuth tokens for live accounts.
- Never copy account tokens out of the real install. A refresh from the copy can invalidate the real install's token and log the account out. To test against the real providers, run `conflux login` in the temp `CONFLUX_HOME` and ask the developer to approve it.
- Never run `conflux on`, `off`, `join` or `service` for real. `CONFLUX_HOME` does not isolate them. They rewrite `~/.claude/settings.json` and `~/.codex/config.toml` and restart the user's conflux service, which can cut off the agent running the command. Test that code through `ClientPaths` with temp paths, as `src/setup.rs` does.
- Never kill conflux by name, for example with `pkill -f conflux`. The developer's hub may be serving their agent sessions, including yours. Kill only a PID you started.

## Try a change in the running app

`cargo run --example dev` starts a throwaway copy of conflux next to the real install. It builds the binary, then runs a hub, a node, and a pretend Claude and OpenAI on free ports in a temp folder. It prints the commands that run Claude Code, Codex, and the CLI against the copy, and the commands that force an account out of quota.

- Pass `-- --real` to use the real providers. The copy starts with no accounts. Run the `conflux login` command it prints and ask the developer to approve it.
- On a Mac, pass `-- --app` to build the menu bar app and start it against the copy. The dev build has its own bundle ID, so it runs next to the installed app. It hides Restart and refuses Set up, because the copy has no background service.
- To see the Mac app's menu, run the `--snapshot` command that `--app` prints. It draws the menu to a PNG, with light and dark side by side. It needs no screen recording permission, so it works over ssh.
- Point Claude Code at the copy with `CLAUDE_CONFIG_DIR`, as the command prints. Setting `ANTHROPIC_BASE_URL` in the shell does not work. `~/.claude/settings.json` wins, and the request goes to the real hub.
- The copy's hub cannot install conflux on your other machines. It runs `false` in place of `ssh`, so Install on the dashboard and `conflux add` fail. To fake a machine, set `CONFLUX_SSH` to a script before you start the copy.
- Stop it with Ctrl-C or the `kill` command it prints. It then stops the hub, node, and Mac app it started and removes the temp folder. Killing the PID of `cargo run` leaves them running.
- To test on another operating system from inside T3 Code, push the branch, list the linked environments, and delegate the test to an agent on a machine that runs that system. The rules in "Don't break the developer's setup" apply on that machine too.

## Hit every path

A fix that works on one path is often missing from the others. Before calling a change done, check each of these that applies:

- HTTP (`src/proxy.rs`) and WebSocket (`src/ws.rs`)
- Claude and Codex
- Hub and node (`src/node.rs`)
- The dashboard, `/conflux/api`, and `conflux status`

## Code style

- No code comments. If code needs explaining, give it better names, add a test, or restructure it.
- Never put real hostnames, IP addresses, tailnet names, or personal paths in code, tests, or docs.
- Don't commit plans, research notes, or agent scratch files.

## Tests

- Prefer integration tests in `tests/` that drive real behavior.
- Don't commit unit tests, instead verify in real environments.
- Extend an existing test before writing a new one.
- Every test must be able to catch a real bug. Don't test defaults, string templates, trivial helpers, or behavior another test already covers.

## Pull requests

- Never make a PR unless the developer explicitly asks for one.
- Use conventional commit titles in plain language, such as `fix(router): stop sending requests to accounts that are out of quota`.
- In the body, state the problem in a sentence or two, then how you fixed it. End with the model and harness that did the work.
- UI changes need before/after screenshots. Motion or timing needs a short gif or video.
- Upload PR evidence to GitHub. Never commit PR-only screenshots or assets.
- Keep each PR focused. One request is one PR.
- When babysitting, call `watch_pull_request` and end your turn. T3 Code wakes you on checks, comments, or conflicts. Do not poll or sleep. Stay quiet when nothing is new. Stop when checks are green on the latest commit.
- If review bots run on the PR, verify each bot finding against the source, fix real ones, and dismiss false positives with a written reason. Wait until the bots are green on the latest commit before you stop.

## Checks

Run `git config core.hooksPath .githooks` once per clone. The pre-commit hook then runs fmt and clippy before each commit.

- `cargo fmt`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- Build in this worktree's own `target/`. Never point `CARGO_TARGET_DIR` at `/tmp` or a scratchpad, because a cold build is several GB.
- Read-only review means no source edits. Building and running tests in the worktree is still fine.
- Reviewers and subagents run focused tests, such as `cargo test --lib router::`. The main agent runs the full suite.
