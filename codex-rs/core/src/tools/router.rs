use crate::client_common::tools::ToolSpec;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::function_tool::FunctionCallError;
use crate::sandboxing::SandboxPermissions;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::ConfiguredToolSpec;
use crate::tools::registry::ToolRegistry;
use crate::tools::spec::ToolsConfig;
use crate::tools::spec::build_specs;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::ShellToolCallParams;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::instrument;

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub tool_name: String,
    pub call_id: String,
    pub payload: ToolPayload,
}

pub struct ToolRouter {
    registry: ToolRegistry,
    specs: Vec<ConfiguredToolSpec>,
    allowed_tools: Option<HashSet<String>>,
}

impl ToolRouter {
    pub fn from_config(
        config: &ToolsConfig,
        mcp_tools: Option<HashMap<String, mcp_types::Tool>>,
        allowed_tools: Option<&[String]>,
    ) -> Self {
        let builder = build_specs(config, mcp_tools);
        let (mut specs, registry) = builder.build();
        let allowed_filter =
            allowed_tools.map(|names| names.iter().cloned().collect::<HashSet<_>>());

        if let Some(filter) = allowed_filter.as_ref() {
            specs.retain(|config| filter.contains(config.spec.name()));
        }

        Self {
            registry,
            specs,
            allowed_tools: allowed_filter,
        }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.specs
            .iter()
            .map(|config| config.spec.clone())
            .collect()
    }

    pub fn tool_supports_parallel(&self, tool_name: &str) -> bool {
        self.specs
            .iter()
            .filter(|config| config.supports_parallel_tool_calls)
            .any(|config| config.spec.name() == tool_name)
    }

    #[instrument(level = "trace", skip_all, err)]
    pub async fn build_tool_call(
        session: &Session,
        item: ResponseItem,
    ) -> Result<Option<ToolCall>, FunctionCallError> {
        match item {
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                if let Some((server, tool)) = session.parse_mcp_tool_name(&name).await {
                    Ok(Some(ToolCall {
                        tool_name: name,
                        call_id,
                        payload: ToolPayload::Mcp {
                            server,
                            tool,
                            raw_arguments: arguments,
                        },
                    }))
                } else {
                    Ok(Some(ToolCall {
                        tool_name: name,
                        call_id,
                        payload: ToolPayload::Function { arguments },
                    }))
                }
            }
            ResponseItem::CustomToolCall {
                name,
                input,
                call_id,
                ..
            } => Ok(Some(ToolCall {
                tool_name: name,
                call_id,
                payload: ToolPayload::Custom { input },
            })),
            ResponseItem::LocalShellCall {
                id,
                call_id,
                action,
                ..
            } => {
                let call_id = call_id
                    .or(id)
                    .ok_or(FunctionCallError::MissingLocalShellCallId)?;

                match action {
                    LocalShellAction::Exec(exec) => {
                        let params = ShellToolCallParams {
                            command: exec.command,
                            workdir: exec.working_directory,
                            timeout_ms: exec.timeout_ms,
                            sandbox_permissions: Some(SandboxPermissions::UseDefault),
                            justification: None,
                        };
                        Ok(Some(ToolCall {
                            tool_name: "local_shell".to_string(),
                            call_id,
                            payload: ToolPayload::LocalShell { params },
                        }))
                    }
                }
            }
            _ => Ok(None),
        }
    }

    #[instrument(level = "trace", skip_all, err)]
    pub async fn dispatch_tool_call(
        &self,
        session: Arc<Session>,
        turn: Arc<TurnContext>,
        tracker: SharedTurnDiffTracker,
        call: ToolCall,
    ) -> Result<ResponseInputItem, FunctionCallError> {
        let ToolCall {
            tool_name,
            call_id,
            payload,
        } = call;
        let payload_outputs_custom = matches!(&payload, ToolPayload::Custom { .. });
        let failure_call_id = call_id.clone();
        if let Some(filter) = self.allowed_tools.as_ref()
            && !filter.contains(tool_name.as_str())
        {
            let err = FunctionCallError::RespondToModel(format!(
                "tool {tool_name} is not allowed for the active subagent"
            ));
            return Ok(Self::failure_response(
                failure_call_id,
                payload_outputs_custom,
                err,
            ));
        }

        let invocation = ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            tool_name,
            payload,
        };

        match self.registry.dispatch(invocation).await {
            Ok(response) => Ok(response),
            Err(FunctionCallError::Fatal(message)) => Err(FunctionCallError::Fatal(message)),
            Err(err) => Ok(Self::failure_response(
                failure_call_id,
                payload_outputs_custom,
                err,
            )),
        }
    }

    fn failure_response(
        call_id: String,
        payload_outputs_custom: bool,
        err: FunctionCallError,
    ) -> ResponseInputItem {
        let message = err.to_string();
        if payload_outputs_custom {
            ResponseInputItem::CustomToolCallOutput {
                call_id,
                output: message,
            }
        } else {
            let output = codex_protocol::models::FunctionCallOutputPayload {
                content: message,
                success: Some(false),
                ..Default::default()
            };
            ResponseInputItem::FunctionCallOutput {
                call_id,
                output_metadata: output.metadata(),
                output,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_common::tools::FreeformTool;
    use crate::client_common::tools::ResponsesApiTool;
    use crate::codex::make_session_and_context;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolOutput;
    use crate::tools::registry::ToolHandler;
    use crate::tools::registry::ToolKind;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use async_trait::async_trait;
    use codex_protocol::openai_models::ConfigShellToolType;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use tokio::sync::Mutex;

    fn tool_spec_name(spec: &ToolSpec) -> String {
        match spec {
            ToolSpec::Function(ResponsesApiTool { name, .. }) => name.clone(),
            ToolSpec::LocalShell {} => "local_shell".to_string(),
            ToolSpec::WebSearch { .. } => "web_search".to_string(),
            ToolSpec::Freeform(FreeformTool { name, .. }) => name.clone(),
        }
    }

    fn minimal_tools_config() -> ToolsConfig {
        ToolsConfig {
            shell_type: ConfigShellToolType::Disabled,
            apply_patch_tool_type: None,
            web_search_request: false,
            web_search_cached: false,
            experimental_supported_tools: vec![],
        }
    }

    #[test]
    fn specs_respect_allow_list() {
        let config = minimal_tools_config();
        let allowed = vec!["list_mcp_resources".to_string()];
        let router = ToolRouter::from_config(&config, None, Some(&allowed));
        let names = router
            .specs()
            .into_iter()
            .map(|spec| tool_spec_name(&spec))
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["list_mcp_resources".to_string()]);
    }

    #[derive(Clone, Default)]
    struct RecordingHandler {
        calls: Arc<AtomicUsize>,
    }

    impl RecordingHandler {
        fn new() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ToolHandler for RecordingHandler {
        fn kind(&self) -> ToolKind {
            ToolKind::Function
        }

        async fn handle(
            &self,
            _invocation: ToolInvocation,
        ) -> Result<ToolOutput, FunctionCallError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::Function {
                content: "ok".to_string(),
                content_items: None,
                success: Some(true),
            })
        }
    }

    #[tokio::test]
    async fn dispatch_blocks_disallowed_tools() {
        let (session, turn_context) = make_session_and_context().await;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::default()));
        let handler = Arc::new(RecordingHandler::new());

        let mut handlers: HashMap<String, Arc<dyn ToolHandler>> = HashMap::new();
        handlers.insert("allowed".to_string(), handler.clone());
        handlers.insert("blocked".to_string(), handler.clone());

        let router = ToolRouter {
            registry: ToolRegistry::new(handlers),
            specs: Vec::new(),
            allowed_tools: Some(HashSet::from(["allowed".to_string()])),
        };

        let allowed_call = ToolCall {
            tool_name: "allowed".to_string(),
            call_id: "allowed-call".to_string(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        };

        router
            .dispatch_tool_call(
                Arc::clone(&session),
                Arc::clone(&turn_context),
                Arc::clone(&tracker),
                allowed_call,
            )
            .await
            .expect("allowed tool should run");
        assert_eq!(handler.call_count(), 1);

        let blocked_call = ToolCall {
            tool_name: "blocked".to_string(),
            call_id: "blocked-call".to_string(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        };

        let response = router
            .dispatch_tool_call(session, turn_context, tracker, blocked_call)
            .await
            .expect("blocked tool should emit a failure response");

        match response {
            ResponseInputItem::FunctionCallOutput { output, .. } => {
                assert_eq!(
                    output.content,
                    "tool blocked is not allowed for the active subagent"
                );
                assert_eq!(output.success, Some(false));
            }
            other => panic!("unexpected response variant: {other:?}"),
        }
        assert_eq!(handler.call_count(), 1);
    }
}
