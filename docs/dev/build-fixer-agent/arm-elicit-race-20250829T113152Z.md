# Fix: Patch approval race on ARM GNU runner

## Context
- Workflow run failed on job: ubuntu-24.04-arm - aarch64-unknown-linux-gnu
- Failure: mcp-server test `suite::codex_tool::test_patch_approval_triggers_elicitation` timed out
- Log excerpt showed: `No pending approval found for sub_id: 1`

## Root cause
- Race condition in `codex-core`: inserted `pending_approvals` after sending event on channel. On fast paths, approval arrives before insertion.

## Change
- Reordered insertion before sending events in:
  - `request_command_approval`
  - `request_patch_approval`

## Verification
- Ran `cargo test -p codex-mcp-server`: all tests passed locally.
