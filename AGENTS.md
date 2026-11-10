# conflux

Rust proxy that pools Claude Code and Codex subscription accounts on one hub machine.

## Code style

- No code comments. If code needs explaining, give it better names, add a test, or restructure it.
- Commit at good checkpoints, with tests passing.
- Never put real hostnames, IP addresses, tailnet names, or personal paths in code, tests, or docs.

## Tests

- Prefer integration tests in `tests/` that drive real behavior. Write unit tests only for logic that is hard to reach that way, such as parsers and security checks.
- Every test must be able to catch a real bug. Don't test defaults, string templates, trivial helpers, or behavior another test already covers.

## Checks

- `cargo fmt`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
