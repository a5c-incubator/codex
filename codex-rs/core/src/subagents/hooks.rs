use std::sync::Arc;

use crate::codex::Session;
use crate::codex::TurnContext;
use crate::default_client::build_reqwest_client;
use crate::sandboxing::SandboxPermissions;
use crate::tools::sandboxing::HookSignal;
use crate::tools::sandboxing::ToolCtx;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::UnifiedExecContext;
use codex_client::HttpTransport;
use codex_client::Request;
use codex_client::ReqwestTransport;
use codex_client::TransportError;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionSource;
use codex_subagent::Hook;
use futures::future::join_all;
use http::Method;
use once_cell::sync::OnceCell;
use serde::Serialize;
use tracing::debug;
use tracing::warn;

const DEFAULT_HOOK_YIELD_MS: u64 = 5_000;

/// Lazy-initialized HTTP transport shared by hook endpoint invocations.
static HOOK_HTTP_TRANSPORT: OnceCell<ReqwestTransport> = OnceCell::new();

fn hook_http_transport() -> ReqwestTransport {
    HOOK_HTTP_TRANSPORT
        .get_or_init(|| ReqwestTransport::new(build_reqwest_client()))
        .clone()
}

/// Hook phase as described in `docs/subagents/architecture.md`.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub enum HookPhase {
    #[serde(rename = "pre_tool_use")]
    PreToolUse,
    #[serde(rename = "post_tool_use")]
    PostToolUse,
    #[serde(rename = "stop")]
    Stop,
}

/// Result for a single hook invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookOutcome {
    pub hook_name: String,
    pub phase: HookPhase,
    pub status: HookStatus,
}

impl HookOutcome {
    fn skipped(phase: HookPhase, hook: &Hook) -> Self {
        Self {
            hook_name: hook.name.clone(),
            phase,
            status: HookStatus::Skipped,
        }
    }

    fn success(phase: HookPhase, hook: &Hook) -> Self {
        Self {
            hook_name: hook.name.clone(),
            phase,
            status: HookStatus::Success,
        }
    }

    fn failed(phase: HookPhase, hook: &Hook, error: impl Into<String>) -> Self {
        Self {
            hook_name: hook.name.clone(),
            phase,
            status: HookStatus::Failed {
                error: error.into(),
            },
        }
    }
}

/// Status for a hook invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookStatus {
    Skipped,
    Success,
    Failed { error: String },
}

/// Context shared across hook invocations for a single tool call or session event.
#[derive(Clone)]
pub struct HookInvocation {
    pub session: Arc<Session>,
    pub turn: Arc<TurnContext>,
    pub call_id: String,
    pub tool_name: Option<String>,
}

impl HookInvocation {
    /// Builds a context from the orchestrator's [`ToolCtx`].
    pub fn from_tool_ctx(tool_ctx: &ToolCtx) -> Self {
        Self {
            session: Arc::clone(&tool_ctx.session),
            turn: Arc::clone(&tool_ctx.turn),
            call_id: tool_ctx.call_id.clone(),
            tool_name: Some(tool_ctx.tool_name.clone()),
        }
    }

    /// Constructs a context for session-level events such as clearing runtimes.
    pub fn for_session_event(
        session: Arc<Session>,
        turn: Arc<TurnContext>,
        tool_name: Option<String>,
        call_id: impl Into<String>,
    ) -> Self {
        Self {
            session,
            turn,
            call_id: call_id.into(),
            tool_name,
        }
    }
}

/// Executes the matching hooks for the active subagent, if any.
pub async fn run_subagent_hooks(
    phase: HookPhase,
    invocation: &HookInvocation,
    signal: &HookSignal,
) -> Vec<HookOutcome> {
    if is_hook_call_id(&invocation.call_id) {
        // Avoid recursion when hook commands trigger tool executions.
        return Vec::new();
    }
    let Some(runtime) = invocation.turn.active_subagent().cloned() else {
        return Vec::new();
    };
    let hook_set = runtime.hooks();
    let hooks = match phase {
        HookPhase::PreToolUse => &hook_set.pre,
        HookPhase::PostToolUse => &hook_set.post,
        HookPhase::Stop => &hook_set.stop,
    };
    if hooks.is_empty() {
        return Vec::new();
    }

    let manifest = runtime.runtime();
    let payload = HookPayload::new(
        phase,
        manifest.manifest().id.clone(),
        invocation.tool_name.clone(),
        invocation.call_id.clone(),
        invocation.turn.client.get_session_source(),
        invocation.turn.sandbox_policy.clone(),
        invocation.turn.approval_policy,
        signal,
    );

    let mut outcomes = Vec::new();
    let mut tasks = Vec::new();

    for hook in hooks {
        if !should_run_hook(hook, invocation.tool_name.as_deref()) {
            outcomes.push(HookOutcome::skipped(phase, hook));
            continue;
        }
        tasks.push(run_single_hook(
            phase,
            hook.clone(),
            invocation.clone(),
            payload.clone(),
        ));
    }

    for outcome in join_all(tasks).await {
        outcomes.push(outcome);
    }
    outcomes
}

fn is_hook_call_id(call_id: &str) -> bool {
    matches!(
        call_id,
        id if id.starts_with("hook-PreToolUse-")
            || id.starts_with("hook-PostToolUse-")
            || id.starts_with("hook-Stop-")
    )
}

fn should_run_hook(hook: &Hook, tool_name: Option<&str>) -> bool {
    let Some(tool_filter) = hook.tools.as_ref() else {
        return true;
    };
    let Some(name) = tool_name else {
        return false;
    };
    tool_filter
        .iter()
        .any(|candidate| candidate.as_str() == name)
}

async fn run_single_hook(
    phase: HookPhase,
    hook: Hook,
    invocation: HookInvocation,
    payload: HookPayload,
) -> HookOutcome {
    match (&hook.command, &hook.endpoint) {
        (Some(command), None) => run_command_hook(phase, &hook, command, invocation, payload).await,
        (None, Some(endpoint)) => run_http_hook(phase, &hook, endpoint, payload).await,
        _ => {
            warn!(
                hook = %hook.name,
                "hook must specify exactly one of `command` or `endpoint`"
            );
            HookOutcome::failed(phase, &hook, "misconfigured hook")
        }
    }
}

async fn run_command_hook(
    phase: HookPhase,
    hook: &Hook,
    command: &str,
    invocation: HookInvocation,
    payload: HookPayload,
) -> HookOutcome {
    let session = invocation.session;
    let turn = invocation.turn;
    let command_args = session.user_shell().derive_exec_args(command, false);

    let manager = &session.services.unified_exec_manager;
    let process_id = manager.allocate_process_id().await;
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        Arc::clone(&turn),
        format!("hook-{:?}-{}", phase, payload.call_id),
    );
    let request = ExecCommandRequest {
        command: command_args,
        process_id: process_id.clone(),
        yield_time_ms: DEFAULT_HOOK_YIELD_MS,
        max_output_tokens: None,
        workdir: None,
        sandbox_permissions: SandboxPermissions::UseDefault,
        justification: Some(format!("subagent hook {}", hook.name)),
    };

    match manager.exec_command(request, &context).await {
        Ok(response) => {
            if response.exit_code.unwrap_or_default() != 0 {
                warn!(
                    hook = %hook.name,
                    phase = ?phase,
                    call_id = %payload.call_id,
                    exit_code = ?response.exit_code,
                    "hook command exited with non-zero status"
                );
                HookOutcome::failed(
                    phase,
                    hook,
                    format!("exit {:?}: {}", response.exit_code, response.output.trim()),
                )
            } else {
                debug!(
                    hook = %hook.name,
                    phase = ?phase,
                    call_id = %payload.call_id,
                    "hook command completed successfully"
                );
                HookOutcome::success(phase, hook)
            }
        }
        Err(err) => {
            warn!(
                hook = %hook.name,
                phase = ?phase,
                call_id = %payload.call_id,
                error = %err,
                "failed to execute hook command"
            );
            HookOutcome::failed(phase, hook, err.to_string())
        }
    }
}

async fn run_http_hook(
    phase: HookPhase,
    hook: &Hook,
    endpoint: &str,
    payload: HookPayload,
) -> HookOutcome {
    let transport = hook_http_transport();
    let request = Request::new(Method::POST, endpoint.to_string()).with_json(&payload);
    match transport.execute(request).await {
        Ok(_) => HookOutcome::success(phase, hook),
        Err(err) => {
            match &err {
                TransportError::Http { status, .. } => {
                    warn!(
                        hook = %hook.name,
                        phase = ?phase,
                        call_id = %payload.call_id,
                        status = %status,
                        "hook endpoint returned error"
                    );
                }
                _ => {
                    warn!(
                        hook = %hook.name,
                        phase = ?phase,
                        call_id = %payload.call_id,
                        error = %err,
                        "hook endpoint request failed"
                    );
                }
            }
            HookOutcome::failed(phase, hook, err.to_string())
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct HookPayload {
    phase: HookPhase,
    #[serde(rename = "manifest_id")]
    manifest_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
    call_id: String,
    session_source: SessionSource,
    sandbox_policy: SandboxPolicy,
    approval_policy: AskForApproval,
    #[serde(skip_serializing_if = "Option::is_none")]
    success: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout_snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr_snippet: Option<String>,
}

impl HookPayload {
    fn new(
        phase: HookPhase,
        manifest_id: String,
        tool_name: Option<String>,
        call_id: String,
        session_source: SessionSource,
        sandbox_policy: SandboxPolicy,
        approval_policy: AskForApproval,
        signal: &HookSignal,
    ) -> Self {
        Self {
            phase,
            manifest_id,
            tool_name,
            call_id,
            session_source,
            sandbox_policy,
            approval_policy,
            success: signal.success,
            stdout_snippet: signal.stdout_snippet.clone(),
            stderr_snippet: signal.stderr_snippet.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::make_session_and_context;
    use codex_protocol::protocol::SandboxPolicy;
    use codex_protocol::protocol::SessionSource;
    use codex_subagent::Hook;
    use insta::assert_json_snapshot;
    use serde_json::Value;

    #[tokio::test]
    async fn http_hook_failure_is_reported() {
        let hook = Hook {
            name: "http".into(),
            description: None,
            command: None,
            endpoint: Some("http://127.0.0.1:9/hook".into()),
            tools: None,
        };
        let payload = HookPayload::new(
            HookPhase::PostToolUse,
            "manifest".into(),
            Some("shell".into()),
            "call".into(),
            SessionSource::Cli,
            SandboxPolicy::DangerFullAccess,
            AskForApproval::Never,
            &HookSignal::pending(),
        );
        let outcome = run_http_hook(
            HookPhase::PostToolUse,
            &hook,
            hook.endpoint.as_ref().unwrap(),
            payload,
        )
        .await;
        assert!(matches!(outcome.status, HookStatus::Failed { .. }));
    }

    #[tokio::test]
    async fn command_hook_non_zero_exit_is_reported() {
        let (session, turn_context) = make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn_context);
        let invocation = HookInvocation::for_session_event(
            Arc::clone(&session),
            Arc::clone(&turn),
            Some("shell".into()),
            "hook-call",
        );
        let payload = HookPayload::new(
            HookPhase::PostToolUse,
            "manifest".into(),
            Some("shell".into()),
            "hook-call".into(),
            SessionSource::Cli,
            SandboxPolicy::DangerFullAccess,
            AskForApproval::Never,
            &HookSignal::pending(),
        );
        let hook = Hook {
            name: "cmd".into(),
            description: None,
            command: Some("exit 1".into()),
            endpoint: None,
            tools: None,
        };
        let outcome = run_command_hook(
            HookPhase::PostToolUse,
            &hook,
            hook.command.as_ref().unwrap(),
            invocation,
            payload,
        )
        .await;
        assert!(matches!(outcome.status, HookStatus::Failed { .. }));
    }

    #[test]
    fn hook_payload_pre_tool_use_snapshot() {
        let json = payload_snapshot(
            HookPhase::PreToolUse,
            HookSignal::pending(),
            Some("shell".into()),
        );
        assert_json_snapshot!("hook_payload_pre_tool_use", json);
    }

    #[test]
    fn hook_payload_post_tool_use_snapshot() {
        let json = payload_snapshot(
            HookPhase::PostToolUse,
            HookSignal::success(Some("hook ok".into())),
            None,
        );
        assert_json_snapshot!("hook_payload_post_tool_use", json);
    }

    fn payload_snapshot(phase: HookPhase, signal: HookSignal, tool_name: Option<String>) -> Value {
        let payload = HookPayload::new(
            phase,
            "docs-demo".into(),
            tool_name,
            format!("call-{phase:?}"),
            SessionSource::Cli,
            SandboxPolicy::DangerFullAccess,
            AskForApproval::Never,
            &signal,
        );
        serde_json::to_value(payload).expect("serialize payload")
    }
}
