//! Manifest data model shared across Codex crates.

use crate::priority::DiscoveryPriority;
use crate::priority::DiscoveryScope;
#[cfg(feature = "manifest-schema")]
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use sha2::Digest;
use sha2::Sha256;

/// Stable identifier derived from filenames or CLI payloads.
pub type AgentId = String;

/// Wrapper for manifest-defined tool identifiers.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "manifest-schema", derive(JsonSchema))]
#[serde(transparent)]
pub struct ToolName(pub String);

impl ToolName {
    /// Returns the raw string identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ToolName {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for ToolName {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Optional model override referenced by the manifest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "manifest-schema", derive(JsonSchema))]
#[serde(transparent)]
pub struct ModelRef(pub String);

impl ModelRef {
    /// Returns the name of the backing foundation model.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Core manifest object described in `docs/subagents/architecture.md`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "manifest-schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct AgentManifest {
    /// Stable identifier derived from filename or CLI payload.
    pub id: AgentId,
    /// Categorization used for built-ins vs custom manifests.
    #[serde(default, skip_serializing_if = "AgentKind::is_default")]
    pub kind: AgentKind,
    /// Display name rendered in CLI/TUI surfaces.
    pub name: String,
    /// Short description used by Claude routing.
    pub description: String,
    /// Optional model override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    /// Tool configuration (inheritance vs explicit allow-list).
    #[serde(
        default,
        rename = "tools",
        skip_serializing_if = "ToolScope::is_inherit"
    )]
    pub tool_scope: ToolScope,
    /// Manifest-defined permission policy.
    #[serde(default)]
    pub permission_mode: PermissionMode,
    /// Hook definitions fired around tool execution.
    #[serde(default)]
    pub hooks: HookSet,
    /// Trigger metadata used for ranking and discoverability.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<TriggerDefinition>,
    /// Optional list of human-readable skills/tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// Markdown prompt body.
    #[serde(default)]
    pub body: String,
    /// Discovery metadata supplied by the loader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<DiscoveryScope>,
    /// Digest of the manifest contents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

impl AgentManifest {
    /// Associates the manifest with a discovery scope.
    #[must_use]
    pub fn with_source(mut self, scope: DiscoveryScope) -> Self {
        self.source = Some(scope);
        self
    }

    /// Associates the manifest with a content digest.
    #[must_use]
    pub fn with_digest(mut self, digest: impl Into<String>) -> Self {
        self.digest = Some(digest.into());
        self
    }

    /// Returns the priority tier inferred from the discovery scope.
    #[must_use]
    pub fn priority(&self) -> DiscoveryPriority {
        self.source
            .as_ref()
            .map_or(DiscoveryPriority::Plugin, DiscoveryScope::priority)
    }
}

/// Enumerates built-in personas vs user authored agents.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "manifest-schema", derive(JsonSchema))]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AgentKind {
    /// Default branch for user-defined manifests.
    Custom,
    /// Built-in persona bundled with Codex.
    BuiltIn {
        /// Identifier for the built-in persona.
        agent: BuiltInAgent,
    },
    /// Plugin-provided manifest.
    Plugin {
        /// Identifier supplied by the plugin registry.
        #[serde(rename = "pluginId")]
        plugin_id: String,
    },
}

impl AgentKind {
    /// Indicates whether the manifest is user supplied.
    #[must_use]
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Custom)
    }

    /// Indicates whether the manifest corresponds to a built-in persona.
    #[must_use]
    pub fn is_built_in(&self) -> bool {
        matches!(self, Self::BuiltIn { .. })
    }
}

impl Default for AgentKind {
    fn default() -> Self {
        Self::Custom
    }
}

/// Built-in personas codified by Claude and mirrored in Codex.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "manifest-schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum BuiltInAgent {
    /// Mirrors Claude's default multi-purpose persona.
    GeneralPurpose,
    /// Read-heavy plan persona.
    Plan,
    /// Fast exploratory persona.
    Explore,
    /// Code review persona.
    Review,
}

/// Tool scoping configuration (inherit vs explicit allow list).
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "manifest-schema", derive(JsonSchema))]
pub enum ToolScope {
    /// Inherit parent session tooling.
    Inherit,
    /// Restricted list of tools.
    Restricted(Vec<ToolName>),
}

impl ToolScope {
    /// Helper that returns the inherited variant.
    #[must_use]
    pub fn inherit() -> Self {
        Self::Inherit
    }

    /// Helper constructing a restricted scope.
    #[must_use]
    pub fn restricted(tools: Vec<ToolName>) -> Self {
        Self::Restricted(tools)
    }

    /// Indicates whether the manifest inherits tools from the parent session.
    #[must_use]
    pub fn is_inherit(&self) -> bool {
        matches!(self, Self::Inherit)
    }

    /// Returns the restricted allow list when set.
    #[must_use]
    pub fn as_slice(&self) -> Option<&[ToolName]> {
        match self {
            Self::Inherit => None,
            Self::Restricted(tools) => Some(tools),
        }
    }
}

impl Default for ToolScope {
    fn default() -> Self {
        Self::Inherit
    }
}

impl Serialize for ToolScope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Inherit => serializer.serialize_none(),
            Self::Restricted(tools) => tools.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ToolScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let maybe_list = Option::<Vec<ToolName>>::deserialize(deserializer)?;
        Ok(match maybe_list {
            Some(values) => Self::Restricted(values),
            None => Self::Inherit,
        })
    }
}

/// Permission policies supported in Claude manifests.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "manifest-schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Inherit the parent session's default approval flow.
    Default,
    /// Ask for approval before applying edits.
    AcceptEdits,
    /// Skip confirmations for read-only tooling.
    DontAsk,
    /// Bypass approvals entirely.
    BypassPermissions,
    /// Plan persona guardrail.
    Plan,
    /// Ignore approvals completely (escape hatch).
    Ignore,
}

impl Default for PermissionMode {
    fn default() -> Self {
        Self::Default
    }
}

/// Hook collection grouped by phase.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "manifest-schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct HookSet {
    /// Hooks fired before tool execution.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre: Vec<Hook>,
    /// Hooks fired after successful tool execution.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post: Vec<Hook>,
    /// Hooks fired when an agent stops.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<Hook>,
}

impl HookSet {
    /// Iterator visiting every hook regardless of phase.
    pub fn iter(&self) -> impl Iterator<Item = &Hook> + '_ {
        self.pre
            .iter()
            .chain(self.post.iter())
            .chain(self.stop.iter())
    }
}

/// Individual hook definition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "manifest-schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Hook {
    /// Hook identifier used for diagnostics.
    pub name: String,
    /// Optional hook description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional subset of tools that should trigger the hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolName>>,
    /// Shell command executed by the hook (mutually exclusive with `endpoint`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// HTTP endpoint invoked by the hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// Trigger metadata carried in manifests.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "manifest-schema", derive(JsonSchema))]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TriggerDefinition {
    /// Keyword-based trigger surfaced in registries.
    Keyword {
        /// Phrase that should boost the manifest.
        phrase: String,
        /// Weight applied to this trigger.
        weight: u8,
    },
    /// Glob-style trigger evaluated against file paths or commands.
    Glob {
        /// Pattern string (e.g. `deploy-*`).
        pattern: String,
        /// Weight applied to this trigger.
        weight: u8,
    },
}

impl TriggerDefinition {
    /// Returns the configured weight.
    #[must_use]
    pub fn weight(&self) -> u8 {
        match self {
            Self::Keyword { weight, .. } | Self::Glob { weight, .. } => *weight,
        }
    }

    /// Returns the human-readable label associated with the trigger.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Keyword { phrase, .. } => phrase,
            Self::Glob { pattern, .. } => pattern,
        }
    }
}

/// Computes a deterministic digest for manifest contents.
#[must_use]
pub fn compute_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "loader")]
    use crate::loader::parse_markdown;
    use crate::priority::DiscoveryScope;
    use crate::priority::PluginId;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::path::PathBuf;

    fn demo_manifest() -> AgentManifest {
        AgentManifest {
            id: "demo".into(),
            kind: AgentKind::BuiltIn {
                agent: BuiltInAgent::Plan,
            },
            name: "Demo".into(),
            description: "Helps with demos".into(),
            model: Some(ModelRef("claude-3.5".into())),
            tool_scope: ToolScope::restricted(vec![ToolName::from("search")]),
            permission_mode: PermissionMode::DontAsk,
            hooks: HookSet {
                pre: vec![Hook {
                    name: "pre".into(),
                    description: Some("pre hook".into()),
                    tools: None,
                    command: Some("echo pre".into()),
                    endpoint: None,
                }],
                post: vec![],
                stop: vec![Hook {
                    name: "stop".into(),
                    description: None,
                    tools: Some(vec![ToolName::from("search")]),
                    command: None,
                    endpoint: Some("https://example.com/stop".into()),
                }],
            },
            triggers: vec![TriggerDefinition::Keyword {
                phrase: "demo".into(),
                weight: 5,
            }],
            skills: vec!["devops".into()],
            body: "You are helpful.".into(),
            source: Some(DiscoveryScope::Plugin {
                path: PathBuf::from("plugin/agents/demo.md"),
                plugin_id: PluginId::new("demo-plugin"),
            }),
            digest: Some("deadbeef".into()),
        }
    }

    #[test]
    fn manifest_round_trips_with_all_fields() -> Result<(), Box<dyn std::error::Error>> {
        let manifest = demo_manifest();
        let serialized = serde_json::to_value(&manifest)?;
        let expected = json!({
            "id": "demo",
            "kind": {
                "type": "builtIn",
                "agent": "plan"
            },
            "name": "Demo",
            "description": "Helps with demos",
            "model": "claude-3.5",
            "tools": ["search"],
            "permissionMode": "dontAsk",
            "hooks": {
                "pre": [{
                    "name": "pre",
                    "description": "pre hook",
                    "command": "echo pre"
                }],
                "stop": [{
                    "name": "stop",
                    "tools": ["search"],
                    "endpoint": "https://example.com/stop"
                }]
            },
            "triggers": [{
                "type": "keyword",
                "phrase": "demo",
                "weight": 5
            }],
            "skills": ["devops"],
            "body": "You are helpful.",
            "source": {
                "kind": "plugin",
                "path": "plugin/agents/demo.md",
                "pluginId": "demo-plugin"
            },
            "digest": "deadbeef"
        });
        assert_eq!(serialized, expected);
        let reparsed: AgentManifest = serde_json::from_value(serialized)?;
        assert_eq!(manifest, reparsed);
        Ok(())
    }

    #[test]
    fn defaults_apply_when_fields_missing() -> Result<(), Box<dyn std::error::Error>> {
        let value = json!({
            "id": "custom",
            "name": "Custom agent",
            "description": "Does things",
            "body": "Prompt"
        });
        let manifest: AgentManifest = serde_json::from_value(value)?;
        assert!(manifest.kind.is_default());
        assert!(manifest.tool_scope.is_inherit());
        assert_eq!(manifest.permission_mode, PermissionMode::Default);
        assert!(manifest.triggers.is_empty());
        assert!(manifest.skills.is_empty());
        assert!(manifest.source.is_none());
        Ok(())
    }

    #[test]
    fn manifest_parses_from_yaml() -> Result<(), Box<dyn std::error::Error>> {
        let yaml = r#"
id: reviewer
name: Reviewer
description: Reviews diffs
permissionMode: acceptEdits
tools:
  - files
body: |
  You are the best reviewer.
"#;
        let manifest: AgentManifest = serde_yaml::from_str(yaml)?;
        let expected = AgentManifest {
            id: "reviewer".into(),
            kind: AgentKind::Custom,
            name: "Reviewer".into(),
            description: "Reviews diffs".into(),
            model: None,
            tool_scope: ToolScope::restricted(vec![ToolName::from("files")]),
            permission_mode: PermissionMode::AcceptEdits,
            hooks: HookSet::default(),
            triggers: Vec::new(),
            skills: Vec::new(),
            body: "You are the best reviewer.\n".into(),
            source: None,
            digest: None,
        };
        assert_eq!(manifest, expected);
        Ok(())
    }

    #[cfg(feature = "loader")]
    #[test]
    fn loads_claude_markdown_manifest() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let markdown = r#"---
id: release-captain
name: Release Captain
description: Guides releases
model: claude-3-opus
permissionMode: plan
tools:
  - edit
  - shell
skills:
  - releases
  - compliance
hooks:
  pre:
    - name: lint
      description: Run lint before tooling
      command: ./scripts/lint.sh
  post:
    - name: audit
      endpoint: https://hooks.example.com/audit
  stop:
    - name: cleanup
      tools: ["shell"]
      command: ./scripts/cleanup.sh
triggers:
  - type: keyword
    phrase: ship it
    weight: 50
  - type: glob
    pattern: deploy-*
    weight: 10
---
You are the release captain.
Ensure staging and production stay green.
"#;

        let manifest = parse_markdown(markdown.as_bytes())?;
        assert_eq!(manifest.id, "release-captain");
        assert_eq!(manifest.name, "Release Captain");
        assert_eq!(manifest.description, "Guides releases");
        assert_eq!(manifest.model, Some(ModelRef("claude-3-opus".into())));
        assert_eq!(
            manifest.tool_scope,
            ToolScope::restricted(vec![ToolName::from("edit"), ToolName::from("shell")])
        );
        assert_eq!(manifest.permission_mode, PermissionMode::Plan);
        assert_eq!(
            manifest.skills,
            vec![String::from("releases"), String::from("compliance")]
        );
        assert_eq!(
            manifest.triggers,
            vec![
                TriggerDefinition::Keyword {
                    phrase: "ship it".into(),
                    weight: 50
                },
                TriggerDefinition::Glob {
                    pattern: "deploy-*".into(),
                    weight: 10
                }
            ]
        );
        assert_eq!(
            manifest.hooks,
            HookSet {
                pre: vec![Hook {
                    name: "lint".into(),
                    description: Some("Run lint before tooling".into()),
                    tools: None,
                    command: Some("./scripts/lint.sh".into()),
                    endpoint: None,
                }],
                post: vec![Hook {
                    name: "audit".into(),
                    description: None,
                    tools: None,
                    command: None,
                    endpoint: Some("https://hooks.example.com/audit".into()),
                }],
                stop: vec![Hook {
                    name: "cleanup".into(),
                    description: None,
                    tools: Some(vec![ToolName::from("shell")]),
                    command: Some("./scripts/cleanup.sh".into()),
                    endpoint: None,
                }],
            }
        );
        assert_eq!(
            manifest.body,
            "You are the release captain.\nEnsure staging and production stay green."
        );
        assert!(manifest.source.is_none());
        assert!(manifest.digest.is_none());
        Ok(())
    }

    #[test]
    fn digest_is_deterministic() {
        let first = compute_digest(b"hello world");
        let second = compute_digest(b"hello world");
        let different = compute_digest(b"world hello");
        assert_eq!(first, second);
        assert_ne!(first, different);
    }

    #[test]
    fn trigger_helpers_expose_weight_and_label() {
        let keyword = TriggerDefinition::Keyword {
            phrase: "deploy".into(),
            weight: 7,
        };
        assert_eq!(keyword.weight(), 7);
        assert_eq!(keyword.label(), "deploy");

        let glob = TriggerDefinition::Glob {
            pattern: "review-*".into(),
            weight: 3,
        };
        assert_eq!(glob.weight(), 3);
        assert_eq!(glob.label(), "review-*");
    }
}
