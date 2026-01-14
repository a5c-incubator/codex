# Subagent smoke coverage

These are the lightweight end-to-end checks we run whenever Step-5 changes touch CLI/TUI plumbing. They ensure the runtime wiring proven by automated tests still works when exercised through user entry points.

## CLI flow

1. From a sample workspace containing project/user/plugin manifests, run:

   ```bash
   codex agents list --json --project-dir <project> --user-dir <user> \
     --plugin demo=<plugin> --cli-manifest <inline-json>
   ```

   Confirm the manifest ordering, validation issues, and summary counts match the expectations codified in `codex-rs/cli/tests/agents_list.rs:15-253`. This also verifies the `--json` surface the TUI consumes is stable.

   Telemetry: run the command with `RUST_LOG=info` (or tail `~/.codex/log/codex-cli.log`) and make sure a `codex.agents_list_invocation` event appears with the expected `invocation=cli_list_json`, scope counts, and issue totals. These logs share the same schema as the runtime refresh telemetry, so a single `rg codex.agents_list_invocation ~/.codex/log -n` covers both manual and daemon refreshes.

2. In the same workspace, start a non-interactive CLI session with a specific agent:

   ```bash
   codex --use-subagent project-alpha --json "investigate the failing test"
   ```

   Ensure the session banner reports the requested subagent before the first turn and the rollout captures the `SubagentLifecycle` telemetry.
3. With *no* `.claude/agents` present (fresh workspace), run:

   ```bash
   codex agents list
   ```

   Confirm the human-readable output starts with the `Claude built-in personas are always available` banner, lists the four IDs, and spells out both `codex --use-subagent builtin-*` and `:use-agent builtin-*` activation hints. Then run:

   ```bash
   codex agents list --json
   ```

   Confirm the built-in personas (`builtin-general-purpose`, `builtin-plan`, `builtin-explore`, `builtin-review`) still appear in JSON form, matching the CLI test `codex-rs/cli/tests/agents_list.rs::json_lists_built_ins_without_custom_sources`. Immediately activate the Plan persona to prove the path works without manifests:

   ```bash
   codex --use-subagent builtin-plan --json "draft a remediation plan for the failing tests"
   codex agents resume-status --json | jq '.[] | select(.agent_id=="builtin-plan")'
   ```

   Expect `session_source: subagent/plan` in both the streamed `SubagentLifecycleEvent` rows and the `resume-status` payload, the transcript path under `~/.codex/subagents/builtin-plan/runs/<run_id>/`, and a non-empty resume token. This smoke guard matches the `built_in_subagents_activate_without_manifests` test but validates the CLI UX and persisted artifacts manually.

## TUI / manual flow

1. Launch the TUI (`codex` with no subcommand) and run `/agents`. Verify the panel mirrors the CLI JSON (including validation issues) and that editing a manifest reflects immediately after rerunning `/agents`.
2. Use `:use-agent project-alpha` inside the chat input. Confirm the status panel shows the active agent and subsequent turns emit the expected lifecycle events.
   - Telemetry: after running the slash command, `tail -F ~/.codex/log/codex-tui.log | rg codex.subagent_client_switch` should show a `requested` event followed by a `completed` event referencing the same `thread_id`/`agent_id`. Errors (unknown agent, validation failure) also show up here, so reviewers can trace the user action even if activation never succeeds.
3. Restart the TUI with `codex --use-subagent builtin-review` to confirm the flag preselects the same agent the slash command activates.
   - `codex exec --use-subagent ...` produces the same `codex.subagent_client_switch` breadcrumbs (origin `exec_flag`) right after the override request is sent, so CLI sessions leave the same trace as the TUI.
4. Built-in explore persona: from a clean session, type `:use-agent builtin-explore`. The header should switch immediately, lifecycle events should show `agent_id=builtin-explore` plus `session_source: subagent/explore`, and clearing the agent should create a new resume token under `~/.codex/subagents/builtin-explore`. Follow up with `codex agents resume-status --json | jq '.[] | select(.agent_id=="builtin-explore")'` to verify the stored run lists `session_source: subagent/explore`.

> These TUI flows are still manual; automation is tracked separately, so capturing the exact smoke steps here keeps the testing matrix auditable.

### SubagentLifecycle telemetry checkpoints

- **CLI (`codex --use-subagent … --json`)** – The first JSON line (`type == "session_configured"`) contains the `rollout_path`. That file lives under `~/.codex/sessions/<yyyy>/<mm>/<dd>/rollout-<timestamp>-<thread>.jsonl` and records every `EventMsg::SubagentLifecycle`. Example:

  ```bash
  codex --use-subagent project-alpha --json "investigate the failing test" | tee /tmp/subagent-session.jsonl
  ROLLOUT=$(jq -r 'select(.msg.type=="session_configured") | .msg.rollout_path' /tmp/subagent-session.jsonl | tail -n1)
  jq 'select(.item.msg.SubagentLifecycle != null) | .item.msg' "$ROLLOUT"
  ```

  Expect a matching `activated`/`stopped` pair with the manifest digest, approval policy, sandbox policy, and resume token. The same data is emitted as the `codex.subagent_lifecycle` tracing event—set `RUST_LOG=codex_core=info` (or wire OTEL exporters) if you need to watch the log stream live during the CLI smoke step. Pair those lifecycle entries with the adjacent `codex.subagent_client_switch` events (surface + status) to prove which user action triggered the activation.

- **TUI (`/agents`, `:use-agent`, `codex --use-subagent …`)** – Run `:rollout` (alias `/rollout`) after switching agents to print the active rollout path, then reuse the `jq` filter above on that file to confirm the lifecycle entries landed. In parallel, `tail -F ~/.codex/log/codex-tui.log | rg codex.subagent_lifecycle` shows the INFO-level tracing records the TUI writes whenever an agent is activated or cleared, and `rg codex.subagent_client_switch ~/.codex/log/codex-tui.log` links each lifecycle event back to the slash-command or CLI action that triggered it. This pairing makes it trivial to explain how a user got into (or out of) a broken subagent state.
