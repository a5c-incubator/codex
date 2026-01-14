# Claude Subagent Audit

## Sources Reviewed
- `subagents.md:1-610` – official Claude Code sub-agent specification (storage layout, CLI API, lifecycle expectations).
- `AGENTS.md:1-111` – repo-scoped contributor instructions currently surfaced to Codex sessions.
- `codex-rs/core/src/project_doc.rs:3-200` – logic that discovers and concatenates `AGENTS.md` files into session instructions.
- `codex-rs/cli/src/main.rs:62-146` – available CLI subcommands/flags (no `/agents` or `--agents` entrypoints today).
- `codex-rs/core/src/state/service.rs:18-35` and `codex-rs/core/src/codex.rs:356-520` – session services, turn context, and configuration plumbing.
- `codex-rs/core/src/codex_delegate.rs:31-166` and `codex-rs/core/src/tasks/review.rs:36-200` – current sub-agent usage (code review task) and how approval/tooling is proxied.
- `codex-rs/core/src/client.rs:360-399` and `codex-rs/codex-api/src/requests/responses.rs:120-159` – `x-openai-subagent` header propagation into OpenAI APIs.
- `codex-rs/protocol/src/protocol.rs:1243-1287` – `SessionSource::SubAgent` and available `SubAgentSource` variants.

## Claude Code Requirements Snapshot
- **Manifest and priority rules** – Claude scans `.claude/agents/`, `~/.claude/agents/`, plugin `agents/` directories, plus a CLI `--agents` JSON override, resolving conflicts project > CLI > user (`subagents.md:74-125`).
- **Schema & capabilities** – Each sub-agent bundles `name`, `description`, `model`, `tools`, `permissionMode`, `skills`, and `hooks` inside Markdown frontmatter; prompts live in the body (`subagents.md:126-207`). Tool scope defaults to “inherit everything” (including MCP tools) unless explicitly constrained (`subagents.md:172-205`).
- **Management UX** – `/agents` exposes CRUD for agents, while manual file edits and CLI flags provide alternative provisioning pathways (`subagents.md:210-248`). Disabling agents uses `Task(AgentName)` filters in settings or CLI flags (`subagents.md:250-270`).
- **Delegation semantics** – Claude auto-selects agents based on descriptions but also accepts explicit “Use the X agent…” commands (`subagents.md:274-295`).
- **Built-ins** – General-purpose, Plan, and Explore subagents have defined models, tool sets, and behaviors (Plan = read-only research in plan mode; Explore = Haiku + read-only shell; General-purpose = edit-capable Sonnet) (`subagents.md:298-407`).
- **Advanced lifecycle** – Agents may be chained, resumed via `agentId`, and store transcripts as `agent-{id}.jsonl`; programmatic invocations can pass `resume` and `subagent_type` (`subagents.md:530-599`).
- **Permissions & safety** – `permissionMode` controls how aggressively an agent should ask for approvals; disabling/hardening is expected at the agent level (`subagents.md:150-170`, `subagents.md:252-270`).

## Current Codex Capabilities
- **Instruction ingest only** – Codex currently discovers layered `AGENTS.md` docs and merges them into user instructions; there is no parser for `.claude/agents/*.md` manifests or CLI `--agents` JSON (`codex-rs/core/src/project_doc.rs:3-200`).
- **CLI entrypoints** – The Rust CLI exposes subcommands such as `exec`, `review`, `login`, sandbox tooling, etc., but nothing analogous to `/agents` or an `--agents` flag (`codex-rs/cli/src/main.rs:62-146`).
- **Session plumbing** – Sessions keep fixed `AskForApproval`/`SandboxPolicy` per turn, expose `SessionServices` (skills manager, notifier, `AgentControl`), and tag every turn with a single `SessionSource` value (`codex-rs/core/src/codex.rs:356-520`, `codex-rs/core/src/state/service.rs:18-35`).
- **Sub-agent support is review-only** – `ReviewTask` clones the current config, swaps in review prompts/model, and spawns a nested Codex via `run_codex_thread_one_shot`; the delegate is always stamped as `SessionSource::SubAgent(SubAgentSource::Review)` and inherits parent approvals via forwarding (`codex-rs/core/src/tasks/review.rs:36-200`, `codex-rs/core/src/codex_delegate.rs:31-166`).
- **API headers already exist** – Whenever a session is marked as `SubAgent`, `ModelClient` sets `x-openai-subagent`, and `codex-api` includes it with streaming requests, so the transport-layer contract is partially satisfied (`codex-rs/core/src/client.rs:360-399`, `codex-rs/codex-api/src/requests/responses.rs:120-159`).
- **Enumerated subagent types are limited** – The protocol only ships `Review`, `Compact`, or `Other(String)` variants, and only review is exercised in production code (`codex-rs/protocol/src/protocol.rs:1243-1287`).
- **Tool approvals & sandboxing remain global** – Tool orchestration, approval caches, and sandbox selection are session-scoped, not per-agent; review mode simply disables web search via feature flags and reuses the same approval flow (`codex-rs/core/src/tasks/review.rs:78-109`, `codex-rs/core/src/tools/orchestrator.rs:15-129`).

## Compatibility Gaps
1. **Manifest ingestion gap** – Claude expects prioritized discovery of `.claude/agents`, user-level manifests, plugin manifests, and CLI `--agents`; Codex currently only reads `AGENTS.md` instructions (`subagents.md:74-125` vs. `codex-rs/core/src/project_doc.rs:3-200`).
2. **No `/agents` UX or CLI flag** – There is no way to list/create agents interactively or pass `--agents` JSON through the CLI (`subagents.md:42-125` vs. `codex-rs/cli/src/main.rs:62-146`), so compatibility would require new command surfaces.
3. **Single hard-coded subagent** – The only `SubAgentSource` used in practice is `Review`; general-purpose/Plan/Explore behaviors and user-defined agents are absent, so Codex cannot satisfy Claude’s expectation of multiple built-in roles and tool sets (`subagents.md:298-407` vs. `codex-rs/core/src/tasks/review.rs:36-200`).
4. **No manifest schema parser** – There is no facility to parse frontmatter (`name`, `tools`, `permissionMode`, `skills`, `hooks`) or to map those onto Codex runtime knobs—currently only `base_instructions`, `model`, and feature toggles are swapped in review mode (`subagents.md:126-207` vs. `codex-rs/core/src/codex.rs:356-520`).
5. **Permission model mismatch** – Claude’s `permissionMode`/`Task(AgentName)` carve-outs do not map to Codex’s `AskForApproval` enum or execpolicy rules, which are session-wide (`subagents.md:150-170`, `subagents.md:252-270` vs. `codex-rs/core/src/codex.rs:356-520` and `codex-rs/core/src/tools/orchestrator.rs:15-129`).
6. **Resume/transcript contract missing** – Claude exposes `agentId` transcripts and resumable agents, but Codex threads emit only `ThreadId` and do not persist sub-agent rollouts separately (`subagents.md:542-599` vs. `codex-rs/core/src/thread_manager.rs:10-205`).
7. **Hook/event gap** – `hooks` (PreToolUse/PostToolUse/Stop) have no analog today; ReviewTask filters events manually but there is no declarative hook engine tied to agent manifests (`subagents.md:187-205` vs. `codex-rs/core/src/tasks/review.rs:111-159`).

## Open Questions
1. **Permission mapping** – How should Claude’s `permissionMode` values translate into Codex’s `AskForApproval` policy and execpolicy overrides? Do we need per-agent approval caches or can we layer agent-specific amendments on top of the existing session-wide policy?
2. **Storage format** – Should Codex reuse `AGENTS.md` discovery to look for `.claude/agents/*.md`, or introduce a new watcher? How do we validate manifests (frontmatter schema, hooks, skills) before exposing them to the session?
3. **Agent lifecycle IDs** – Claude surfaces `agentId` for resuming work; should Codex map that to `ThreadId`, invent a new identifier per sub-agent invocation, or expose rollouts under `.codex/agents/agent-{id}.jsonl`?
4. **Tool scopes and hooks** – What is the plan for enforcing per-agent tool allow-lists and lifecycle hooks? Today all tools are globally available unless restricted by execpolicy; we need clarity on whether to filter tool JSON before sending prompts, block approvals, or both.
5. **Built-in agent parity** – Claude users expect to invoke “Explore”/“Plan” even if they did not author manifests. Do we introduce dedicated Codex tasks (similar to ReviewTask) for each built-in and surface them through new commands, or can they be configured via manifests that live in the repo?
