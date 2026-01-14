use std::collections::HashMap;
use std::fmt::Debug;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use crate::AuthManager;
use crate::CodexAuth;
use crate::SandboxState;
use crate::agent::ActivationContext;
use crate::agent::ActivationError;
use crate::agent::AgentControl;
use crate::agent::AgentRegistry;
use crate::agent::AgentRuntimeProfile;
use crate::agent::AgentStatus;
use crate::agent::RefreshInvocation;
use crate::agent::agent_status_from_event;
use crate::agent::registry::RefreshOutcome;
use crate::client_common::REVIEW_PROMPT;
use crate::compact;
use crate::compact::run_inline_auto_compact_task;
use crate::compact::should_use_remote_compact_task;
use crate::compact_remote::run_inline_remote_auto_compact_task;
use crate::exec_policy::ExecPolicyManager;
use crate::features::Feature;
use crate::features::Features;
use crate::models_manager::manager::ModelsManager;
use crate::parse_command::parse_command;
use crate::parse_turn_item;
use crate::stream_events_utils::HandleOutputCtx;
use crate::stream_events_utils::handle_non_tool_response_item;
use crate::stream_events_utils::handle_output_item_done;
use crate::terminal;
use crate::truncate::TruncationPolicy;
use crate::user_notification::UserNotifier;
use crate::util::error_or_panic;
use async_channel::Receiver;
use async_channel::Sender;
use codex_protocol::ThreadId;
use codex_protocol::approvals::ExecPolicyAmendment;
use codex_protocol::items::TurnItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::HasLegacyEvent;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::RawResponseItemEvent;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubagentOverride;
use codex_protocol::protocol::TaskStartedEvent;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnContextItem;
use codex_rmcp_client::ElicitationResponse;
use codex_subagent::DiscoveryTargetArgs;
use codex_subagent::FsManifestLoader;
use codex_subagent::HookSet;
use codex_subagent::SubagentDiscoveryOverrides;
use codex_subagent::build_discovery_targets;
use futures::future::BoxFuture;
use futures::prelude::*;
use futures::stream::FuturesOrdered;
use mcp_types::CallToolResult;
use mcp_types::ListResourceTemplatesRequestParams;
use mcp_types::ListResourceTemplatesResult;
use mcp_types::ListResourcesRequestParams;
use mcp_types::ListResourcesResult;
use mcp_types::ReadResourceRequestParams;
use mcp_types::ReadResourceResult;
use mcp_types::RequestId;
use serde_json;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::field;
use tracing::info;
use tracing::instrument;
use tracing::trace_span;
use tracing::warn;

use crate::ModelProviderInfo;
use crate::WireApi;
use crate::client::ModelClient;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::compact::collect_user_messages;
use crate::config::Config;
use crate::config::Constrained;
use crate::config::ConstraintResult;
use crate::config::GhostSnapshotConfig;
use crate::config::types::ShellEnvironmentPolicy;
use crate::context_manager::ContextManager;
use crate::environment_context::EnvironmentContext;
use crate::error::CodexErr;
use crate::error::Result as CodexResult;
#[cfg(test)]
use crate::exec::StreamOutput;
use crate::exec_policy::ExecPolicyUpdateError;
use crate::feedback_tags;
use crate::mcp::auth::compute_auth_statuses;
use crate::mcp_connection_manager::McpConnectionManager;
use crate::model_provider_info::CHAT_WIRE_API_DEPRECATION_SUMMARY;
use crate::project_doc::get_user_instructions;
use crate::protocol::AgentMessageContentDeltaEvent;
use crate::protocol::AgentReasoningSectionBreakEvent;
use crate::protocol::ApplyPatchApprovalRequestEvent;
use crate::protocol::AskForApproval;
use crate::protocol::BackgroundEventEvent;
use crate::protocol::DeprecationNoticeEvent;
use crate::protocol::ErrorEvent;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::ExecApprovalRequestEvent;
use crate::protocol::Op;
use crate::protocol::RateLimitSnapshot;
use crate::protocol::ReasoningContentDeltaEvent;
use crate::protocol::ReasoningRawContentDeltaEvent;
use crate::protocol::ReviewDecision;
use crate::protocol::SandboxPolicy;
use crate::protocol::SessionConfiguredEvent;
use crate::protocol::SkillErrorInfo;
use crate::protocol::SkillMetadata as ProtocolSkillMetadata;
use crate::protocol::StreamErrorEvent;
use crate::protocol::Submission;
use crate::protocol::TokenCountEvent;
use crate::protocol::TokenUsage;
use crate::protocol::TokenUsageInfo;
use crate::protocol::TurnDiffEvent;
use crate::protocol::WarningEvent;
use crate::rollout::RolloutRecorder;
use crate::rollout::RolloutRecorderParams;
use crate::rollout::map_session_init_error;
use crate::shell;
use crate::shell_snapshot::ShellSnapshot;
use crate::skills::SkillError;
use crate::skills::SkillInjections;
use crate::skills::SkillMetadata;
use crate::skills::SkillsManager;
use crate::skills::build_skill_injections;
use crate::state::ActiveSubagentRuntime;
use crate::state::ActiveTurn;
use crate::state::SessionServices;
use crate::state::SessionState;
use crate::subagents::SubagentResumeToken;
use crate::subagents::SubagentTranscriptStore;
use crate::tasks::GhostSnapshotTask;
use crate::tasks::ReviewTask;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;
use crate::tools::ToolRouter;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::sandboxing::ApprovalStore;
use crate::tools::spec::ToolsConfig;
use crate::tools::spec::ToolsConfigParams;
use crate::tools::spec::build_specs;
use crate::turn_diff_tracker::TurnDiffTracker;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::user_instructions::DeveloperInstructions;
use crate::user_instructions::UserInstructions;
use crate::user_notification::UserNotification;
use crate::util::backoff;
use codex_async_utils::OrCancelExt;
use codex_otel::OtelManager;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::user_input::UserInput;
use codex_utils_readiness::Readiness;
use codex_utils_readiness::ReadinessFlag;

/// The high-level interface to the Codex system.
/// It operates as a queue pair where you send submissions and receive events.
pub struct Codex {
    pub(crate) next_id: AtomicU64,
    pub(crate) tx_sub: Sender<Submission>,
    pub(crate) rx_event: Receiver<Event>,
    // Last known status of the agent.
    pub(crate) agent_status: Arc<RwLock<AgentStatus>>,
}

/// Wrapper returned by [`Codex::spawn`] containing the spawned [`Codex`],
/// the submission id for the initial `ConfigureSession` request and the
/// unique session id.
pub struct CodexSpawnOk {
    pub codex: Codex,
    pub thread_id: ThreadId,
    #[deprecated(note = "use thread_id")]
    pub conversation_id: ThreadId,
}

pub(crate) const INITIAL_SUBMIT_ID: &str = "";
pub(crate) const SUBMISSION_CHANNEL_CAPACITY: usize = 64;
static CHAT_WIRE_API_DEPRECATION_EMITTED: AtomicBool = AtomicBool::new(false);

fn maybe_push_chat_wire_api_deprecation(
    config: &Config,
    post_session_configured_events: &mut Vec<Event>,
) {
    if config.model_provider.wire_api != WireApi::Chat {
        return;
    }

    if CHAT_WIRE_API_DEPRECATION_EMITTED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    post_session_configured_events.push(Event {
        id: INITIAL_SUBMIT_ID.to_owned(),
        msg: EventMsg::DeprecationNotice(DeprecationNoticeEvent {
            summary: CHAT_WIRE_API_DEPRECATION_SUMMARY.to_string(),
            details: None,
        }),
    });
}

impl Codex {
    /// Spawn a new [`Codex`] and initialize the session.
    pub(crate) async fn spawn(
        config: Config,
        auth_manager: Arc<AuthManager>,
        models_manager: Arc<ModelsManager>,
        skills_manager: Arc<SkillsManager>,
        conversation_history: InitialHistory,
        session_source: SessionSource,
        agent_control: AgentControl,
    ) -> CodexResult<CodexSpawnOk> {
        let (tx_sub, rx_sub) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);
        let (tx_event, rx_event) = async_channel::unbounded();

        let loaded_skills = skills_manager.skills_for_config(&config);
        // let loaded_skills = if config.features.enabled(Feature::Skills) {
        //     Some(skills_manager.skills_for_config(&config))
        // } else {
        //     None
        // };

        for err in &loaded_skills.errors {
            error!(
                "failed to load skill {}: {}",
                err.path.display(),
                err.message
            );
        }

        let user_instructions = get_user_instructions(&config, Some(&loaded_skills.skills)).await;

        let exec_policy = ExecPolicyManager::load(&config.features, &config.config_layer_stack)
            .await
            .map_err(|err| CodexErr::Fatal(format!("failed to load execpolicy: {err}")))?;

        let config = Arc::new(config);
        if config.features.enabled(Feature::RemoteModels)
            && let Err(err) = models_manager
                .refresh_available_models_with_cache(&config)
                .await
        {
            error!("failed to refresh available models: {err:?}");
        }
        let model = models_manager.get_model(&config.model, &config).await;
        let session_configuration = SessionConfiguration {
            provider: config.model_provider.clone(),
            model: model.clone(),
            model_reasoning_effort: config.model_reasoning_effort,
            model_reasoning_summary: config.model_reasoning_summary,
            developer_instructions: config.developer_instructions.clone(),
            user_instructions,
            base_instructions: config.base_instructions.clone(),
            compact_prompt: config.compact_prompt.clone(),
            subagent_discovery_overrides: config.subagent_discovery_overrides.clone(),
            approval_policy: config.approval_policy.clone(),
            sandbox_policy: config.sandbox_policy.clone(),
            cwd: config.cwd.clone(),
            original_config_do_not_use: Arc::clone(&config),
            session_source,
        };

        // Generate a unique ID for the lifetime of this Codex session.
        let session_source_clone = session_configuration.session_source.clone();
        let agent_status = Arc::new(RwLock::new(AgentStatus::PendingInit));

        let session = Session::new(
            session_configuration,
            config.clone(),
            auth_manager.clone(),
            models_manager.clone(),
            exec_policy,
            tx_event.clone(),
            Arc::clone(&agent_status),
            conversation_history,
            session_source_clone,
            skills_manager,
            agent_control,
        )
        .await
        .map_err(|e| {
            error!("Failed to create session: {e:#}");
            map_session_init_error(&e, &config.codex_home)
        })?;
        let thread_id = session.conversation_id;

        // This task will run until Op::Shutdown is received.
        tokio::spawn(submission_loop(session, config, rx_sub));
        let codex = Codex {
            next_id: AtomicU64::new(0),
            tx_sub,
            rx_event,
            agent_status,
        };

        #[allow(deprecated)]
        Ok(CodexSpawnOk {
            codex,
            thread_id,
            conversation_id: thread_id,
        })
    }

    /// Submit the `op` wrapped in a `Submission` with a unique ID.
    pub async fn submit(&self, op: Op) -> CodexResult<String> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .to_string();
        let sub = Submission { id: id.clone(), op };
        self.submit_with_id(sub).await?;
        Ok(id)
    }

    /// Use sparingly: prefer `submit()` so Codex is responsible for generating
    /// unique IDs for each submission.
    pub async fn submit_with_id(&self, sub: Submission) -> CodexResult<()> {
        self.tx_sub
            .send(sub)
            .await
            .map_err(|_| CodexErr::InternalAgentDied)?;
        Ok(())
    }

    pub async fn next_event(&self) -> CodexResult<Event> {
        let event = self
            .rx_event
            .recv()
            .await
            .map_err(|_| CodexErr::InternalAgentDied)?;
        Ok(event)
    }

    pub(crate) async fn agent_status(&self) -> AgentStatus {
        let status = self.agent_status.read().await;
        status.clone()
    }
}

/// Context for an initialized model agent
///
/// A session has at most 1 running task at a time, and can be interrupted by user input.
pub(crate) struct Session {
    conversation_id: ThreadId,
    tx_event: Sender<Event>,
    agent_status: Arc<RwLock<AgentStatus>>,
    state: Mutex<SessionState>,
    /// The set of enabled features should be invariant for the lifetime of the
    /// session.
    features: Features,
    pub(crate) active_turn: Mutex<Option<ActiveTurn>>,
    pub(crate) services: SessionServices,
    next_internal_sub_id: AtomicU64,
    agent_registration_done: AtomicBool,
}

/// The context needed for a single turn of the thread.
#[derive(Debug)]
pub(crate) struct TurnContext {
    pub(crate) sub_id: String,
    pub(crate) client: ModelClient,
    /// The session's current working directory. All relative paths provided by
    /// the model as well as sandbox policies are resolved against this path
    /// instead of `std::env::current_dir()`.
    pub(crate) cwd: PathBuf,
    pub(crate) developer_instructions: Option<String>,
    pub(crate) base_instructions: Option<String>,
    pub(crate) compact_prompt: Option<String>,
    pub(crate) user_instructions: Option<String>,
    pub(crate) approval_policy: AskForApproval,
    pub(crate) sandbox_policy: SandboxPolicy,
    pub(crate) shell_environment_policy: ShellEnvironmentPolicy,
    pub(crate) tools_config: ToolsConfig,
    pub(crate) ghost_snapshot: GhostSnapshotConfig,
    pub(crate) final_output_json_schema: Option<Value>,
    pub(crate) codex_linux_sandbox_exe: Option<PathBuf>,
    pub(crate) tool_call_gate: Arc<ReadinessFlag>,
    pub(crate) truncation_policy: TruncationPolicy,
    /// Snapshot of the active subagent runtime for this turn, if any.
    /// Carries the manifest hook metadata and tool allow list so the
    /// orchestrator can honor scoped permissions without mutating the
    /// session-level configuration.
    pub(crate) active_subagent: Option<ActiveSubagentRuntime>,
}

impl TurnContext {
    pub(crate) fn resolve_path(&self, path: Option<String>) -> PathBuf {
        path.as_ref()
            .map(PathBuf::from)
            .map_or_else(|| self.cwd.clone(), |p| self.cwd.join(p))
    }

    pub(crate) fn compact_prompt(&self) -> &str {
        self.compact_prompt
            .as_deref()
            .unwrap_or(compact::SUMMARIZATION_PROMPT)
    }

    pub(crate) fn has_active_subagent(&self) -> bool {
        self.active_subagent.is_some()
    }

    pub(crate) fn active_subagent(&self) -> Option<&ActiveSubagentRuntime> {
        self.active_subagent.as_ref()
    }

    pub(crate) fn allowed_tool_names(&self) -> Option<&[String]> {
        self.active_subagent
            .as_ref()
            .map(ActiveSubagentRuntime::allowed_tools)
    }

    pub(crate) fn hook_set(&self) -> Option<&HookSet> {
        self.active_subagent
            .as_ref()
            .map(ActiveSubagentRuntime::hooks)
    }
}

#[derive(Clone)]
pub(crate) struct SessionConfiguration {
    /// Provider identifier ("openai", "openrouter", ...).
    provider: ModelProviderInfo,

    /// If not specified, server will use its default model.
    model: String,

    model_reasoning_effort: Option<ReasoningEffortConfig>,
    model_reasoning_summary: ReasoningSummaryConfig,

    /// Developer instructions that supplement the base instructions.
    developer_instructions: Option<String>,

    /// Model instructions that are appended to the base instructions.
    user_instructions: Option<String>,

    /// Base instructions override.
    base_instructions: Option<String>,

    /// Compact prompt override.
    compact_prompt: Option<String>,

    /// CLI-provided subagent manifest overrides.
    subagent_discovery_overrides: Option<SubagentDiscoveryOverrides>,

    /// When to escalate for approval for execution
    approval_policy: Constrained<AskForApproval>,
    /// How to sandbox commands executed in the system
    sandbox_policy: Constrained<SandboxPolicy>,

    /// Working directory that should be treated as the *root* of the
    /// session. All relative paths supplied by the model as well as the
    /// execution sandbox are resolved against this directory **instead**
    /// of the process-wide current working directory. CLI front-ends are
    /// expected to expand this to an absolute path before sending the
    /// `ConfigureSession` operation so that the business-logic layer can
    /// operate deterministically.
    cwd: PathBuf,

    // TODO(pakrym): Remove config from here
    original_config_do_not_use: Arc<Config>,
    /// Source of the session (cli, vscode, exec, mcp, ...)
    session_source: SessionSource,
}

impl SessionConfiguration {
    pub(crate) fn apply(&self, updates: &SessionSettingsUpdate) -> ConstraintResult<Self> {
        let mut next_configuration = self.clone();
        if let Some(model) = updates.model.clone() {
            next_configuration.model = model;
        }
        if let Some(effort) = updates.reasoning_effort {
            next_configuration.model_reasoning_effort = effort;
        }
        if let Some(summary) = updates.reasoning_summary {
            next_configuration.model_reasoning_summary = summary;
        }
        if let Some(approval_policy) = updates.approval_policy {
            next_configuration.approval_policy.set(approval_policy)?;
        }
        if let Some(sandbox_policy) = updates.sandbox_policy.clone() {
            next_configuration.sandbox_policy.set(sandbox_policy)?;
        }
        if let Some(cwd) = updates.cwd.clone() {
            next_configuration.cwd = cwd;
        }
        Ok(next_configuration)
    }
}

#[derive(Default, Clone)]
pub(crate) struct SessionSettingsUpdate {
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) approval_policy: Option<AskForApproval>,
    pub(crate) sandbox_policy: Option<SandboxPolicy>,
    pub(crate) model: Option<String>,
    pub(crate) reasoning_effort: Option<Option<ReasoningEffortConfig>>,
    pub(crate) reasoning_summary: Option<ReasoningSummaryConfig>,
    pub(crate) final_output_json_schema: Option<Option<Value>>,
    pub(crate) subagent: Option<SubagentOverride>,
}

impl Session {
    /// Don't expand the number of mutated arguments on config. We are in the process of getting rid of it.
    fn build_per_turn_config(session_configuration: &SessionConfiguration) -> Config {
        // todo(aibrahim): store this state somewhere else so we don't need to mut config
        let config = session_configuration.original_config_do_not_use.clone();
        let mut per_turn_config = (*config).clone();
        per_turn_config.model_reasoning_effort = session_configuration.model_reasoning_effort;
        per_turn_config.model_reasoning_summary = session_configuration.model_reasoning_summary;
        per_turn_config.features = config.features.clone();
        per_turn_config
    }

    async fn ensure_subagents_registered(&self, client: &ModelClient) {
        if self
            .agent_registration_done
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let manifests = {
            let registry = self.services.agent_registry.read().await;
            registry.manifests_snapshot()
        };
        if manifests.is_empty() {
            return;
        }
        let counts = AgentRegistry::manifest_counts(&manifests);
        let provider_name = client.provider().name.as_str();
        match client.register_subagents(&manifests).await {
            Ok(()) => {
                info!(
                    provider = provider_name,
                    custom_manifests = counts.custom,
                    built_in_manifests = counts.built_in,
                    total_manifests = manifests.len(),
                    "registered subagents with model provider"
                );
            }
            Err(err) => {
                let validation_failures = match &err {
                    CodexErr::InvalidRequest(message) => Some(message.as_str()),
                    _ => None,
                };
                if let Some(details) = validation_failures {
                    warn!(
                        provider = provider_name,
                        custom_manifests = counts.custom,
                        built_in_manifests = counts.built_in,
                        total_manifests = manifests.len(),
                        error = ?err,
                        validation_failures = details,
                        "failed to register subagents with model provider"
                    );
                } else {
                    warn!(
                        provider = provider_name,
                        custom_manifests = counts.custom,
                        built_in_manifests = counts.built_in,
                        total_manifests = manifests.len(),
                        error = ?err,
                        "failed to register subagents with model provider"
                    );
                }
                self.agent_registration_done.store(false, Ordering::SeqCst);
            }
        }
    }

    async fn collect_available_tool_names(
        &self,
        session_configuration: &SessionConfiguration,
    ) -> (Vec<String>, usize, usize) {
        let per_turn_config = Self::build_per_turn_config(session_configuration);
        let model_info = self
            .services
            .models_manager
            .construct_model_info(session_configuration.model.as_str(), &per_turn_config)
            .await;
        let tools_config = ToolsConfig::new(&ToolsConfigParams {
            model_info: &model_info,
            features: &per_turn_config.features,
        });
        let (specs, _) = build_specs(&tools_config, None).build();
        let mut built_in_tool_names = specs
            .iter()
            .map(|spec| spec.spec.name().to_string())
            .collect::<Vec<_>>();
        let built_in_tool_count = built_in_tool_names.len();

        let mcp_tools = self
            .services
            .mcp_connection_manager
            .read()
            .await
            .list_all_tools()
            .await;
        let mut mcp_tool_names = mcp_tools.into_keys().collect::<Vec<_>>();
        mcp_tool_names.sort();
        let mcp_tool_count = mcp_tool_names.len();

        built_in_tool_names.extend(mcp_tool_names);
        (built_in_tool_names, built_in_tool_count, mcp_tool_count)
    }

    pub(crate) async fn activate_subagent(
        &self,
        agent_id: &str,
        resume_token: Option<String>,
    ) -> Result<Arc<AgentRuntimeProfile>, ActivationError> {
        let session_configuration = {
            let state = self.state.lock().await;
            state.session_configuration.clone()
        };
        let (available_tools, built_in_tool_count, mcp_tool_count) = self
            .collect_available_tool_names(&session_configuration)
            .await;
        let approval_policy = session_configuration.approval_policy.value();
        let parent_model = session_configuration.model.clone();
        let activation_ctx =
            ActivationContext::new(parent_model.as_str(), approval_policy, &available_tools);

        let registry = self.services.agent_registry.read().await;
        let activation_result = registry.activate(agent_id, &activation_ctx);
        drop(registry);

        let profile = match activation_result {
            Ok(profile) => profile,
            Err(err) => {
                self.clear_active_subagent().await;
                warn!(
                    agent_id = agent_id,
                    error = %err,
                    "failed to activate subagent runtime"
                );
                return Err(err);
            }
        };

        let runtime = Arc::new(profile);
        let cached_policy = runtime.approval_policy();
        let cached_tools = runtime.allowed_tools().to_vec();
        let cached_hooks = runtime.hooks().clone();
        if let Some(token) = resume_token
            && let Err(err) = self.apply_resume_history(agent_id, &token).await
        {
            warn!(agent_id = agent_id, error = %err, "failed to apply resume history");
        }
        let manifest = runtime.manifest();
        let transcript = match self
            .services
            .subagent_transcripts
            .start_run(
                manifest.id.as_str(),
                runtime.session_source(),
                &session_configuration.cwd,
                Some(manifest.body.as_str()),
                Some(session_configuration.provider.name.as_str()),
            )
            .await
        {
            Ok(handle) => Some(handle),
            Err(err) => {
                warn!(
                    agent_id = agent_id,
                    error = %err,
                    "failed to initialize subagent transcript"
                );
                None
            }
        };
        let snapshot = ActiveSubagentRuntime::new(
            Arc::clone(&runtime),
            cached_policy,
            cached_tools,
            cached_hooks,
            transcript,
        );
        self.services.set_active_subagent(snapshot).await;

        let manifest = runtime.manifest();
        let is_built_in = manifest.kind.is_built_in();
        info!(
            agent_id = agent_id,
            built_in = is_built_in,
            model = runtime.model(),
            approval_policy = ?runtime.approval_policy(),
            built_in_tools = built_in_tool_count,
            mcp_tools = mcp_tool_count,
            "activated subagent runtime"
        );

        Ok(runtime)
    }

    pub(crate) async fn clear_active_subagent(&self) {
        self.services.clear_active_subagent().await;
    }

    #[allow(clippy::too_many_arguments)]
    fn make_turn_context(
        auth_manager: Option<Arc<AuthManager>>,
        otel_manager: &OtelManager,
        provider: ModelProviderInfo,
        session_configuration: &SessionConfiguration,
        per_turn_config: Config,
        model_info: ModelInfo,
        conversation_id: ThreadId,
        sub_id: String,
        active_subagent: Option<ActiveSubagentRuntime>,
    ) -> TurnContext {
        let otel_manager = otel_manager.clone().with_model(
            session_configuration.model.as_str(),
            model_info.slug.as_str(),
        );

        let per_turn_config = Arc::new(per_turn_config);
        let client = ModelClient::new(
            per_turn_config.clone(),
            auth_manager,
            model_info.clone(),
            otel_manager,
            provider,
            session_configuration.model_reasoning_effort,
            session_configuration.model_reasoning_summary,
            conversation_id,
            session_configuration.session_source.clone(),
        );

        let tools_config = ToolsConfig::new(&ToolsConfigParams {
            model_info: &model_info,
            features: &per_turn_config.features,
        });

        TurnContext {
            sub_id,
            client,
            cwd: session_configuration.cwd.clone(),
            developer_instructions: session_configuration.developer_instructions.clone(),
            base_instructions: session_configuration.base_instructions.clone(),
            compact_prompt: session_configuration.compact_prompt.clone(),
            user_instructions: session_configuration.user_instructions.clone(),
            approval_policy: session_configuration.approval_policy.value(),
            sandbox_policy: session_configuration.sandbox_policy.get().clone(),
            shell_environment_policy: per_turn_config.shell_environment_policy.clone(),
            tools_config,
            ghost_snapshot: per_turn_config.ghost_snapshot.clone(),
            final_output_json_schema: None,
            codex_linux_sandbox_exe: per_turn_config.codex_linux_sandbox_exe.clone(),
            tool_call_gate: Arc::new(ReadinessFlag::new()),
            truncation_policy: model_info.truncation_policy.into(),
            active_subagent,
        }
    }

    fn apply_active_subagent_overrides(
        mut session_configuration: SessionConfiguration,
        active_subagent_runtime: Option<ActiveSubagentRuntime>,
    ) -> (SessionConfiguration, Option<ActiveSubagentRuntime>) {
        if let Some(runtime) = active_subagent_runtime.as_ref() {
            let profile = runtime.runtime();
            session_configuration.model = profile.model().to_owned();
            session_configuration.session_source = profile.session_source().clone();

            let mut approval_policy = session_configuration.approval_policy.clone();
            if let Err(err) = approval_policy.set(runtime.approval_policy()) {
                warn!(
                    error = %err,
                    "failed to apply subagent approval override; falling back to allow-any constraint"
                );
                approval_policy = Constrained::allow_any(runtime.approval_policy());
            }
            session_configuration.approval_policy = approval_policy;
        }

        (session_configuration, active_subagent_runtime)
    }

    #[allow(clippy::too_many_arguments)]
    async fn new(
        session_configuration: SessionConfiguration,
        config: Arc<Config>,
        auth_manager: Arc<AuthManager>,
        models_manager: Arc<ModelsManager>,
        exec_policy: ExecPolicyManager,
        tx_event: Sender<Event>,
        agent_status: Arc<RwLock<AgentStatus>>,
        initial_history: InitialHistory,
        session_source: SessionSource,
        skills_manager: Arc<SkillsManager>,
        agent_control: AgentControl,
    ) -> anyhow::Result<Arc<Self>> {
        debug!(
            "Configuring session: model={}; provider={:?}",
            session_configuration.model, session_configuration.provider
        );
        if !session_configuration.cwd.is_absolute() {
            return Err(anyhow::anyhow!(
                "cwd is not absolute: {:?}",
                session_configuration.cwd
            ));
        }

        let (conversation_id, rollout_params) = match &initial_history {
            InitialHistory::New | InitialHistory::Forked(_) => {
                let conversation_id = ThreadId::default();
                (
                    conversation_id,
                    RolloutRecorderParams::new(
                        conversation_id,
                        session_configuration.user_instructions.clone(),
                        session_source,
                    ),
                )
            }
            InitialHistory::Resumed(resumed_history) => (
                resumed_history.conversation_id,
                RolloutRecorderParams::resume(resumed_history.rollout_path.clone()),
            ),
        };

        // Kick off independent async setup tasks in parallel to reduce startup latency.
        //
        // - initialize RolloutRecorder with new or resumed session info
        // - perform default shell discovery
        // - load history metadata
        let rollout_fut = RolloutRecorder::new(&config, rollout_params);

        let history_meta_fut = crate::message_history::history_metadata(&config);
        let auth_statuses_fut = compute_auth_statuses(
            config.mcp_servers.iter(),
            config.mcp_oauth_credentials_store_mode,
        );

        // Join all independent futures.
        let (rollout_recorder, (history_log_id, history_entry_count), auth_statuses) =
            tokio::join!(rollout_fut, history_meta_fut, auth_statuses_fut);

        let rollout_recorder = rollout_recorder.map_err(|e| {
            error!("failed to initialize rollout recorder: {e:#}");
            anyhow::Error::from(e)
        })?;
        let rollout_path = rollout_recorder.rollout_path.clone();

        let mut post_session_configured_events = Vec::<Event>::new();

        for (alias, feature) in config.features.legacy_feature_usages() {
            let canonical = feature.key();
            let summary = format!("`{alias}` is deprecated. Use `[features].{canonical}` instead.");
            let details = if alias == canonical {
                None
            } else {
                Some(format!(
                    "Enable it with `--enable {canonical}` or `[features].{canonical}` in config.toml. See https://github.com/openai/codex/blob/main/docs/config.md#feature-flags for details."
                ))
            };
            post_session_configured_events.push(Event {
                id: INITIAL_SUBMIT_ID.to_owned(),
                msg: EventMsg::DeprecationNotice(DeprecationNoticeEvent { summary, details }),
            });
        }
        maybe_push_chat_wire_api_deprecation(&config, &mut post_session_configured_events);

        let auth = auth_manager.auth().await;
        let auth = auth.as_ref();
        let otel_manager = OtelManager::new(
            conversation_id,
            session_configuration.model.as_str(),
            session_configuration.model.as_str(),
            auth.and_then(CodexAuth::get_account_id),
            auth.and_then(CodexAuth::get_account_email),
            auth.map(|a| a.mode),
            config.otel.log_user_prompt,
            terminal::user_agent(),
            session_configuration.session_source.clone(),
        );
        config.features.emit_metrics(&otel_manager);
        otel_manager.counter(
            "codex.session.started",
            1,
            &[(
                "is_git",
                if get_git_repo_root(&session_configuration.cwd).is_some() {
                    "true"
                } else {
                    "false"
                },
            )],
        );

        otel_manager.conversation_starts(
            config.model_provider.name.as_str(),
            config.model_reasoning_effort,
            config.model_reasoning_summary,
            config.model_context_window,
            config.model_auto_compact_token_limit,
            config.approval_policy.value(),
            config.sandbox_policy.get().clone(),
            config.mcp_servers.keys().map(String::as_str).collect(),
            config.active_profile.clone(),
        );

        let mut default_shell = shell::default_user_shell();
        // Create the mutable state for the Session.
        if config.features.enabled(Feature::ShellSnapshot) {
            default_shell.shell_snapshot =
                ShellSnapshot::try_new(&config.codex_home, &default_shell)
                    .await
                    .map(Arc::new);
        }
        let state = SessionState::new(session_configuration.clone());

        let agent_registry = init_agent_registry(
            session_configuration.cwd.as_path(),
            session_configuration.subagent_discovery_overrides.clone(),
            Some(&otel_manager),
        );

        let services = SessionServices {
            mcp_connection_manager: Arc::new(RwLock::new(McpConnectionManager::default())),
            mcp_startup_cancellation_token: CancellationToken::new(),
            unified_exec_manager: UnifiedExecProcessManager::default(),
            notifier: UserNotifier::new(config.notify.clone()),
            rollout: Mutex::new(Some(rollout_recorder)),
            user_shell: Arc::new(default_shell),
            show_raw_agent_reasoning: config.show_raw_agent_reasoning,
            exec_policy,
            auth_manager: Arc::clone(&auth_manager),
            otel_manager,
            models_manager: Arc::clone(&models_manager),
            tool_approvals: Mutex::new(ApprovalStore::default()),
            skills_manager,
            agent_control,
            agent_registry,
            active_subagent_runtime: RwLock::new(None),
            subagent_transcripts: SubagentTranscriptStore::new(config.codex_home.clone()),
        };

        let sess = Arc::new(Session {
            conversation_id,
            tx_event: tx_event.clone(),
            agent_status: Arc::clone(&agent_status),
            state: Mutex::new(state),
            features: config.features.clone(),
            active_turn: Mutex::new(None),
            services,
            next_internal_sub_id: AtomicU64::new(0),
            agent_registration_done: AtomicBool::new(false),
        });

        // Dispatch the SessionConfiguredEvent first and then report any errors.
        // If resuming, include converted initial messages in the payload so UIs can render them immediately.
        let initial_messages = initial_history.get_event_msgs();
        let events = std::iter::once(Event {
            id: INITIAL_SUBMIT_ID.to_owned(),
            msg: EventMsg::SessionConfigured(SessionConfiguredEvent {
                session_id: conversation_id,
                model: session_configuration.model.clone(),
                model_provider_id: config.model_provider_id.clone(),
                approval_policy: session_configuration.approval_policy.value(),
                sandbox_policy: session_configuration.sandbox_policy.get().clone(),
                cwd: session_configuration.cwd.clone(),
                reasoning_effort: session_configuration.model_reasoning_effort,
                history_log_id,
                history_entry_count,
                initial_messages,
                rollout_path,
            }),
        })
        .chain(post_session_configured_events.into_iter());
        for event in events {
            sess.send_event_raw(event).await;
        }

        // Construct sandbox_state before initialize() so it can be sent to each
        // MCP server immediately after it becomes ready (avoiding blocking).
        let sandbox_state = SandboxState {
            sandbox_policy: session_configuration.sandbox_policy.get().clone(),
            codex_linux_sandbox_exe: config.codex_linux_sandbox_exe.clone(),
            sandbox_cwd: session_configuration.cwd.clone(),
        };
        sess.services
            .mcp_connection_manager
            .write()
            .await
            .initialize(
                config.mcp_servers.clone(),
                config.mcp_oauth_credentials_store_mode,
                auth_statuses.clone(),
                tx_event.clone(),
                sess.services.mcp_startup_cancellation_token.clone(),
                sandbox_state,
            )
            .await;

        // record_initial_history can emit events. We record only after the SessionConfiguredEvent is emitted.
        sess.record_initial_history(initial_history).await;

        Ok(sess)
    }

    pub(crate) fn get_tx_event(&self) -> Sender<Event> {
        self.tx_event.clone()
    }

    /// Ensure all rollout writes are durably flushed.
    pub(crate) async fn flush_rollout(&self) {
        let recorder = {
            let guard = self.services.rollout.lock().await;
            guard.clone()
        };
        if let Some(rec) = recorder
            && let Err(e) = rec.flush().await
        {
            warn!("failed to flush rollout recorder: {e}");
        }
    }

    fn next_internal_sub_id(&self) -> String {
        let id = self
            .next_internal_sub_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("auto-compact-{id}")
    }

    async fn get_total_token_usage(&self) -> i64 {
        let state = self.state.lock().await;
        state.get_total_token_usage()
    }

    async fn record_initial_history(&self, conversation_history: InitialHistory) {
        let turn_context = self.new_default_turn().await;
        match conversation_history {
            InitialHistory::New => {
                // Build and record initial items (user instructions + environment context)
                let items = self.build_initial_context(&turn_context);
                self.record_conversation_items(&turn_context, &items).await;
                // Ensure initial items are visible to immediate readers (e.g., tests, forks).
                self.flush_rollout().await;
            }
            InitialHistory::Resumed(_) | InitialHistory::Forked(_) => {
                let rollout_items = conversation_history.get_rollout_items();
                let persist = matches!(conversation_history, InitialHistory::Forked(_));

                // If resuming, warn when the last recorded model differs from the current one.
                if let InitialHistory::Resumed(_) = conversation_history
                    && let Some(prev) = rollout_items.iter().rev().find_map(|it| {
                        if let RolloutItem::TurnContext(ctx) = it {
                            Some(ctx.model.as_str())
                        } else {
                            None
                        }
                    })
                {
                    let curr = turn_context.client.get_model();
                    if prev != curr {
                        warn!(
                            "resuming session with different model: previous={prev}, current={curr}"
                        );
                        self.send_event(
                            &turn_context,
                            EventMsg::Warning(WarningEvent {
                                message: format!(
                                    "This session was recorded with model `{prev}` but is resuming with `{curr}`. \
                         Consider switching back to `{prev}` as it may affect Codex performance."
                                ),
                            }),
                        )
                            .await;
                    }
                }

                // Always add response items to conversation history
                let reconstructed_history =
                    self.reconstruct_history_from_rollout(&turn_context, &rollout_items);
                if !reconstructed_history.is_empty() {
                    self.record_into_history(&reconstructed_history, &turn_context)
                        .await;
                }

                // Seed usage info from the recorded rollout so UIs can show token counts
                // immediately on resume/fork.
                if let Some(info) = Self::last_token_info_from_rollout(&rollout_items) {
                    let mut state = self.state.lock().await;
                    state.set_token_info(Some(info));
                }

                // If persisting, persist all rollout items as-is (recorder filters)
                if persist && !rollout_items.is_empty() {
                    self.persist_rollout_items(&rollout_items).await;
                }
                // Flush after seeding history and any persisted rollout copy.
                self.flush_rollout().await;
            }
        }
    }

    fn last_token_info_from_rollout(rollout_items: &[RolloutItem]) -> Option<TokenUsageInfo> {
        rollout_items.iter().rev().find_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::TokenCount(ev)) => ev.info.clone(),
            _ => None,
        })
    }

    pub(crate) async fn update_settings(
        &self,
        updates: SessionSettingsUpdate,
    ) -> ConstraintResult<()> {
        let mut state = self.state.lock().await;

        match state.session_configuration.apply(&updates) {
            Ok(updated) => {
                state.session_configuration = updated;
                Ok(())
            }
            Err(err) => {
                warn!("rejected session settings update: {err}");
                Err(err)
            }
        }
    }

    pub(crate) async fn new_turn_with_sub_id(
        &self,
        sub_id: String,
        updates: SessionSettingsUpdate,
    ) -> ConstraintResult<Arc<TurnContext>> {
        let (session_configuration, sandbox_policy_changed) = {
            let mut state = self.state.lock().await;
            match state.session_configuration.clone().apply(&updates) {
                Ok(next) => {
                    let sandbox_policy_changed =
                        state.session_configuration.sandbox_policy != next.sandbox_policy;
                    state.session_configuration = next.clone();
                    (next, sandbox_policy_changed)
                }
                Err(err) => {
                    drop(state);
                    self.send_event_raw(Event {
                        id: sub_id.clone(),
                        msg: EventMsg::Error(ErrorEvent {
                            message: err.to_string(),
                            codex_error_info: Some(CodexErrorInfo::BadRequest),
                        }),
                    })
                    .await;
                    return Err(err);
                }
            }
        };

        Ok(self
            .new_turn_from_configuration(
                sub_id,
                session_configuration,
                updates.final_output_json_schema,
                sandbox_policy_changed,
            )
            .await)
    }

    async fn new_turn_from_configuration(
        &self,
        sub_id: String,
        session_configuration: SessionConfiguration,
        final_output_json_schema: Option<Option<Value>>,
        sandbox_policy_changed: bool,
    ) -> Arc<TurnContext> {
        let active_subagent_runtime = self.services.active_subagent().await;
        let (session_configuration, active_subagent_runtime) =
            Self::apply_active_subagent_overrides(session_configuration, active_subagent_runtime);

        let per_turn_config = Self::build_per_turn_config(&session_configuration);

        if sandbox_policy_changed {
            let sandbox_state = SandboxState {
                sandbox_policy: per_turn_config.sandbox_policy.get().clone(),
                codex_linux_sandbox_exe: per_turn_config.codex_linux_sandbox_exe.clone(),
                sandbox_cwd: per_turn_config.cwd.clone(),
            };
            if let Err(e) = self
                .services
                .mcp_connection_manager
                .read()
                .await
                .notify_sandbox_state_change(&sandbox_state)
                .await
            {
                warn!("Failed to notify sandbox state change to MCP servers: {e:#}");
            }
        }

        let model_info = self
            .services
            .models_manager
            .construct_model_info(session_configuration.model.as_str(), &per_turn_config)
            .await;
        let mut turn_context: TurnContext = Self::make_turn_context(
            Some(Arc::clone(&self.services.auth_manager)),
            &self.services.otel_manager,
            session_configuration.provider.clone(),
            &session_configuration,
            per_turn_config,
            model_info,
            self.conversation_id,
            sub_id,
            active_subagent_runtime,
        );
        self.ensure_subagents_registered(&turn_context.client).await;
        if let Some(final_schema) = final_output_json_schema {
            turn_context.final_output_json_schema = final_schema;
        }
        Arc::new(turn_context)
    }

    pub(crate) async fn new_default_turn(&self) -> Arc<TurnContext> {
        self.new_default_turn_with_sub_id(self.next_internal_sub_id())
            .await
    }

    pub(crate) async fn new_default_turn_with_sub_id(&self, sub_id: String) -> Arc<TurnContext> {
        let session_configuration = {
            let state = self.state.lock().await;
            state.session_configuration.clone()
        };
        self.new_turn_from_configuration(sub_id, session_configuration, None, false)
            .await
    }

    fn build_environment_update_item(
        &self,
        previous: Option<&Arc<TurnContext>>,
        next: &TurnContext,
    ) -> Option<ResponseItem> {
        let prev = previous?;

        let shell = self.user_shell();
        let prev_context = EnvironmentContext::from_turn_context(prev.as_ref(), shell.as_ref());
        let next_context = EnvironmentContext::from_turn_context(next, shell.as_ref());
        if prev_context.equals_except_shell(&next_context) {
            return None;
        }
        Some(ResponseItem::from(EnvironmentContext::diff(
            prev.as_ref(),
            next,
            shell.as_ref(),
        )))
    }

    /// Persist the event to rollout and send it to clients.
    pub(crate) async fn send_event(&self, turn_context: &TurnContext, msg: EventMsg) {
        let legacy_source = msg.clone();
        let event = Event {
            id: turn_context.sub_id.clone(),
            msg,
        };
        self.send_event_raw(event).await;

        let show_raw_agent_reasoning = self.show_raw_agent_reasoning();
        for legacy in legacy_source.as_legacy_events(show_raw_agent_reasoning) {
            let legacy_event = Event {
                id: turn_context.sub_id.clone(),
                msg: legacy,
            };
            self.send_event_raw(legacy_event).await;
        }
    }

    pub(crate) async fn send_event_raw(&self, event: Event) {
        // Record the last known agent status.
        if let Some(status) = agent_status_from_event(&event.msg) {
            let mut guard = self.agent_status.write().await;
            *guard = status;
        }
        // Persist the event into rollout (recorder filters as needed)
        let rollout_items = vec![RolloutItem::EventMsg(event.msg.clone())];
        self.persist_rollout_items(&rollout_items).await;
        if let Err(e) = self.tx_event.send(event).await {
            error!("failed to send tool call event: {e}");
        }
    }

    /// Persist the event to the rollout file, flush it, and only then deliver it to clients.
    ///
    /// Most events can be delivered immediately after queueing the rollout write, but some
    /// clients (e.g. app-server thread/rollback) re-read the rollout file synchronously on
    /// receipt of the event and depend on the marker already being visible on disk.
    pub(crate) async fn send_event_raw_flushed(&self, event: Event) {
        // Record the last known agent status.
        if let Some(status) = agent_status_from_event(&event.msg) {
            let mut guard = self.agent_status.write().await;
            *guard = status;
        }
        self.persist_rollout_items(&[RolloutItem::EventMsg(event.msg.clone())])
            .await;
        self.flush_rollout().await;
        if let Err(e) = self.tx_event.send(event).await {
            error!("failed to send tool call event: {e}");
        }
    }

    pub(crate) async fn emit_turn_item_started(&self, turn_context: &TurnContext, item: &TurnItem) {
        self.send_event(
            turn_context,
            EventMsg::ItemStarted(ItemStartedEvent {
                thread_id: self.conversation_id,
                turn_id: turn_context.sub_id.clone(),
                item: item.clone(),
            }),
        )
        .await;
    }

    pub(crate) async fn emit_turn_item_completed(
        &self,
        turn_context: &TurnContext,
        item: TurnItem,
    ) {
        self.send_event(
            turn_context,
            EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: self.conversation_id,
                turn_id: turn_context.sub_id.clone(),
                item,
            }),
        )
        .await;
    }

    /// Adds an execpolicy amendment to both the in-memory and on-disk policies so future
    /// commands can use the newly approved prefix.
    pub(crate) async fn persist_execpolicy_amendment(
        &self,
        amendment: &ExecPolicyAmendment,
    ) -> Result<(), ExecPolicyUpdateError> {
        let features = self.features.clone();
        let codex_home = self
            .state
            .lock()
            .await
            .session_configuration
            .original_config_do_not_use
            .codex_home
            .clone();

        if !features.enabled(Feature::ExecPolicy) {
            error!("attempted to append execpolicy rule while execpolicy feature is disabled");
            return Err(ExecPolicyUpdateError::FeatureDisabled);
        }

        self.services
            .exec_policy
            .append_amendment_and_update(&codex_home, amendment)
            .await?;

        Ok(())
    }

    /// Emit an exec approval request event and await the user's decision.
    ///
    /// The request is keyed by `sub_id`/`call_id` so matching responses are delivered
    /// to the correct in-flight turn. If the task is aborted, this returns the
    /// default `ReviewDecision` (`Denied`).
    #[allow(clippy::too_many_arguments)]
    pub async fn request_command_approval(
        &self,
        turn_context: &TurnContext,
        call_id: String,
        command: Vec<String>,
        cwd: PathBuf,
        reason: Option<String>,
        proposed_execpolicy_amendment: Option<ExecPolicyAmendment>,
    ) -> ReviewDecision {
        let sub_id = turn_context.sub_id.clone();
        // Add the tx_approve callback to the map before sending the request.
        let (tx_approve, rx_approve) = oneshot::channel();
        let event_id = sub_id.clone();
        let prev_entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_approval(sub_id, tx_approve)
                }
                None => None,
            }
        };
        if prev_entry.is_some() {
            warn!("Overwriting existing pending approval for sub_id: {event_id}");
        }

        let parsed_cmd = parse_command(&command);
        let event = EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
            call_id,
            turn_id: turn_context.sub_id.clone(),
            command,
            cwd,
            reason,
            proposed_execpolicy_amendment,
            parsed_cmd,
        });
        self.send_event(turn_context, event).await;
        rx_approve.await.unwrap_or_default()
    }

    pub async fn request_patch_approval(
        &self,
        turn_context: &TurnContext,
        call_id: String,
        changes: HashMap<PathBuf, FileChange>,
        reason: Option<String>,
        grant_root: Option<PathBuf>,
    ) -> oneshot::Receiver<ReviewDecision> {
        let sub_id = turn_context.sub_id.clone();
        // Add the tx_approve callback to the map before sending the request.
        let (tx_approve, rx_approve) = oneshot::channel();
        let event_id = sub_id.clone();
        let prev_entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_approval(sub_id, tx_approve)
                }
                None => None,
            }
        };
        if prev_entry.is_some() {
            warn!("Overwriting existing pending approval for sub_id: {event_id}");
        }

        let event = EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
            call_id,
            turn_id: turn_context.sub_id.clone(),
            changes,
            reason,
            grant_root,
        });
        self.send_event(turn_context, event).await;
        rx_approve
    }

    pub async fn notify_approval(&self, sub_id: &str, decision: ReviewDecision) {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_approval(sub_id)
                }
                None => None,
            }
        };
        match entry {
            Some(tx_approve) => {
                tx_approve.send(decision).ok();
            }
            None => {
                warn!("No pending approval found for sub_id: {sub_id}");
            }
        }
    }

    pub async fn resolve_elicitation(
        &self,
        server_name: String,
        id: RequestId,
        response: ElicitationResponse,
    ) -> anyhow::Result<()> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .resolve_elicitation(server_name, id, response)
            .await
    }

    /// Records input items: always append to conversation history and
    /// persist these response items to rollout.
    pub(crate) async fn record_conversation_items(
        &self,
        turn_context: &TurnContext,
        items: &[ResponseItem],
    ) {
        self.record_into_history(items, turn_context).await;
        self.persist_rollout_response_items(items).await;
        self.send_raw_response_items(turn_context, items).await;
    }

    async fn apply_resume_history(&self, agent_id: &str, token_str: &str) -> io::Result<()> {
        let token = SubagentResumeToken::decode(token_str)?;
        if token.agent_id != agent_id {
            return Err(io::Error::other(format!(
                "resume token targets `{}` but `{agent_id}` requested",
                token.agent_id
            )));
        }
        let responses = self
            .services
            .subagent_transcripts
            .resume_history(&token)
            .await?;
        if responses.is_empty() {
            return Ok(());
        }
        {
            let mut state = self.state.lock().await;
            state.record_items(responses.iter(), TruncationPolicy::Tokens(usize::MAX));
        }
        self.persist_rollout_response_items(&responses).await;
        Ok(())
    }

    fn reconstruct_history_from_rollout(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
    ) -> Vec<ResponseItem> {
        let mut history = ContextManager::new();
        for item in rollout_items {
            match item {
                RolloutItem::ResponseItem(response_item) => {
                    history.record_items(
                        std::iter::once(response_item),
                        turn_context.truncation_policy,
                    );
                }
                RolloutItem::Compacted(compacted) => {
                    if let Some(replacement) = &compacted.replacement_history {
                        history.replace(replacement.clone());
                    } else {
                        let user_messages = collect_user_messages(history.raw_items());
                        let rebuilt = compact::build_compacted_history(
                            self.build_initial_context(turn_context),
                            &user_messages,
                            &compacted.message,
                        );
                        history.replace(rebuilt);
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                    history.drop_last_n_user_turns(rollback.num_turns);
                }
                _ => {}
            }
        }
        history.raw_items().to_vec()
    }

    /// Append ResponseItems to the in-memory conversation history only.
    pub(crate) async fn record_into_history(
        &self,
        items: &[ResponseItem],
        turn_context: &TurnContext,
    ) {
        let mut state = self.state.lock().await;
        state.record_items(items.iter(), turn_context.truncation_policy);
    }

    pub(crate) async fn record_model_warning(&self, message: impl Into<String>, ctx: &TurnContext) {
        let item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!("Warning: {}", message.into()),
            }],
        };

        self.record_conversation_items(ctx, &[item]).await;
    }

    pub(crate) async fn replace_history(&self, items: Vec<ResponseItem>) {
        let mut state = self.state.lock().await;
        state.replace_history(items);
    }

    async fn persist_rollout_response_items(&self, items: &[ResponseItem]) {
        let rollout_items: Vec<RolloutItem> = items
            .iter()
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect();
        self.persist_rollout_items(&rollout_items).await;
    }

    pub fn enabled(&self, feature: Feature) -> bool {
        self.features.enabled(feature)
    }

    pub(crate) fn features(&self) -> Features {
        self.features.clone()
    }

    async fn send_raw_response_items(&self, turn_context: &TurnContext, items: &[ResponseItem]) {
        for item in items {
            self.send_event(
                turn_context,
                EventMsg::RawResponseItem(RawResponseItemEvent { item: item.clone() }),
            )
            .await;
        }
    }

    pub(crate) fn build_initial_context(&self, turn_context: &TurnContext) -> Vec<ResponseItem> {
        let mut items = Vec::<ResponseItem>::with_capacity(3);
        let shell = self.user_shell();
        if let Some(developer_instructions) = turn_context.developer_instructions.as_deref() {
            items.push(DeveloperInstructions::new(developer_instructions.to_string()).into());
        }
        if let Some(user_instructions) = turn_context.user_instructions.as_deref() {
            items.push(
                UserInstructions {
                    text: user_instructions.to_string(),
                    directory: turn_context.cwd.to_string_lossy().into_owned(),
                }
                .into(),
            );
        }
        items.push(ResponseItem::from(EnvironmentContext::new(
            Some(turn_context.cwd.clone()),
            Some(turn_context.approval_policy),
            Some(turn_context.sandbox_policy.clone()),
            shell.as_ref().clone(),
        )));
        items
    }

    pub(crate) async fn persist_rollout_items(&self, items: &[RolloutItem]) {
        let recorder = {
            let guard = self.services.rollout.lock().await;
            guard.clone()
        };
        if let Some(rec) = recorder
            && let Err(e) = rec.record_items(items).await
        {
            error!("failed to record rollout items: {e:#}");
        }
        if !items.is_empty()
            && let Some(active) = self.services.active_subagent().await
            && let Some(transcript) = active.transcript()
            && let Err(err) = transcript.record_items(items).await
        {
            warn!(
                error = %err,
                "failed to record subagent transcript items"
            );
        }
    }

    pub(crate) async fn clone_history(&self) -> ContextManager {
        let state = self.state.lock().await;
        state.clone_history()
    }

    pub(crate) async fn update_token_usage_info(
        &self,
        turn_context: &TurnContext,
        token_usage: Option<&TokenUsage>,
    ) {
        {
            let mut state = self.state.lock().await;
            if let Some(token_usage) = token_usage {
                state.update_token_info_from_usage(
                    token_usage,
                    turn_context.client.get_model_context_window(),
                );
            }
        }
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn recompute_token_usage(&self, turn_context: &TurnContext) {
        let Some(estimated_total_tokens) = self
            .clone_history()
            .await
            .estimate_token_count(turn_context)
        else {
            return;
        };
        {
            let mut state = self.state.lock().await;
            let mut info = state.token_info().unwrap_or(TokenUsageInfo {
                total_token_usage: TokenUsage::default(),
                last_token_usage: TokenUsage::default(),
                model_context_window: None,
            });

            info.last_token_usage = TokenUsage {
                input_tokens: 0,
                cached_input_tokens: 0,
                output_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: estimated_total_tokens.max(0),
            };

            if info.model_context_window.is_none() {
                info.model_context_window = turn_context.client.get_model_context_window();
            }

            state.set_token_info(Some(info));
        }
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn update_rate_limits(
        &self,
        turn_context: &TurnContext,
        new_rate_limits: RateLimitSnapshot,
    ) {
        {
            let mut state = self.state.lock().await;
            state.set_rate_limits(new_rate_limits);
        }
        self.send_token_count_event(turn_context).await;
    }

    async fn send_token_count_event(&self, turn_context: &TurnContext) {
        let (info, rate_limits) = {
            let state = self.state.lock().await;
            state.token_info_and_rate_limits()
        };
        let event = EventMsg::TokenCount(TokenCountEvent { info, rate_limits });
        self.send_event(turn_context, event).await;
    }

    pub(crate) async fn set_total_tokens_full(&self, turn_context: &TurnContext) {
        if let Some(context_window) = turn_context.client.get_model_context_window() {
            let mut state = self.state.lock().await;
            state.set_token_usage_full(context_window);
        }
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn record_response_item_and_emit_turn_item(
        &self,
        turn_context: &TurnContext,
        response_item: ResponseItem,
    ) {
        // Add to conversation history and persist response item to rollout.
        self.record_conversation_items(turn_context, std::slice::from_ref(&response_item))
            .await;

        // Derive a turn item and emit lifecycle events if applicable.
        if let Some(item) = parse_turn_item(&response_item) {
            self.emit_turn_item_started(turn_context, &item).await;
            self.emit_turn_item_completed(turn_context, item).await;
        }
    }

    pub(crate) async fn notify_background_event(
        &self,
        turn_context: &TurnContext,
        message: impl Into<String>,
    ) {
        let event = EventMsg::BackgroundEvent(BackgroundEventEvent {
            message: message.into(),
        });
        self.send_event(turn_context, event).await;
    }

    pub(crate) async fn notify_stream_error(
        &self,
        turn_context: &TurnContext,
        message: impl Into<String>,
        codex_error: CodexErr,
    ) {
        let additional_details = codex_error.to_string();
        let codex_error_info = CodexErrorInfo::ResponseStreamDisconnected {
            http_status_code: codex_error.http_status_code_value(),
        };
        let event = EventMsg::StreamError(StreamErrorEvent {
            message: message.into(),
            codex_error_info: Some(codex_error_info),
            additional_details: Some(additional_details),
        });
        self.send_event(turn_context, event).await;
    }

    async fn maybe_start_ghost_snapshot(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        cancellation_token: CancellationToken,
    ) {
        if !self.enabled(Feature::GhostCommit) {
            return;
        }
        let token = match turn_context.tool_call_gate.subscribe().await {
            Ok(token) => token,
            Err(err) => {
                warn!("failed to subscribe to ghost snapshot readiness: {err}");
                return;
            }
        };

        info!("spawning ghost snapshot task");
        let task = GhostSnapshotTask::new(token);
        Arc::new(task)
            .run(
                Arc::new(SessionTaskContext::new(self.clone())),
                turn_context.clone(),
                Vec::new(),
                cancellation_token,
            )
            .await;
    }

    /// Returns the input if there was no task running to inject into
    pub async fn inject_input(&self, input: Vec<UserInput>) -> Result<(), Vec<UserInput>> {
        let mut active = self.active_turn.lock().await;
        match active.as_mut() {
            Some(at) => {
                let mut ts = at.turn_state.lock().await;
                ts.push_pending_input(input.into());
                Ok(())
            }
            None => Err(input),
        }
    }

    pub async fn get_pending_input(&self) -> Vec<ResponseInputItem> {
        let mut active = self.active_turn.lock().await;
        match active.as_mut() {
            Some(at) => {
                let mut ts = at.turn_state.lock().await;
                ts.take_pending_input()
            }
            None => Vec::with_capacity(0),
        }
    }

    pub async fn list_resources(
        &self,
        server: &str,
        params: Option<ListResourcesRequestParams>,
    ) -> anyhow::Result<ListResourcesResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .list_resources(server, params)
            .await
    }

    pub async fn list_resource_templates(
        &self,
        server: &str,
        params: Option<ListResourceTemplatesRequestParams>,
    ) -> anyhow::Result<ListResourceTemplatesResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .list_resource_templates(server, params)
            .await
    }

    pub async fn read_resource(
        &self,
        server: &str,
        params: ReadResourceRequestParams,
    ) -> anyhow::Result<ReadResourceResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .read_resource(server, params)
            .await
    }

    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Option<serde_json::Value>,
    ) -> anyhow::Result<CallToolResult> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .call_tool(server, tool, arguments)
            .await
    }

    pub(crate) async fn parse_mcp_tool_name(&self, tool_name: &str) -> Option<(String, String)> {
        self.services
            .mcp_connection_manager
            .read()
            .await
            .parse_tool_name(tool_name)
            .await
    }

    pub async fn interrupt_task(self: &Arc<Self>) {
        info!("interrupt received: abort current task, if any");
        let has_active_turn = { self.active_turn.lock().await.is_some() };
        if has_active_turn {
            self.abort_all_tasks(TurnAbortReason::Interrupted).await;
        } else {
            self.cancel_mcp_startup().await;
        }
    }

    pub(crate) fn notifier(&self) -> &UserNotifier {
        &self.services.notifier
    }

    pub(crate) fn user_shell(&self) -> Arc<shell::Shell> {
        Arc::clone(&self.services.user_shell)
    }

    fn show_raw_agent_reasoning(&self) -> bool {
        self.services.show_raw_agent_reasoning
    }

    async fn cancel_mcp_startup(&self) {
        self.services.mcp_startup_cancellation_token.cancel();
    }
}

async fn submission_loop(sess: Arc<Session>, config: Arc<Config>, rx_sub: Receiver<Submission>) {
    // Seed with context in case there is an OverrideTurnContext first.
    let mut previous_context: Option<Arc<TurnContext>> = Some(sess.new_default_turn().await);

    // To break out of this loop, send Op::Shutdown.
    while let Ok(sub) = rx_sub.recv().await {
        debug!(?sub, "Submission");
        match sub.op.clone() {
            Op::Interrupt => {
                handlers::interrupt(&sess).await;
            }
            Op::OverrideTurnContext {
                cwd,
                approval_policy,
                sandbox_policy,
                model,
                effort,
                summary,
                subagent,
            } => {
                handlers::override_turn_context(
                    &sess,
                    sub.id.clone(),
                    SessionSettingsUpdate {
                        cwd,
                        approval_policy,
                        sandbox_policy,
                        model,
                        reasoning_effort: effort,
                        reasoning_summary: summary,
                        subagent,
                        ..Default::default()
                    },
                )
                .await;
            }
            Op::UserInput { .. } | Op::UserTurn { .. } => {
                handlers::user_input_or_turn(&sess, sub.id.clone(), sub.op, &mut previous_context)
                    .await;
            }
            Op::ExecApproval { id, decision } => {
                handlers::exec_approval(&sess, id, decision).await;
            }
            Op::PatchApproval { id, decision } => {
                handlers::patch_approval(&sess, id, decision).await;
            }
            Op::AddToHistory { text } => {
                handlers::add_to_history(&sess, &config, text).await;
            }
            Op::GetHistoryEntryRequest { offset, log_id } => {
                handlers::get_history_entry_request(&sess, &config, sub.id.clone(), offset, log_id)
                    .await;
            }
            Op::ListMcpTools => {
                handlers::list_mcp_tools(&sess, &config, sub.id.clone()).await;
            }
            Op::ListCustomPrompts => {
                handlers::list_custom_prompts(&sess, sub.id.clone()).await;
            }
            Op::ListSkills { cwds, force_reload } => {
                handlers::list_skills(&sess, sub.id.clone(), cwds, force_reload).await;
            }
            Op::Undo => {
                handlers::undo(&sess, sub.id.clone()).await;
            }
            Op::Compact => {
                handlers::compact(&sess, sub.id.clone()).await;
            }
            Op::ThreadRollback { num_turns } => {
                handlers::thread_rollback(&sess, sub.id.clone(), num_turns).await;
            }
            Op::RunUserShellCommand { command } => {
                handlers::run_user_shell_command(
                    &sess,
                    sub.id.clone(),
                    command,
                    &mut previous_context,
                )
                .await;
            }
            Op::ResolveElicitation {
                server_name,
                request_id,
                decision,
            } => {
                handlers::resolve_elicitation(&sess, server_name, request_id, decision).await;
            }
            Op::Shutdown => {
                if handlers::shutdown(&sess, sub.id.clone()).await {
                    break;
                }
            }
            Op::Review { review_request } => {
                handlers::review(&sess, &config, sub.id.clone(), review_request).await;
            }
            _ => {} // Ignore unknown ops; enum is non_exhaustive to allow extensions.
        }
    }
    debug!("Agent loop exited");
}

/// Operation handlers
mod handlers {
    use crate::agent::ActivationError;
    use crate::codex::Session;
    use crate::codex::SessionSettingsUpdate;
    use crate::codex::TurnContext;

    use crate::codex::spawn_review_thread;
    use crate::config::Config;

    use crate::mcp::auth::compute_auth_statuses;
    use crate::mcp::collect_mcp_snapshot_from_manager;
    use crate::review_prompts::resolve_review_request;
    use crate::subagents::hooks::HookInvocation;
    use crate::subagents::hooks::HookPhase;
    use crate::subagents::hooks::run_subagent_hooks;
    use crate::tasks::CompactTask;
    use crate::tasks::RegularTask;
    use crate::tasks::UndoTask;
    use crate::tasks::UserShellCommandTask;
    use crate::tools::sandboxing::HookSignal;
    use codex_protocol::custom_prompts::CustomPrompt;
    use codex_protocol::protocol::CodexErrorInfo;
    use codex_protocol::protocol::ErrorEvent;
    use codex_protocol::protocol::Event;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::ListCustomPromptsResponseEvent;
    use codex_protocol::protocol::ListSkillsResponseEvent;
    use codex_protocol::protocol::Op;
    use codex_protocol::protocol::ReviewDecision;
    use codex_protocol::protocol::ReviewRequest;
    use codex_protocol::protocol::SkillsListEntry;
    use codex_protocol::protocol::SubagentOverride;
    use codex_protocol::protocol::ThreadRolledBackEvent;
    use codex_protocol::protocol::TurnAbortReason;
    use codex_protocol::protocol::WarningEvent;

    use crate::context_manager::is_user_turn_boundary;
    use codex_protocol::user_input::UserInput;
    use codex_rmcp_client::ElicitationAction;
    use codex_rmcp_client::ElicitationResponse;
    use mcp_types::RequestId;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tracing::info;
    use tracing::warn;

    pub async fn interrupt(sess: &Arc<Session>) {
        sess.interrupt_task().await;
    }

    pub async fn override_turn_context(
        sess: &Arc<Session>,
        sub_id: String,
        mut updates: SessionSettingsUpdate,
    ) {
        let subagent_override = updates.subagent.take();
        let override_for_error = subagent_override.clone();
        if let Err(err) = sess.update_settings(updates).await {
            if let Some(subagent) = override_for_error {
                let message = err.to_string();
                log_override_from_settings_failure(sess, &subagent, message.as_str()).await;
            }
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: err.to_string(),
                    codex_error_info: Some(CodexErrorInfo::BadRequest),
                }),
            })
            .await;
            return;
        }

        if let Some(subagent) = subagent_override
            && let Err(err) = apply_subagent_override(sess, &sub_id, subagent).await
        {
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: err.to_string(),
                    codex_error_info: Some(CodexErrorInfo::BadRequest),
                }),
            })
            .await;
        }
    }

    async fn apply_subagent_override(
        sess: &Arc<Session>,
        sub_id: &str,
        subagent: SubagentOverride,
    ) -> Result<(), ActivationError> {
        match subagent {
            SubagentOverride::Clear { origin } => {
                let active_runtime = sess.services.active_subagent().await;
                if let Some(runtime) = active_runtime {
                    let runtime_ref = runtime.runtime();
                    let manifest = runtime_ref.manifest();
                    let cleared_agent_id = manifest.id.clone();
                    let cleared_scope = manifest.source.clone();
                    clear_active_subagent_with_hooks(sess, sub_id, "clear").await;
                    emit_subagent_switch_event(
                        sess,
                        SubagentSwitchAction::Clear,
                        Some(cleared_agent_id.as_str()),
                        cleared_scope.as_ref(),
                        origin,
                        SubagentSwitchStatus::Success,
                        None,
                    )
                    .await;
                } else {
                    emit_subagent_switch_event(
                        sess,
                        SubagentSwitchAction::Clear,
                        None,
                        None,
                        origin,
                        SubagentSwitchStatus::NoActiveRuntime,
                        None,
                    )
                    .await;
                }
                Ok(())
            }
            SubagentOverride::Activate { id, resume, origin } => {
                if is_subagent_active(sess, &id).await {
                    emit_subagent_switch_event(
                        sess,
                        SubagentSwitchAction::Activate,
                        Some(id.as_str()),
                        None,
                        origin,
                        SubagentSwitchStatus::AlreadyActive,
                        None,
                    )
                    .await;
                    return Ok(());
                }
                if let Err(err) = ensure_agent_known(sess, &id).await {
                    let message = err.to_string();
                    emit_subagent_switch_event(
                        sess,
                        SubagentSwitchAction::Activate,
                        Some(id.as_str()),
                        None,
                        origin,
                        SubagentSwitchStatus::Error,
                        Some(message.as_str()),
                    )
                    .await;
                    return Err(err);
                }
                clear_active_subagent_with_hooks(sess, sub_id, "switch").await;
                match sess.activate_subagent(&id, resume.map(|r| r.token)).await {
                    Ok(runtime) => {
                        let snapshot = sess.services.active_subagent().await;
                        emit_subagent_lifecycle_event(
                            sess,
                            sub_id,
                            runtime.as_ref(),
                            snapshot,
                            codex_protocol::protocol::SubagentLifecyclePhase::Activated,
                            None,
                        )
                        .await;
                        emit_subagent_switch_event(
                            sess,
                            SubagentSwitchAction::Activate,
                            Some(id.as_str()),
                            runtime.manifest().source.as_ref(),
                            origin,
                            SubagentSwitchStatus::Success,
                            None,
                        )
                        .await;
                        Ok(())
                    }
                    Err(err) => {
                        let message = err.to_string();
                        emit_subagent_switch_event(
                            sess,
                            SubagentSwitchAction::Activate,
                            Some(id.as_str()),
                            None,
                            origin,
                            SubagentSwitchStatus::Error,
                            Some(message.as_str()),
                        )
                        .await;
                        Err(err)
                    }
                }
            }
        }
    }

    async fn emit_subagent_lifecycle_event(
        sess: &Arc<Session>,
        sub_id: &str,
        runtime: &crate::agent::AgentRuntimeProfile,
        active_runtime: Option<crate::state::ActiveSubagentRuntime>,
        phase: codex_protocol::protocol::SubagentLifecyclePhase,
        resume_token: Option<String>,
    ) {
        let sandbox_policy = {
            let state = sess.state.lock().await;
            state.session_configuration.sandbox_policy.get().clone()
        };
        let manifest = runtime.manifest();
        let agent_id = manifest.id.clone();
        let manifest_digest = manifest.digest.clone();
        let approval_policy = runtime.approval_policy();
        let model = runtime.model().to_owned();
        let session_source = runtime.session_source().clone();
        let tool_scope = codex_protocol::protocol::SubagentToolScope {
            mode: if manifest.tool_scope.is_inherit() {
                codex_protocol::protocol::SubagentToolScopeMode::Inherit
            } else {
                codex_protocol::protocol::SubagentToolScopeMode::Restricted
            },
            tools: active_runtime
                .as_ref()
                .map(|snapshot| snapshot.allowed_tools().to_vec())
                .unwrap_or_else(|| runtime.allowed_tools().to_vec()),
        };

        sess.services.otel_manager.subagent_lifecycle(
            phase,
            agent_id.as_str(),
            manifest_digest.as_deref(),
            approval_policy,
            &sandbox_policy,
            resume_token.as_deref(),
        );

        let event = codex_protocol::protocol::SubagentLifecycleEvent {
            thread_id: sess.conversation_id,
            call_id: sub_id.to_string(),
            phase,
            agent_id,
            agent_name: Some(manifest.name.clone()),
            manifest_digest,
            model: Some(model),
            session_source: Some(session_source),
            tool_scope: Some(tool_scope),
            approval_policy,
            sandbox_policy,
            resume_token,
        };
        sess.send_event_raw(Event {
            id: sub_id.to_string(),
            msg: EventMsg::SubagentLifecycle(event),
        })
        .await;
    }

    #[derive(Clone, Copy)]
    enum SubagentSwitchAction {
        Activate,
        Clear,
    }

    impl SubagentSwitchAction {
        fn as_str(&self) -> &'static str {
            match self {
                Self::Activate => "activate",
                Self::Clear => "clear",
            }
        }
    }

    #[derive(Clone, Copy)]
    enum SubagentSwitchStatus {
        Success,
        AlreadyActive,
        NoActiveRuntime,
        Error,
    }

    impl SubagentSwitchStatus {
        fn as_str(&self) -> &'static str {
            match self {
                Self::Success => "success",
                Self::AlreadyActive => "already_active",
                Self::NoActiveRuntime => "no_active_runtime",
                Self::Error => "error",
            }
        }
    }

    async fn emit_subagent_switch_event(
        sess: &Arc<Session>,
        action: SubagentSwitchAction,
        agent_id: Option<&str>,
        scope: Option<&codex_subagent::DiscoveryScope>,
        origin: Option<codex_protocol::protocol::SubagentOverrideOrigin>,
        status: SubagentSwitchStatus,
        error: Option<&str>,
    ) {
        let session_source = {
            let state = sess.state.lock().await;
            state.session_configuration.session_source.clone()
        };
        let scope_label = scope
            .map(crate::agent::registry::telemetry_scope_label)
            .map(String::from);
        sess.services.otel_manager.subagent_switch(
            action.as_str(),
            status.as_str(),
            &session_source,
            agent_id,
            scope_label.as_deref(),
            origin.as_ref(),
            error,
        );
    }

    async fn log_override_from_settings_failure(
        sess: &Arc<Session>,
        subagent: &SubagentOverride,
        error: &str,
    ) {
        match subagent {
            SubagentOverride::Clear { origin } => {
                emit_subagent_switch_event(
                    sess,
                    SubagentSwitchAction::Clear,
                    None,
                    None,
                    *origin,
                    SubagentSwitchStatus::Error,
                    Some(error),
                )
                .await;
            }
            SubagentOverride::Activate { id, origin, .. } => {
                emit_subagent_switch_event(
                    sess,
                    SubagentSwitchAction::Activate,
                    Some(id.as_str()),
                    None,
                    *origin,
                    SubagentSwitchStatus::Error,
                    Some(error),
                )
                .await;
            }
        }
    }

    async fn ensure_agent_known(
        sess: &Arc<Session>,
        agent_id: &str,
    ) -> Result<(), ActivationError> {
        let registry = sess.services.agent_registry.read().await;
        if registry.has_agent(agent_id) {
            Ok(())
        } else {
            Err(ActivationError::UnknownAgent {
                agent_id: agent_id.to_string(),
            })
        }
    }

    async fn is_subagent_active(sess: &Arc<Session>, agent_id: &str) -> bool {
        sess.services
            .active_subagent()
            .await
            .map(|runtime| runtime.runtime().manifest().id.clone())
            .as_deref()
            == Some(agent_id)
    }

    async fn clear_active_subagent_with_hooks(sess: &Arc<Session>, sub_id: &str, reason: &str) {
        let Some(active_runtime) = sess.services.active_subagent().await else {
            return;
        };

        let hook_turn = sess
            .new_default_turn_with_sub_id(format!("{sub_id}-subagent-stop"))
            .await;
        let invocation = HookInvocation::for_session_event(
            Arc::clone(sess),
            hook_turn,
            None,
            format!("{reason}-{sub_id}"),
        );
        run_subagent_hooks(HookPhase::Stop, &invocation, &HookSignal::pending()).await;
        let runtime = active_runtime.runtime();
        let cached_snapshot = Some(active_runtime.clone());
        let resume_token = match active_runtime.finish_transcript().await {
            Ok(token) => token,
            Err(err) => {
                warn!(
                    error = %err,
                    "failed to finalize subagent transcript during clear"
                );
                None
            }
        };
        emit_subagent_lifecycle_event(
            sess,
            sub_id,
            runtime.as_ref(),
            cached_snapshot,
            codex_protocol::protocol::SubagentLifecyclePhase::Stopped,
            resume_token,
        )
        .await;
        info!(
            agent_id = %active_runtime.runtime().manifest().id,
            "cleared active subagent runtime"
        );
        sess.clear_active_subagent().await;
    }

    pub async fn user_input_or_turn(
        sess: &Arc<Session>,
        sub_id: String,
        op: Op,
        previous_context: &mut Option<Arc<TurnContext>>,
    ) {
        let (items, updates) = match op {
            Op::UserTurn {
                cwd,
                approval_policy,
                sandbox_policy,
                model,
                effort,
                summary,
                final_output_json_schema,
                items,
            } => (
                items,
                SessionSettingsUpdate {
                    cwd: Some(cwd),
                    approval_policy: Some(approval_policy),
                    sandbox_policy: Some(sandbox_policy),
                    model: Some(model),
                    reasoning_effort: Some(effort),
                    reasoning_summary: Some(summary),
                    final_output_json_schema: Some(final_output_json_schema),
                    subagent: None,
                },
            ),
            Op::UserInput {
                items,
                final_output_json_schema,
            } => (
                items,
                SessionSettingsUpdate {
                    final_output_json_schema: Some(final_output_json_schema),
                    subagent: None,
                    ..Default::default()
                },
            ),
            _ => unreachable!(),
        };

        let Ok(current_context) = sess.new_turn_with_sub_id(sub_id, updates).await else {
            // new_turn_with_sub_id already emits the error event.
            return;
        };
        current_context
            .client
            .get_otel_manager()
            .user_prompt(&items);

        // Attempt to inject input into current task
        if let Err(items) = sess.inject_input(items).await {
            if let Some(env_item) =
                sess.build_environment_update_item(previous_context.as_ref(), &current_context)
            {
                sess.record_conversation_items(&current_context, std::slice::from_ref(&env_item))
                    .await;
            }

            sess.spawn_task(Arc::clone(&current_context), items, RegularTask)
                .await;
            *previous_context = Some(current_context);
        }
    }

    pub async fn run_user_shell_command(
        sess: &Arc<Session>,
        sub_id: String,
        command: String,
        previous_context: &mut Option<Arc<TurnContext>>,
    ) {
        let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;
        sess.spawn_task(
            Arc::clone(&turn_context),
            Vec::new(),
            UserShellCommandTask::new(command),
        )
        .await;
        *previous_context = Some(turn_context);
    }

    pub async fn resolve_elicitation(
        sess: &Arc<Session>,
        server_name: String,
        request_id: RequestId,
        decision: codex_protocol::approvals::ElicitationAction,
    ) {
        let action = match decision {
            codex_protocol::approvals::ElicitationAction::Accept => ElicitationAction::Accept,
            codex_protocol::approvals::ElicitationAction::Decline => ElicitationAction::Decline,
            codex_protocol::approvals::ElicitationAction::Cancel => ElicitationAction::Cancel,
        };
        let response = ElicitationResponse {
            action,
            content: None,
        };
        if let Err(err) = sess
            .resolve_elicitation(server_name, request_id, response)
            .await
        {
            warn!(
                error = %err,
                "failed to resolve elicitation request in session"
            );
        }
    }

    /// Propagate a user's exec approval decision to the session.
    /// Also optionally applies an execpolicy amendment.
    pub async fn exec_approval(sess: &Arc<Session>, id: String, decision: ReviewDecision) {
        if let ReviewDecision::ApprovedExecpolicyAmendment {
            proposed_execpolicy_amendment,
        } = &decision
            && let Err(err) = sess
                .persist_execpolicy_amendment(proposed_execpolicy_amendment)
                .await
        {
            let message = format!("Failed to apply execpolicy amendment: {err}");
            tracing::warn!("{message}");
            let warning = EventMsg::Warning(WarningEvent { message });
            sess.send_event_raw(Event {
                id: id.clone(),
                msg: warning,
            })
            .await;
        }
        match decision {
            ReviewDecision::Abort => {
                sess.interrupt_task().await;
            }
            other => sess.notify_approval(&id, other).await,
        }
    }

    pub async fn patch_approval(sess: &Arc<Session>, id: String, decision: ReviewDecision) {
        match decision {
            ReviewDecision::Abort => {
                sess.interrupt_task().await;
            }
            other => sess.notify_approval(&id, other).await,
        }
    }

    pub async fn add_to_history(sess: &Arc<Session>, config: &Arc<Config>, text: String) {
        let id = sess.conversation_id;
        let config = Arc::clone(config);
        tokio::spawn(async move {
            if let Err(e) = crate::message_history::append_entry(&text, &id, &config).await {
                warn!("failed to append to message history: {e}");
            }
        });
    }

    pub async fn get_history_entry_request(
        sess: &Arc<Session>,
        config: &Arc<Config>,
        sub_id: String,
        offset: usize,
        log_id: u64,
    ) {
        let config = Arc::clone(config);
        let sess_clone = Arc::clone(sess);

        tokio::spawn(async move {
            // Run lookup in blocking thread because it does file IO + locking.
            let entry_opt = tokio::task::spawn_blocking(move || {
                crate::message_history::lookup(log_id, offset, &config)
            })
            .await
            .unwrap_or(None);

            let event = Event {
                id: sub_id,
                msg: EventMsg::GetHistoryEntryResponse(
                    crate::protocol::GetHistoryEntryResponseEvent {
                        offset,
                        log_id,
                        entry: entry_opt.map(|e| codex_protocol::message_history::HistoryEntry {
                            conversation_id: e.session_id,
                            ts: e.ts,
                            text: e.text,
                        }),
                    },
                ),
            };

            sess_clone.send_event_raw(event).await;
        });
    }

    pub async fn list_mcp_tools(sess: &Session, config: &Arc<Config>, sub_id: String) {
        let mcp_connection_manager = sess.services.mcp_connection_manager.read().await;
        let snapshot = collect_mcp_snapshot_from_manager(
            &mcp_connection_manager,
            compute_auth_statuses(
                config.mcp_servers.iter(),
                config.mcp_oauth_credentials_store_mode,
            )
            .await,
        )
        .await;
        let event = Event {
            id: sub_id,
            msg: EventMsg::McpListToolsResponse(snapshot),
        };
        sess.send_event_raw(event).await;
    }

    pub async fn list_custom_prompts(sess: &Session, sub_id: String) {
        let custom_prompts: Vec<CustomPrompt> =
            if let Some(dir) = crate::custom_prompts::default_prompts_dir() {
                crate::custom_prompts::discover_prompts_in(&dir).await
            } else {
                Vec::new()
            };

        let event = Event {
            id: sub_id,
            msg: EventMsg::ListCustomPromptsResponse(ListCustomPromptsResponseEvent {
                custom_prompts,
            }),
        };
        sess.send_event_raw(event).await;
    }

    pub async fn list_skills(
        sess: &Session,
        sub_id: String,
        cwds: Vec<PathBuf>,
        force_reload: bool,
    ) {
        let cwds = if cwds.is_empty() {
            let state = sess.state.lock().await;
            vec![state.session_configuration.cwd.clone()]
        } else {
            cwds
        };

        let skills_manager = &sess.services.skills_manager;
        let mut skills = Vec::new();
        for cwd in cwds {
            let outcome = skills_manager.skills_for_cwd(&cwd, force_reload).await;
            let errors = super::errors_to_info(&outcome.errors);
            let skills_metadata = super::skills_to_info(&outcome.skills);
            skills.push(SkillsListEntry {
                cwd,
                skills: skills_metadata,
                errors,
            });
        }

        let event = Event {
            id: sub_id,
            msg: EventMsg::ListSkillsResponse(ListSkillsResponseEvent { skills }),
        };
        sess.send_event_raw(event).await;
    }

    pub async fn undo(sess: &Arc<Session>, sub_id: String) {
        let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;
        sess.spawn_task(turn_context, Vec::new(), UndoTask::new())
            .await;
    }

    pub async fn compact(sess: &Arc<Session>, sub_id: String) {
        let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;

        sess.spawn_task(
            Arc::clone(&turn_context),
            vec![UserInput::Text {
                text: turn_context.compact_prompt().to_string(),
            }],
            CompactTask,
        )
        .await;
    }

    pub async fn thread_rollback(sess: &Arc<Session>, sub_id: String, num_turns: u32) {
        if num_turns == 0 {
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: "num_turns must be >= 1".to_string(),
                    codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
                }),
            })
            .await;
            return;
        }

        let has_active_turn = { sess.active_turn.lock().await.is_some() };
        if has_active_turn {
            sess.send_event_raw(Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    message: "Cannot rollback while a turn is in progress.".to_string(),
                    codex_error_info: Some(CodexErrorInfo::ThreadRollbackFailed),
                }),
            })
            .await;
            return;
        }

        let turn_context = sess.new_default_turn_with_sub_id(sub_id).await;

        let mut history = sess.clone_history().await;
        history.drop_last_n_user_turns(num_turns);

        // Replace with the raw items. We don't want to replace with a normalized
        // version of the history.
        sess.replace_history(history.raw_items().to_vec()).await;
        sess.recompute_token_usage(turn_context.as_ref()).await;

        sess.send_event_raw_flushed(Event {
            id: turn_context.sub_id.clone(),
            msg: EventMsg::ThreadRolledBack(ThreadRolledBackEvent { num_turns }),
        })
        .await;
    }

    pub async fn shutdown(sess: &Arc<Session>, sub_id: String) -> bool {
        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;
        sess.services
            .unified_exec_manager
            .terminate_all_processes()
            .await;
        info!("Shutting down Codex instance");
        let history = sess.clone_history().await;
        let turn_count = history
            .raw_items()
            .iter()
            .filter(|item| is_user_turn_boundary(item))
            .count();
        sess.services.otel_manager.counter(
            "conversation.turn.count",
            i64::try_from(turn_count).unwrap_or(0),
            &[],
        );

        // Gracefully flush and shutdown rollout recorder on session end so tests
        // that inspect the rollout file do not race with the background writer.
        let recorder_opt = {
            let mut guard = sess.services.rollout.lock().await;
            guard.take()
        };
        if let Some(rec) = recorder_opt
            && let Err(e) = rec.shutdown().await
        {
            warn!("failed to shutdown rollout recorder: {e}");
            let event = Event {
                id: sub_id.clone(),
                msg: EventMsg::Error(ErrorEvent {
                    message: "Failed to shutdown rollout recorder".to_string(),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            };
            sess.send_event_raw(event).await;
        }

        let event = Event {
            id: sub_id,
            msg: EventMsg::ShutdownComplete,
        };
        sess.send_event_raw(event).await;
        true
    }

    pub async fn review(
        sess: &Arc<Session>,
        config: &Arc<Config>,
        sub_id: String,
        review_request: ReviewRequest,
    ) {
        let turn_context = sess.new_default_turn_with_sub_id(sub_id.clone()).await;
        match resolve_review_request(review_request, turn_context.cwd.as_path()) {
            Ok(resolved) => {
                spawn_review_thread(
                    Arc::clone(sess),
                    Arc::clone(config),
                    turn_context.clone(),
                    sub_id,
                    resolved,
                )
                .await;
            }
            Err(err) => {
                let event = Event {
                    id: sub_id,
                    msg: EventMsg::Error(ErrorEvent {
                        message: err.to_string(),
                        codex_error_info: Some(CodexErrorInfo::Other),
                    }),
                };
                sess.send_event(&turn_context, event.msg).await;
            }
        }
    }
}

/// Spawn a review thread using the given prompt.
async fn spawn_review_thread(
    sess: Arc<Session>,
    config: Arc<Config>,
    parent_turn_context: Arc<TurnContext>,
    sub_id: String,
    resolved: crate::review_prompts::ResolvedReviewRequest,
) {
    let model = config.review_model.clone();
    let review_model_info = sess
        .services
        .models_manager
        .construct_model_info(&model, &config)
        .await;
    // For reviews, disable web_search and view_image regardless of global settings.
    let mut review_features = sess.features.clone();
    review_features
        .disable(crate::features::Feature::WebSearchRequest)
        .disable(crate::features::Feature::WebSearchCached);
    let tools_config = ToolsConfig::new(&ToolsConfigParams {
        model_info: &review_model_info,
        features: &review_features,
    });

    let base_instructions = REVIEW_PROMPT.to_string();
    let review_prompt = resolved.prompt.clone();
    let provider = parent_turn_context.client.get_provider();
    let auth_manager = parent_turn_context.client.get_auth_manager();
    let model_info = review_model_info.clone();

    // Build per‑turn client with the requested model/family.
    let mut per_turn_config = (*config).clone();
    per_turn_config.model_reasoning_effort = Some(ReasoningEffortConfig::Low);
    per_turn_config.model_reasoning_summary = ReasoningSummaryConfig::Detailed;
    per_turn_config.features = review_features.clone();

    let otel_manager = parent_turn_context.client.get_otel_manager().with_model(
        config.review_model.as_str(),
        review_model_info.slug.as_str(),
    );

    let per_turn_config = Arc::new(per_turn_config);
    let client = ModelClient::new(
        per_turn_config.clone(),
        auth_manager,
        model_info.clone(),
        otel_manager,
        provider,
        per_turn_config.model_reasoning_effort,
        per_turn_config.model_reasoning_summary,
        sess.conversation_id,
        parent_turn_context.client.get_session_source(),
    );

    let review_turn_context = TurnContext {
        sub_id: sub_id.to_string(),
        client,
        tools_config,
        ghost_snapshot: parent_turn_context.ghost_snapshot.clone(),
        developer_instructions: None,
        user_instructions: None,
        base_instructions: Some(base_instructions.clone()),
        compact_prompt: parent_turn_context.compact_prompt.clone(),
        approval_policy: parent_turn_context.approval_policy,
        sandbox_policy: parent_turn_context.sandbox_policy.clone(),
        shell_environment_policy: parent_turn_context.shell_environment_policy.clone(),
        cwd: parent_turn_context.cwd.clone(),
        final_output_json_schema: None,
        codex_linux_sandbox_exe: parent_turn_context.codex_linux_sandbox_exe.clone(),
        tool_call_gate: Arc::new(ReadinessFlag::new()),
        truncation_policy: model_info.truncation_policy.into(),
        active_subagent: None,
    };

    // Seed the child task with the review prompt as the initial user message.
    let input: Vec<UserInput> = vec![UserInput::Text {
        text: review_prompt,
    }];
    let tc = Arc::new(review_turn_context);
    sess.spawn_task(tc.clone(), input, ReviewTask::new()).await;

    // Announce entering review mode so UIs can switch modes.
    let review_request = ReviewRequest {
        target: resolved.target,
        user_facing_hint: Some(resolved.user_facing_hint),
    };
    sess.send_event(&tc, EventMsg::EnteredReviewMode(review_request))
        .await;
}

fn skills_to_info(skills: &[SkillMetadata]) -> Vec<ProtocolSkillMetadata> {
    skills
        .iter()
        .map(|skill| ProtocolSkillMetadata {
            name: skill.name.clone(),
            description: skill.description.clone(),
            short_description: skill.short_description.clone(),
            path: skill.path.clone(),
            scope: skill.scope,
        })
        .collect()
}

fn errors_to_info(errors: &[SkillError]) -> Vec<SkillErrorInfo> {
    errors
        .iter()
        .map(|err| SkillErrorInfo {
            path: err.path.clone(),
            message: err.message.clone(),
        })
        .collect()
}

/// Takes a user message as input and runs a loop where, at each turn, the model
/// replies with either:
///
/// - requested function calls
/// - an assistant message
///
/// While it is possible for the model to return multiple of these items in a
/// single turn, in practice, we generally one item per turn:
///
/// - If the model requests a function call, we execute it and send the output
///   back to the model in the next turn.
/// - If the model sends only an assistant message, we record it in the
///   conversation history and consider the task complete.
///
pub(crate) async fn run_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
    cancellation_token: CancellationToken,
) -> Option<String> {
    if input.is_empty() {
        return None;
    }

    let model_info = turn_context.client.get_model_info();
    let auto_compact_limit = model_info.auto_compact_token_limit().unwrap_or(i64::MAX);
    let total_usage_tokens = sess.get_total_token_usage().await;
    if total_usage_tokens >= auto_compact_limit {
        run_auto_compact(&sess, &turn_context).await;
    }
    let event = EventMsg::TaskStarted(TaskStartedEvent {
        model_context_window: turn_context.client.get_model_context_window(),
    });
    sess.send_event(&turn_context, event).await;

    let skills_outcome = Some(
        sess.services
            .skills_manager
            .skills_for_cwd(&turn_context.cwd, false)
            .await,
    );

    let SkillInjections {
        items: skill_items,
        warnings: skill_warnings,
    } = build_skill_injections(&input, skills_outcome.as_ref()).await;

    for message in skill_warnings {
        sess.send_event(&turn_context, EventMsg::Warning(WarningEvent { message }))
            .await;
    }

    let initial_input_for_turn: ResponseInputItem = ResponseInputItem::from(input);
    let response_item: ResponseItem = initial_input_for_turn.clone().into();
    sess.record_response_item_and_emit_turn_item(turn_context.as_ref(), response_item)
        .await;

    if !skill_items.is_empty() {
        sess.record_conversation_items(&turn_context, &skill_items)
            .await;
    }

    sess.maybe_start_ghost_snapshot(Arc::clone(&turn_context), cancellation_token.child_token())
        .await;
    let mut last_agent_message: Option<String> = None;
    // Although from the perspective of codex.rs, TurnDiffTracker has the lifecycle of a Task which contains
    // many turns, from the perspective of the user, it is a single turn.
    let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

    loop {
        // Note that pending_input would be something like a message the user
        // submitted through the UI while the model was running. Though the UI
        // may support this, the model might not.
        let pending_input = sess
            .get_pending_input()
            .await
            .into_iter()
            .map(ResponseItem::from)
            .collect::<Vec<ResponseItem>>();

        // Construct the input that we will send to the model.
        let turn_input: Vec<ResponseItem> = {
            sess.record_conversation_items(&turn_context, &pending_input)
                .await;
            sess.clone_history().await.for_prompt()
        };

        let turn_input_messages = turn_input
            .iter()
            .filter_map(|item| match parse_turn_item(item) {
                Some(TurnItem::UserMessage(user_message)) => Some(user_message),
                _ => None,
            })
            .map(|user_message| user_message.message())
            .collect::<Vec<String>>();
        match run_turn(
            Arc::clone(&sess),
            Arc::clone(&turn_context),
            Arc::clone(&turn_diff_tracker),
            turn_input,
            cancellation_token.child_token(),
        )
        .await
        {
            Ok(turn_output) => {
                let TurnRunResult {
                    needs_follow_up,
                    last_agent_message: turn_last_agent_message,
                } = turn_output;
                let total_usage_tokens = sess.get_total_token_usage().await;
                let token_limit_reached = total_usage_tokens >= auto_compact_limit;

                // as long as compaction works well in getting us way below the token limit, we shouldn't worry about being in an infinite loop.
                if token_limit_reached && needs_follow_up {
                    run_auto_compact(&sess, &turn_context).await;
                    continue;
                }

                if !needs_follow_up {
                    last_agent_message = turn_last_agent_message;
                    sess.notifier()
                        .notify(&UserNotification::AgentTurnComplete {
                            thread_id: sess.conversation_id.to_string(),
                            turn_id: turn_context.sub_id.clone(),
                            cwd: turn_context.cwd.display().to_string(),
                            input_messages: turn_input_messages,
                            last_assistant_message: last_agent_message.clone(),
                        });
                    break;
                }
                continue;
            }
            Err(CodexErr::TurnAborted) => {
                // Aborted turn is reported via a different event.
                break;
            }
            Err(CodexErr::InvalidImageRequest()) => {
                let mut state = sess.state.lock().await;
                error_or_panic(
                    "Invalid image detected, replacing it in the last turn to prevent poisoning",
                );
                state.history.replace_last_turn_images("Invalid image");
            }
            Err(e) => {
                info!("Turn error: {e:#}");
                let event = EventMsg::Error(e.to_error_event(None));
                sess.send_event(&turn_context, event).await;
                // let the user continue the conversation
                break;
            }
        }
    }

    last_agent_message
}

async fn run_auto_compact(sess: &Arc<Session>, turn_context: &Arc<TurnContext>) {
    if should_use_remote_compact_task(sess.as_ref(), &turn_context.client.get_provider()) {
        run_inline_remote_auto_compact_task(Arc::clone(sess), Arc::clone(turn_context)).await;
    } else {
        run_inline_auto_compact_task(Arc::clone(sess), Arc::clone(turn_context)).await;
    }
}

#[instrument(level = "trace",
    skip_all,
    fields(
        turn_id = %turn_context.sub_id,
        model = %turn_context.client.get_model(),
        cwd = %turn_context.cwd.display()
    )
)]
async fn run_turn(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    turn_diff_tracker: SharedTurnDiffTracker,
    input: Vec<ResponseItem>,
    cancellation_token: CancellationToken,
) -> CodexResult<TurnRunResult> {
    let mcp_tools = sess
        .services
        .mcp_connection_manager
        .read()
        .await
        .list_all_tools()
        .or_cancel(&cancellation_token)
        .await?;
    let router = Arc::new(ToolRouter::from_config(
        &turn_context.tools_config,
        Some(
            mcp_tools
                .into_iter()
                .map(|(name, tool)| (name, tool.tool))
                .collect(),
        ),
        turn_context.allowed_tool_names(),
    ));

    let model_supports_parallel = turn_context
        .client
        .get_model_info()
        .supports_parallel_tool_calls;

    let prompt = Prompt {
        input,
        tools: router.specs(),
        parallel_tool_calls: model_supports_parallel,
        base_instructions_override: turn_context.base_instructions.clone(),
        output_schema: turn_context.final_output_json_schema.clone(),
    };

    let mut retries = 0;
    loop {
        let err = match try_run_turn(
            Arc::clone(&router),
            Arc::clone(&sess),
            Arc::clone(&turn_context),
            Arc::clone(&turn_diff_tracker),
            &prompt,
            cancellation_token.child_token(),
        )
        .await
        {
            Ok(output) => return Ok(output),
            Err(CodexErr::ContextWindowExceeded) => {
                sess.set_total_tokens_full(&turn_context).await;
                return Err(CodexErr::ContextWindowExceeded);
            }
            Err(CodexErr::UsageLimitReached(e)) => {
                let rate_limits = e.rate_limits.clone();
                if let Some(rate_limits) = rate_limits {
                    sess.update_rate_limits(&turn_context, rate_limits).await;
                }
                return Err(CodexErr::UsageLimitReached(e));
            }
            Err(err) => err,
        };

        if !err.is_retryable() {
            return Err(err);
        }

        // Use the configured provider-specific stream retry budget.
        let max_retries = turn_context.client.get_provider().stream_max_retries();
        if retries < max_retries {
            retries += 1;
            let delay = match &err {
                CodexErr::Stream(_, requested_delay) => {
                    requested_delay.unwrap_or_else(|| backoff(retries))
                }
                _ => backoff(retries),
            };
            warn!("stream disconnected - retrying turn ({retries}/{max_retries} in {delay:?})...",);

            // Surface retry information to any UI/front‑end so the
            // user understands what is happening instead of staring
            // at a seemingly frozen screen.
            sess.notify_stream_error(
                &turn_context,
                format!("Reconnecting... {retries}/{max_retries}"),
                err,
            )
            .await;

            tokio::time::sleep(delay).await;
        } else {
            return Err(err);
        }
    }
}

#[derive(Debug)]
struct TurnRunResult {
    needs_follow_up: bool,
    last_agent_message: Option<String>,
}

async fn drain_in_flight(
    in_flight: &mut FuturesOrdered<BoxFuture<'static, CodexResult<ResponseInputItem>>>,
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) -> CodexResult<()> {
    while let Some(res) = in_flight.next().await {
        match res {
            Ok(response_input) => {
                sess.record_conversation_items(&turn_context, &[response_input.into()])
                    .await;
            }
            Err(err) => {
                error_or_panic(format!("in-flight tool future failed during drain: {err}"));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = "trace",
    skip_all,
    fields(
        turn_id = %turn_context.sub_id,
        model = %turn_context.client.get_model()
    )
)]
async fn try_run_turn(
    router: Arc<ToolRouter>,
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    turn_diff_tracker: SharedTurnDiffTracker,
    prompt: &Prompt,
    cancellation_token: CancellationToken,
) -> CodexResult<TurnRunResult> {
    let rollout_item = RolloutItem::TurnContext(TurnContextItem {
        cwd: turn_context.cwd.clone(),
        approval_policy: turn_context.approval_policy,
        sandbox_policy: turn_context.sandbox_policy.clone(),
        model: turn_context.client.get_model(),
        effort: turn_context.client.get_reasoning_effort(),
        summary: turn_context.client.get_reasoning_summary(),
        base_instructions: turn_context.base_instructions.clone(),
        user_instructions: turn_context.user_instructions.clone(),
        developer_instructions: turn_context.developer_instructions.clone(),
        final_output_json_schema: turn_context.final_output_json_schema.clone(),
        truncation_policy: Some(turn_context.truncation_policy.into()),
    });

    feedback_tags!(
        model = turn_context.client.get_model(),
        approval_policy = turn_context.approval_policy,
        sandbox_policy = turn_context.sandbox_policy,
        effort = turn_context.client.get_reasoning_effort(),
        auth_mode = sess.services.auth_manager.get_auth_mode(),
        features = sess.features.enabled_features(),
    );

    sess.persist_rollout_items(&[rollout_item]).await;
    let mut stream = turn_context
        .client
        .clone()
        .stream(prompt)
        .instrument(trace_span!("stream_request"))
        .or_cancel(&cancellation_token)
        .await??;

    let tool_runtime = ToolCallRuntime::new(
        Arc::clone(&router),
        Arc::clone(&sess),
        Arc::clone(&turn_context),
        Arc::clone(&turn_diff_tracker),
    );
    let mut in_flight: FuturesOrdered<BoxFuture<'static, CodexResult<ResponseInputItem>>> =
        FuturesOrdered::new();
    let mut needs_follow_up = false;
    let mut last_agent_message: Option<String> = None;
    let mut active_item: Option<TurnItem> = None;
    let mut should_emit_turn_diff = false;
    let receiving_span = trace_span!("receiving_stream");
    let outcome: CodexResult<TurnRunResult> = loop {
        let handle_responses = trace_span!(
            parent: &receiving_span,
            "handle_responses",
            otel.name = field::Empty,
            tool_name = field::Empty,
            from = field::Empty,
        );

        let event = match stream
            .next()
            .instrument(trace_span!(parent: &handle_responses, "receiving"))
            .or_cancel(&cancellation_token)
            .await
        {
            Ok(event) => event,
            Err(codex_async_utils::CancelErr::Cancelled) => break Err(CodexErr::TurnAborted),
        };

        let event = match event {
            Some(res) => res?,
            None => {
                break Err(CodexErr::Stream(
                    "stream closed before response.completed".into(),
                    None,
                ));
            }
        };

        sess.services
            .otel_manager
            .record_responses(&handle_responses, &event);

        match event {
            ResponseEvent::Created => {}
            ResponseEvent::OutputItemDone(item) => {
                let previously_active_item = active_item.take();
                let mut ctx = HandleOutputCtx {
                    sess: sess.clone(),
                    turn_context: turn_context.clone(),
                    tool_runtime: tool_runtime.clone(),
                    cancellation_token: cancellation_token.child_token(),
                };

                let output_result = handle_output_item_done(&mut ctx, item, previously_active_item)
                    .instrument(handle_responses)
                    .await?;
                if let Some(tool_future) = output_result.tool_future {
                    in_flight.push_back(tool_future);
                }
                if let Some(agent_message) = output_result.last_agent_message {
                    last_agent_message = Some(agent_message);
                }
                needs_follow_up |= output_result.needs_follow_up;
            }
            ResponseEvent::OutputItemAdded(item) => {
                if let Some(turn_item) = handle_non_tool_response_item(&item).await {
                    let tracked_item = turn_item.clone();
                    sess.emit_turn_item_started(&turn_context, &turn_item).await;

                    active_item = Some(tracked_item);
                }
            }
            ResponseEvent::RateLimits(snapshot) => {
                // Update internal state with latest rate limits, but defer sending until
                // token usage is available to avoid duplicate TokenCount events.
                sess.update_rate_limits(&turn_context, snapshot).await;
            }
            ResponseEvent::ModelsEtag(etag) => {
                // Update internal state with latest models etag
                sess.services
                    .models_manager
                    .refresh_if_new_etag(etag, sess.features.enabled(Feature::RemoteModels))
                    .await;
            }
            ResponseEvent::Completed {
                response_id: _,
                token_usage,
            } => {
                sess.update_token_usage_info(&turn_context, token_usage.as_ref())
                    .await;
                should_emit_turn_diff = true;

                break Ok(TurnRunResult {
                    needs_follow_up,
                    last_agent_message,
                });
            }
            ResponseEvent::OutputTextDelta(delta) => {
                // In review child threads, suppress assistant text deltas; the
                // UI will show a selection popup from the final ReviewOutput.
                if let Some(active) = active_item.as_ref() {
                    let event = AgentMessageContentDeltaEvent {
                        thread_id: sess.conversation_id.to_string(),
                        turn_id: turn_context.sub_id.clone(),
                        item_id: active.id(),
                        delta: delta.clone(),
                    };
                    sess.send_event(&turn_context, EventMsg::AgentMessageContentDelta(event))
                        .await;
                } else {
                    error_or_panic("OutputTextDelta without active item".to_string());
                }
            }
            ResponseEvent::ReasoningSummaryDelta {
                delta,
                summary_index,
            } => {
                if let Some(active) = active_item.as_ref() {
                    let event = ReasoningContentDeltaEvent {
                        thread_id: sess.conversation_id.to_string(),
                        turn_id: turn_context.sub_id.clone(),
                        item_id: active.id(),
                        delta,
                        summary_index,
                    };
                    sess.send_event(&turn_context, EventMsg::ReasoningContentDelta(event))
                        .await;
                } else {
                    error_or_panic("ReasoningSummaryDelta without active item".to_string());
                }
            }
            ResponseEvent::ReasoningSummaryPartAdded { summary_index } => {
                if let Some(active) = active_item.as_ref() {
                    let event =
                        EventMsg::AgentReasoningSectionBreak(AgentReasoningSectionBreakEvent {
                            item_id: active.id(),
                            summary_index,
                        });
                    sess.send_event(&turn_context, event).await;
                } else {
                    error_or_panic("ReasoningSummaryPartAdded without active item".to_string());
                }
            }
            ResponseEvent::ReasoningContentDelta {
                delta,
                content_index,
            } => {
                if let Some(active) = active_item.as_ref() {
                    let event = ReasoningRawContentDeltaEvent {
                        thread_id: sess.conversation_id.to_string(),
                        turn_id: turn_context.sub_id.clone(),
                        item_id: active.id(),
                        delta,
                        content_index,
                    };
                    sess.send_event(&turn_context, EventMsg::ReasoningRawContentDelta(event))
                        .await;
                } else {
                    error_or_panic("ReasoningRawContentDelta without active item".to_string());
                }
            }
        }
    };

    drain_in_flight(&mut in_flight, sess.clone(), turn_context.clone()).await?;

    if should_emit_turn_diff {
        let unified_diff = {
            let mut tracker = turn_diff_tracker.lock().await;
            tracker.get_unified_diff()
        };
        if let Ok(Some(unified_diff)) = unified_diff {
            let msg = EventMsg::TurnDiff(TurnDiffEvent { unified_diff });
            sess.clone().send_event(&turn_context, msg).await;
        }
    }

    outcome
}

pub(super) fn get_last_assistant_message_from_turn(responses: &[ResponseItem]) -> Option<String> {
    responses.iter().rev().find_map(|item| {
        if let ResponseItem::Message { role, content, .. } = item {
            if role == "assistant" {
                content.iter().rev().find_map(|ci| {
                    if let ContentItem::OutputText { text } = ci {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        } else {
            None
        }
    })
}

fn init_agent_registry(
    cwd: &Path,
    overrides: Option<SubagentDiscoveryOverrides>,
    otel: Option<&OtelManager>,
) -> Arc<RwLock<AgentRegistry>> {
    let loader = Arc::new(FsManifestLoader::new());
    let mut registry = AgentRegistry::new(loader);
    let targets = match overrides.as_ref() {
        Some(overrides) => build_discovery_targets(&DiscoveryTargetArgs {
            cwd,
            project_dir_override: None,
            user_dir_override: None,
            overrides,
        })
        .unwrap_or_else(|err| {
            warn!(
                error = %err,
                "failed to apply CLI subagent overrides; falling back to defaults"
            );
            AgentRegistry::default_targets(cwd)
        }),
        None => AgentRegistry::default_targets(cwd),
    };
    match registry.refresh_with_telemetry(&targets, otel, RefreshInvocation::SessionStartup) {
        Ok(RefreshOutcome { report, issues }) => {
            info!(
                custom = report.custom_manifests(),
                built_ins = report.built_in_manifests,
                duplicates = report.skipped_duplicates,
                issues = issues.len(),
                scope_cli = report.scope_breakdown.cli,
                scope_plugin = report.scope_breakdown.plugin,
                scope_project = report.scope_breakdown.project,
                scope_user = report.scope_breakdown.user,
                "loaded subagent manifests"
            );
            if !issues.is_empty() {
                for issue in issues {
                    let path = issue.path_label().unwrap_or_else(|| "<unknown>".into());
                    let scope = issue
                        .scope_label()
                        .unwrap_or_else(|| "unknown scope".into());
                    warn!(
                        path = %path,
                        scope = %scope,
                        "manifest validation issue: {}",
                        issue.message
                    );
                }
            }
        }
        Err(err) => {
            let error_message = err.to_string();
            warn!(
                "failed to load subagent manifests from {}: {error_message}",
                cwd.display()
            );
        }
    }
    Arc::new(RwLock::new(registry))
}

#[cfg(test)]
pub(crate) use tests::make_session_and_context;

use crate::git_info::get_git_repo_root;
#[cfg(test)]
pub(crate) use tests::make_session_and_context_with_rx;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CodexAuth;
    use crate::config::ConfigBuilder;
    use crate::exec::ExecToolCallOutput;
    use crate::function_tool::FunctionCallError;
    use crate::shell::default_user_shell;
    use crate::tools::format_exec_output_str;

    use codex_protocol::models::FunctionCallOutputPayload;

    use crate::protocol::CompactedItem;
    use crate::protocol::CreditsSnapshot;
    use crate::protocol::InitialHistory;
    use crate::protocol::RateLimitSnapshot;
    use crate::protocol::RateLimitWindow;
    use crate::protocol::ResumedHistory;
    use crate::protocol::TokenCountEvent;
    use crate::protocol::TokenUsage;
    use crate::protocol::TokenUsageInfo;
    use crate::state::TaskKind;
    use crate::tasks::SessionTask;
    use crate::tasks::SessionTaskContext;
    use crate::tools::ToolRouter;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolOutput;
    use crate::tools::context::ToolPayload;
    use crate::tools::handlers::ShellHandler;
    use crate::tools::handlers::UnifiedExecHandler;
    use crate::tools::registry::ToolHandler;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_app_server_protocol::AuthMode;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use std::path::Path;
    use std::time::Duration;
    use tokio::time::sleep;

    use mcp_types::ContentBlock;
    use mcp_types::TextContent;
    use pretty_assertions::assert_eq;
    use serde::Deserialize;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration as StdDuration;

    #[tokio::test]
    async fn reconstruct_history_matches_live_compactions() {
        let (session, turn_context) = make_session_and_context().await;
        let (rollout_items, expected) = sample_rollout(&session, &turn_context);

        let reconstructed = session.reconstruct_history_from_rollout(&turn_context, &rollout_items);

        assert_eq!(expected, reconstructed);
    }

    #[tokio::test]
    async fn record_initial_history_reconstructs_resumed_transcript() {
        let (session, turn_context) = make_session_and_context().await;
        let (rollout_items, expected) = sample_rollout(&session, &turn_context);

        session
            .record_initial_history(InitialHistory::Resumed(ResumedHistory {
                conversation_id: ThreadId::default(),
                history: rollout_items,
                rollout_path: PathBuf::from("/tmp/resume.jsonl"),
            }))
            .await;

        let history = session.state.lock().await.clone_history();
        assert_eq!(expected, history.raw_items());
    }

    #[tokio::test]
    async fn record_initial_history_seeds_token_info_from_rollout() {
        let (session, turn_context) = make_session_and_context().await;
        let (mut rollout_items, _expected) = sample_rollout(&session, &turn_context);

        let info1 = TokenUsageInfo {
            total_token_usage: TokenUsage {
                input_tokens: 10,
                cached_input_tokens: 0,
                output_tokens: 20,
                reasoning_output_tokens: 0,
                total_tokens: 30,
            },
            last_token_usage: TokenUsage {
                input_tokens: 3,
                cached_input_tokens: 0,
                output_tokens: 4,
                reasoning_output_tokens: 0,
                total_tokens: 7,
            },
            model_context_window: Some(1_000),
        };
        let info2 = TokenUsageInfo {
            total_token_usage: TokenUsage {
                input_tokens: 100,
                cached_input_tokens: 50,
                output_tokens: 200,
                reasoning_output_tokens: 25,
                total_tokens: 375,
            },
            last_token_usage: TokenUsage {
                input_tokens: 10,
                cached_input_tokens: 0,
                output_tokens: 20,
                reasoning_output_tokens: 5,
                total_tokens: 35,
            },
            model_context_window: Some(2_000),
        };

        rollout_items.push(RolloutItem::EventMsg(EventMsg::TokenCount(
            TokenCountEvent {
                info: Some(info1),
                rate_limits: None,
            },
        )));
        rollout_items.push(RolloutItem::EventMsg(EventMsg::TokenCount(
            TokenCountEvent {
                info: None,
                rate_limits: None,
            },
        )));
        rollout_items.push(RolloutItem::EventMsg(EventMsg::TokenCount(
            TokenCountEvent {
                info: Some(info2.clone()),
                rate_limits: None,
            },
        )));
        rollout_items.push(RolloutItem::EventMsg(EventMsg::TokenCount(
            TokenCountEvent {
                info: None,
                rate_limits: None,
            },
        )));

        session
            .record_initial_history(InitialHistory::Resumed(ResumedHistory {
                conversation_id: ThreadId::default(),
                history: rollout_items,
                rollout_path: PathBuf::from("/tmp/resume.jsonl"),
            }))
            .await;

        let actual = session.state.lock().await.token_info();
        assert_eq!(actual, Some(info2));
    }

    #[tokio::test]
    async fn record_initial_history_reconstructs_forked_transcript() {
        let (session, turn_context) = make_session_and_context().await;
        let (rollout_items, expected) = sample_rollout(&session, &turn_context);

        session
            .record_initial_history(InitialHistory::Forked(rollout_items))
            .await;

        let history = session.state.lock().await.clone_history();
        assert_eq!(expected, history.raw_items());
    }

    #[tokio::test]
    async fn thread_rollback_drops_last_turn_from_history() {
        let (sess, tc, rx) = make_session_and_context_with_rx().await;

        let initial_context = sess.build_initial_context(tc.as_ref());
        sess.record_into_history(&initial_context, tc.as_ref())
            .await;

        let turn_1 = vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "turn 1 user".to_string(),
                }],
            },
            ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "turn 1 assistant".to_string(),
                }],
            },
        ];
        sess.record_into_history(&turn_1, tc.as_ref()).await;

        let turn_2 = vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "turn 2 user".to_string(),
                }],
            },
            ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "turn 2 assistant".to_string(),
                }],
            },
        ];
        sess.record_into_history(&turn_2, tc.as_ref()).await;

        handlers::thread_rollback(&sess, "sub-1".to_string(), 1).await;

        let rollback_event = wait_for_thread_rolled_back(&rx).await;
        assert_eq!(rollback_event.num_turns, 1);

        let mut expected = Vec::new();
        expected.extend(initial_context);
        expected.extend(turn_1);

        let history = sess.clone_history().await;
        assert_eq!(expected, history.raw_items());
    }

    #[tokio::test]
    async fn thread_rollback_clears_history_when_num_turns_exceeds_existing_turns() {
        let (sess, tc, rx) = make_session_and_context_with_rx().await;

        let initial_context = sess.build_initial_context(tc.as_ref());
        sess.record_into_history(&initial_context, tc.as_ref())
            .await;

        let turn_1 = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "turn 1 user".to_string(),
            }],
        }];
        sess.record_into_history(&turn_1, tc.as_ref()).await;

        handlers::thread_rollback(&sess, "sub-1".to_string(), 99).await;

        let rollback_event = wait_for_thread_rolled_back(&rx).await;
        assert_eq!(rollback_event.num_turns, 99);

        let history = sess.clone_history().await;
        assert_eq!(initial_context, history.raw_items());
    }

    #[tokio::test]
    async fn thread_rollback_fails_when_turn_in_progress() {
        let (sess, tc, rx) = make_session_and_context_with_rx().await;

        let initial_context = sess.build_initial_context(tc.as_ref());
        sess.record_into_history(&initial_context, tc.as_ref())
            .await;

        *sess.active_turn.lock().await = Some(crate::state::ActiveTurn::default());
        handlers::thread_rollback(&sess, "sub-1".to_string(), 1).await;

        let error_event = wait_for_thread_rollback_failed(&rx).await;
        assert_eq!(
            error_event.codex_error_info,
            Some(CodexErrorInfo::ThreadRollbackFailed)
        );

        let history = sess.clone_history().await;
        assert_eq!(initial_context, history.raw_items());
    }

    #[tokio::test]
    async fn thread_rollback_fails_when_num_turns_is_zero() {
        let (sess, tc, rx) = make_session_and_context_with_rx().await;

        let initial_context = sess.build_initial_context(tc.as_ref());
        sess.record_into_history(&initial_context, tc.as_ref())
            .await;

        handlers::thread_rollback(&sess, "sub-1".to_string(), 0).await;

        let error_event = wait_for_thread_rollback_failed(&rx).await;
        assert_eq!(error_event.message, "num_turns must be >= 1");
        assert_eq!(
            error_event.codex_error_info,
            Some(CodexErrorInfo::ThreadRollbackFailed)
        );

        let history = sess.clone_history().await;
        assert_eq!(initial_context, history.raw_items());
    }

    #[tokio::test]
    async fn set_rate_limits_retains_previous_credits() {
        let codex_home = tempfile::tempdir().expect("create temp dir");
        let config = build_test_config(codex_home.path()).await;
        let config = Arc::new(config);
        let model = ModelsManager::get_model_offline(config.model.as_deref());
        let session_configuration = SessionConfiguration {
            provider: config.model_provider.clone(),
            model,
            model_reasoning_effort: config.model_reasoning_effort,
            model_reasoning_summary: config.model_reasoning_summary,
            developer_instructions: config.developer_instructions.clone(),
            user_instructions: config.user_instructions.clone(),
            base_instructions: config.base_instructions.clone(),
            compact_prompt: config.compact_prompt.clone(),
            subagent_discovery_overrides: config.subagent_discovery_overrides.clone(),
            approval_policy: config.approval_policy.clone(),
            sandbox_policy: config.sandbox_policy.clone(),
            cwd: config.cwd.clone(),
            original_config_do_not_use: Arc::clone(&config),
            session_source: SessionSource::Exec,
        };

        let mut state = SessionState::new(session_configuration);
        let initial = RateLimitSnapshot {
            primary: Some(RateLimitWindow {
                used_percent: 10.0,
                window_minutes: Some(15),
                resets_at: Some(1_700),
            }),
            secondary: None,
            credits: Some(CreditsSnapshot {
                has_credits: true,
                unlimited: false,
                balance: Some("10.00".to_string()),
            }),
            plan_type: Some(codex_protocol::account::PlanType::Plus),
        };
        state.set_rate_limits(initial.clone());

        let update = RateLimitSnapshot {
            primary: Some(RateLimitWindow {
                used_percent: 40.0,
                window_minutes: Some(30),
                resets_at: Some(1_800),
            }),
            secondary: Some(RateLimitWindow {
                used_percent: 5.0,
                window_minutes: Some(60),
                resets_at: Some(1_900),
            }),
            credits: None,
            plan_type: None,
        };
        state.set_rate_limits(update.clone());

        assert_eq!(
            state.latest_rate_limits,
            Some(RateLimitSnapshot {
                primary: update.primary.clone(),
                secondary: update.secondary,
                credits: initial.credits,
                plan_type: initial.plan_type,
            })
        );
    }

    #[tokio::test]
    async fn set_rate_limits_updates_plan_type_when_present() {
        let codex_home = tempfile::tempdir().expect("create temp dir");
        let config = build_test_config(codex_home.path()).await;
        let config = Arc::new(config);
        let model = ModelsManager::get_model_offline(config.model.as_deref());
        let session_configuration = SessionConfiguration {
            provider: config.model_provider.clone(),
            model,
            model_reasoning_effort: config.model_reasoning_effort,
            model_reasoning_summary: config.model_reasoning_summary,
            developer_instructions: config.developer_instructions.clone(),
            user_instructions: config.user_instructions.clone(),
            base_instructions: config.base_instructions.clone(),
            compact_prompt: config.compact_prompt.clone(),
            subagent_discovery_overrides: config.subagent_discovery_overrides.clone(),
            approval_policy: config.approval_policy.clone(),
            sandbox_policy: config.sandbox_policy.clone(),
            cwd: config.cwd.clone(),
            original_config_do_not_use: Arc::clone(&config),
            session_source: SessionSource::Exec,
        };

        let mut state = SessionState::new(session_configuration);
        let initial = RateLimitSnapshot {
            primary: Some(RateLimitWindow {
                used_percent: 15.0,
                window_minutes: Some(20),
                resets_at: Some(1_600),
            }),
            secondary: Some(RateLimitWindow {
                used_percent: 5.0,
                window_minutes: Some(45),
                resets_at: Some(1_650),
            }),
            credits: Some(CreditsSnapshot {
                has_credits: true,
                unlimited: false,
                balance: Some("15.00".to_string()),
            }),
            plan_type: Some(codex_protocol::account::PlanType::Plus),
        };
        state.set_rate_limits(initial.clone());

        let update = RateLimitSnapshot {
            primary: Some(RateLimitWindow {
                used_percent: 35.0,
                window_minutes: Some(25),
                resets_at: Some(1_700),
            }),
            secondary: None,
            credits: None,
            plan_type: Some(codex_protocol::account::PlanType::Pro),
        };
        state.set_rate_limits(update.clone());

        assert_eq!(
            state.latest_rate_limits,
            Some(RateLimitSnapshot {
                primary: update.primary,
                secondary: update.secondary,
                credits: initial.credits,
                plan_type: update.plan_type,
            })
        );
    }

    #[test]
    fn prefers_structured_content_when_present() {
        let ctr = CallToolResult {
            // Content present but should be ignored because structured_content is set.
            content: vec![text_block("ignored")],
            is_error: None,
            structured_content: Some(json!({
                "ok": true,
                "value": 42
            })),
        };

        let got = FunctionCallOutputPayload::from(&ctr);
        let expected = FunctionCallOutputPayload {
            content: serde_json::to_string(&json!({
                "ok": true,
                "value": 42
            }))
            .unwrap(),
            success: Some(true),
            ..Default::default()
        };

        assert_eq!(expected, got);
    }

    #[tokio::test]
    async fn includes_timed_out_message() {
        let exec = ExecToolCallOutput {
            exit_code: 0,
            stdout: StreamOutput::new(String::new()),
            stderr: StreamOutput::new(String::new()),
            aggregated_output: StreamOutput::new("Command output".to_string()),
            duration: StdDuration::from_secs(1),
            timed_out: true,
        };
        let (_, turn_context) = make_session_and_context().await;

        let out = format_exec_output_str(&exec, turn_context.truncation_policy);

        assert_eq!(
            out,
            "command timed out after 1000 milliseconds\nCommand output"
        );
    }

    #[test]
    fn falls_back_to_content_when_structured_is_null() {
        let ctr = CallToolResult {
            content: vec![text_block("hello"), text_block("world")],
            is_error: None,
            structured_content: Some(serde_json::Value::Null),
        };

        let got = FunctionCallOutputPayload::from(&ctr);
        let expected = FunctionCallOutputPayload {
            content: serde_json::to_string(&vec![text_block("hello"), text_block("world")])
                .unwrap(),
            success: Some(true),
            ..Default::default()
        };

        assert_eq!(expected, got);
    }

    #[test]
    fn success_flag_reflects_is_error_true() {
        let ctr = CallToolResult {
            content: vec![text_block("unused")],
            is_error: Some(true),
            structured_content: Some(json!({ "message": "bad" })),
        };

        let got = FunctionCallOutputPayload::from(&ctr);
        let expected = FunctionCallOutputPayload {
            content: serde_json::to_string(&json!({ "message": "bad" })).unwrap(),
            success: Some(false),
            ..Default::default()
        };

        assert_eq!(expected, got);
    }

    #[test]
    fn success_flag_true_with_no_error_and_content_used() {
        let ctr = CallToolResult {
            content: vec![text_block("alpha")],
            is_error: Some(false),
            structured_content: None,
        };

        let got = FunctionCallOutputPayload::from(&ctr);
        let expected = FunctionCallOutputPayload {
            content: serde_json::to_string(&vec![text_block("alpha")]).unwrap(),
            success: Some(true),
            ..Default::default()
        };

        assert_eq!(expected, got);
    }

    async fn wait_for_thread_rolled_back(
        rx: &async_channel::Receiver<Event>,
    ) -> crate::protocol::ThreadRolledBackEvent {
        let deadline = StdDuration::from_secs(2);
        let start = std::time::Instant::now();
        loop {
            let remaining = deadline.saturating_sub(start.elapsed());
            let evt = tokio::time::timeout(remaining, rx.recv())
                .await
                .expect("timeout waiting for event")
                .expect("event");
            match evt.msg {
                EventMsg::ThreadRolledBack(payload) => return payload,
                _ => continue,
            }
        }
    }

    async fn wait_for_thread_rollback_failed(rx: &async_channel::Receiver<Event>) -> ErrorEvent {
        let deadline = StdDuration::from_secs(2);
        let start = std::time::Instant::now();
        loop {
            let remaining = deadline.saturating_sub(start.elapsed());
            let evt = tokio::time::timeout(remaining, rx.recv())
                .await
                .expect("timeout waiting for event")
                .expect("event");
            match evt.msg {
                EventMsg::Error(payload)
                    if payload.codex_error_info == Some(CodexErrorInfo::ThreadRollbackFailed) =>
                {
                    return payload;
                }
                _ => continue,
            }
        }
    }

    fn text_block(s: &str) -> ContentBlock {
        ContentBlock::TextContent(TextContent {
            annotations: None,
            text: s.to_string(),
            r#type: "text".to_string(),
        })
    }

    async fn build_test_config(codex_home: &Path) -> Config {
        ConfigBuilder::default()
            .codex_home(codex_home.to_path_buf())
            .build()
            .await
            .expect("load default test config")
    }

    fn otel_manager(
        conversation_id: ThreadId,
        config: &Config,
        model_info: &ModelInfo,
        session_source: SessionSource,
    ) -> OtelManager {
        OtelManager::new(
            conversation_id,
            ModelsManager::get_model_offline(config.model.as_deref()).as_str(),
            model_info.slug.as_str(),
            None,
            Some("test@test.com".to_string()),
            Some(AuthMode::ChatGPT),
            false,
            "test".to_string(),
            session_source,
        )
    }

    pub(crate) async fn make_session_and_context() -> (Session, TurnContext) {
        let (tx_event, _rx_event) = async_channel::unbounded();
        let codex_home = tempfile::tempdir().expect("create temp dir");
        let config = build_test_config(codex_home.path()).await;
        let config = Arc::new(config);
        let conversation_id = ThreadId::default();
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let models_manager = Arc::new(ModelsManager::new(
            config.codex_home.clone(),
            auth_manager.clone(),
        ));
        let agent_control = AgentControl::default();
        let exec_policy = ExecPolicyManager::default();
        let agent_status = Arc::new(RwLock::new(AgentStatus::PendingInit));
        let model = ModelsManager::get_model_offline(config.model.as_deref());
        let session_configuration = SessionConfiguration {
            provider: config.model_provider.clone(),
            model,
            model_reasoning_effort: config.model_reasoning_effort,
            model_reasoning_summary: config.model_reasoning_summary,
            developer_instructions: config.developer_instructions.clone(),
            user_instructions: config.user_instructions.clone(),
            base_instructions: config.base_instructions.clone(),
            compact_prompt: config.compact_prompt.clone(),
            subagent_discovery_overrides: config.subagent_discovery_overrides.clone(),
            approval_policy: config.approval_policy.clone(),
            sandbox_policy: config.sandbox_policy.clone(),
            cwd: config.cwd.clone(),
            original_config_do_not_use: Arc::clone(&config),
            session_source: SessionSource::Exec,
        };
        let per_turn_config = Session::build_per_turn_config(&session_configuration);
        let model_info = ModelsManager::construct_model_info_offline(
            session_configuration.model.as_str(),
            &per_turn_config,
        );
        let otel_manager = otel_manager(
            conversation_id,
            config.as_ref(),
            &model_info,
            session_configuration.session_source.clone(),
        );

        let state = SessionState::new(session_configuration.clone());
        let skills_manager = Arc::new(SkillsManager::new(config.codex_home.clone()));
        let agent_registry = init_agent_registry(
            session_configuration.cwd.as_path(),
            session_configuration.subagent_discovery_overrides.clone(),
            Some(&otel_manager),
        );

        let services = SessionServices {
            mcp_connection_manager: Arc::new(RwLock::new(McpConnectionManager::default())),
            mcp_startup_cancellation_token: CancellationToken::new(),
            unified_exec_manager: UnifiedExecProcessManager::default(),
            notifier: UserNotifier::new(None),
            rollout: Mutex::new(None),
            user_shell: Arc::new(default_user_shell()),
            show_raw_agent_reasoning: config.show_raw_agent_reasoning,
            exec_policy,
            auth_manager: auth_manager.clone(),
            otel_manager: otel_manager.clone(),
            models_manager: Arc::clone(&models_manager),
            tool_approvals: Mutex::new(ApprovalStore::default()),
            skills_manager,
            agent_control,
            agent_registry,
            active_subagent_runtime: RwLock::new(None),
            subagent_transcripts: SubagentTranscriptStore::new(config.codex_home.clone()),
        };

        let turn_context = Session::make_turn_context(
            Some(Arc::clone(&auth_manager)),
            &otel_manager,
            session_configuration.provider.clone(),
            &session_configuration,
            per_turn_config,
            model_info,
            conversation_id,
            "turn_id".to_string(),
            None,
        );

        let session = Session {
            conversation_id,
            tx_event,
            agent_status: Arc::clone(&agent_status),
            state: Mutex::new(state),
            features: config.features.clone(),
            active_turn: Mutex::new(None),
            services,
            next_internal_sub_id: AtomicU64::new(0),
            agent_registration_done: AtomicBool::new(false),
        };

        (session, turn_context)
    }

    // Like make_session_and_context, but returns Arc<Session> and the event receiver
    // so tests can assert on emitted events.
    pub(crate) async fn make_session_and_context_with_rx() -> (
        Arc<Session>,
        Arc<TurnContext>,
        async_channel::Receiver<Event>,
    ) {
        let (tx_event, rx_event) = async_channel::unbounded();
        let codex_home = tempfile::tempdir().expect("create temp dir");
        let config = build_test_config(codex_home.path()).await;
        let config = Arc::new(config);
        let conversation_id = ThreadId::default();
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let models_manager = Arc::new(ModelsManager::new(
            config.codex_home.clone(),
            auth_manager.clone(),
        ));
        let agent_control = AgentControl::default();
        let exec_policy = ExecPolicyManager::default();
        let agent_status = Arc::new(RwLock::new(AgentStatus::PendingInit));
        let model = ModelsManager::get_model_offline(config.model.as_deref());
        let session_configuration = SessionConfiguration {
            provider: config.model_provider.clone(),
            model,
            model_reasoning_effort: config.model_reasoning_effort,
            model_reasoning_summary: config.model_reasoning_summary,
            developer_instructions: config.developer_instructions.clone(),
            user_instructions: config.user_instructions.clone(),
            base_instructions: config.base_instructions.clone(),
            compact_prompt: config.compact_prompt.clone(),
            subagent_discovery_overrides: config.subagent_discovery_overrides.clone(),
            approval_policy: config.approval_policy.clone(),
            sandbox_policy: config.sandbox_policy.clone(),
            cwd: config.cwd.clone(),
            original_config_do_not_use: Arc::clone(&config),
            session_source: SessionSource::Exec,
        };
        let per_turn_config = Session::build_per_turn_config(&session_configuration);
        let model_info = ModelsManager::construct_model_info_offline(
            session_configuration.model.as_str(),
            &per_turn_config,
        );
        let otel_manager = otel_manager(
            conversation_id,
            config.as_ref(),
            &model_info,
            session_configuration.session_source.clone(),
        );

        let state = SessionState::new(session_configuration.clone());
        let skills_manager = Arc::new(SkillsManager::new(config.codex_home.clone()));

        let agent_registry = init_agent_registry(
            session_configuration.cwd.as_path(),
            session_configuration.subagent_discovery_overrides.clone(),
            Some(&otel_manager),
        );

        let services = SessionServices {
            mcp_connection_manager: Arc::new(RwLock::new(McpConnectionManager::default())),
            mcp_startup_cancellation_token: CancellationToken::new(),
            unified_exec_manager: UnifiedExecProcessManager::default(),
            notifier: UserNotifier::new(None),
            rollout: Mutex::new(None),
            user_shell: Arc::new(default_user_shell()),
            show_raw_agent_reasoning: config.show_raw_agent_reasoning,
            exec_policy,
            auth_manager: Arc::clone(&auth_manager),
            otel_manager: otel_manager.clone(),
            models_manager: Arc::clone(&models_manager),
            tool_approvals: Mutex::new(ApprovalStore::default()),
            skills_manager,
            agent_control,
            agent_registry,
            active_subagent_runtime: RwLock::new(None),
            subagent_transcripts: SubagentTranscriptStore::new(config.codex_home.clone()),
        };

        let turn_context = Arc::new(Session::make_turn_context(
            Some(Arc::clone(&auth_manager)),
            &otel_manager,
            session_configuration.provider.clone(),
            &session_configuration,
            per_turn_config,
            model_info,
            conversation_id,
            "turn_id".to_string(),
            None,
        ));

        let session = Arc::new(Session {
            conversation_id,
            tx_event,
            agent_status: Arc::clone(&agent_status),
            state: Mutex::new(state),
            features: config.features.clone(),
            active_turn: Mutex::new(None),
            services,
            next_internal_sub_id: AtomicU64::new(0),
            agent_registration_done: AtomicBool::new(false),
        });

        (session, turn_context, rx_event)
    }

    #[tokio::test]
    async fn record_model_warning_appends_user_message() {
        let (mut session, turn_context) = make_session_and_context().await;
        let features = Features::with_defaults();
        session.features = features;

        session
            .record_model_warning("too many unified exec processes", &turn_context)
            .await;

        let history = session.clone_history().await;
        let history_items = history.raw_items();
        let last = history_items.last().expect("warning recorded");

        match last {
            ResponseItem::Message { role, content, .. } => {
                assert_eq!(role, "user");
                assert_eq!(
                    content,
                    &vec![ContentItem::InputText {
                        text: "Warning: too many unified exec processes".to_string(),
                    }]
                );
            }
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[derive(Clone, Copy)]
    struct NeverEndingTask {
        kind: TaskKind,
        listen_to_cancellation_token: bool,
    }

    #[async_trait::async_trait]
    impl SessionTask for NeverEndingTask {
        fn kind(&self) -> TaskKind {
            self.kind
        }

        async fn run(
            self: Arc<Self>,
            _session: Arc<SessionTaskContext>,
            _ctx: Arc<TurnContext>,
            _input: Vec<UserInput>,
            cancellation_token: CancellationToken,
        ) -> Option<String> {
            if self.listen_to_cancellation_token {
                cancellation_token.cancelled().await;
                return None;
            }
            loop {
                sleep(Duration::from_secs(60)).await;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[test_log::test]
    async fn abort_regular_task_emits_turn_aborted_only() {
        let (sess, tc, rx) = make_session_and_context_with_rx().await;
        let input = vec![UserInput::Text {
            text: "hello".to_string(),
        }];
        sess.spawn_task(
            Arc::clone(&tc),
            input,
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: false,
            },
        )
        .await;

        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;

        let evt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("event");
        match evt.msg {
            EventMsg::TurnAborted(e) => assert_eq!(TurnAbortReason::Interrupted, e.reason),
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn abort_gracefuly_emits_turn_aborted_only() {
        let (sess, tc, rx) = make_session_and_context_with_rx().await;
        let input = vec![UserInput::Text {
            text: "hello".to_string(),
        }];
        sess.spawn_task(
            Arc::clone(&tc),
            input,
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: true,
            },
        )
        .await;

        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;

        let evt = rx.recv().await.expect("event");
        match evt.msg {
            EventMsg::TurnAborted(e) => assert_eq!(TurnAbortReason::Interrupted, e.reason),
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_review_task_emits_exited_then_aborted_and_records_history() {
        let (sess, tc, rx) = make_session_and_context_with_rx().await;
        let input = vec![UserInput::Text {
            text: "start review".to_string(),
        }];
        sess.spawn_task(Arc::clone(&tc), input, ReviewTask::new())
            .await;

        sess.abort_all_tasks(TurnAbortReason::Interrupted).await;

        // Drain events until we observe ExitedReviewMode; earlier
        // RawResponseItem entries (e.g., environment context) may arrive first.
        loop {
            let evt = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("timeout waiting for first event")
                .expect("first event");
            match evt.msg {
                EventMsg::ExitedReviewMode(ev) => {
                    assert!(ev.review_output.is_none());
                    break;
                }
                // Ignore any non-critical events before exit.
                _ => continue,
            }
        }
        loop {
            let evt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout waiting for next event")
                .expect("event");
            match evt.msg {
                EventMsg::RawResponseItem(_) => continue,
                EventMsg::ItemStarted(_) | EventMsg::ItemCompleted(_) => continue,
                EventMsg::AgentMessage(_) => continue,
                EventMsg::TurnAborted(e) => {
                    assert_eq!(TurnAbortReason::Interrupted, e.reason);
                    break;
                }
                other => panic!("unexpected second event: {other:?}"),
            }
        }

        // TODO(jif) investigate what is this?
        let history = sess.clone_history().await;
        let _ = history.raw_items();
    }

    #[tokio::test]
    async fn fatal_tool_error_stops_turn_and_reports_error() {
        let (session, turn_context, _rx) = make_session_and_context_with_rx().await;
        let tools = {
            session
                .services
                .mcp_connection_manager
                .read()
                .await
                .list_all_tools()
                .await
        };
        let router = ToolRouter::from_config(
            &turn_context.tools_config,
            Some(
                tools
                    .into_iter()
                    .map(|(name, tool)| (name, tool.tool))
                    .collect(),
            ),
            None,
        );
        let item = ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "call-1".to_string(),
            name: "shell".to_string(),
            input: "{}".to_string(),
        };

        let call = ToolRouter::build_tool_call(session.as_ref(), item.clone())
            .await
            .expect("build tool call")
            .expect("tool call present");
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let err = router
            .dispatch_tool_call(
                Arc::clone(&session),
                Arc::clone(&turn_context),
                tracker,
                call,
            )
            .await
            .expect_err("expected fatal error");

        match err {
            FunctionCallError::Fatal(message) => {
                assert_eq!(message, "tool shell invoked with incompatible payload");
            }
            other => panic!("expected FunctionCallError::Fatal, got {other:?}"),
        }
    }

    fn sample_rollout(
        session: &Session,
        turn_context: &TurnContext,
    ) -> (Vec<RolloutItem>, Vec<ResponseItem>) {
        let mut rollout_items = Vec::new();
        let mut live_history = ContextManager::new();

        let initial_context = session.build_initial_context(turn_context);
        for item in &initial_context {
            rollout_items.push(RolloutItem::ResponseItem(item.clone()));
        }
        live_history.record_items(initial_context.iter(), turn_context.truncation_policy);

        let user1 = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "first user".to_string(),
            }],
        };
        live_history.record_items(std::iter::once(&user1), turn_context.truncation_policy);
        rollout_items.push(RolloutItem::ResponseItem(user1.clone()));

        let assistant1 = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "assistant reply one".to_string(),
            }],
        };
        live_history.record_items(std::iter::once(&assistant1), turn_context.truncation_policy);
        rollout_items.push(RolloutItem::ResponseItem(assistant1.clone()));

        let summary1 = "summary one";
        let snapshot1 = live_history.clone().for_prompt();
        let user_messages1 = collect_user_messages(&snapshot1);
        let rebuilt1 = compact::build_compacted_history(
            session.build_initial_context(turn_context),
            &user_messages1,
            summary1,
        );
        live_history.replace(rebuilt1);
        rollout_items.push(RolloutItem::Compacted(CompactedItem {
            message: summary1.to_string(),
            replacement_history: None,
        }));

        let user2 = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "second user".to_string(),
            }],
        };
        live_history.record_items(std::iter::once(&user2), turn_context.truncation_policy);
        rollout_items.push(RolloutItem::ResponseItem(user2.clone()));

        let assistant2 = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "assistant reply two".to_string(),
            }],
        };
        live_history.record_items(std::iter::once(&assistant2), turn_context.truncation_policy);
        rollout_items.push(RolloutItem::ResponseItem(assistant2.clone()));

        let summary2 = "summary two";
        let snapshot2 = live_history.clone().for_prompt();
        let user_messages2 = collect_user_messages(&snapshot2);
        let rebuilt2 = compact::build_compacted_history(
            session.build_initial_context(turn_context),
            &user_messages2,
            summary2,
        );
        live_history.replace(rebuilt2);
        rollout_items.push(RolloutItem::Compacted(CompactedItem {
            message: summary2.to_string(),
            replacement_history: None,
        }));

        let user3 = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "third user".to_string(),
            }],
        };
        live_history.record_items(std::iter::once(&user3), turn_context.truncation_policy);
        rollout_items.push(RolloutItem::ResponseItem(user3.clone()));

        let assistant3 = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "assistant reply three".to_string(),
            }],
        };
        live_history.record_items(std::iter::once(&assistant3), turn_context.truncation_policy);
        rollout_items.push(RolloutItem::ResponseItem(assistant3.clone()));

        (rollout_items, live_history.for_prompt())
    }

    #[tokio::test]
    async fn rejects_escalated_permissions_when_policy_not_on_request() {
        use crate::exec::ExecParams;
        use crate::protocol::AskForApproval;
        use crate::protocol::SandboxPolicy;
        use crate::sandboxing::SandboxPermissions;
        use crate::turn_diff_tracker::TurnDiffTracker;
        use std::collections::HashMap;

        let (session, mut turn_context_raw) = make_session_and_context().await;
        // Ensure policy is NOT OnRequest so the early rejection path triggers
        turn_context_raw.approval_policy = AskForApproval::OnFailure;
        let session = Arc::new(session);
        let mut turn_context = Arc::new(turn_context_raw);

        let timeout_ms = 1000;
        let sandbox_permissions = SandboxPermissions::RequireEscalated;
        let params = ExecParams {
            command: if cfg!(windows) {
                vec![
                    "cmd.exe".to_string(),
                    "/C".to_string(),
                    "echo hi".to_string(),
                ]
            } else {
                vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "echo hi".to_string(),
                ]
            },
            cwd: turn_context.cwd.clone(),
            expiration: timeout_ms.into(),
            env: HashMap::new(),
            sandbox_permissions,
            justification: Some("test".to_string()),
            arg0: None,
        };

        let params2 = ExecParams {
            sandbox_permissions: SandboxPermissions::UseDefault,
            command: params.command.clone(),
            cwd: params.cwd.clone(),
            expiration: timeout_ms.into(),
            env: HashMap::new(),
            justification: params.justification.clone(),
            arg0: None,
        };

        let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

        let tool_name = "shell";
        let call_id = "test-call".to_string();

        let handler = ShellHandler;
        let resp = handler
            .handle(ToolInvocation {
                session: Arc::clone(&session),
                turn: Arc::clone(&turn_context),
                tracker: Arc::clone(&turn_diff_tracker),
                call_id,
                tool_name: tool_name.to_string(),
                payload: ToolPayload::Function {
                    arguments: serde_json::json!({
                        "command": params.command.clone(),
                        "workdir": Some(turn_context.cwd.to_string_lossy().to_string()),
                        "timeout_ms": params.expiration.timeout_ms(),
                        "sandbox_permissions": params.sandbox_permissions,
                        "justification": params.justification.clone(),
                    })
                    .to_string(),
                },
            })
            .await;

        let Err(FunctionCallError::RespondToModel(output)) = resp else {
            panic!("expected error result");
        };

        let expected = format!(
            "approval policy is {policy:?}; reject command — you should not ask for escalated permissions if the approval policy is {policy:?}",
            policy = turn_context.approval_policy
        );

        pretty_assertions::assert_eq!(output, expected);

        // Now retry the same command WITHOUT escalated permissions; should succeed.
        // Force DangerFullAccess to avoid platform sandbox dependencies in tests.
        Arc::get_mut(&mut turn_context)
            .expect("unique turn context Arc")
            .sandbox_policy = SandboxPolicy::DangerFullAccess;

        let resp2 = handler
            .handle(ToolInvocation {
                session: Arc::clone(&session),
                turn: Arc::clone(&turn_context),
                tracker: Arc::clone(&turn_diff_tracker),
                call_id: "test-call-2".to_string(),
                tool_name: tool_name.to_string(),
                payload: ToolPayload::Function {
                    arguments: serde_json::json!({
                        "command": params2.command.clone(),
                        "workdir": Some(turn_context.cwd.to_string_lossy().to_string()),
                        "timeout_ms": params2.expiration.timeout_ms(),
                        "sandbox_permissions": params2.sandbox_permissions,
                        "justification": params2.justification.clone(),
                    })
                    .to_string(),
                },
            })
            .await;

        let output = match resp2.expect("expected Ok result") {
            ToolOutput::Function { content, .. } => content,
            _ => panic!("unexpected tool output"),
        };

        #[derive(Deserialize, PartialEq, Eq, Debug)]
        struct ResponseExecMetadata {
            exit_code: i32,
        }

        #[derive(Deserialize)]
        struct ResponseExecOutput {
            output: String,
            metadata: ResponseExecMetadata,
        }

        let exec_output: ResponseExecOutput =
            serde_json::from_str(&output).expect("valid exec output json");

        pretty_assertions::assert_eq!(exec_output.metadata, ResponseExecMetadata { exit_code: 0 });
        assert!(exec_output.output.contains("hi"));
    }
    #[tokio::test]
    async fn unified_exec_rejects_escalated_permissions_when_policy_not_on_request() {
        use crate::protocol::AskForApproval;
        use crate::sandboxing::SandboxPermissions;
        use crate::turn_diff_tracker::TurnDiffTracker;

        let (session, mut turn_context_raw) = make_session_and_context().await;
        turn_context_raw.approval_policy = AskForApproval::OnFailure;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context_raw);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

        let handler = UnifiedExecHandler;
        let resp = handler
            .handle(ToolInvocation {
                session: Arc::clone(&session),
                turn: Arc::clone(&turn_context),
                tracker: Arc::clone(&tracker),
                call_id: "exec-call".to_string(),
                tool_name: "exec_command".to_string(),
                payload: ToolPayload::Function {
                    arguments: serde_json::json!({
                        "cmd": "echo hi",
                        "sandbox_permissions": SandboxPermissions::RequireEscalated,
                        "justification": "need unsandboxed execution",
                    })
                    .to_string(),
                },
            })
            .await;

        let Err(FunctionCallError::RespondToModel(output)) = resp else {
            panic!("expected error result");
        };

        let expected = format!(
            "approval policy is {policy:?}; reject command — you cannot ask for escalated permissions if the approval policy is {policy:?}",
            policy = turn_context.approval_policy
        );

        pretty_assertions::assert_eq!(output, expected);
    }
}
