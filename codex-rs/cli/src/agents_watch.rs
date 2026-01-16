use std::sync::Arc;
use std::sync::RwLock;

use anyhow::Result;
use codex_core::agent::AgentRegistry;
use codex_core::agent::AgentRegistryWatch;
use codex_core::agent::AgentRegistryWatchConfig;
use codex_core::agent::RefreshInvocation;
use codex_core::agent::RefreshOutcome;
use codex_exec::subagent_args::DiscoveryTargetArgs;
use codex_exec::subagent_args::build_discovery_targets;
use codex_subagent::DiscoveryScope;
use codex_subagent::DiscoveryTarget;
use codex_subagent::FsManifestLoader;
use codex_subagent::ManifestError;
use codex_subagent::ManifestLoader;

/// Shared helper that bootstraps discovery + watch plumbing for `codex agents`.
///
/// The CLI uses this for `codex agents list`, and the TUI will reuse it in an
/// upcoming iteration so the `/agents` panel can subscribe to the same watch
/// stream without shelling out to the CLI.
pub struct AgentsWatchBootstrap {
    targets: Vec<DiscoveryTarget>,
    registry: Arc<RwLock<AgentRegistry>>,
    missing_sources: bool,
    built_in_ids: Vec<String>,
    override_scope_labels: Vec<String>,
    outcome: RefreshOutcome,
}

impl AgentsWatchBootstrap {
    /// Builds a registry + watch targets for the provided discovery args.
    pub fn new(
        discovery_args: DiscoveryTargetArgs<'_>,
        invocation: RefreshInvocation,
    ) -> Result<Self> {
        bootstrap_agents_watch_with_loader(discovery_args, invocation, None)
    }

    /// Exposes the discovery targets used for the initial refresh.
    pub fn discovery_targets(&self) -> &[DiscoveryTarget] {
        &self.targets
    }

    /// Indicates whether any manifest sources were detected.
    pub fn missing_sources(&self) -> bool {
        self.missing_sources
    }

    /// Returns the built-in agent ids surfaced during the initial refresh.
    pub fn built_in_ids(&self) -> &[String] {
        &self.built_in_ids
    }

    /// Returns human-readable labels for CLI and plugin override scopes.
    pub fn override_scope_labels(&self) -> &[String] {
        &self.override_scope_labels
    }

    /// Returns the initial refresh outcome so callers can render summaries.
    pub fn outcome(&self) -> &RefreshOutcome {
        &self.outcome
    }

    /// Provides a handle to the shared registry so callers can snapshot manifests.
    pub fn registry(&self) -> Arc<RwLock<AgentRegistry>> {
        Arc::clone(&self.registry)
    }

    /// Starts a background watch worker for the collected discovery targets.
    pub fn start_watch(
        &self,
        config: AgentRegistryWatchConfig,
    ) -> Result<AgentRegistryWatch, ManifestError> {
        AgentRegistry::start_watch(self.registry(), self.targets.clone(), None, config)
    }
}

/// Bootstraps manifest discovery, optionally injecting a custom loader (primarily for tests).
pub fn bootstrap_agents_watch(
    discovery_args: DiscoveryTargetArgs<'_>,
    invocation: RefreshInvocation,
) -> Result<AgentsWatchBootstrap> {
    bootstrap_agents_watch_with_loader(discovery_args, invocation, None)
}

fn bootstrap_agents_watch_with_loader(
    discovery_args: DiscoveryTargetArgs<'_>,
    invocation: RefreshInvocation,
    loader: Option<Arc<dyn ManifestLoader>>,
) -> Result<AgentsWatchBootstrap> {
    let targets = build_discovery_targets(&discovery_args)?;
    let missing_sources = targets.is_empty();
    let loader: Arc<dyn ManifestLoader> =
        loader.unwrap_or_else(|| Arc::new(FsManifestLoader::new()));
    let mut registry = AgentRegistry::new(loader);
    let outcome = registry.refresh_with_telemetry(&targets, None, invocation)?;
    let built_in_ids = collect_built_in_manifest_ids(&registry);
    let override_scope_labels = collect_override_scope_labels(&targets);
    let registry = Arc::new(RwLock::new(registry));
    Ok(AgentsWatchBootstrap {
        targets,
        registry,
        missing_sources,
        built_in_ids,
        override_scope_labels,
        outcome,
    })
}

fn collect_built_in_manifest_ids(registry: &AgentRegistry) -> Vec<String> {
    registry
        .manifests()
        .filter_map(|manifest| match manifest.source.as_ref() {
            Some(DiscoveryScope::BuiltIn { .. }) => Some(manifest.id.as_str().to_string()),
            _ => None,
        })
        .collect()
}

fn collect_override_scope_labels(targets: &[DiscoveryTarget]) -> Vec<String> {
    targets.iter().filter_map(override_scope_label).collect()
}

fn override_scope_label(target: &DiscoveryTarget) -> Option<String> {
    match target {
        DiscoveryTarget::CliJson { label, .. } => {
            Some(format!("cli:{}", label.as_deref().unwrap_or("inline")))
        }
        DiscoveryTarget::CliManifestFile { path, .. } => {
            Some(format!("cli-file:{}", path.display()))
        }
        DiscoveryTarget::PluginDir { plugin, path } => {
            Some(format!("plugin:{} ({})", plugin.as_str(), path.display()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_exec::subagent_args::PluginDirArg;
    use codex_exec::subagent_args::SubagentOverrideInput;
    use codex_exec::subagent_args::parse_subagent_overrides;
    use codex_subagent::PluginId;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn agents_list_bootstrap_loads_project_scopes() -> Result<()> {
        let temp = TempDir::new()?;
        let project_dir = temp.path().join(".claude").join("agents");
        std::fs::create_dir_all(&project_dir)?;
        std::fs::write(
            project_dir.join("demo.md"),
            r#"---
id: watcher-demo
name: Watcher Demo
description: Demo manifest
permissionMode: default
---
Watcher body
"#,
        )?;
        let overrides = parse_subagent_overrides(&SubagentOverrideInput {
            cli_manifests: &[],
            cli_manifest_files: &[],
            plugin_dirs: &[],
        })?;
        let discovery_args = DiscoveryTargetArgs {
            cwd: temp.path(),
            project_dir_override: Some(project_dir.as_path()),
            user_dir_override: None,
            overrides: &overrides,
        };
        let bootstrap = bootstrap_agents_watch(discovery_args, RefreshInvocation::CliListHuman)?;
        assert!(!bootstrap.missing_sources());
        assert!(bootstrap.discovery_targets().iter().any(
            |target| matches!(target, DiscoveryTarget::ProjectDir(path) if path.ends_with("agents"))
        ));
        let registry = bootstrap.registry();
        let guard = registry
            .read()
            .expect("registry read lock should be available");
        let found = guard
            .manifests()
            .any(|manifest| manifest.id.as_str() == "watcher-demo");
        assert!(found);
        Ok(())
    }

    #[test]
    fn agents_list_bootstrap_tracks_built_in_ids() -> Result<()> {
        let temp = TempDir::new()?;
        let overrides = parse_subagent_overrides(&SubagentOverrideInput {
            cli_manifests: &[json!({
                "id": "inline-demo",
                "name": "Inline Demo",
                "description": "inline",
                "body": "Prompt"
            })
            .to_string()],
            cli_manifest_files: &[],
            plugin_dirs: &[],
        })?;
        let discovery_args = DiscoveryTargetArgs {
            cwd: temp.path(),
            project_dir_override: None,
            user_dir_override: None,
            overrides: &overrides,
        };
        let bootstrap = bootstrap_agents_watch(discovery_args, RefreshInvocation::CliListJson)?;
        assert!(
            bootstrap
                .built_in_ids()
                .iter()
                .any(|id| id.starts_with("builtin-"))
        );
        Ok(())
    }

    #[test]
    fn agents_list_bootstrap_handles_plugins() -> Result<()> {
        let temp = TempDir::new()?;
        let plugin_dir = temp.path().join("plugins").join("agents");
        std::fs::create_dir_all(&plugin_dir)?;
        std::fs::write(
            plugin_dir.join("plugin.md"),
            r#"---
id: plugin-agent
name: Plugin Agent
description: plugin
permissionMode: default
---
Plugin body
"#,
        )?;
        let plugin_arg = PluginDirArg {
            id: PluginId::new("demo"),
            path: plugin_dir.clone(),
        };
        let overrides = parse_subagent_overrides(&SubagentOverrideInput {
            cli_manifests: &[],
            cli_manifest_files: &[],
            plugin_dirs: &[plugin_arg],
        })?;
        let discovery_args = DiscoveryTargetArgs {
            cwd: temp.path(),
            project_dir_override: None,
            user_dir_override: None,
            overrides: &overrides,
        };
        let bootstrap = bootstrap_agents_watch(discovery_args, RefreshInvocation::CliListHuman)?;
        assert!(bootstrap
            .discovery_targets()
            .iter()
            .any(|target| matches!(target, DiscoveryTarget::PluginDir { path, .. } if path == &plugin_dir)));
        Ok(())
    }

    #[test]
    fn agents_list_bootstrap_reports_override_scopes() -> Result<()> {
        let temp = TempDir::new()?;
        let cli_path = temp.path().join("cli.json");
        std::fs::write(
            &cli_path,
            serde_json::to_vec(&json!({
                "id": "cli-file",
                "name": "CLI File",
                "description": "file",
                "body": "body"
            }))?,
        )?;
        let plugin_dir = temp.path().join("plugins");
        std::fs::create_dir_all(&plugin_dir)?;
        std::fs::write(
            plugin_dir.join("plugin.md"),
            r#"---
id: plugin-override
name: Plugin Override
description: override
permissionMode: default
---
Body
"#,
        )?;

        let plugin_arg = PluginDirArg {
            id: PluginId::new("demo"),
            path: plugin_dir,
        };
        let overrides = parse_subagent_overrides(&SubagentOverrideInput {
            cli_manifests: &["{\"id\":\"cli-inline\",\"name\":\"inline\",\"description\":\"inline\",\"body\":\"inline\"}".to_string()],
            cli_manifest_files: &[cli_path],
            plugin_dirs: &[plugin_arg],
        })?;
        let discovery_args = DiscoveryTargetArgs {
            cwd: temp.path(),
            project_dir_override: None,
            user_dir_override: None,
            overrides: &overrides,
        };
        let bootstrap = bootstrap_agents_watch(discovery_args, RefreshInvocation::CliListHuman)?;
        let labels = bootstrap.override_scope_labels();
        assert!(labels.iter().any(|label| label.starts_with("cli:cli")));
        assert!(labels.iter().any(|label| label.starts_with("cli-file:")));
        assert!(labels.iter().any(|label| label.starts_with("plugin:demo")));
        Ok(())
    }
}
