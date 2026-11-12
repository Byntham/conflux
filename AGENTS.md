# conflux

Rust proxy that pools Claude Code and Codex subscription accounts on one hub machine.

## Approach

- Don't keep complexity just because it exists, or add machinery to look thorough. Find the real constraint, then make the smallest change that makes the right behavior obvious. Fight scope creep.
- If a rule here fights the task, say so and get the developer's sign-off before breaking it.

## Don't break the developer's setup

- Never run `conflux` against the real install. Set `CONFLUX_HOME` to a temp dir and point the `CONFLUX_*` endpoint variables at a fake upstream. The real install holds OAuth tokens for live accounts.
- Never run `conflux on`, `off`, `join` or `service` for real. `CONFLUX_HOME` does not isolate them. They rewrite `~/.claude/settings.json` and `~/.codex/config.toml` and restart the user's conflux service, which can cut off the agent running the command. Test that code through `ClientPaths` with temp paths, as `src/setup.rs` does.
- Never kill conflux by name, for example with `pkill -f conflux`. The developer's hub may be serving their agent sessions, including yours. Kill only a PID you started.
- Never call Anthropic or OpenAI for real. Tests use the fake upstream in `tests/common`.

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

- `cargo fmt`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- Build in this worktree's own `target/`. Never point `CARGO_TARGET_DIR` at `/tmp` or a scratchpad, because a cold build is several GB.
- Read-only review means no source edits. Building and running tests in the worktree is still fine.
- Reviewers and subagents run focused tests, such as `cargo test --lib router::`. The main agent runs the full suite.
