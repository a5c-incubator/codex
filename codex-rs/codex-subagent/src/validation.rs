//! Schema validation helpers for manifests.

use crate::error::ValidationIssue;
use crate::error::ValidationIssues;
use crate::manifest::AgentManifest;
use crate::manifest::Hook;
use crate::manifest::ToolName;
use crate::manifest::TriggerDefinition;

/// Validates a manifest against the schema documented in docs/subagents/architecture.md.
pub fn validate_manifest(manifest: &AgentManifest) -> Result<(), ValidationIssues> {
    let mut issues = ValidationIssues::new();

    check_string(&mut issues, &manifest.id, "id");
    check_string(&mut issues, &manifest.name, "name");
    check_string(&mut issues, &manifest.description, "description");
    check_string(&mut issues, &manifest.body, "body");

    if let Some(model) = &manifest.model {
        if model.as_str().trim().is_empty() {
            issues.push(ValidationIssue::InvalidField {
                field: "model",
                message: "model cannot be empty".into(),
            });
        }
    }

    if let Some(tools) = manifest.tool_scope.as_slice() {
        if tools.is_empty() {
            issues.push(ValidationIssue::InvalidField {
                field: "tools",
                message: "tools must contain at least one entry".into(),
            });
        }
        check_tools(&mut issues, tools);
    }

    for hook in manifest.hooks.iter() {
        check_hook(&mut issues, hook);
    }

    for trigger in &manifest.triggers {
        check_trigger(&mut issues, trigger);
    }

    for skill in &manifest.skills {
        check_string(&mut issues, skill, "skills");
    }

    if manifest.source.is_none() {
        issues.push(ValidationIssue::Priority(
            "manifest missing discovery scope; loader must annotate sources".into(),
        ));
    }

    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues)
    }
}

fn check_string(issues: &mut ValidationIssues, value: &str, field: &'static str) {
    if value.trim().is_empty() {
        issues.push(ValidationIssue::MissingField(field));
    }
}

fn check_tools(issues: &mut ValidationIssues, tools: &[ToolName]) {
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    for tool in tools {
        if tool.as_str().trim().is_empty() {
            issues.push(ValidationIssue::InvalidTool {
                name: tool.as_str().into(),
                message: "tool names cannot be empty".into(),
            });
        }
        if !seen.insert(tool.as_str()) {
            issues.push(ValidationIssue::InvalidTool {
                name: tool.as_str().into(),
                message: "duplicate tool".into(),
            });
        }
    }
}

fn check_hook(issues: &mut ValidationIssues, hook: &Hook) {
    let has_command = hook
        .command
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty());
    let has_endpoint = hook
        .endpoint
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty());

    if !has_command && !has_endpoint {
        issues.push(ValidationIssue::ConflictingHook {
            hook: hook.name.clone(),
        });
    }

    if has_command && has_endpoint {
        issues.push(ValidationIssue::InvalidField {
            field: "hook",
            message: format!("hook {} cannot set command and endpoint", hook.name),
        });
    }

    if let Some(tools) = &hook.tools {
        check_tools(issues, tools);
    }
}

fn check_trigger(issues: &mut ValidationIssues, trigger: &TriggerDefinition) {
    match trigger {
        TriggerDefinition::Keyword { phrase, weight } => {
            check_string(issues, phrase, "triggers.phrase");
            check_weight(issues, *weight, phrase);
        }
        TriggerDefinition::Glob { pattern, weight } => {
            check_string(issues, pattern, "triggers.pattern");
            check_weight(issues, *weight, pattern);
        }
    }
}

fn check_weight(issues: &mut ValidationIssues, weight: u8, label: &str) {
    if !(1..=100).contains(&weight) {
        issues.push(ValidationIssue::InvalidTrigger {
            trigger: label.into(),
            message: "weight must be between 1 and 100".into(),
        });
    }
}

#[cfg(all(test, feature = "schema"))]
mod tests {
    use super::*;
    use crate::manifest::AgentKind;
    use crate::manifest::Hook;
    use crate::manifest::HookSet;
    use crate::manifest::PermissionMode;
    use crate::manifest::ToolName;
    use crate::manifest::ToolScope;
    use crate::manifest::TriggerDefinition;
    use crate::priority::DiscoveryScope;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    fn base_manifest() -> AgentManifest {
        AgentManifest {
            id: "alpha".into(),
            kind: AgentKind::default(),
            name: "Alpha".into(),
            description: "Example".into(),
            model: None,
            tool_scope: ToolScope::default(),
            permission_mode: PermissionMode::Default,
            hooks: HookSet::default(),
            triggers: vec![],
            skills: vec![],
            body: "Body".into(),
            source: Some(DiscoveryScope::Project {
                path: PathBuf::from(".claude/agents/alpha.md"),
            }),
            digest: Some("digest".into()),
        }
    }

    #[test]
    fn happy_path_manifest_validates() {
        let manifest = base_manifest();
        assert!(validate_manifest(&manifest).is_ok());
    }

    #[test]
    fn missing_fields_surface_issues() {
        let mut manifest = base_manifest();
        manifest.id.clear();
        manifest.body.clear();
        manifest.source = None;
        let issues = match validate_manifest(&manifest) {
            Ok(_) => panic!("validation should have failed"),
            Err(err) => err,
        };
        assert_eq!(
            issues.as_slice(),
            &[
                ValidationIssue::MissingField("id"),
                ValidationIssue::MissingField("body"),
                ValidationIssue::Priority(
                    "manifest missing discovery scope; loader must annotate sources".into()
                )
            ]
        );
    }

    #[test]
    fn hook_requires_single_action() {
        let mut manifest = base_manifest();
        manifest.hooks.pre.push(Hook {
            name: "invalid-hook".into(),
            description: None,
            tools: None,
            command: Some("echo hi".into()),
            endpoint: Some("https://example.com/hook".into()),
        });

        let issues = validate_manifest(&manifest)
            .expect_err("validation should fail for conflicting hook")
            .into_vec();
        assert_eq!(
            issues,
            vec![ValidationIssue::InvalidField {
                field: "hook",
                message: "hook invalid-hook cannot set command and endpoint".into(),
            }]
        );
    }

    #[test]
    fn duplicate_tools_and_trigger_weights_surface_issues() {
        let mut manifest = base_manifest();
        manifest.tool_scope =
            ToolScope::restricted(vec![ToolName::from("shell"), ToolName::from("shell")]);
        manifest.triggers.push(TriggerDefinition::Keyword {
            phrase: "high-weight".into(),
            weight: 150,
        });

        let issues = validate_manifest(&manifest)
            .expect_err("validation should fail for duplicate tools + trigger weight")
            .into_vec();
        assert_eq!(
            issues,
            vec![
                ValidationIssue::InvalidTool {
                    name: "shell".into(),
                    message: "duplicate tool".into(),
                },
                ValidationIssue::InvalidTrigger {
                    trigger: "high-weight".into(),
                    message: "weight must be between 1 and 100".into(),
                }
            ]
        );
    }
}
