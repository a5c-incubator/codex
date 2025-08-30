#!/usr/bin/env bash
set -euo pipefail

# Capture JSONL logs and fail the script if any command run by the agent
# exited non-zero, or if the agent reported an error/abort.

require() { command -v "$1" >/dev/null 2>&1 || { echo "Missing dependency: $1" >&2; exit 2; }; }
require jq

LOG_FILE=${1:-codex.jsonl}

codex exec --json --full-auto "update CHANGELOG for next release" | tee "$LOG_FILE"

if jq -e 'select(
      (.msg.type=="exec_command_end" and (.msg.exit_code // 0) != 0)
      or .msg.type=="error"
      or .msg.type=="turn_aborted"
    )' "$LOG_FILE" >/dev/null; then
  echo "codex exec reported failures; see $LOG_FILE" >&2
  exit 1
fi

echo "codex exec completed successfully"

