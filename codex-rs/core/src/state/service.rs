use std::io;
use std::sync::Arc;

use crate::AuthManager;
use crate::RolloutRecorder;
use crate::agent::AgentControl;
use crate::agent::AgentRegistry;
use crate::agent::AgentRuntimeProfile;
use crate::exec_policy::ExecPolicyManager;
use crate::mcp_connection_manager::McpConnectionManager;
use crate::models_manager::manager::ModelsManager;
use crate::skills::SkillsManager;
use crate::subagents::SubagentTranscriptStore;
use crate::tools::sandboxing::ApprovalStore;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::user_notification::UserNotifier;
use codex_otel::OtelManager;
use codex_protocol::protocol::AskForApproval;
use codex_subagent::HookSet;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub(crate) struct ActiveSubagentRuntime {
    runtime: Arc<AgentRuntimeProfile>,
    approval_policy: AskForApproval,
    allowed_tools: Vec<String>,
    hooks: HookSet,
    transcript: Option<crate::subagents::SubagentTranscript>,
}

impl ActiveSubagentRuntime {
    pub(crate) fn new(
        runtime: Arc<AgentRuntimeProfile>,
        approval_policy: AskForApproval,
        allowed_tools: Vec<String>,
        hooks: HookSet,
        transcript: Option<crate::subagents::SubagentTranscript>,
    ) -> Self {
        // We eagerly copy the lightweight policy/tool/HookSet data so each turn can
        // read a consistent snapshot without borrowing the registry lock again.
        Self {
            runtime,
            approval_policy,
            allowed_tools,
            hooks,
            transcript,
        }
    }

    #[must_use]
    pub(crate) fn runtime(&self) -> Arc<AgentRuntimeProfile> {
        Arc::clone(&self.runtime)
    }

    #[must_use]
    pub(crate) fn approval_policy(&self) -> AskForApproval {
        self.approval_policy
    }

    #[must_use]
    pub(crate) fn allowed_tools(&self) -> &[String] {
        &self.allowed_tools
    }

    #[must_use]
    pub(crate) fn hooks(&self) -> &HookSet {
        &self.hooks
    }

    #[must_use]
    pub(crate) fn transcript(&self) -> Option<crate::subagents::SubagentTranscript> {
        self.transcript.clone()
    }
    pub(crate) async fn finish_transcript(&self) -> io::Result<Option<String>> {
        if let Some(transcript) = &self.transcript {
            transcript.finish().await
        } else {
            Ok(None)
        }
    }
}

pub(crate) struct SessionServices {
    pub(crate) mcp_connection_manager: Arc<RwLock<McpConnectionManager>>,
    pub(crate) mcp_startup_cancellation_token: CancellationToken,
    pub(crate) unified_exec_manager: UnifiedExecProcessManager,
    pub(crate) notifier: UserNotifier,
    pub(crate) rollout: Mutex<Option<RolloutRecorder>>,
    pub(crate) user_shell: Arc<crate::shell::Shell>,
    pub(crate) show_raw_agent_reasoning: bool,
    pub(crate) exec_policy: ExecPolicyManager,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) models_manager: Arc<ModelsManager>,
    pub(crate) otel_manager: OtelManager,
    pub(crate) tool_approvals: Mutex<ApprovalStore>,
    pub(crate) skills_manager: Arc<SkillsManager>,
    pub(crate) agent_control: AgentControl,
    #[allow(dead_code)]
    pub(crate) agent_registry: Arc<RwLock<AgentRegistry>>,
    pub(crate) active_subagent_runtime: RwLock<Option<ActiveSubagentRuntime>>,
    pub(crate) subagent_transcripts: SubagentTranscriptStore,
}

impl SessionServices {
    /// Stores the active runtime; callers should clone only inexpensive data
    /// (Arc handles, small Vecs) before invoking this setter.
    pub(crate) async fn set_active_subagent(&self, runtime: ActiveSubagentRuntime) {
        let mut guard = self.active_subagent_runtime.write().await;
        *guard = Some(runtime);
    }

    /// Clears the active runtime, returning the session to the primary agent.
    pub(crate) async fn clear_active_subagent(&self) {
        let mut guard = self.active_subagent_runtime.write().await;
        guard.take();
    }

    /// Returns the currently active runtime snapshot, if any.
    pub(crate) async fn active_subagent(&self) -> Option<ActiveSubagentRuntime> {
        let guard = self.active_subagent_runtime.read().await;
        guard.clone()
    }
}
