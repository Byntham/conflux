# conflux

Rust proxy that pools Claude Code and Codex subscription accounts on one hub machine.

## Code style

- No code comments. If code needs explaining, give it better names, add a test, or restructure it.
- Commit at good checkpoints, with tests passing.
- Never put real hostnames, IP addresses, tailnet names, or personal paths in code, tests, or docs.

## Checks

- `cargo fmt`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
