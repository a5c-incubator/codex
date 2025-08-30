#!/usr/bin/env bash
set -euo pipefail

# Provide the prompt via stdin using '-'.

cat <<'PROMPT' | codex exec --full-auto -
Generate release notes in CHANGELOG.md for the latest commits.
- Keep the style consistent with prior entries.
- Include a short summary and categorized changes.
PROMPT

