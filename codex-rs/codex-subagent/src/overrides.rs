use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use dirs::home_dir;
use dunce::canonicalize as normalize_path;
use serde::Serialize;
use serde_json::Value;

use crate::DiscoveryTarget;
use crate::PluginId;

/// Represents CLI-provided manifest overrides so callers can extend discovery targets.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct SubagentDiscoveryOverrides {
    /// Inline CLI manifest payloads plus their labels.
    pub cli_manifests: Vec<CliManifestOverride>,
    /// Plugin directories specified on the CLI (by plugin id).
    pub plugin_dirs: Vec<PluginDirArg>,
}

impl SubagentDiscoveryOverrides {
    /// Returns true when no CLI or plugin overrides are present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cli_manifests.is_empty() && self.plugin_dirs.is_empty()
    }
}

/// Inline or file-backed CLI manifest payload plus its diagnostic label.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CliManifestOverride {
    /// Full manifest payload provided by the CLI.
    pub manifest: Value,
    /// Human-friendly label (e.g., `cli` or the manifest path).
    pub label: Option<String>,
    /// Optional source path (present for `--cli-manifest-file` overrides).
    pub path: Option<PathBuf>,
}

/// Parsed `--plugin id=path` argument.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PluginDirArg {
    /// Identifier for the plugin that provided the manifests.
    pub id: PluginId,
    /// Directory containing the plugin's manifest files.
    pub path: PathBuf,
}

/// Arguments required to parse CLI overrides into a structured payload.
pub struct SubagentOverrideInput<'a> {
    /// Inline JSON manifests passed via `--subagent-manifest`.
    pub cli_manifests: &'a [String],
    /// CLI manifest file paths passed via `--subagent-manifest-file`.
    pub cli_manifest_files: &'a [PathBuf],
    /// Parsed plugin directory arguments (via `--plugin`).
    pub plugin_dirs: &'a [PluginDirArg],
}

/// Arguments describing how to build discovery targets for the registry.
pub struct DiscoveryTargetArgs<'a> {
    /// Base working directory used to locate the default manifests.
    pub cwd: &'a Path,
    /// Optional alternate project manifest directory.
    pub project_dir_override: Option<&'a Path>,
    /// Optional alternate user manifest directory.
    pub user_dir_override: Option<&'a Path>,
    /// CLI overrides to merge into the discovery targets.
    pub overrides: &'a SubagentDiscoveryOverrides,
}

/// Parses CLI-provided overrides (inline manifests, files, and plugin directories).
pub fn parse_subagent_overrides(
    input: &SubagentOverrideInput<'_>,
) -> Result<SubagentDiscoveryOverrides> {
    let mut overrides = SubagentDiscoveryOverrides::default();

    for plugin in input.plugin_dirs {
        if !plugin.path.exists() {
            return Err(anyhow!(
                "plugin manifest path {} does not exist",
                plugin.path.display()
            ));
        }
        overrides.plugin_dirs.push(plugin.clone());
    }

    for inline in input.cli_manifests {
        let manifest: Value = serde_json::from_str(inline)
            .with_context(|| "failed to parse CLI manifest JSON payload")?;
        overrides.cli_manifests.push(CliManifestOverride {
            manifest,
            label: Some("cli".into()),
            path: None,
        });
    }

    for path in input.cli_manifest_files {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read CLI manifest file {}", path.display()))?;
        let manifest: Value = serde_json::from_str(&contents)
            .with_context(|| format!("invalid JSON in CLI manifest file {}", path.display()))?;
        overrides.cli_manifests.push(CliManifestOverride {
            manifest,
            label: Some(path.to_string_lossy().into_owned()),
            path: Some(normalize_path(path).unwrap_or_else(|_| path.to_path_buf())),
        });
    }

    Ok(overrides)
}

/// Builds the registry discovery targets using defaults plus CLI overrides.
pub fn build_discovery_targets(args: &DiscoveryTargetArgs<'_>) -> Result<Vec<DiscoveryTarget>> {
    let mut targets = default_discovery_targets(args.cwd);

    if let Some(project_dir) = args.project_dir_override {
        prune_targets(&mut targets, |target| {
            matches!(target, DiscoveryTarget::ProjectDir(_))
        });
        add_dir_target(
            &mut targets,
            project_dir.to_path_buf(),
            DiscoveryTarget::ProjectDir,
            true,
        )?;
    }

    if let Some(user_dir) = args.user_dir_override {
        prune_targets(&mut targets, |target| {
            matches!(target, DiscoveryTarget::UserDir(_))
        });
        add_dir_target(
            &mut targets,
            user_dir.to_path_buf(),
            DiscoveryTarget::UserDir,
            true,
        )?;
    }

    for plugin in &args.overrides.plugin_dirs {
        add_plugin_target(&mut targets, plugin)?;
    }

    for manifest in &args.overrides.cli_manifests {
        if let Some(path) = &manifest.path {
            targets.push(DiscoveryTarget::CliManifestFile {
                path: path.clone(),
                label: manifest.label.clone(),
            });
        } else {
            targets.push(DiscoveryTarget::CliJson {
                manifest: manifest.manifest.clone(),
                label: manifest.label.clone(),
            });
        }
    }

    Ok(targets)
}

/// Returns the default project and user discovery targets rooted at `cwd`.
pub fn default_discovery_targets(cwd: &Path) -> Vec<DiscoveryTarget> {
    let mut targets = Vec::new();

    let project = cwd.join(".claude").join("agents");
    if path_exists(&project) {
        targets.push(DiscoveryTarget::ProjectDir(project));
    }

    if let Some(home) = home_dir() {
        let user = home.join(".claude").join("agents");
        if path_exists(&user) {
            targets.push(DiscoveryTarget::UserDir(user));
        }
    }

    targets
}

fn prune_targets<F>(targets: &mut Vec<DiscoveryTarget>, predicate: F)
where
    F: Fn(&DiscoveryTarget) -> bool,
{
    targets.retain(|target| !predicate(target));
}

fn add_dir_target<F>(
    targets: &mut Vec<DiscoveryTarget>,
    path: PathBuf,
    builder: F,
    required: bool,
) -> Result<()>
where
    F: FnOnce(PathBuf) -> DiscoveryTarget,
{
    if path.exists() {
        targets.push(builder(path));
    } else if required {
        return Err(anyhow!("manifest path {} does not exist", path.display()));
    }
    Ok(())
}

fn add_plugin_target(targets: &mut Vec<DiscoveryTarget>, plugin: &PluginDirArg) -> Result<()> {
    if !plugin.path.exists() {
        return Err(anyhow!(
            "plugin manifest path {} does not exist",
            plugin.path.display()
        ));
    }
    targets.push(DiscoveryTarget::PluginDir {
        path: plugin.path.clone(),
        plugin: plugin.id.clone(),
    });
    Ok(())
}

fn path_exists(path: &Path) -> bool {
    path.exists()
}

/// Parses a `--plugin` CLI argument of the form `id=path`.
pub fn parse_plugin_dir(input: &str) -> std::result::Result<PluginDirArg, String> {
    let (id, path) = input
        .split_once('=')
        .ok_or_else(|| "expected format plugin_id=path".to_string())?;
    let id = id.trim();
    if id.is_empty() {
        return Err("plugin id cannot be empty".into());
    }
    let path = PathBuf::from(path.trim());
    if path.as_os_str().is_empty() {
        return Err("plugin path cannot be empty".into());
    }
    Ok(PluginDirArg {
        id: PluginId::new(id),
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn parse_plugin_spec_happy_path() {
        let parsed = parse_plugin_dir("demo=./plugins/demo").expect("parse");
        assert_eq!(parsed.id.as_str(), "demo");
        assert_eq!(parsed.path, PathBuf::from("./plugins/demo"));
    }

    #[test]
    fn parse_plugin_spec_rejects_empty_parts() {
        assert!(parse_plugin_dir("=path").is_err());
        assert!(parse_plugin_dir("demo=").is_err());
        assert!(parse_plugin_dir("demo").is_err());
    }

    #[test]
    fn parses_cli_overrides_from_inline_and_files() {
        let temp = tempdir().expect("tempdir");
        let file_path = temp.path().join("agent.json");
        fs::write(&file_path, r#"{"name":"file"}"#).expect("write");

        let cli_manifests = vec![r#"{"name":"inline"}"#.to_string()];
        let cli_manifest_files = vec![file_path.clone()];
        let plugin_dirs = vec![PluginDirArg {
            id: PluginId::new("demo"),
            path: temp.path().into(),
        }];

        let overrides = parse_subagent_overrides(&SubagentOverrideInput {
            cli_manifests: &cli_manifests,
            cli_manifest_files: &cli_manifest_files,
            plugin_dirs: &plugin_dirs,
        })
        .expect("parse overrides");

        assert_eq!(overrides.cli_manifests.len(), 2);
        assert_eq!(overrides.plugin_dirs.len(), 1);
        assert_eq!(overrides.cli_manifests[0].label.as_deref(), Some("cli"));
        assert_eq!(
            overrides.cli_manifests[1].label.as_deref(),
            Some(file_path.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn parse_overrides_rejects_invalid_json() {
        let manifests = vec!["not-json".into()];
        let err = parse_subagent_overrides(&SubagentOverrideInput {
            cli_manifests: &manifests,
            cli_manifest_files: &[],
            plugin_dirs: &[],
        })
        .expect_err("invalid json");
        assert!(
            err.to_string()
                .contains("failed to parse CLI manifest JSON payload"),
            "{err}"
        );
    }

    #[test]
    fn parse_overrides_rejects_missing_files() {
        let manifest_files = vec![PathBuf::from("missing.json")];
        let err = parse_subagent_overrides(&SubagentOverrideInput {
            cli_manifests: &[],
            cli_manifest_files: &manifest_files,
            plugin_dirs: &[],
        })
        .expect_err("missing file");
        assert!(
            err.to_string().contains("failed to read CLI manifest file"),
            "{err}"
        );
    }

    #[test]
    fn parse_overrides_rejects_missing_plugin_dirs() {
        let plugin_dirs = vec![PluginDirArg {
            id: PluginId::new("demo"),
            path: PathBuf::from("missing"),
        }];
        let err = parse_subagent_overrides(&SubagentOverrideInput {
            cli_manifests: &[],
            cli_manifest_files: &[],
            plugin_dirs: &plugin_dirs,
        })
        .expect_err("missing plugin");
        assert!(err.to_string().contains("plugin manifest path"), "{err}");
    }

    #[test]
    fn parse_overrides_allows_duplicate_plugin_ids() {
        let temp = tempdir().expect("tempdir");
        let other = tempdir().expect("tempdir");
        fs::create_dir_all(temp.path()).expect("dirs");
        fs::create_dir_all(other.path()).expect("dirs");

        let plugin_dirs = vec![
            PluginDirArg {
                id: PluginId::new("duplicate"),
                path: temp.path().into(),
            },
            PluginDirArg {
                id: PluginId::new("duplicate"),
                path: other.path().into(),
            },
        ];

        let overrides = parse_subagent_overrides(&SubagentOverrideInput {
            cli_manifests: &[],
            cli_manifest_files: &[],
            plugin_dirs: &plugin_dirs,
        })
        .expect("parse duplicate overrides");

        assert_eq!(overrides.plugin_dirs.len(), 2);
        assert_eq!(overrides.plugin_dirs[0].id, overrides.plugin_dirs[1].id);
    }

    #[test]
    fn build_targets_respects_overrides() {
        let temp = tempdir().expect("tempdir");
        let cwd = temp.path();
        let default_project = cwd.join(".claude").join("agents");
        fs::create_dir_all(&default_project).expect("default project");

        let override_project = cwd.join("custom_project");
        let override_user = cwd.join("custom_user");
        fs::create_dir_all(&override_project).expect("override project");
        fs::create_dir_all(&override_user).expect("override user");

        let overrides = SubagentDiscoveryOverrides {
            cli_manifests: vec![CliManifestOverride {
                manifest: serde_json::json!({"name": "inline"}),
                label: Some("cli".into()),
                path: None,
            }],
            plugin_dirs: vec![PluginDirArg {
                id: PluginId::new("demo"),
                path: override_project.clone(),
            }],
        };

        let targets = build_discovery_targets(&DiscoveryTargetArgs {
            cwd,
            project_dir_override: Some(override_project.as_path()),
            user_dir_override: Some(override_user.as_path()),
            overrides: &overrides,
        })
        .expect("build targets");

        assert!(targets.iter().any(|target| matches!(
            target,
            DiscoveryTarget::ProjectDir(path) if path == &override_project
        )));
        assert!(targets.iter().any(|target| matches!(
            target,
            DiscoveryTarget::UserDir(path) if path == &override_user
        )));
        assert!(targets.iter().any(|target| matches!(
            target,
            DiscoveryTarget::PluginDir { plugin, .. } if plugin.as_str() == "demo"
        )));
        assert!(targets.iter().any(|target| matches!(
            target,
            DiscoveryTarget::CliJson { label, .. } if label.as_deref() == Some("cli")
        )));
    }
}
