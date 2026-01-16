use codex_subagent::DiscoveryScope;
use codex_subagent::DiscoveryTarget;
use codex_subagent::FsManifestLoader;
use codex_subagent::ManifestLoader;
use codex_subagent::PluginId;
use insta::assert_json_snapshot;
use serde_json::json;
use std::path::PathBuf;

fn fixture_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join(name)
}

fn sample_targets() -> Vec<DiscoveryTarget> {
    vec![
        DiscoveryTarget::ProjectDir(fixture_dir("project")),
        DiscoveryTarget::UserDir(fixture_dir("user")),
        DiscoveryTarget::PluginDir {
            path: fixture_dir("plugin"),
            plugin: PluginId::new("demo-plugin"),
        },
        DiscoveryTarget::CliJson {
            manifest: json!({
                "id": "delta",
                "name": "Delta Specialist",
                "description": "Handles delta debugging",
                "permissionMode": "plan",
                "body": "Plan carefully before executing.",
                "triggers": [{ "type": "keyword", "phrase": "plan", "weight": 3 }]
            }),
            label: Some("cli".into()),
        },
    ]
}

#[test]
fn loader_snapshot() -> Result<(), Box<dyn std::error::Error>> {
    let loader = FsManifestLoader::new();
    let outcome = loader.load(&sample_targets())?;
    assert!(outcome.issues.is_empty(), "fixture should be valid");
    assert_json_snapshot!("loader_manifests", outcome.manifests);
    Ok(())
}

#[test]
fn priority_snapshot() -> Result<(), Box<dyn std::error::Error>> {
    let loader = FsManifestLoader::new();
    let outcome = loader.load(&sample_targets())?;
    assert!(outcome.issues.is_empty(), "fixture should be valid");
    let summary: Vec<_> = outcome
        .manifests
        .iter()
        .map(|manifest| {
            (
                manifest.id.clone(),
                manifest.priority().label().to_string(),
                manifest
                    .source
                    .as_ref()
                    .map(scope_label)
                    .unwrap_or_else(|| "unknown".into()),
            )
        })
        .collect();
    assert_json_snapshot!("priority_summary", summary);
    Ok(())
}

fn scope_label(scope: &DiscoveryScope) -> String {
    match scope {
        DiscoveryScope::Project { .. } => "project".into(),
        DiscoveryScope::CliJson { .. } => "cli".into(),
        DiscoveryScope::User { .. } => "user".into(),
        DiscoveryScope::Plugin { plugin_id, .. } => format!("plugin: {}", plugin_id.as_str()),
        DiscoveryScope::BuiltIn { .. } => "built-in".into(),
    }
}
