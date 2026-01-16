//! Built-in manifest definitions mirrored from Claude personas.

use crate::manifest::compute_digest;
use crate::manifest::AgentKind;
use crate::manifest::AgentManifest;
use crate::manifest::BuiltInAgent;
use crate::manifest::HookSet;
use crate::manifest::ModelRef;
use crate::manifest::PermissionMode;
use crate::manifest::ToolName;
use crate::manifest::ToolScope;
use crate::priority::DiscoveryScope;

/// Returns the built-in manifests bundled with Codex.
#[must_use]
pub fn built_in_manifests() -> Vec<AgentManifest> {
    vec![
        general_purpose_manifest(),
        plan_manifest(),
        explore_manifest(),
        review_manifest(),
    ]
}

fn general_purpose_manifest() -> AgentManifest {
    build_manifest(
        "builtin-general-purpose",
        BuiltInAgent::GeneralPurpose,
        "Claude General-purpose",
        "Default multi-tool persona for complex edits.",
        None,
        PermissionMode::Default,
        ToolScope::inherit(),
        "You are Codex's general-purpose subagent. Blend reasoning, planning, and precise execution across all tools.",
    )
}

fn plan_manifest() -> AgentManifest {
    build_manifest(
        "builtin-plan",
        BuiltInAgent::Plan,
        "Claude Plan",
        "Deterministic planner that proposes safe steps.",
        Some(ModelRef("claude-3.5-sonnet".into())),
        PermissionMode::Plan,
        ToolScope::restricted(vec![
            ToolName::from("read"),
            ToolName::from("search"),
            ToolName::from("test"),
        ]),
        "You are Codex's plan subagent. Produce careful numbered plans before any execution.",
    )
}

fn explore_manifest() -> AgentManifest {
    build_manifest(
        "builtin-explore",
        BuiltInAgent::Explore,
        "Claude Explore",
        "Fast exploratory agent for read-only reconnaissance.",
        Some(ModelRef("claude-3.5-haiku".into())),
        PermissionMode::DontAsk,
        ToolScope::restricted(vec![
            ToolName::from("read"),
            ToolName::from("search"),
            ToolName::from("repo_scan"),
        ]),
        "You are Codex's explore subagent. Move quickly, gather context, and avoid destructive actions.",
    )
}

fn review_manifest() -> AgentManifest {
    build_manifest(
        "builtin-review",
        BuiltInAgent::Review,
        "Claude Review",
        "Code review specialist that suggests improvements.",
        Some(ModelRef("claude-3.5-sonnet".into())),
        PermissionMode::AcceptEdits,
        ToolScope::restricted(vec![
            ToolName::from("diff"),
            ToolName::from("read"),
            ToolName::from("test"),
        ]),
        "You are Codex's review subagent. Inspect diffs carefully and suggest actionable improvements.",
    )
}

fn build_manifest(
    id: &str,
    agent: BuiltInAgent,
    name: &str,
    description: &str,
    model: Option<ModelRef>,
    permission_mode: PermissionMode,
    tool_scope: ToolScope,
    body: &str,
) -> AgentManifest {
    let body_str = body.to_string();
    let digest = compute_digest(body_str.as_bytes());
    AgentManifest {
        id: id.into(),
        kind: AgentKind::BuiltIn {
            agent: agent.clone(),
        },
        name: name.into(),
        description: description.into(),
        model,
        tool_scope,
        permission_mode,
        hooks: HookSet::default(),
        triggers: Vec::new(),
        skills: Vec::new(),
        body: body_str,
        source: Some(DiscoveryScope::BuiltIn { agent }),
        digest: Some(digest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn built_ins_cover_expected_personas() {
        let manifests = built_in_manifests();
        assert_eq!(manifests.len(), 4);
        let ids: Vec<_> = manifests
            .iter()
            .map(|manifest| manifest.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec![
                "builtin-general-purpose",
                "builtin-plan",
                "builtin-explore",
                "builtin-review"
            ]
        );
        for manifest in &manifests {
            match manifest.source.as_ref() {
                Some(DiscoveryScope::BuiltIn { .. }) => (),
                other => panic!("unexpected scope: {other:?}"),
            }
            assert!(
                !manifest.body.trim().is_empty(),
                "manifest body must be present"
            );
        }
    }
}
