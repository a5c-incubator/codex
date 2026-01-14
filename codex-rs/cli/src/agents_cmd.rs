//! CLI helpers for discovering Claude-compatible subagents.
//!
//! The long-term goal is to shell everything through `codex-core`'s agent
//! registry, but for now we expose a lightweight `codex agents list` command
//! that leverages the shared `codex-subagent` loader and honor the discovery
//! scopes described in `docs/subagents/architecture.md`.

use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use codex_core::TranscriptIndex;
use codex_core::TranscriptRunSummary;
use codex_core::agent::AgentRegistry;
use codex_core::agent::RefreshInvocation;
use codex_core::agent::RefreshIssue;
use codex_core::agent::RefreshOutcome;
use codex_core::agent::RefreshReport;
use codex_core::config::find_codex_home;
use codex_exec::subagent_args::DiscoveryTargetArgs;
use codex_exec::subagent_args::PluginDirArg;
use codex_exec::subagent_args::SubagentOverrideInput;
use codex_exec::subagent_args::build_discovery_targets;
use codex_exec::subagent_args::parse_plugin_dir;
use codex_exec::subagent_args::parse_subagent_overrides;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_subagent::DiscoveryScope;
use codex_subagent::FsManifestLoader;
use codex_subagent::ManifestLoader;
use codex_subagent::PermissionMode;
use owo_colors::OwoColorize;
use serde::Serialize;

/// Entry point for `codex agents ...`.
#[derive(Debug, Parser)]
pub struct AgentsCli {
    #[command(subcommand)]
    command: AgentsSubcommand,
}

#[derive(Debug, Subcommand)]
enum AgentsSubcommand {
    /// List discovered manifests with their effective priority, source, and built-in personas.
    List(AgentsListArgs),

    /// Show recorded subagent transcripts and resume tokens.
    ResumeStatus(AgentsResumeStatusArgs),
}

#[derive(Debug, Args)]
struct AgentsListArgs {
    /// Output JSON instead of human-readable text.
    #[arg(long)]
    json: bool,

    /// Override the project-level manifest directory (defaults to ./\.claude/agents).
    #[arg(long)]
    project_dir: Option<PathBuf>,

    /// Override the user-level manifest directory (defaults to ~/.claude/agents).
    #[arg(long)]
    user_dir: Option<PathBuf>,

    /// Add a plugin-provided manifest directory (format: plugin_id=path).
    #[arg(long = "plugin", value_parser = parse_plugin_dir)]
    plugin_dirs: Vec<PluginDirArg>,

    /// Provide an inline CLI manifest JSON payload (repeatable).
    #[arg(long = "cli-manifest", value_name = "JSON")]
    cli_manifest: Vec<String>,

    /// Provide a path to a CLI manifest JSON payload (repeatable).
    #[arg(long = "cli-manifest-file", value_name = "PATH")]
    cli_manifest_file: Vec<PathBuf>,
}

#[derive(Debug, Args)]
struct AgentsResumeStatusArgs {
    /// Output JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

/// Execute the selected agents command.
pub fn run(cli: AgentsCli) -> Result<()> {
    match cli.command {
        AgentsSubcommand::List(args) => list_agents(args),
        AgentsSubcommand::ResumeStatus(args) => resume_status(args),
    }
}

fn list_agents(args: AgentsListArgs) -> Result<()> {
    let cwd = env::current_dir()?;
    let overrides = parse_subagent_overrides(&SubagentOverrideInput {
        cli_manifests: &args.cli_manifest,
        cli_manifest_files: &args.cli_manifest_file,
        plugin_dirs: &args.plugin_dirs,
    })?;
    let targets = build_discovery_targets(&DiscoveryTargetArgs {
        cwd: &cwd,
        project_dir_override: args.project_dir.as_deref(),
        user_dir_override: args.user_dir.as_deref(),
        overrides: &overrides,
    })?;
    let missing_sources = targets.is_empty();
    let loader: Arc<dyn ManifestLoader> = Arc::new(FsManifestLoader::new());
    let mut registry = AgentRegistry::new(loader);
    let invocation = if args.json {
        RefreshInvocation::CliListJson
    } else {
        RefreshInvocation::CliListHuman
    };
    let outcome = match registry.refresh_with_telemetry(&targets, None, invocation) {
        Ok(outcome) => {
            log_agents_list_invocation(invocation, &outcome.report, &outcome.issues);
            outcome
        }
        Err(err) => {
            let message = err.to_string();
            log_agents_list_failure(invocation, &message);
            return Err(err.into());
        }
    };

    let built_in_ids = built_in_manifest_ids(&registry);

    if args.json {
        print_json(&registry, &outcome)?;
        eprint_issues(&outcome.issues);
        if missing_sources {
            emit_builtin_hint(&built_in_ids, |line| eprintln!("{line}"));
        }
        return Ok(());
    }

    if missing_sources {
        emit_builtin_hint(&built_in_ids, |line| println!("{line}"));
    }

    let manifests: Vec<_> = registry.manifests().collect();
    print_builtin_summary(&manifests);
    print_human(&manifests);
    print_issues(&outcome.issues);
    print_summary(&outcome.report);
    Ok(())
}

fn resume_status(args: AgentsResumeStatusArgs) -> Result<()> {
    let codex_home = find_codex_home()?;
    let subagents_dir = codex_home.join("subagents");
    let mut records = if subagents_dir.exists() {
        collect_resume_indices(&subagents_dir)?
    } else {
        Vec::new()
    };
    if args.json {
        #[derive(Serialize)]
        struct AgentResumeStatus<'a> {
            agent_id: &'a str,
            runs: &'a [TranscriptRunSummary],
        }
        let payload: Vec<_> = records
            .iter()
            .map(|(agent_id, index)| AgentResumeStatus {
                agent_id,
                runs: &index.runs,
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    if !subagents_dir.exists() || records.is_empty() {
        println!("{}", "No subagent transcripts recorded yet.".yellow());
        return Ok(());
    }
    records.sort_by(|a, b| a.0.cmp(&b.0));
    for (agent_id, index) in records {
        println!(
            "{} ({})",
            agent_id.green(),
            format!("{} run(s)", index.runs.len()).dimmed()
        );
        if index.runs.is_empty() {
            println!("  {}", "No runs recorded.".yellow());
            println!();
            continue;
        }
        for run in &index.runs {
            println!(
                "  {} {} | updated {} | events {}",
                "run".dimmed(),
                run.run_id.cyan(),
                run.updated_at,
                run.event_count
            );
            println!(
                "    scope: {} | provider: {}",
                describe_session_source(&run.session_source).dimmed(),
                describe_provider_scope(run).dimmed()
            );
            println!("    transcript: {}", run.transcript_path.display());
            println!("    resume: {}", run.resume_token);
        }
        println!();
    }
    Ok(())
}

fn collect_resume_indices(dir: &Path) -> Result<Vec<(String, TranscriptIndex)>> {
    let mut records = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let agent_id = entry.file_name().to_string_lossy().to_string();
        let index_path = entry.path().join("index.json");
        if !index_path.exists() {
            continue;
        }
        let data = fs::read_to_string(&index_path)?;
        let index: TranscriptIndex = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse {}", index_path.display()))?;
        records.push((agent_id, index));
    }
    Ok(records)
}

fn describe_provider_scope(run: &TranscriptRunSummary) -> String {
    run.provider
        .as_deref()
        .map_or_else(|| "session default".to_string(), str::to_string)
}

fn describe_session_source(source: &SessionSource) -> String {
    match source {
        SessionSource::SubAgent(sub) => format!("subagent/{}", describe_subagent_source(sub)),
        other => other.to_string(),
    }
}

fn describe_subagent_source(source: &SubAgentSource) -> String {
    match source {
        SubAgentSource::Review => "review".into(),
        SubAgentSource::Compact => "compact".into(),
        SubAgentSource::GeneralPurpose => "general-purpose".into(),
        SubAgentSource::Plan => "plan".into(),
        SubAgentSource::Explore => "explore".into(),
        SubAgentSource::Other(id) => id.clone(),
    }
}

fn print_builtin_summary(manifests: &[Arc<codex_subagent::AgentManifest>]) {
    let built_ins: Vec<_> = manifests
        .iter()
        .map(Arc::as_ref)
        .filter(|manifest| {
            matches!(
                manifest.source.as_ref(),
                Some(DiscoveryScope::BuiltIn { .. })
            )
        })
        .collect();
    if built_ins.is_empty() {
        return;
    }
    println!(
        "{}",
        "Claude built-in personas are always available:".magenta()
    );
    for manifest in built_ins {
        println!(
            "  - {} ({}) -> run `codex --use-subagent {}` or `:use-agent {}`",
            manifest.name.as_str().green(),
            manifest.id.as_str().dimmed(),
            manifest.id.as_str(),
            manifest.id.as_str()
        );
    }
    println!(
        "{}",
        "Use `codex agents resume-status` to inspect their transcripts under ~/.codex/subagents/builtin-*."
            .dimmed()
    );
    println!();
}

fn print_human(manifests: &[Arc<codex_subagent::AgentManifest>]) {
    if manifests.is_empty() {
        println!(
            "{}",
            "No subagents discovered. Add manifests under .claude/agents or ~/.claude/agents."
                .yellow()
        );
        return;
    }

    for manifest in manifests {
        println!(
            "{} ({})",
            manifest.name.as_str().green(),
            manifest.id.as_str().dimmed()
        );
        println!("  Description: {}", manifest.description);
        println!("  Priority: {}", manifest.priority().label());
        println!("  Provider: {}", provider_label(manifest));
        if let Some(model) = &manifest.model {
            println!("  Model: {}", model.as_str());
        } else {
            println!("  Model: inherit session default");
        }
        println!("  Tools: {}", tool_summary(manifest));
        println!(
            "  Permission: {}",
            permission_label(&manifest.permission_mode)
        );
        if !manifest.skills.is_empty() {
            println!("  Skills: {}", manifest.skills.join(", "));
        }
        if !manifest.triggers.is_empty() {
            println!(
                "  Triggers: {}",
                manifest
                    .triggers
                    .iter()
                    .map(trigger_label)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        println!(
            "  Status: {}",
            format!(
                "ready - run `codex --use-subagent {}` to activate",
                manifest.id.as_str()
            )
            .cyan()
        );
        println!();
    }
}

fn provider_label(manifest: &codex_subagent::AgentManifest) -> String {
    if let Some(scope) = &manifest.source {
        describe_scope(scope)
    } else {
        "unknown".into()
    }
}

fn tool_summary(manifest: &codex_subagent::AgentManifest) -> String {
    match manifest.tool_scope.as_slice() {
        Some(tools) if !tools.is_empty() => {
            let list = tools
                .iter()
                .map(codex_subagent::ToolName::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            format!("restricted to {list}")
        }
        _ => "inherit parent session tools".into(),
    }
}

fn print_issues(issues: &[RefreshIssue]) {
    emit_issues(issues, |line| println!("{line}"));
}

fn eprint_issues(issues: &[RefreshIssue]) {
    emit_issues(issues, |line| eprintln!("{line}"));
}

fn emit_issues<F>(issues: &[RefreshIssue], mut emit: F)
where
    F: FnMut(String),
{
    if issues.is_empty() {
        return;
    }

    emit("Validation issues detected:".yellow().to_string());
    for issue in issues {
        let mut details = Vec::new();
        if let Some(path) = issue.path_label() {
            details.push(path);
        }
        if let Some(scope) = issue.scope_label() {
            details.push(scope);
        }
        let location = if details.is_empty() {
            "unknown manifest".into()
        } else {
            details.join(" | ")
        };
        emit(format!(
            "  - {}: {}",
            location.dimmed(),
            issue.message.red()
        ));
    }
    emit(String::new());
}

fn print_summary(report: &RefreshReport) {
    let custom = report.custom_manifests();
    let built_ins = report.built_in_manifests;
    let duplicates = report.skipped_duplicates;
    println!(
        "{}",
        format!(
            "Discovered {custom} custom manifests + {built_ins} built-ins ({duplicates} duplicate IDs skipped)."
        )
        .green()
    );
}

fn built_in_manifest_ids(registry: &AgentRegistry) -> Vec<String> {
    registry
        .manifests()
        .filter_map(|manifest| {
            if matches!(
                manifest.source.as_ref(),
                Some(DiscoveryScope::BuiltIn { .. })
            ) {
                Some(manifest.id.as_str().to_string())
            } else {
                None
            }
        })
        .collect()
}

fn emit_builtin_hint<F>(built_in_ids: &[String], mut emit: F)
where
    F: FnMut(String),
{
    emit(
        "No manifest sources detected; built-in Claude personas are ready out of the box."
            .yellow()
            .to_string(),
    );
    if built_in_ids.is_empty() {
        emit(
            "Add manifests under .claude/agents or pass --project-dir/--user-dir to hide this notice."
                .dimmed()
                .to_string(),
        );
        return;
    }
    let list = built_in_ids.join(", ");
    emit(format!("Available built-ins: {list}.").dimmed().to_string());
    let example = built_in_ids
        .first()
        .map_or("builtin-general-purpose", String::as_str);
    emit(
        format!(
            "Activate one with `codex --use-subagent <id>` (e.g. `codex --use-subagent {example}`) \
or run `:use-agent <id>` in the TUI."
        )
        .dimmed()
        .to_string(),
    );
}

fn describe_scope(scope: &DiscoveryScope) -> String {
    match scope {
        DiscoveryScope::Project { path } => format!("project ({})", path.display()),
        DiscoveryScope::CliJson { label } => label.clone().unwrap_or_else(|| "cli".into()),
        DiscoveryScope::User { path } => format!("user ({})", path.display()),
        DiscoveryScope::Plugin { plugin_id, .. } => format!("plugin: {}", plugin_id.as_str()),
        DiscoveryScope::BuiltIn { agent } => format!("built-in ({agent:?})"),
    }
}

fn log_agents_list_invocation(
    invocation: RefreshInvocation,
    report: &RefreshReport,
    issues: &[RefreshIssue],
) {
    let scopes = report.scope_breakdown;
    tracing::event!(
        tracing::Level::INFO,
        event.name = "codex.agents_list_invocation",
        invocation = invocation.as_str(),
        status = "success",
        total = report.total_manifests,
        built_in = report.built_in_manifests,
        custom = report.custom_manifests(),
        duplicates = report.skipped_duplicates,
        scope.project = scopes.project,
        scope.cli = scopes.cli,
        scope.user = scopes.user,
        scope.plugin = scopes.plugin,
        scope.built_in = scopes.built_in,
        scope.unknown = scopes.unknown,
        issues = issues.len(),
    );
}

fn log_agents_list_failure(invocation: RefreshInvocation, error: &str) {
    tracing::event!(
        tracing::Level::WARN,
        event.name = "codex.agents_list_invocation",
        invocation = invocation.as_str(),
        status = "error",
        error.message = error,
    );
}

fn permission_label(mode: &PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::DontAsk => "dontAsk",
        PermissionMode::BypassPermissions => "bypassPermissions",
        PermissionMode::Plan => "plan",
        PermissionMode::Ignore => "ignore",
    }
}

fn trigger_label(trigger: &codex_subagent::TriggerDefinition) -> String {
    match trigger {
        codex_subagent::TriggerDefinition::Keyword { phrase, weight } => {
            format!("keyword:{phrase} ({weight})")
        }
        codex_subagent::TriggerDefinition::Glob { pattern, weight } => {
            format!("glob:{pattern} ({weight})")
        }
    }
}

fn print_json(registry: &AgentRegistry, outcome: &RefreshOutcome) -> Result<()> {
    #[derive(Serialize)]
    struct AgentsListJson {
        manifests: Vec<codex_subagent::AgentManifest>,
        summary: JsonSummary,
        issues: Vec<JsonIssue>,
    }

    #[derive(Serialize)]
    struct JsonSummary {
        custom: usize,
        built_in: usize,
        duplicates: usize,
    }

    #[derive(Serialize)]
    struct JsonIssue {
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        message: String,
    }

    let manifests: Vec<_> = registry
        .manifests()
        .map(|manifest| (*manifest).clone())
        .collect();
    let report = outcome.report;
    let payload = AgentsListJson {
        manifests,
        summary: JsonSummary {
            custom: report.custom_manifests(),
            built_in: report.built_in_manifests,
            duplicates: report.skipped_duplicates,
        },
        issues: outcome
            .issues
            .iter()
            .map(|issue| JsonIssue {
                path: issue.path_label(),
                scope: issue.scope_label(),
                message: issue.message.clone(),
            })
            .collect(),
    };
    let json = serde_json::to_string_pretty(&payload)?;
    println!("{json}");
    Ok(())
}
