# CLI/TUI: /usage – guardrails usage and reset times

## Plan
- Add CLI subcommand `usage` with clear help text.
- Add TUI slash command `/usage` to display usage in history.
- Implement best-effort usage fetch in `codex-login` (ChatGPT/API key), fallback if unavailable.
- Do not add persistent TUI header indicator yet (avoid snapshot churn).

## Notes
- Gracefully handle unavailable data (no crash, actionable message).
- Avoid touching CODEX_SANDBOX_* env var logic.
