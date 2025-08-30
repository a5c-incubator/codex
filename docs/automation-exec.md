# Automation with `codex exec`

Run Codex headless for CI, cron jobs, or local scripts. The `codex exec` subcommand processes a prompt non‑interactively, streams progress to stdout, and exits when the task completes.

- Basic use: `codex exec "…"`
- From stdin: `echo "…" | codex exec -`
- JSONL logs: `codex exec --json "…" | tee codex.jsonl`
- Save last reply: `codex exec --output-last-message out.txt "…"`

## Prompt input

- Argument: `codex exec "bump CHANGELOG for next release"`
- Stdin: `echo "bump CHANGELOG" | codex exec -`
  - If no argument is given and stdin is a TTY, `codex exec` exits with an error. Use `-` to force reading from stdin.

## Exit codes

`codex exec` exits with:
- 0 on normal completion.
- 1 for usage/config errors, e.g.:
  - No prompt provided (and stdin not forced via `-`).
  - Failed `-c` overrides parsing.
  - Outside a Git repo without `--skip-git-repo-check`.

Note: Non‑zero exit codes from model‑executed shell commands are surfaced in logs/events, but do not change the process exit code. In CI, use `--json` and filter events (see below) to fail your job on specific conditions.

## Approvals and sandbox

- Non‑interactive runs default to “never ask for approval”.
- Recommended for automation: `--full-auto` (workspace‑write sandbox, no prompts):
  - `codex exec --full-auto "update CHANGELOG for next release"`
- Or pick an explicit sandbox: `--sandbox read-only|workspace-write|danger-full-access`.
  - See `docs/sandbox.md` for details and policies.

## Output modes

- Human logs (default): timestamped, human‑readable progress and diffs. Use `--color=never` for plain logs.
- JSONL logs: `--json` prints one JSON object per line for events. Streaming deltas are suppressed; all other events are included. The first two lines are a compact config summary and the prompt you provided, then event lines follow.
- Last message file: `--output-last-message path.txt` writes the assistant’s final message to a file.

### JSONL examples (with jq)

Capture logs:

```
codex exec --json --full-auto "update CHANGELOG for next release" | tee codex.jsonl
```

List commands and exit codes:

```
rg '"type":"exec_command_end"' codex.jsonl \
  | jq -r 'fromjson | select(.msg.type=="exec_command_end") | [.msg.call_id, (.msg.exit_code // 0), (.msg.duration_ms // null)] | @tsv'
```

Fail if any command exits non‑zero or the run was aborted:

```
# exits 1 if any matching event exists
jq -e 'select(
  (.msg.type=="exec_command_end" and (.msg.exit_code // 0) != 0)
  or .msg.type=="error"
  or .msg.type=="turn_aborted"
)' codex.jsonl >/dev/null
```

Save the last assistant message for downstream steps:

```
codex exec --json --output-last-message last.txt "summarize recent changes"
cat last.txt
```

## CI snippets

### GitHub Actions

```
- name: Install Codex
  run: npm install -g @openai/codex

- name: Bump changelog via Codex (JSONL; fail on tool errors)
  env:
    OPENAI_API_KEY: ${{ secrets.OPENAI_KEY }}
  run: |
    set -euo pipefail
    codex exec --json --full-auto "update CHANGELOG for next release" | tee codex.jsonl
    # Fail the job if any command failed, or agent reported an error/abort
    jq -e 'select(
      (.msg.type=="exec_command_end" and (.msg.exit_code // 0) != 0)
      or .msg.type=="error"
      or .msg.type=="turn_aborted"
    )' codex.jsonl >/dev/null && { echo "codex exec reported failures"; exit 1; } || true
```

Tips:
- Prefer `--full-auto` for CI to allow workspace writes under a sandbox.
- For headless machines, API key auth is simplest. See `docs/authentication.md`.
- Set `--color=never` to produce plain logs if needed.

## Local scripts

See `scripts/codex-exec-examples/` for runnable examples:
- `run_sync.sh`: simple synchronous run with human logs.
- `stdin_prompt.sh`: pass the prompt via stdin.
- `capture_jsonl_and_fail_on_errors.sh`: capture JSONL, validate with jq, and set an appropriate exit code.

## Related docs

- Sandbox & approvals: `docs/sandbox.md`
- Configuration: `docs/config.md`
- Advanced tips: `docs/advanced.md`

