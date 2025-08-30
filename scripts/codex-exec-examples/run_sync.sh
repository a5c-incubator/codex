#!/usr/bin/env bash
set -euo pipefail

# Simple synchronous run with human-readable logs.
# Requires Codex CLI to be installed and authenticated.

codex exec --full-auto --color=never "update CHANGELOG for next release"

