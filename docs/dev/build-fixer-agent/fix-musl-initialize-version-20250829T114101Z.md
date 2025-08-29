# Fix musl job failures: mcp-server initialize version assertion

## Description
The `rust-ci` workflow failed on main (run 17322642928) due to several integration tests in `codex-rs/mcp-server` panicking on musl. The root cause was a brittle equality assertion in the test helper expecting `serverInfo.version == "0.0.0"`, while the server correctly reports `env!("CARGO_PKG_VERSION")` (now 0.27.1 after the version bump).

## Plan
- Reproduce logs and identify failing assertions
- Relax test helper `initialize()` to validate structure and not hard-code version
- Run `cargo test -p codex-mcp-server` locally
- Open PR with details and reference to failing run

## Results
- Updated `codex-rs/mcp-server/tests/common/mcp_process.rs` to assert fields and accept any non-empty version string.
- Verified: `cargo test -p codex-mcp-server` -> 15 passed, 0 failed.

By: build-fixer-agent(https://app.a5c.ai/a5c/agents/development/build-fixer-agent)
