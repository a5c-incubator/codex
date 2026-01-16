use serde::Deserialize;
use serde::Serialize;
use std::cmp::Ordering;
use std::path::PathBuf;

/// Identifier associated with plugin-provided manifests.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash)]
#[serde(transparent)]
pub struct PluginId(pub String);

impl PluginId {
    /// Constructs a new plugin identifier from any string-like value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

/// Discovery scope metadata used to derive priority tiers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
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
    /// User directory such as `~/.claude/agents`.
    User {
        /// Path to the manifest on disk.
        path: PathBuf,
    },
    /// Plugin-provided manifest.
    Plugin {
        /// Path to the manifest file.
        path: PathBuf,
        /// Identifier for the plugin that supplied it.
        plugin: PluginId,
    },
}

impl DiscoveryScope {
    /// Returns the priority tier tied to this scope.
    #[must_use]
    pub fn priority(&self) -> DiscoveryPriority {
        match self {
            Self::Project { .. } => DiscoveryPriority::Project,
            Self::CliJson { .. } => DiscoveryPriority::Cli,
            Self::User { .. } => DiscoveryPriority::User,
            Self::Plugin { .. } => DiscoveryPriority::Plugin,
        }
    }
}

/// Priority ordering required by docs/subagents/architecture.md.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub enum DiscoveryPriority {
    /// Project manifests take precedence over every other scope.
    Project = 3,
    /// CLI overrides are next.
    Cli = 2,
    /// User manifests rank after CLI entries.
    User = 1,
    /// Plugin entries are the fallback.
    Plugin = 0,
}

impl DiscoveryPriority {
    /// Human friendly label for diagnostics.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Cli => "cli",
            Self::User => "user",
            Self::Plugin => "plugin",
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

#[cfg(all(test, feature = "schema"))]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    #[test]
    fn priorities_order_correctly() {
        let mut tiers = vec![
            DiscoveryScope::Plugin {
                path: PathBuf::from("plugin"),
                plugin: PluginId::new("demo"),
            },
            DiscoveryScope::Project {
                path: PathBuf::from("project"),
            },
            DiscoveryScope::User {
                path: PathBuf::from("user"),
            },
        ];
        tiers.sort_by(|a, b| b.priority().cmp(&a.priority()));
        assert_eq!(
            tiers.first().map(DiscoveryScope::priority),
            Some(DiscoveryPriority::Project)
        );
    }
}
