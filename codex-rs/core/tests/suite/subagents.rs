#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use async_channel::Sender as AsyncSender;
use codex_core::agent::AgentRegistry;
use codex_core::agent::AgentRegistryWatch;
use codex_core::agent::AgentRegistryWatchConfig;
use codex_core::agent::AgentRegistryWatchTryRecvError;
use codex_core::agent::RefreshInvocation;
use codex_core::agent::RegistryEvent;
use codex_core::agent::RegistryEventKind;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_core::protocol::SubagentLifecycleEvent;
use codex_core::protocol::SubagentLifecyclePhase;
use codex_core::protocol::SubagentOverride;
use codex_core::protocol::SubagentToolScopeMode;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_subagent::AgentKind;
use codex_subagent::AgentManifest;
use codex_subagent::CliManifestOverride;
use codex_subagent::DiscoveryTarget;
use codex_subagent::FsManifestLoader;
use codex_subagent::HookSet;
use codex_subagent::LoadOutcome;
use codex_subagent::LoaderEvent;
use codex_subagent::LoaderWatch;
use codex_subagent::ManifestError;
use codex_subagent::ManifestLoader;
use codex_subagent::PermissionMode;
use codex_subagent::PluginDirArg;
use codex_subagent::PluginId;
use codex_subagent::SubagentDiscoveryOverrides;
use codex_subagent::ToolScope;
use codex_subagent::compute_digest;
use core_test_support::format_with_current_shell;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::oneshot;

const TOOL_GATING_AGENT: &str = "gate-agent";
const HOOK_AGENT: &str = "hook-agent";
const LIFECYCLE_AGENT: &str = "lifecycle-agent";
const SWITCH_PRIMARY_AGENT: &str = "switch-primary";
const SWITCH_SECONDARY_AGENT: &str = "switch-secondary";
const TRANSCRIPT_AGENT: &str = "transcript-agent";

#[test]
fn registry_watch_coalesces_events() -> Result<()> {
    let loader = Arc::new(WatchTestLoader::new(vec![test_manifest("alpha")]));
    let mut registry = AgentRegistry::new(loader.clone());
    registry.refresh(&[])?;
    let shared = Arc::new(RwLock::new(registry));
    let targets = vec![dummy_scope()];
    let mut watch = AgentRegistry::start_watch(
        Arc::clone(&shared),
        targets,
        None,
        AgentRegistryWatchConfig {
            debounce: Duration::from_millis(40),
            idle_poll_interval: Duration::from_millis(5),
        },
    )?;

    loader.emit(dummy_scope());
    loader.emit(dummy_scope());

    let event = wait_for_registry_event(&watch, Duration::from_secs(1))
        .expect("watch yielded refresh event");
    assert_eq!(event.invocation, RefreshInvocation::Watch);
    assert_eq!(
        event.scopes.len(),
        1,
        "scopes should dedupe during debounce"
    );
    match event.kind {
        RegistryEventKind::RefreshSuccess { .. } => {}
        other => panic!("expected refresh success, got {other:?}"),
    }
    assert_eq!(loader.load_count(), 2, "initial + watch refreshes");

    watch.close();
    Ok(())
}

#[test]
fn registry_watch_stops_when_handle_is_dropped() -> Result<()> {
    let loader = Arc::new(WatchTestLoader::new(vec![test_manifest("alpha")]));
    let mut registry = AgentRegistry::new(loader.clone());
    registry.refresh(&[])?;
    let shared = Arc::new(RwLock::new(registry));
    let watch = AgentRegistry::start_watch(
        Arc::clone(&shared),
        vec![dummy_scope()],
        None,
        AgentRegistryWatchConfig::default(),
    )?;
    drop(watch);

    loader.emit(dummy_scope());
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        loader.load_count(),
        1,
        "watch thread should exit after drop"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_tool_scope_blocks_disallowed_tools() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = core_test_support::responses::start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        write_manifest(
            &config.cwd,
            TOOL_GATING_AGENT,
            r#"
id: gate-agent
name: Tool Gate
description: Blocks shell invocations
permissionMode: default
tools:
  - read_file
hooks:
  pre: []
  post: []
  stop: []
body: |
  Keep responses short.
"#
            .to_string(),
        );
    });

    let test = builder.build(&server).await?;
    activate_subagent(&test, TOOL_GATING_AGENT).await?;

    let call_id = "blocked-shell";
    let args = json!({
        "command": ["/bin/echo", "blocked"],
        "timeout_ms": 1_000,
    });
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "shell", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let output_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_turn("run the shell command").await?;

    let (content, success) = output_mock
        .single_request()
        .function_call_output_content_and_success(call_id)
        .expect("function call output present");
    assert_eq!(success, Some(false));
    assert_eq!(
        content.unwrap_or_default(),
        "tool shell is not allowed for the active subagent"
    );

    let header = output_mock.single_request().header("x-openai-subagent");
    assert_eq!(header.as_deref(), Some(TOOL_GATING_AGENT));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_persists_transcript() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = core_test_support::responses::start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        write_manifest(
            &config.cwd,
            TRANSCRIPT_AGENT,
            r#"
id: transcript-agent
name: Transcript Agent
description: Writes transcripts
permissionMode: default
hooks:
  pre: []
  post: []
  stop: []
body: |
  Respond briefly.
"#
            .to_string(),
        );
    });

    let test = builder.build(&server).await?;
    activate_subagent(&test, TRANSCRIPT_AGENT).await?;

    let _output_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "hello from transcript"),
            ev_completed("resp-1"),
        ]),
    )
    .await;

    test.submit_turn("say hi").await?;
    test.codex
        .submit(Op::OverrideTurnContext {
            cwd: None,
            approval_policy: None,
            sandbox_policy: None,
            model: None,
            effort: None,
            summary: None,
            subagent: Some(SubagentOverride::Clear { origin: None }),
        })
        .await?;
    wait_for_event(&test.codex, |ev| {
        matches!(
            ev,
            EventMsg::SubagentLifecycle(event)
                if event.phase == SubagentLifecyclePhase::Stopped
                    && event.agent_id == TRANSCRIPT_AGENT
        )
    })
    .await;

    let agent_root = test
        .codex_home_path()
        .join("subagents")
        .join(TRANSCRIPT_AGENT);
    assert!(
        agent_root.exists(),
        "expected transcript directory at {}",
        agent_root.display()
    );
    println!("agent root: {}", agent_root.display());
    let index_path = agent_root.join("index.json");
    println!("index path before read: {}", index_path.display());
    let index_data = fs::read_to_string(&index_path)?;
    let index: codex_core::TranscriptIndex = serde_json::from_str(&index_data)?;
    assert!(
        !index.runs.is_empty(),
        "transcript index should include at least one run"
    );
    let run = &index.runs[0];
    assert_eq!(run.agent_id, TRANSCRIPT_AGENT);
    let resume_token = codex_core::SubagentResumeToken::decode(&run.resume_token)?;
    assert_eq!(resume_token.agent_id, TRANSCRIPT_AGENT);
    assert_eq!(resume_token.transcript_path, run.transcript_path);
    let run_dir = run
        .transcript_path
        .parent()
        .expect("transcript path has run dir")
        .to_path_buf();
    let run_resume_path = run_dir.join("resume.token");
    assert!(
        run_resume_path.exists(),
        "expected run-scoped resume token at {}",
        run_resume_path.display()
    );
    let run_resume_contents = fs::read_to_string(&run_resume_path)?;
    assert_eq!(run_resume_contents, run.resume_token);
    let run_index_path = run_dir.join("index.json");
    assert!(
        run_index_path.exists(),
        "expected run-scoped index at {}",
        run_index_path.display()
    );
    let run_index_data = fs::read_to_string(&run_index_path)?;
    let run_index: codex_core::TranscriptRunSummary = serde_json::from_str(&run_index_data)?;
    pretty_assertions::assert_eq!(run_index.run_id, run.run_id);
    pretty_assertions::assert_eq!(run_index.agent_id, run.agent_id);
    pretty_assertions::assert_eq!(run_index.resume_token, run.resume_token);
    let transcript_contents = fs::read_to_string(&run.transcript_path)?;
    assert!(
        transcript_contents.contains("\"subagent_lifecycle\""),
        "expected lifecycle events in transcript: {transcript_contents}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn built_in_subagents_activate_without_manifests() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = core_test_support::responses::start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build(&server).await?;
    let codex_home = test.codex_home_path().to_path_buf();

    let built_ins = vec![
        (
            "builtin-general-purpose",
            SessionSource::SubAgent(SubAgentSource::GeneralPurpose),
        ),
        (
            "builtin-plan",
            SessionSource::SubAgent(SubAgentSource::Plan),
        ),
        (
            "builtin-explore",
            SessionSource::SubAgent(SubAgentSource::Explore),
        ),
        (
            "builtin-review",
            SessionSource::SubAgent(SubAgentSource::Review),
        ),
    ];

    for (agent_id, expected_source) in built_ins {
        activate_subagent(&test, agent_id).await?;
        let activated =
            wait_for_lifecycle_event(&test, agent_id, SubagentLifecyclePhase::Activated).await;
        pretty_assertions::assert_eq!(
            activated.session_source.as_ref(),
            Some(&expected_source),
            "activated lifecycle event should report session source for {agent_id}"
        );

        test.codex
            .submit(Op::OverrideTurnContext {
                cwd: None,
                approval_policy: None,
                sandbox_policy: None,
                model: None,
                effort: None,
                summary: None,
                subagent: Some(SubagentOverride::Clear { origin: None }),
            })
            .await?;

        let stopped =
            wait_for_lifecycle_event(&test, agent_id, SubagentLifecyclePhase::Stopped).await;
        pretty_assertions::assert_eq!(
            stopped.session_source.as_ref(),
            Some(&expected_source),
            "stopped lifecycle event should report session source for {agent_id}"
        );
        let resume_token = stopped
            .resume_token
            .as_deref()
            .expect("built-in stop events should include resume tokens");

        assert_resume_artifacts(&codex_home, agent_id, &expected_source, resume_token)?;
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_hooks_fire_and_stop_on_clear() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let hook_server = HookServer::start().await;
    let server = core_test_support::responses::start_mock_server().await;
    let base = hook_server.base_url();

    let mut builder = test_codex().with_config(move |config| {
        write_manifest(
            &config.cwd,
            HOOK_AGENT,
            format!(
                r#"
id: hook-agent
name: Hook Recorder
description: Captures hook payloads
permissionMode: default
tools:
  - shell
hooks:
  pre:
    - name: pre
      endpoint: {base}/pre
  post:
    - name: post
      endpoint: {base}/post
  stop:
    - name: stop
      endpoint: {base}/stop
body: |
  Always run the requested command.
"#
            ),
        );
    });

    let test = builder.build(&server).await?;
    activate_subagent(&test, HOOK_AGENT).await?;

    let call_id = "hook-shell";
    let command = format_with_current_shell("echo hook success");
    let args = json!({
        "command": command,
        "timeout_ms": 1_000,
    });
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "shell", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let completion_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_turn("run the hook command").await?;

    let header = completion_mock.single_request().header("x-openai-subagent");
    assert_eq!(header.as_deref(), Some(HOOK_AGENT));

    let events = hook_server.events();
    assert_eq!(events.len(), 2, "expected pre + post hook payloads");

    assert_hook_phase(&events[0], "pre_tool_use", None);
    assert_hook_phase(&events[1], "post_tool_use", Some(true));

    test.codex
        .submit(Op::OverrideTurnContext {
            cwd: None,
            approval_policy: None,
            sandbox_policy: None,
            model: None,
            effort: None,
            summary: None,
            subagent: Some(SubagentOverride::Clear { origin: None }),
        })
        .await?;
    wait_for_event(&test.codex, |ev| {
        matches!(
            ev,
            EventMsg::SubagentLifecycle(event)
                if event.phase == SubagentLifecyclePhase::Stopped
                    && event.agent_id == HOOK_AGENT
        )
    })
    .await;

    let events = hook_server.events();
    assert_eq!(
        events.len(),
        3,
        "expected stop hook payload after clearing runtime"
    );
    assert_hook_phase(&events[2], "stop", None);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_lifecycle_events_include_metadata() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = core_test_support::responses::start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        write_manifest(
            &config.cwd,
            LIFECYCLE_AGENT,
            r#"
id: lifecycle-agent
name: Lifecycle Events
description: Emits lifecycle telemetry
permissionMode: bypassPermissions
hooks:
  pre: []
  post: []
  stop: []
body: |
  respond normally.
"#
            .to_string(),
        );
    });

    let test = builder.build(&server).await?;
    let manifest_on_disk = manifest_path(test.cwd_path(), LIFECYCLE_AGENT);
    let expected_digest = compute_digest(&fs::read(&manifest_on_disk)?);
    let expected_policy = AskForApproval::Never;

    activate_subagent(&test, LIFECYCLE_AGENT).await?;
    let activated = wait_for_event(&test.codex, |ev| {
        matches!(
            ev,
            EventMsg::SubagentLifecycle(event) if event.phase == SubagentLifecyclePhase::Activated
        )
    })
    .await;
    if let EventMsg::SubagentLifecycle(event) = activated {
        assert_eq!(event.agent_id, LIFECYCLE_AGENT);
        assert_eq!(event.phase, SubagentLifecyclePhase::Activated);
        assert_eq!(event.agent_name.as_deref(), Some("Lifecycle Events"));
        assert_eq!(
            event.manifest_digest.as_deref(),
            Some(expected_digest.as_str())
        );
        assert_eq!(
            event.model.as_deref(),
            Some(test.session_configured.model.as_str())
        );
        let scope = event
            .tool_scope
            .as_ref()
            .expect("tool scope summary should be present");
        assert_eq!(scope.mode, SubagentToolScopeMode::Inherit);
        assert_eq!(event.approval_policy, expected_policy);
    } else {
        panic!("expected SubagentLifecycle event");
    }

    test.codex
        .submit(Op::OverrideTurnContext {
            cwd: None,
            approval_policy: None,
            sandbox_policy: None,
            model: None,
            effort: None,
            summary: None,
            subagent: Some(SubagentOverride::Clear { origin: None }),
        })
        .await?;

    let stopped = wait_for_event(&test.codex, |ev| {
        matches!(
            ev,
            EventMsg::SubagentLifecycle(event) if event.phase == SubagentLifecyclePhase::Stopped
        )
    })
    .await;
    if let EventMsg::SubagentLifecycle(event) = stopped {
        assert_eq!(event.agent_id, LIFECYCLE_AGENT);
        assert_eq!(event.phase, SubagentLifecyclePhase::Stopped);
        assert_eq!(event.agent_name.as_deref(), Some("Lifecycle Events"));
        assert_eq!(
            event.manifest_digest.as_deref(),
            Some(expected_digest.as_str())
        );
        assert_eq!(
            event.model.as_deref(),
            Some(test.session_configured.model.as_str())
        );
        let scope = event
            .tool_scope
            .as_ref()
            .expect("tool scope summary should be present");
        assert_eq!(scope.mode, SubagentToolScopeMode::Inherit);
        assert_eq!(event.approval_policy, expected_policy);
    } else {
        panic!("expected SubagentLifecycle stop event");
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_discovery_overrides_emit_manifest_digests() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = core_test_support::responses::start_mock_server().await;
    let cli_manifest = json!({
        "id": "cli-override-agent",
        "name": "CLI Override Agent",
        "description": "Injected via CLI overrides",
        "permissionMode": "default",
        "hooks": {
            "pre": [],
            "post": [],
            "stop": []
        },
        "body": "Answer using the CLI manifest."
    });
    let cli_manifest_bytes =
        serde_json::to_vec(&cli_manifest).expect("serialize CLI manifest for digest");
    let cli_digest = compute_digest(&cli_manifest_bytes);

    let plugin_manifest = r#"
id: plugin-override-agent
name: Plugin Override Agent
description: Loaded from plugin directory overrides
permissionMode: default
hooks:
  pre: []
  post: []
  stop: []
body: |
  Respond using the plugin manifest.
"#;
    let plugin_digest = compute_digest(plugin_manifest.as_bytes());

    let mut builder = test_codex().with_config({
        let cli_manifest = cli_manifest.clone();
        let plugin_manifest = plugin_manifest.to_string();
        move |config| {
            let plugin_dir = config.cwd.join("plugin_overrides");
            fs::create_dir_all(&plugin_dir).expect("create plugin override dir");
            let manifest_path = plugin_dir.join("plugin-override-agent.yaml");
            fs::write(&manifest_path, &plugin_manifest).expect("write plugin manifest");

            config.subagent_discovery_overrides = Some(SubagentDiscoveryOverrides {
                cli_manifests: vec![CliManifestOverride {
                    manifest: cli_manifest,
                    label: Some("inline-cli".into()),
                }],
                plugin_dirs: vec![PluginDirArg {
                    id: PluginId::new("plugin-source"),
                    path: plugin_dir,
                }],
            });
        }
    });

    let test = builder.build(&server).await?;

    activate_subagent(&test, "cli-override-agent").await?;
    let cli_event = wait_for_lifecycle_event(
        &test,
        "cli-override-agent",
        SubagentLifecyclePhase::Activated,
    )
    .await;
    pretty_assertions::assert_eq!(
        cli_event.manifest_digest.as_deref(),
        Some(cli_digest.as_str()),
        "CLI override manifest digest should match compute_digest"
    );
    test.codex
        .submit(Op::OverrideTurnContext {
            cwd: None,
            approval_policy: None,
            sandbox_policy: None,
            model: None,
            effort: None,
            summary: None,
            subagent: Some(SubagentOverride::Clear { origin: None }),
        })
        .await?;
    wait_for_lifecycle_event(&test, "cli-override-agent", SubagentLifecyclePhase::Stopped).await;

    activate_subagent(&test, "plugin-override-agent").await?;
    let plugin_event = wait_for_lifecycle_event(
        &test,
        "plugin-override-agent",
        SubagentLifecyclePhase::Activated,
    )
    .await;
    pretty_assertions::assert_eq!(
        plugin_event.manifest_digest.as_deref(),
        Some(plugin_digest.as_str()),
        "plugin override manifest digest should match compute_digest"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_switch_emits_stop_before_next_activation() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = core_test_support::responses::start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        write_manifest(
            &config.cwd,
            SWITCH_PRIMARY_AGENT,
            r#"
id: switch-primary
name: Primary Agent
description: First agent activated
permissionMode: default
hooks:
  pre: []
  post: []
  stop: []
body: |
  respond normally.
"#
            .to_string(),
        );
        write_manifest(
            &config.cwd,
            SWITCH_SECONDARY_AGENT,
            r#"
id: switch-secondary
name: Secondary Agent
description: Second agent activated
permissionMode: default
hooks:
  pre: []
  post: []
  stop: []
body: |
  respond normally.
"#
            .to_string(),
        );
    });

    let test = builder.build(&server).await?;
    activate_subagent(&test, SWITCH_PRIMARY_AGENT).await?;
    wait_for_event(&test.codex, |ev| {
        matches!(
            ev,
            EventMsg::SubagentLifecycle(event)
                if event.phase == SubagentLifecyclePhase::Activated
                    && event.agent_id == SWITCH_PRIMARY_AGENT
        )
    })
    .await;

    test.codex
        .submit(Op::OverrideTurnContext {
            cwd: None,
            approval_policy: None,
            sandbox_policy: None,
            model: None,
            effort: None,
            summary: None,
            subagent: Some(SubagentOverride::Activate {
                id: SWITCH_SECONDARY_AGENT.to_string(),
                resume: None,
                origin: None,
            }),
        })
        .await?;

    let first = wait_for_event(&test.codex, |ev| {
        matches!(ev, EventMsg::SubagentLifecycle(_))
    })
    .await;
    let second = wait_for_event(&test.codex, |ev| {
        matches!(ev, EventMsg::SubagentLifecycle(_))
    })
    .await;

    let first_event = match first {
        EventMsg::SubagentLifecycle(event) => event,
        _ => unreachable!("predicate only matches lifecycle events"),
    };
    let second_event = match second {
        EventMsg::SubagentLifecycle(event) => event,
        _ => unreachable!("predicate only matches lifecycle events"),
    };

    assert_eq!(first_event.phase, SubagentLifecyclePhase::Stopped);
    assert_eq!(first_event.agent_id, SWITCH_PRIMARY_AGENT);
    assert_eq!(second_event.phase, SubagentLifecyclePhase::Activated);
    assert_eq!(second_event.agent_id, SWITCH_SECONDARY_AGENT);

    Ok(())
}

#[test]
fn registry_reports_invalid_manifest_issues() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let project_dir = temp.path().join("project");
    fs::create_dir_all(&project_dir)?;

    fs::write(
        project_dir.join("invalid.yaml"),
        r#"id: invalid-registry-agent
name: Invalid Registry Agent
description: Invalid manifest to test loader issues
body: Invalid body
permissionMode: default
tools:
  - shell
  - shell
hooks:
  pre:
    - name: invalid-webhook
      command: echo pre
      endpoint: https://example.com/pre
triggers:
  - type: keyword
    phrase: high-weight
    weight: 150
"#,
    )?;

    fs::write(
        project_dir.join("valid.yaml"),
        r#"id: valid-registry-agent
name: Valid Registry Agent
description: Confirm registry retains valid manifests
body: Valid body
permissionMode: default
"#,
    )?;

    let loader = Arc::new(FsManifestLoader::new());
    let mut registry = AgentRegistry::new(loader);
    let outcome = registry.refresh(&[DiscoveryTarget::ProjectDir(project_dir)])?;
    assert!(
        outcome.issues.iter().any(|issue| issue
            .message
            .contains("invalid hook: hook invalid-webhook cannot set command and endpoint")),
        "expected conflicting hook issue in {:?}",
        outcome.issues
    );
    assert!(
        outcome.issues.iter().any(|issue| issue
            .message
            .contains("invalid trigger high-weight: weight must be between 1 and 100")),
        "expected invalid trigger issue in {:?}",
        outcome.issues
    );
    assert!(
        registry.has_agent("valid-registry-agent"),
        "valid manifest should still be registered"
    );
    Ok(())
}

#[derive(Debug)]
struct WatchTestLoader {
    manifests: Mutex<Vec<AgentManifest>>,
    load_count: AtomicUsize,
    watch_sender: Mutex<Option<AsyncSender<LoaderEvent>>>,
}

impl WatchTestLoader {
    fn new(manifests: Vec<AgentManifest>) -> Self {
        Self {
            manifests: Mutex::new(manifests),
            load_count: AtomicUsize::new(0),
            watch_sender: Mutex::new(None),
        }
    }

    fn emit(&self, scope: DiscoveryTarget) {
        let sender = self.watch_sender.lock().expect("watch sender lock").clone();
        if let Some(sender) = sender {
            let _ = sender.try_send(LoaderEvent { scope });
        }
    }

    fn load_count(&self) -> usize {
        self.load_count.load(Ordering::SeqCst)
    }
}

impl ManifestLoader for WatchTestLoader {
    fn load(&self, _targets: &[DiscoveryTarget]) -> Result<LoadOutcome, ManifestError> {
        self.load_count.fetch_add(1, Ordering::SeqCst);
        Ok(LoadOutcome {
            manifests: self.manifests.lock().expect("manifest lock").clone(),
            issues: Vec::new(),
        })
    }

    fn watch(&self, _targets: &[DiscoveryTarget]) -> Result<LoaderWatch, ManifestError> {
        let (sender, receiver) = async_channel::unbounded();
        *self.watch_sender.lock().expect("watch sender lock") = Some(sender);
        Ok(LoaderWatch::from_receiver_for_tests(receiver))
    }
}

fn wait_for_registry_event(watch: &AgentRegistryWatch, timeout: Duration) -> Option<RegistryEvent> {
    let deadline = Instant::now() + timeout;
    loop {
        match watch.try_recv() {
            Ok(event) => return Some(event),
            Err(AgentRegistryWatchTryRecvError::Closed) => return None,
            Err(AgentRegistryWatchTryRecvError::Empty) => {
                if Instant::now() >= deadline {
                    return None;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn test_manifest(id: &str) -> AgentManifest {
    AgentManifest {
        id: id.into(),
        kind: AgentKind::Custom,
        name: format!("{id} agent"),
        description: format!("{id} description"),
        model: None,
        tool_scope: ToolScope::default(),
        permission_mode: PermissionMode::Default,
        hooks: HookSet::default(),
        triggers: Vec::new(),
        skills: Vec::new(),
        body: "Prompt".into(),
        source: None,
        digest: None,
    }
}

fn dummy_scope() -> DiscoveryTarget {
    DiscoveryTarget::ProjectDir(PathBuf::from("proj"))
}

fn assert_hook_phase(event: &HookEvent, phase: &str, success: Option<bool>) {
    assert!(
        event.path.ends_with(phase.split('_').next().unwrap_or("")),
        "hook endpoint should match phase"
    );
    assert_eq!(event.body["phase"], phase);
    let got = event.body["success"].as_bool();
    assert_eq!(got, success);
}

async fn activate_subagent(
    test: &core_test_support::test_codex::TestCodex,
    agent_id: &str,
) -> Result<()> {
    test.codex
        .submit(Op::OverrideTurnContext {
            cwd: None,
            approval_policy: None,
            sandbox_policy: None,
            model: None,
            effort: None,
            summary: None,
            subagent: Some(SubagentOverride::Activate {
                id: agent_id.to_string(),
                resume: None,
                origin: None,
            }),
        })
        .await?;
    Ok(())
}

async fn wait_for_lifecycle_event(
    test: &core_test_support::test_codex::TestCodex,
    agent_id: &str,
    phase: SubagentLifecyclePhase,
) -> SubagentLifecycleEvent {
    let event = wait_for_event(&test.codex, |ev| {
        matches!(
            ev,
            EventMsg::SubagentLifecycle(lifecycle)
                if lifecycle.agent_id == agent_id && lifecycle.phase == phase
        )
    })
    .await;
    match event {
        EventMsg::SubagentLifecycle(event) => event,
        other => panic!("expected SubagentLifecycle event, got {other:?}"),
    }
}

fn assert_resume_artifacts(
    codex_home: &Path,
    agent_id: &str,
    expected_source: &SessionSource,
    resume_token: &str,
) -> Result<()> {
    let agent_dir = codex_home.join("subagents").join(agent_id);
    assert!(
        agent_dir.exists(),
        "expected subagent dir at {}",
        agent_dir.display()
    );
    let index_path = agent_dir.join("index.json");
    let index_data = fs::read_to_string(&index_path)?;
    let index: codex_core::TranscriptIndex = serde_json::from_str(&index_data)?;
    let run = index
        .runs
        .iter()
        .find(|summary| summary.resume_token == resume_token)
        .unwrap_or_else(|| {
            panic!(
                "resume token {resume_token} missing from {}",
                index_path.display()
            )
        });
    assert_eq!(
        &run.session_source, expected_source,
        "session source mismatch for {agent_id}"
    );
    let run_dir = run
        .transcript_path
        .parent()
        .expect("transcript path has parent")
        .to_path_buf();
    assert!(
        run_dir.starts_with(agent_dir.join("runs")),
        "expected transcript under runs/, got {}",
        run_dir.display()
    );
    assert!(
        run.transcript_path.exists(),
        "transcript missing at {}",
        run.transcript_path.display()
    );
    let run_resume = run_dir.join("resume.token");
    assert_eq!(
        fs::read_to_string(&run_resume)?.trim(),
        resume_token,
        "run resume token mismatch for {agent_id}"
    );
    let latest_resume = agent_dir.join("resume.token");
    assert_eq!(
        fs::read_to_string(&latest_resume)?.trim(),
        resume_token,
        "agent resume token mismatch for {agent_id}"
    );
    Ok(())
}

fn write_manifest(cwd: &Path, agent_id: &str, manifest: String) {
    let path = manifest_path(cwd, agent_id);
    let dir = path.parent().expect("manifest parent dir present");
    fs::create_dir_all(dir).expect("create manifest dir");
    fs::write(path, manifest).expect("write manifest");
}

fn manifest_path(cwd: &Path, agent_id: &str) -> PathBuf {
    cwd.join(".claude")
        .join("agents")
        .join(format!("{agent_id}.yaml"))
}

struct HookServer {
    addr: SocketAddr,
    events: Arc<Mutex<Vec<HookEvent>>>,
    shutdown: Option<oneshot::Sender<()>>,
}

#[derive(Clone)]
struct HookEvent {
    path: String,
    body: Value,
}

impl HookServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind hook server");
        let addr = listener.local_addr().expect("hook addr");
        let events = Arc::new(Mutex::new(Vec::new()));
        let (tx, mut rx) = oneshot::channel();
        let events_clone = Arc::clone(&events);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    res = listener.accept() => {
                        if let Ok((stream, _)) = res {
                            let events = Arc::clone(&events_clone);
                            tokio::spawn(async move {
                                let _ = handle_connection(stream, events).await;
                            });
                        }
                    }
                    _ = &mut rx => break,
                }
            }
        });

        Self {
            addr,
            events,
            shutdown: Some(tx),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn events(&self) -> Vec<HookEvent> {
        self.events.lock().expect("hook events lock").clone()
    }
}

impl Drop for HookServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    events: Arc<Mutex<Vec<HookEvent>>>,
) -> std::io::Result<()> {
    let mut buffer = Vec::new();
    loop {
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..n]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let header_end = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|idx| idx + 4)
        .unwrap_or(buffer.len());

    let request = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = request.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let _method = parts.next();
    let path = parts.next().unwrap_or("/").to_string();

    let mut content_length = 0usize;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }

    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let mut chunk = vec![0u8; content_length - body.len()];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }

    if let Ok(value) = serde_json::from_slice::<Value>(&body[..content_length]) {
        events
            .lock()
            .expect("hook events lock")
            .push(HookEvent { path, body: value });
    }

    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
        .await?;
    Ok(())
}
