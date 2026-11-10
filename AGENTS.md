# conflux

Rust proxy that pools Claude Code and Codex subscription accounts on one hub machine.

## Code style

- No code comments. If code needs explaining, give it better names, add a test, or restructure it.
- Commit at good checkpoints, with tests passing.
- Never put real hostnames, IP addresses, tailnet names, or personal paths in code, tests, or docs.

## Tests

- Prefer integration tests in `tests/` that drive real behavior. Write unit tests only for logic that is hard to reach that way, such as parsers and security checks.
- Every test must be able to catch a real bug. Don't test defaults, string templates, trivial helpers, or behavior another test already covers.

## Pull requests

- Never make a PR unless the developer explicitly asks for one.
- Use conventional commit titles in plain language, such as `fix(router): stop sending requests to accounts that are out of quota`.
- In the body, state the problem in a sentence or two, then how you fixed it. End with the model and harness that did the work.
- UI changes need before/after screenshots. Motion or timing needs a short video.
- Upload PR evidence to GitHub. Never commit PR-only screenshots or assets.
- Keep each PR focused. One request is one PR.
- When babysitting, call `watch_pull_request` and end your turn. T3 Code wakes you on checks, comments, or conflicts. Do not poll or sleep. Stay quiet when nothing is new. Stop when checks are green on the latest commit.
- If review bots run on the PR, verify each bot finding against the source, fix real ones, and dismiss false positives with a written reason. Wait until the bots are green on the latest commit before you stop.

## Checks

- `cargo fmt`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
