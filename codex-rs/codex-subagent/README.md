# codex-subagent (scaffold)

`codex-subagent` is the shared data/loader crate for Claude-compatible
subagent manifests. It captures the public interfaces described in
[`docs/subagents/architecture.md`](../docs/subagents/architecture.md) so
other crates (CLI, core runtime, tooling) can link against a stable API
while the remaining orchestration work proceeds.

## Modules

| Module | Purpose | Feature gate |
| --- | --- | --- |
| `manifest` | `AgentManifest`, enums (permission/tool scope), helper methods, serde impls | always on |
| `priority` | `DiscoveryScope`, `DiscoveryPriority`, plugin identifiers | always on |
| `validation` | Schema-level validation helpers (`validate_manifest`) + issue types | always on |
| `error` | Error/issue types shared by loaders and validators | always on |
| `loader` | Filesystem loader (`FsManifestLoader`, `DiscoveryTarget`) + watch stubs | `loader` (default) |
| `schema` | Placeholder for future schema exports (`ManifestSchema`) | `manifest-schema` (default) |

## Feature flags

```
default = ["manifest-schema", "schema", "loader"]
manifest-schema = ["schemars"]
schema = ["manifest-schema"]
loader = ["schema", "walkdir"]
```

Disable `loader` if you only need the data model/serde impls (for example,
consumers that receive manifests via RPC can avoid `walkdir`). `schema` and
`manifest-schema` will eventually expose full JSON-schema exports; today they
just keep the public API surface stable.

## Testing

```bash
cargo test -p codex-subagent
just fix -p codex-subagent   # clippy --fix
just fmt                     # workspace fmt
```

The test suite includes:

- JSON/YAML round-trip unit tests for `AgentManifest`
- Priority ordering tests for `DiscoveryScope`
- Validation helpers (trigger/tool invariants)
- Snapshot tests (`insta`) covering loader output and priority summaries

## Status

This crate intentionally stops short of implementing:

- Loader watch integration (returns `ManifestError::WatchUnsupported`)
- Schema emission (placeholder `ManifestSchema`)
- Runtime registry/orchestration logic (lives in `codex-core`)

Those will land in later steps of the subagent roadmap. For now, the crate
provides a typed contract so downstream work can compile and tests can
exercise the model/loader behavior in isolation.
