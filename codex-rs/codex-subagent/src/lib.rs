#![deny(missing_docs)]
#![doc = "Subagent scaffolding; see docs/subagents/architecture.md for the complete design."]

mod builtins;
mod error;
#[cfg(feature = "loader")]
mod loader;
mod manifest;
#[cfg(feature = "loader")]
/// CLI override helpers for manifest discovery.
pub mod overrides;
mod priority;
mod validation;

#[cfg(feature = "manifest-schema")]
/// Placeholder schema module for manifest validation structs.
pub mod schema {
    /// TODO: add schema validation structs described in docs/subagents/architecture.md.
    #[derive(Debug, Default, Clone)]
    pub struct ManifestSchema;
}

pub use builtins::built_in_manifests;
pub use error::ManifestError;
pub use error::ValidationIssue;
pub use error::ValidationIssues;
pub use manifest::compute_digest;
pub use manifest::AgentId;
pub use manifest::AgentKind;
pub use manifest::AgentManifest;
pub use manifest::BuiltInAgent;
pub use manifest::Hook;
pub use manifest::HookSet;
pub use manifest::ModelRef;
pub use manifest::PermissionMode;
pub use manifest::ToolName;
pub use manifest::ToolScope;
pub use manifest::TriggerDefinition;
pub use priority::DiscoveryPriority;
pub use priority::DiscoveryScope;
pub use priority::PluginId;
pub use validation::validate_manifest;

#[cfg(feature = "loader")]
pub use overrides::build_discovery_targets;
#[cfg(feature = "loader")]
pub use overrides::default_discovery_targets;
#[cfg(feature = "loader")]
pub use overrides::parse_plugin_dir;
#[cfg(feature = "loader")]
pub use overrides::parse_subagent_overrides;
#[cfg(feature = "loader")]
pub use overrides::CliManifestOverride;
#[cfg(feature = "loader")]
pub use overrides::DiscoveryTargetArgs;
#[cfg(feature = "loader")]
pub use overrides::PluginDirArg;
#[cfg(feature = "loader")]
pub use overrides::SubagentDiscoveryOverrides;
#[cfg(feature = "loader")]
pub use overrides::SubagentOverrideInput;

#[cfg(feature = "loader")]
pub use loader::DiscoveryTarget;
#[cfg(feature = "loader")]
pub use loader::FsManifestLoader;
#[cfg(feature = "loader")]
pub use loader::LoadOutcome;
#[cfg(feature = "loader")]
pub use loader::LoaderEvent;
#[cfg(feature = "loader")]
pub use loader::LoaderIssue;
#[cfg(feature = "loader")]
pub use loader::LoaderWatch;
#[cfg(feature = "loader")]
pub use loader::LoaderWatchEventStream;
#[cfg(feature = "loader")]
pub use loader::LoaderWatchTryRecvError;
#[cfg(feature = "loader")]
pub use loader::ManifestLoader;
#[cfg(feature = "manifest-schema")]
pub use schema::ManifestSchema;
