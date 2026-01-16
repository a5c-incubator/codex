//! Discovery scope metadata and priority tiers.

use crate::manifest::BuiltInAgent;
#[cfg(feature = "manifest-schema")]
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::cmp::Ordering;
use std::path::PathBuf;

/// Identifier associated with plugin-provided manifests.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "manifest-schema", derive(JsonSchema))]
#[serde(transparent)]
pub struct PluginId(pub String);

impl PluginId {
    /// Constructs a new plugin identifier.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the string representation of the plugin id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for PluginId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Discovery scope tracked for every manifest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "manifest-schema", derive(JsonSchema))]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DiscoveryScope {
    /// Project `.claude/agents` directory or file.
    Project {
        /// Path to the manifest on disk.
        path: PathBuf,
    },
    /// CLI `--agents` JSON payload.
    CliJson {
        /// Optional user supplied label for diagnostics.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// User directory (e.g. `~/.claude/agents`).
    User {
        /// Path to the manifest on disk.
        path: PathBuf,
    },
    /// Plugin-provided manifest.
    Plugin {
        /// Path to the manifest file.
        path: PathBuf,
        /// Identifier for the plugin that supplied the manifest.
        #[serde(rename = "pluginId")]
        plugin_id: PluginId,
    },
    /// Manifest bundled with Codex itself.
    BuiltIn {
        /// Identifier for the built-in agent.
        agent: BuiltInAgent,
    },
}

impl DiscoveryScope {
    /// Returns the priority tier derived from the scope.
    #[must_use]
    pub fn priority(&self) -> DiscoveryPriority {
        match self {
            Self::Project { .. } => DiscoveryPriority::Project,
            Self::CliJson { .. } => DiscoveryPriority::Cli,
            Self::User { .. } => DiscoveryPriority::User,
            Self::Plugin { .. } => DiscoveryPriority::Plugin,
            Self::BuiltIn { .. } => DiscoveryPriority::BuiltIn,
        }
    }
}

/// Priority ordering required by Claude manifest discovery rules.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "manifest-schema", derive(JsonSchema))]
#[repr(u8)]
pub enum DiscoveryPriority {
    /// Project manifests take precedence over every other scope.
    Project = 4,
    /// CLI overrides are evaluated after project manifests.
    Cli = 3,
    /// User manifests rank below CLI overrides.
    User = 2,
    /// Plugins outrank only built-in manifests.
    Plugin = 1,
    /// Built-in manifests are always fallback defaults.
    BuiltIn = 0,
}

impl DiscoveryPriority {
    /// Human friendly label used in diagnostics.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Cli => "cli",
            Self::User => "user",
            Self::Plugin => "plugin",
            Self::BuiltIn => "built-in",
        }
    }

    const fn rank(self) -> u8 {
        self as u8
    }
}

impl Ord for DiscoveryPriority {
    fn cmp(&self, other: &Self) -> Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl PartialOrd for DiscoveryPriority {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::cmp::Reverse;
    use std::path::PathBuf;

    #[test]
    fn priorities_sort_descending() {
        let mut values = vec![
            DiscoveryScope::Plugin {
                path: PathBuf::from("plugins/demo.md"),
                plugin_id: PluginId::new("demo"),
            },
            DiscoveryScope::User {
                path: PathBuf::from("user/demo.md"),
            },
            DiscoveryScope::Project {
                path: PathBuf::from(".claude/agents/demo.md"),
            },
            DiscoveryScope::CliJson {
                label: Some("flag".into()),
            },
            DiscoveryScope::BuiltIn {
                agent: BuiltInAgent::Plan,
            },
        ];
        values.sort_by_key(|scope| Reverse(scope.priority()));
        let tiers: Vec<DiscoveryPriority> = values.iter().map(DiscoveryScope::priority).collect();
        assert_eq!(
            tiers,
            vec![
                DiscoveryPriority::Project,
                DiscoveryPriority::Cli,
                DiscoveryPriority::User,
                DiscoveryPriority::Plugin,
                DiscoveryPriority::BuiltIn,
            ]
        );
    }
}
