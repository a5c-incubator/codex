use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use sha2::Digest;
use sha2::Sha256;

use crate::priority::DiscoveryPriority;
use crate::priority::DiscoveryScope;

/// Alias so manifest identifiers can change without touching every consumer.
pub type AgentId = String;

/// Named tool reference stored in manifests.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash)]
#[serde(transparent)]
pub struct ToolName(pub String);

impl ToolName {
    /// Returns the inner identifier.
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

/// Optional model override for the agent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelRef(pub String);

impl ModelRef {
    /// Returns the underlying model name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Core manifest object described in docs/subagents/architecture.md.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentManifest {
    /// Stable identifier derived from the filename or CLI payload.
    pub id: AgentId,
    /// Categorization used for built-ins vs. custom agents.
    #[serde(default, skip_serializing_if = "AgentKind::is_default")]
    pub kind: AgentKind,
    /// Display name for CLI/TUI surfaces.
    pub name: String,
    /// Short description used by Claude routing.
    pub description: String,
    /// Optional model override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    /// Tool allow-list. When omitted, parent tools apply.
    #[serde(
        default,
        rename = "tools",
        skip_serializing_if = "ToolScope::is_inherit"
    )]
    pub tool_scope: ToolScope,
    /// Permission policy.
    #[serde(default)]
    pub permission_mode: PermissionMode,
    /// Hook definitions around tool execution.
    #[serde(default)]
    pub hooks: HookSet,
    /// Trigger metadata.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<TriggerDefinition>,
    /// Optional list of skill tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// Markdown prompt body.
    #[serde(default)]
    pub body: String,
    /// Manifest provenance (set by the loader).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<DiscoveryScope>,
    /// Digest of the manifest contents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

impl AgentManifest {
    /// Annotates the manifest with its discovery scope.
    #[must_use]
    pub fn with_source(mut self, scope: DiscoveryScope) -> Self {
        self.source = Some(scope);
        self
    }

    /// Annotates the manifest with a digest.
    #[must_use]
    pub fn with_digest(mut self, digest: impl Into<String>) -> Self {
        self.digest = Some(digest.into());
        self
    }

    /// Returns the discovery priority derived from the scope.
    #[must_use]
    pub fn priority(&self) -> DiscoveryPriority {
        self.source
            .as_ref()
            .map_or(DiscoveryPriority::Plugin, DiscoveryScope::priority)
    }
}

/// Distinguishes built-in personas from user-authored manifests.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AgentKind {
    /// Default for user-authored manifests.
    Custom,
    /// Built-in manifest included with Codex.
    BuiltIn {
        /// Built-in persona identifier.
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
    /// Returns true when the manifest should behave like a user-defined agent.
    #[must_use]
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Custom)
    }

    /// Indicates whether the manifest is one of the baked-in personas.
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

/// Enumerates the built-in personas shipped with Codex.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
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

/// Tool scope configuration derived from manifest front matter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolScope {
    /// Inherit the parent session's tool graph.
    Inherit,
    /// Restrict the agent to an explicit allow-list.
    Restricted(Vec<ToolName>),
}

impl ToolScope {
    /// Returns the inherited variant.
    #[must_use]
    pub fn inherit() -> Self {
        Self::Inherit
    }

    /// Creates a restricted scope from the provided list.
    #[must_use]
    pub fn restricted(tools: Vec<ToolName>) -> Self {
        Self::Restricted(tools)
    }

    /// Indicates whether the manifest inherits tools.
    #[must_use]
    pub fn is_inherit(&self) -> bool {
        matches!(self, Self::Inherit)
    }

    /// Returns the restricted allow-list if present.
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
        let maybe = Option::<Vec<ToolName>>::deserialize(deserializer)?;
        Ok(match maybe {
            Some(tools) => Self::Restricted(tools),
            None => Self::Inherit,
        })
    }
}

/// Set of hooks fired around tool execution.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookSet {
    /// Hooks fired before a tool executes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre: Vec<Hook>,
    /// Hooks fired after successful tool execution.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post: Vec<Hook>,
    /// Hooks fired when the agent stops.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<Hook>,
}

impl HookSet {
    /// Iterator over every hook regardless of phase.
    pub fn iter(&self) -> impl Iterator<Item = &Hook> + '_ {
        self.pre
            .iter()
            .chain(self.post.iter())
            .chain(self.stop.iter())
    }
}

/// Individual hook definition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hook {
    /// Hook identifier used for diagnostics.
    pub name: String,
    /// Optional hook description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional tool restrictions for the hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolName>>,
    /// Shell command to run. Mutually exclusive with `endpoint`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Endpoint to call. Mutually exclusive with `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// Trigger metadata to help the registry rank agents.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TriggerDefinition {
    /// Keyword-based trigger.
    Keyword {
        /// Keyword that should match.
        phrase: String,
        /// Weight applied to this trigger.
        weight: u8,
    },
    /// Glob/pattern-based trigger.
    Glob {
        /// Glob-style pattern.
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

    /// Returns the user facing label.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Keyword { phrase, .. } => phrase,
            Self::Glob { pattern, .. } => pattern,
        }
    }
}

/// Permission policy enum matching Claude's manifest contract.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Inherit the parent session's approvals.
    Default,
    /// Accept edits with approval prompts.
    AcceptEdits,
    /// Skip approvals for read-only tools.
    DontAsk,
    /// Bypass approvals entirely.
    BypassPermissions,
    /// Plan persona.
    Plan,
    /// Ignore approvals (special cases only).
    Ignore,
}

impl Default for PermissionMode {
    fn default() -> Self {
        Self::Default
    }
}

/// Helper to compute deterministic digests for manifests.
#[must_use]
pub fn compute_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(all(test, feature = "schema"))]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::path::PathBuf;

    #[test]
    fn trigger_weights_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let manifest = AgentManifest {
            id: "example".into(),
            kind: AgentKind::BuiltIn {
                agent: BuiltInAgent::Plan,
            },
            name: "Example".into(),
            description: "Example agent".into(),
            model: Some(ModelRef("claude-3.5".into())),
            tool_scope: ToolScope::restricted(vec![
                ToolName::from("search"),
                ToolName::from("shell"),
            ]),
            permission_mode: PermissionMode::DontAsk,
            hooks: HookSet::default(),
            triggers: vec![TriggerDefinition::Keyword {
                phrase: "deploy".into(),
                weight: 7,
            }],
            skills: vec!["devops".into()],
            body: "You are helpful.".into(),
            source: Some(DiscoveryScope::Project {
                path: PathBuf::from(".claude/agents/deploy.md"),
            }),
            digest: Some("abc".into()),
        };

        let serialized = serde_json::to_value(&manifest)?;
        let expected = json!({
            "id": "example",
            "kind": {
                "type": "builtIn",
                "agent": "plan"
            },
            "name": "Example",
            "description": "Example agent",
            "model": "claude-3.5",
            "tools": ["search", "shell"],
            "permissionMode": "dontAsk",
            "hooks": {},
            "triggers": [{
                "type": "keyword",
                "phrase": "deploy",
                "weight": 7
            }],
            "skills": ["devops"],
            "body": "You are helpful.",
            "source": {
                "kind": "project",
                "path": ".claude/agents/deploy.md"
            },
            "digest": "abc"
        });
        assert_eq!(serialized, expected);
        let round_trip: AgentManifest = serde_json::from_value(serialized)?;
        assert_eq!(manifest, round_trip);
        Ok(())
    }

    #[test]
    fn defaults_apply_when_optional_fields_missing() -> Result<(), Box<dyn std::error::Error>> {
        let yaml = r#"
id: review-specialist
name: Review Specialist
description: Analyze diffs carefully
body: Prompt
"#;
        let manifest: AgentManifest = serde_yaml::from_str(yaml)?;
        assert!(manifest.kind.is_default());
        assert!(manifest.tool_scope.is_inherit());
        assert_eq!(manifest.permission_mode, PermissionMode::Default);
        Ok(())
    }

    #[test]
    fn digest_consistent() {
        let digest = compute_digest(b"hello world");
        assert_eq!(digest, compute_digest(b"hello world"));
        assert_ne!(digest, compute_digest(b"world hello"));
    }

    #[test]
    fn trigger_labels() {
        let trig = TriggerDefinition::Glob {
            pattern: "review-*".into(),
            weight: 5,
        };
        assert_eq!(trig.weight(), 5);
        assert_eq!(trig.label(), "review-*");
    }

    #[test]
    fn permission_default() -> Result<(), Box<dyn std::error::Error>> {
        let manifest: AgentManifest = serde_yaml::from_str(
            r#"
id: audit
name: Audit
description: Checks things
body: Prompt
"#,
        )?;
        assert!(matches!(manifest.permission_mode, PermissionMode::Default));
        Ok(())
    }
}
