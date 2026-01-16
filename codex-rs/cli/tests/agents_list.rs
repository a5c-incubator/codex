use std::fs;
use std::path::Path;

use anyhow::Result;
use codex_core::TranscriptIndex;
use codex_core::TranscriptRunSummary;
use codex_protocol::protocol::SessionSource;
use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use strip_ansi_escapes::strip;
use tempfile::TempDir;

fn codex_command() -> Result<assert_cmd::Command> {
    Ok(assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin(
        "codex",
    )?))
}

#[test]
fn json_lists_built_ins_and_issues() -> Result<()> {
    let workspace = TempDir::new()?;
    let project_dir = workspace.path().join("project");
    let user_dir = workspace.path().join("user");
    let plugin_dir = workspace.path().join("plugin");
    fs::create_dir_all(&project_dir)?;
    fs::create_dir_all(&user_dir)?;
    fs::create_dir_all(&plugin_dir)?;

    write_manifest(
        &project_dir.join("alpha.md"),
        r#"---
id: project-alpha
name: Project Alpha
description: Project manifest
permissionMode: default
---
Project body
"#,
    )?;

    write_manifest(
        &user_dir.join("plan.md"),
        r#"---
id: builtin-plan
name: Custom Plan
description: Overrides built-in plan
permissionMode: plan
---
User body
"#,
    )?;

    write_manifest(
        &plugin_dir.join("plugin.md"),
        r#"---
id: plugin-agent
name: Plugin Agent
description: Plugin manifest
permissionMode: default
---
Plugin body
"#,
    )?;

    // Missing name triggers a validation issue that should surface in CLI output.
    write_manifest(
        &project_dir.join("invalid.md"),
        r#"---
id: invalid-agent
name:
description: Invalid
permissionMode: default
---
Invalid body
"#,
    )?;

    let plugin_arg = format!("demo={}", plugin_dir.display());
    let mut cmd = codex_command()?;
    cmd.current_dir(workspace.path());
    let output = cmd
        .arg("agents")
        .arg("list")
        .arg("--json")
        .arg("--project-dir")
        .arg(&project_dir)
        .arg("--user-dir")
        .arg(&user_dir)
        .arg("--plugin")
        .arg(&plugin_arg)
        .arg("--cli-manifest")
        .arg(r#"{"id":"cli-inline","name":"CLI Agent","description":"CLI","body":"Body"}"#)
        .output()?;
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("Validation issues detected"),
        "stderr should highlight issues, got: {stderr}"
    );
    assert!(
        stderr.contains("invalid.md"),
        "stderr should mention invalid manifest path, got: {stderr}"
    );
    let parsed: JsonValue = serde_json::from_str(stdout.trim())?;
    let manifests = parsed
        .get("manifests")
        .and_then(JsonValue::as_array)
        .expect("manifests array");
    let ids: Vec<_> = manifests
        .iter()
        .map(|manifest| {
            manifest
                .get("id")
                .and_then(JsonValue::as_str)
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        ids,
        vec![
            "project-alpha",
            "cli-inline",
            "builtin-plan",
            "plugin-agent",
            "builtin-general-purpose",
            "builtin-explore",
            "builtin-review"
        ]
    );

    let summary = parsed
        .get("summary")
        .and_then(JsonValue::as_object)
        .expect("summary object");
    assert_eq!(summary.get("custom").and_then(JsonValue::as_u64), Some(4));
    assert_eq!(summary.get("built_in").and_then(JsonValue::as_u64), Some(3));
    assert_eq!(
        summary.get("duplicates").and_then(JsonValue::as_u64),
        Some(1)
    );

    let issues = parsed
        .get("issues")
        .and_then(JsonValue::as_array)
        .expect("issues array");
    assert_eq!(issues.len(), 1);
    let issue = &issues[0];
    let issue_path = issue
        .get("path")
        .and_then(JsonValue::as_str)
        .expect("issue path");
    assert!(
        issue_path.ends_with("invalid.md"),
        "unexpected issue path: {issue_path}"
    );
    let scope = issue
        .get("scope")
        .and_then(JsonValue::as_str)
        .expect("issue scope");
    assert!(
        scope.contains("project"),
        "scope should mention project, got: {scope}"
    );
    assert_eq!(
        issue.get("message").and_then(JsonValue::as_str),
        Some("missing required field: name")
    );

    Ok(())
}

#[test]
fn json_lists_built_ins_without_custom_sources() -> Result<()> {
    let workspace = TempDir::new()?;
    let mut cmd = codex_command()?;
    cmd.current_dir(workspace.path());
    let output = cmd.arg("agents").arg("list").arg("--json").output()?;
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let stderr_plain = String::from_utf8(strip(&output.stderr)?)?;
    assert!(
        stderr_plain.contains("built-in Claude personas"),
        "stderr missing built-in hint: {stderr_plain}"
    );
    assert!(
        stderr_plain.contains("codex --use-subagent"),
        "stderr missing --use-subagent hint: {stderr_plain}"
    );
    let parsed: JsonValue = serde_json::from_str(stdout.trim())?;
    let manifests = parsed
        .get("manifests")
        .and_then(JsonValue::as_array)
        .expect("manifests array");
    let ids: Vec<_> = manifests
        .iter()
        .map(|manifest| {
            manifest
                .get("id")
                .and_then(JsonValue::as_str)
                .expect("manifest id")
                .to_string()
        })
        .collect();
    assert_eq!(
        ids,
        vec![
            "builtin-general-purpose",
            "builtin-plan",
            "builtin-explore",
            "builtin-review"
        ]
    );
    let summary = parsed
        .get("summary")
        .and_then(JsonValue::as_object)
        .expect("summary object");
    assert_eq!(summary.get("custom").and_then(JsonValue::as_u64), Some(0));
    assert_eq!(summary.get("built_in").and_then(JsonValue::as_u64), Some(4));
    Ok(())
}

fn write_manifest(path: &Path, contents: &str) -> Result<()> {
    fs::write(path, contents)?;
    Ok(())
}

#[test]
fn human_list_includes_provider_and_status() -> Result<()> {
    let workspace = TempDir::new()?;
    let project_dir = workspace.path().join("project");
    fs::create_dir_all(&project_dir)?;

    write_manifest(
        &project_dir.join("demo.md"),
        r#"---
id: project-alpha
name: Project Alpha
description: Project manifest
permissionMode: default
tools:
  - read_file
---
Project body
"#,
    )?;

    let mut cmd = codex_command()?;
    cmd.current_dir(workspace.path());
    let output = cmd
        .arg("agents")
        .arg("list")
        .arg("--project-dir")
        .arg(&project_dir)
        .output()?;
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(strip(&output.stdout)?)?;
    assert!(
        stdout.contains("Claude built-in personas are always available"),
        "builtin summary missing in output:\n{stdout}"
    );
    assert!(
        stdout.contains("codex --use-subagent builtin-general-purpose"),
        "builtin activation hint missing:\n{stdout}"
    );
    assert!(
        stdout.contains("Provider: project"),
        "provider missing in output:\n{stdout}"
    );
    assert!(
        stdout.contains("Tools: restricted to read_file"),
        "tool summary missing in output:\n{stdout}"
    );
    assert!(
        stdout.contains("Status: ready - run `codex --use-subagent project-alpha` to activate"),
        "status line missing in output:\n{stdout}"
    );
    Ok(())
}

#[test]
fn register_manifest_then_refreshes_cli_view() -> Result<()> {
    let workspace = TempDir::new()?;
    let project_dir = workspace.path().join("project");
    fs::create_dir_all(&project_dir)?;

    let first = run_agents_list_json(workspace.path(), &project_dir)?;
    let summary = first
        .get("summary")
        .and_then(JsonValue::as_object)
        .expect("summary object");
    assert_eq!(summary.get("custom").and_then(JsonValue::as_u64), Some(0));
    assert!(
        first
            .get("manifests")
            .and_then(JsonValue::as_array)
            .expect("manifests array")
            .iter()
            .all(|manifest| manifest.get("id").and_then(JsonValue::as_str) != Some("project-live"))
    );

    write_manifest(
        &project_dir.join("live.md"),
        r#"---
id: project-live
name: Live Agent
description: Appears after refresh
permissionMode: default
---
Live body
"#,
    )?;

    let second = run_agents_list_json(workspace.path(), &project_dir)?;
    let manifests = second
        .get("manifests")
        .and_then(JsonValue::as_array)
        .expect("manifests array");
    let ids: Vec<_> = manifests
        .iter()
        .filter_map(|manifest| manifest.get("id").and_then(JsonValue::as_str))
        .collect();
    assert!(
        ids.contains(&"project-live"),
        "expected project-live in {ids:?}"
    );
    let refreshed_summary = second
        .get("summary")
        .and_then(JsonValue::as_object)
        .expect("summary object");
    assert_eq!(
        refreshed_summary.get("custom").and_then(JsonValue::as_u64),
        Some(1)
    );

    Ok(())
}

#[test]
fn surfaces_complex_validation_issues() -> Result<()> {
    let workspace = TempDir::new()?;
    let project_dir = workspace.path().join("project");
    fs::create_dir_all(&project_dir)?;

    write_manifest(
        &project_dir.join("invalid-complex.md"),
        r#"---
id: invalid-complex
name: Invalid Complex Agent
description: Exercises complex validation paths
permissionMode: default
tools:
  - shell
  - shell
hooks:
  pre:
    - name: invalid-webhook
      command: echo pre
      endpoint: https://hooks.invalid/pre
triggers:
  - type: keyword
    phrase: high-weight
    weight: 150
---
Invalid body
"#,
    )?;

    let output = run_agents_list_json(workspace.path(), &project_dir)?;
    let issues = output
        .get("issues")
        .and_then(JsonValue::as_array)
        .expect("issues array");
    let messages: Vec<_> = issues
        .iter()
        .filter_map(|issue| issue.get("message").and_then(JsonValue::as_str))
        .collect();
    assert!(
        messages.iter().any(|message| message
            .contains("invalid hook: hook invalid-webhook cannot set command and endpoint")),
        "expected conflicting hook message in {messages:?}"
    );
    assert!(
        messages.iter().any(|message| message
            .contains("invalid trigger high-weight: weight must be between 1 and 100")),
        "expected trigger weight message in {messages:?}"
    );

    Ok(())
}

fn run_agents_list_json(workspace: &Path, project_dir: &Path) -> Result<JsonValue> {
    let mut cmd = codex_command()?;
    cmd.current_dir(workspace);
    let output = cmd
        .arg("agents")
        .arg("list")
        .arg("--json")
        .arg("--project-dir")
        .arg(project_dir)
        .output()?;
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[test]
fn agents_resume_status_lists_runs() -> Result<()> {
    let codex_home = TempDir::new()?;
    let agent_dir = codex_home.path().join("subagents").join("demo");
    let run_dir = agent_dir.join("runs").join("run-1");
    fs::create_dir_all(&run_dir)?;
    let transcript_path = run_dir.join("agent-run-1.jsonl");
    fs::write(&transcript_path, "line\n")?;
    let index = TranscriptIndex {
        runs: vec![TranscriptRunSummary {
            agent_id: "demo".into(),
            run_id: "run-1".into(),
            transcript_path,
            resume_token: "token-123".into(),
            updated_at: "2026-01-12T00:00:00Z".into(),
            event_count: 2,
            session_source: SessionSource::Cli,
            provider: Some("anthropic".into()),
        }],
    };
    fs::create_dir_all(&agent_dir)?;
    fs::write(
        agent_dir.join("index.json"),
        serde_json::to_vec_pretty(&index)?,
    )?;
    let mut cmd = codex_command()?;
    cmd.env("CODEX_HOME", codex_home.path());
    let output = cmd
        .arg("agents")
        .arg("resume-status")
        .arg("--json")
        .output()?;
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let payload: JsonValue = serde_json::from_str(&stdout)?;
    let agents = payload
        .as_array()
        .expect("resume-status JSON payload should be array");
    assert_eq!(agents.len(), 1, "expected one agent record: {stdout}");
    let runs = agents[0]
        .get("runs")
        .and_then(serde_json::Value::as_array)
        .expect("runs array missing");
    assert_eq!(runs.len(), 1, "expected a single run entry: {stdout}");
    let run = &runs[0];
    assert_eq!(run.get("agent_id").and_then(|v| v.as_str()), Some("demo"));
    assert_eq!(run.get("run_id").and_then(|v| v.as_str()), Some("run-1"));
    assert_eq!(
        run.get("resume_token").and_then(|v| v.as_str()),
        Some("token-123")
    );
    assert_eq!(
        run.get("provider").and_then(|v| v.as_str()),
        Some("anthropic")
    );
    Ok(())
}

#[test]
fn agents_resume_status_json_handles_empty_dir() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut cmd = codex_command()?;
    cmd.env("CODEX_HOME", codex_home.path());
    let output = cmd
        .arg("agents")
        .arg("resume-status")
        .arg("--json")
        .output()?;
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let payload: JsonValue = serde_json::from_str(stdout.trim())?;
    assert_eq!(payload, JsonValue::Array(Vec::new()));
    Ok(())
}
