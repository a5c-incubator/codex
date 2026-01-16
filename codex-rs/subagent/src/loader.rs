use crate::error::ManifestError;
use crate::manifest::AgentManifest;
use crate::manifest::compute_digest;
use crate::priority::DiscoveryPriority;
use crate::priority::DiscoveryScope;
use crate::priority::PluginId;
use crate::validation::validate_manifest;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use walkdir::WalkDir;

/// Event sent by loaders when they detect a file system change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderEvent {
    /// Scope that triggered the change.
    pub scope: DiscoveryTarget,
}

/// Abstraction over the manifest discovery stack described in docs/subagents/architecture.md.
pub trait ManifestLoader: Send + Sync {
    /// Loads manifests from the provided discovery targets.
    fn load(&self, targets: &[DiscoveryTarget]) -> Result<Vec<AgentManifest>, ManifestError>;

    /// Starts watching the given targets for changes.
    fn watch(&self, targets: &[DiscoveryTarget]) -> Result<(), ManifestError> {
        let _ = targets;
        Err(ManifestError::WatchUnsupported)
    }
}

/// File-system backed implementation that understands Markdown + front matter, YAML, and JSON manifests as described in `docs/subagents/architecture.md`.
///
/// # Examples
///
/// ```no_run
/// use codex_subagent::{DiscoveryScope, DiscoveryTarget, FsManifestLoader, ManifestLoader};
/// use std::path::PathBuf;
///
/// # fn main() -> Result<(), codex_subagent::ManifestError> {
/// let loader = FsManifestLoader::new();
/// let manifests = loader.load(&[DiscoveryTarget::ProjectDir(PathBuf::from("./.claude/agents"))])?;
/// assert!(manifests.iter().all(|manifest| matches!(manifest.source.as_ref(), Some(DiscoveryScope::Project { .. }))));
/// # Ok(()) }
/// ```
#[derive(Debug, Default)]
pub struct FsManifestLoader;

impl FsManifestLoader {
    /// Creates a new loader.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    fn parse_json_payload(
        &self,
        payload: &Value,
        scope: DiscoveryScope,
    ) -> Result<AgentManifest, ManifestError> {
        let cli_path = PathBuf::from("<cli>");
        let mut manifest: AgentManifest =
            serde_json::from_value(payload.clone()).map_err(|err| ManifestError::Parse {
                path: cli_path.clone(),
                source: Box::new(err),
            })?;
        manifest.source = Some(scope);
        let serialized = serde_json::to_vec(payload).map_err(|err| ManifestError::Parse {
            path: cli_path.clone(),
            source: Box::new(err),
        })?;
        manifest.digest = Some(compute_digest(&serialized));
        self.validate(cli_path, manifest)
    }

    fn validate(
        &self,
        path: PathBuf,
        manifest: AgentManifest,
    ) -> Result<AgentManifest, ManifestError> {
        validate_manifest(&manifest).map_err(|issues| ManifestError::Validation {
            path: path.clone(),
            issues,
        })?;
        Ok(manifest)
    }

    fn parse_file(
        &self,
        path: &Path,
        scope: DiscoveryScope,
    ) -> Result<AgentManifest, ManifestError> {
        let bytes = fs::read(path).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let digest = compute_digest(&bytes);
        let format = manifest_format(path).ok_or_else(|| {
            ManifestError::Inline(format!(
                "unsupported manifest extension: {}",
                path.display()
            ))
        })?;
        let mut manifest = match format {
            ManifestFormat::Markdown => {
                parse_markdown(&bytes).map_err(|err| ManifestError::Parse {
                    path: path.to_path_buf(),
                    source: err,
                })?
            }
            ManifestFormat::Yaml => parse_yaml::<AgentManifest>(&bytes, path)?,
            ManifestFormat::Json => parse_json::<AgentManifest>(&bytes, path)?,
        };
        manifest.source = Some(scope);
        manifest.digest = Some(digest);
        self.validate(path.to_path_buf(), manifest)
    }
}

impl ManifestLoader for FsManifestLoader {
    fn load(&self, targets: &[DiscoveryTarget]) -> Result<Vec<AgentManifest>, ManifestError> {
        let mut manifests = Vec::new();
        let mut ids: HashMap<String, DiscoveryScope> = HashMap::new();

        for target in targets {
            match target {
                DiscoveryTarget::CliJson { manifest, label } => {
                    let scope = DiscoveryScope::CliJson {
                        label: label.clone(),
                    };
                    let manifest = self.parse_json_payload(manifest, scope.clone())?;
                    guard_duplicate(&mut ids, &manifest)?;
                    manifests.push(manifest);
                }
                DiscoveryTarget::ProjectDir(path) => {
                    let scoped = self.read_directory(path, |p| DiscoveryScope::Project {
                        path: p.to_path_buf(),
                    })?;
                    for manifest in scoped {
                        guard_duplicate(&mut ids, &manifest)?;
                        manifests.push(manifest);
                    }
                }
                DiscoveryTarget::UserDir(path) => {
                    let scoped = self.read_directory(path, |p| DiscoveryScope::User {
                        path: p.to_path_buf(),
                    })?;
                    for manifest in scoped {
                        guard_duplicate(&mut ids, &manifest)?;
                        manifests.push(manifest);
                    }
                }
                DiscoveryTarget::PluginDir { path, plugin } => {
                    let plugin_id = plugin.clone();
                    let scoped = self.read_directory(path, |p| DiscoveryScope::Plugin {
                        path: p.to_path_buf(),
                        plugin: plugin_id.clone(),
                    })?;
                    for manifest in scoped {
                        guard_duplicate(&mut ids, &manifest)?;
                        manifests.push(manifest);
                    }
                }
            }
        }

        manifests.sort_by(|a, b| {
            b.priority()
                .cmp(&a.priority())
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(manifests)
    }
}

impl FsManifestLoader {
    fn read_directory<F>(
        &self,
        path: &Path,
        scope_builder: F,
    ) -> Result<Vec<AgentManifest>, ManifestError>
    where
        F: Fn(&Path) -> DiscoveryScope,
    {
        let mut collected = Vec::new();
        let metadata = fs::metadata(path).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        if metadata.is_file() {
            let manifest = self.parse_file(path, scope_builder(path))?;
            collected.push(manifest);
            return Ok(collected);
        }

        for entry in WalkDir::new(path).max_depth(1) {
            let entry = entry.map_err(|err| {
                let description = err.to_string();
                let io_err = match err.into_io_error() {
                    Some(inner) => inner,
                    None => io::Error::new(io::ErrorKind::Other, description),
                };
                ManifestError::Io {
                    path: path.to_path_buf(),
                    source: io_err,
                }
            })?;
            if entry.path() == path {
                continue;
            }
            if !entry.file_type().is_file() || !is_supported(entry.path()) {
                continue;
            }
            let manifest = self.parse_file(entry.path(), scope_builder(entry.path()))?;
            collected.push(manifest);
        }

        Ok(collected)
    }
}

fn guard_duplicate(
    ids: &mut HashMap<String, DiscoveryScope>,
    manifest: &AgentManifest,
) -> Result<(), ManifestError> {
    let scope = manifest
        .source
        .clone()
        .ok_or_else(|| ManifestError::Inline(format!("manifest {} missing source", manifest.id)))?;
    if let Some(existing) = ids.insert(manifest.id.clone(), scope.clone()) {
        return Err(ManifestError::DuplicateId {
            agent_id: manifest.id.clone(),
            first: existing,
            second: scope,
        });
    }
    Ok(())
}

fn is_supported(path: &Path) -> bool {
    manifest_format(path).is_some()
}

fn parse_markdown(bytes: &[u8]) -> Result<AgentManifest, Box<dyn std::error::Error + Send + Sync>> {
    let contents = String::from_utf8(bytes.to_vec())?;
    let (front_matter, body) = split_front_matter(&contents)?;
    let mut manifest: AgentManifest = serde_yaml::from_str(&front_matter)?;
    manifest.body = body.trim_start().to_owned();
    Ok(manifest)
}

fn parse_yaml<T>(bytes: &[u8], path: &Path) -> Result<T, ManifestError>
where
    T: DeserializeOwned,
{
    serde_yaml::from_slice(bytes).map_err(|err| ManifestError::Parse {
        path: path.to_path_buf(),
        source: Box::new(err),
    })
}

fn parse_json<T>(bytes: &[u8], path: &Path) -> Result<T, ManifestError>
where
    T: DeserializeOwned,
{
    serde_json::from_slice(bytes).map_err(|err| ManifestError::Parse {
        path: path.to_path_buf(),
        source: Box::new(err),
    })
}

enum ManifestFormat {
    Markdown,
    Json,
    Yaml,
}

fn manifest_format(path: &Path) -> Option<ManifestFormat> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "md" | "markdown" => Some(ManifestFormat::Markdown),
        "json" => Some(ManifestFormat::Json),
        "yaml" | "yml" => Some(ManifestFormat::Yaml),
        _ => None,
    }
}

fn split_front_matter(
    contents: &str,
) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    let mut lines = contents.lines();
    match lines.next() {
        Some(line) if line.trim() == "---" => {}
        _ => {
            return Err(format!("missing YAML front matter delimiter").into());
        }
    }

    let mut front = String::new();
    let mut closing_found = false;
    for line in &mut lines {
        if line.trim() == "---" {
            closing_found = true;
            break;
        }
        front.push_str(line);
        front.push('\n');
    }

    if !closing_found {
        return Err("missing closing YAML front matter delimiter".into());
    }

    let body = lines.collect::<Vec<_>>().join("\n");
    Ok((front.trim().to_owned(), body))
}

/// Discovery sources surfaced by the loader API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryTarget {
    /// Directory or file inside the project tree.
    ProjectDir(PathBuf),
    /// Inline CLI JSON manifest.
    CliJson {
        /// Manifest payload provided by the CLI.
        manifest: Value,
        /// Optional label used for diagnostics.
        label: Option<String>,
    },
    /// User-level manifests.
    UserDir(PathBuf),
    /// Plugin-managed manifests.
    PluginDir {
        /// Path to the manifest.
        path: PathBuf,
        /// Identifier for the plugin providing the manifest.
        plugin: PluginId,
    },
}

impl DiscoveryTarget {
    /// Priority tier for this target.
    #[must_use]
    pub fn priority(&self) -> DiscoveryPriority {
        match self {
            Self::ProjectDir(_) => DiscoveryPriority::Project,
            Self::CliJson { .. } => DiscoveryPriority::Cli,
            Self::UserDir(_) => DiscoveryPriority::User,
            Self::PluginDir { .. } => DiscoveryPriority::Plugin,
        }
    }
}

#[cfg(all(test, feature = "fs-loader"))]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn sorts_by_priority() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let project = dir.path().join("project.md");
        fs::write(
            &project,
            b"---\nid: project\nname: Project\ndescription: P\nbody: text\n---\nBody",
        )?;
        let user = dir.path().join("user.md");
        fs::write(
            &user,
            b"---\nid: user\nname: User\ndescription: U\nbody: text\n---\nBody",
        )?;

        let loader = FsManifestLoader::new();
        let manifests = loader.load(&[
            DiscoveryTarget::UserDir(user.clone()),
            DiscoveryTarget::ProjectDir(project.clone()),
            DiscoveryTarget::CliJson {
                manifest: json!({
                    "id": "cli",
                    "name": "Cli",
                    "description": "cli",
                    "body": "Body"
                }),
                label: Some("flag".into()),
            },
        ])?;
        let ids: Vec<_> = manifests.into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["project", "cli", "user"]);
        Ok(())
    }
}
