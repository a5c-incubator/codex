use crate::common::Reasoning;
use crate::common::ResponsesApiRequest;
use crate::common::TextControls;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::requests::headers::build_conversation_headers;
use crate::requests::headers::insert_header;
use crate::requests::headers::subagent_header;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use http::HeaderMap;
use serde_json::Value;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Compression {
    #[default]
    None,
    Zstd,
}

/// Assembled request body plus headers for a Responses stream request.
pub struct ResponsesRequest {
    pub body: Value,
    pub headers: HeaderMap,
    pub compression: Compression,
}

#[derive(Default)]
pub struct ResponsesRequestBuilder<'a> {
    model: Option<&'a str>,
    instructions: Option<&'a str>,
    input: Option<&'a [ResponseItem]>,
    tools: Option<&'a [Value]>,
    parallel_tool_calls: bool,
    reasoning: Option<Reasoning>,
    include: Vec<String>,
    prompt_cache_key: Option<String>,
    text: Option<TextControls>,
    conversation_id: Option<String>,
    session_source: Option<SessionSource>,
    store_override: Option<bool>,
    headers: HeaderMap,
    compression: Compression,
}

impl<'a> ResponsesRequestBuilder<'a> {
    pub fn new(model: &'a str, instructions: &'a str, input: &'a [ResponseItem]) -> Self {
        Self {
            model: Some(model),
            instructions: Some(instructions),
            input: Some(input),
            ..Default::default()
        }
    }

    pub fn tools(mut self, tools: &'a [Value]) -> Self {
        self.tools = Some(tools);
        self
    }

    pub fn parallel_tool_calls(mut self, enabled: bool) -> Self {
        self.parallel_tool_calls = enabled;
        self
    }

    pub fn reasoning(mut self, reasoning: Option<Reasoning>) -> Self {
        self.reasoning = reasoning;
        self
    }

    pub fn include(mut self, include: Vec<String>) -> Self {
        self.include = include;
        self
    }

    pub fn prompt_cache_key(mut self, key: Option<String>) -> Self {
        self.prompt_cache_key = key;
        self
    }

    pub fn text(mut self, text: Option<TextControls>) -> Self {
        self.text = text;
        self
    }

    pub fn conversation(mut self, conversation_id: Option<String>) -> Self {
        self.conversation_id = conversation_id;
        self
    }

    pub fn session_source(mut self, source: Option<SessionSource>) -> Self {
        self.session_source = source;
        self
    }

    pub fn store_override(mut self, store: Option<bool>) -> Self {
        self.store_override = store;
        self
    }

    pub fn extra_headers(mut self, headers: HeaderMap) -> Self {
        self.headers = headers;
        self
    }

    pub fn compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    pub fn build(self, provider: &Provider) -> Result<ResponsesRequest, ApiError> {
        let model = self
            .model
            .ok_or_else(|| ApiError::Stream("missing model for responses request".into()))?;
        let instructions = self
            .instructions
            .ok_or_else(|| ApiError::Stream("missing instructions for responses request".into()))?;
        let input = self
            .input
            .ok_or_else(|| ApiError::Stream("missing input for responses request".into()))?;
        let tools = self.tools.unwrap_or_default();

        let store = self
            .store_override
            .unwrap_or_else(|| provider.is_azure_responses_endpoint());

        let req = ResponsesApiRequest {
            model,
            instructions,
            input,
            tools,
            tool_choice: "auto",
            parallel_tool_calls: self.parallel_tool_calls,
            reasoning: self.reasoning,
            store,
            stream: true,
            include: self.include,
            prompt_cache_key: self.prompt_cache_key,
            text: self.text,
        };

        let mut body = serde_json::to_value(&req)
            .map_err(|e| ApiError::Stream(format!("failed to encode responses request: {e}")))?;

        if !include_output_metadata_in_request(provider) {
            strip_output_metadata(&mut body);
        }

        if store && provider.is_azure_responses_endpoint() {
            attach_item_ids(&mut body, input);
        }

        let mut headers = self.headers;
        headers.extend(build_conversation_headers(self.conversation_id));
        if let Some(subagent) = subagent_header(&self.session_source) {
            insert_header(&mut headers, "x-openai-subagent", &subagent);
        }

        Ok(ResponsesRequest {
            body,
            headers,
            compression: self.compression,
        })
    }
}

fn attach_item_ids(payload_json: &mut Value, original_items: &[ResponseItem]) {
    let Some(input_value) = payload_json.get_mut("input") else {
        return;
    };
    let Value::Array(items) = input_value else {
        return;
    };

    for (value, item) in items.iter_mut().zip(original_items.iter()) {
        if let ResponseItem::Reasoning { id, .. }
        | ResponseItem::Message { id: Some(id), .. }
        | ResponseItem::WebSearchCall { id: Some(id), .. }
        | ResponseItem::FunctionCall { id: Some(id), .. }
        | ResponseItem::LocalShellCall { id: Some(id), .. }
        | ResponseItem::CustomToolCall { id: Some(id), .. } = item
        {
            if id.is_empty() {
                continue;
            }

            if let Some(obj) = value.as_object_mut() {
                obj.insert("id".to_string(), Value::String(id.clone()));
            }
        }
    }
}

const OUTPUT_METADATA_ENV_VAR: &str = "CODEX_INCLUDE_OUTPUT_METADATA_IN_REQUEST";

fn include_output_metadata_in_request(provider: &Provider) -> bool {
    if let Ok(value) = std::env::var(OUTPUT_METADATA_ENV_VAR)
        && let Some(parsed) = parse_env_bool(&value)
    {
        return parsed;
    }

    !is_public_openai_endpoint(provider)
}

fn parse_env_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn is_public_openai_endpoint(provider: &Provider) -> bool {
    let base = provider.base_url.to_ascii_lowercase();
    base.contains("api.openai.com") || provider.is_azure_responses_endpoint()
}

fn strip_output_metadata(payload_json: &mut Value) {
    let Some(input_value) = payload_json.get_mut("input") else {
        return;
    };
    let Value::Array(items) = input_value else {
        return;
    };

    for item in items {
        if let Some(obj) = item.as_object_mut() {
            obj.remove("output_metadata");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::RetryConfig;
    use crate::provider::WireApi;
    use codex_protocol::models::FunctionCallOutputMetadata;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::protocol::SubAgentSource;
    use http::HeaderValue;
    use pretty_assertions::assert_eq;
    use std::sync::Mutex;
    use std::time::Duration;

    fn provider(name: &str, base_url: &str) -> Provider {
        Provider {
            name: name.to_string(),
            base_url: base_url.to_string(),
            query_params: None,
            wire: WireApi::Responses,
            headers: HeaderMap::new(),
            retry: RetryConfig {
                max_attempts: 1,
                base_delay: Duration::from_millis(50),
                retry_429: false,
                retry_5xx: true,
                retry_transport: true,
            },
            stream_idle_timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn azure_default_store_attaches_ids_and_headers() {
        let provider = provider("azure", "https://example.openai.azure.com/v1");
        let input = vec![
            ResponseItem::Message {
                id: Some("m1".into()),
                role: "assistant".into(),
                content: Vec::new(),
            },
            ResponseItem::Message {
                id: None,
                role: "assistant".into(),
                content: Vec::new(),
            },
        ];

        let request = ResponsesRequestBuilder::new("gpt-test", "inst", &input)
            .conversation(Some("conv-1".into()))
            .session_source(Some(SessionSource::SubAgent(SubAgentSource::Review)))
            .build(&provider)
            .expect("request");

        assert_eq!(request.body.get("store"), Some(&Value::Bool(true)));

        let ids: Vec<Option<String>> = request
            .body
            .get("input")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .map(|item| item.get("id").and_then(|v| v.as_str().map(str::to_string)))
            .collect();
        assert_eq!(ids, vec![Some("m1".to_string()), None]);

        assert_eq!(
            request.headers.get("conversation_id"),
            Some(&HeaderValue::from_static("conv-1"))
        );
        assert_eq!(
            request.headers.get("session_id"),
            Some(&HeaderValue::from_static("conv-1"))
        );
        assert_eq!(
            request.headers.get("x-openai-subagent"),
            Some(&HeaderValue::from_static("review"))
        );
    }

    #[test]
    fn strips_output_metadata_for_public_openai_by_default() {
        let _lock = OUTPUT_METADATA_ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var(OUTPUT_METADATA_ENV_VAR);
        }

        let input = vec![ResponseItem::FunctionCallOutput {
            call_id: "call-1".into(),
            output: FunctionCallOutputPayload {
                content: "ok".into(),
                success: Some(true),
                ..Default::default()
            },
            output_metadata: Some(FunctionCallOutputMetadata { success: true }),
        }];

        let provider = provider("openai", "https://api.openai.com/v1");
        let request = ResponsesRequestBuilder::new("gpt-test", "inst", &input)
            .build(&provider)
            .expect("request");

        let first_item = request
            .body
            .get("input")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .and_then(|item| item.as_object())
            .expect("request input item");

        assert!(!first_item.contains_key("output_metadata"));
    }

    #[test]
    fn keeps_output_metadata_when_env_opt_in() {
        let _lock = OUTPUT_METADATA_ENV_LOCK.lock().unwrap();
        let _guard = EnvVarGuard::set(Some("1"));

        let input = vec![ResponseItem::FunctionCallOutput {
            call_id: "call-1".into(),
            output: FunctionCallOutputPayload {
                content: "ok".into(),
                success: Some(false),
                ..Default::default()
            },
            output_metadata: Some(FunctionCallOutputMetadata { success: false }),
        }];

        let provider = provider("openai", "https://api.openai.com/v1");
        let request = ResponsesRequestBuilder::new("gpt-test", "inst", &input)
            .build(&provider)
            .expect("request");

        let metadata = request
            .body
            .get("input")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .and_then(|item| item.get("output_metadata"))
            .and_then(|v| v.get("success"))
            .and_then(serde_json::Value::as_bool);

        assert_eq!(metadata, Some(false));
    }

    static OUTPUT_METADATA_ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(value: Option<&str>) -> Self {
            let previous = std::env::var(OUTPUT_METADATA_ENV_VAR).ok();
            match value {
                Some(v) => unsafe { std::env::set_var(OUTPUT_METADATA_ENV_VAR, v) },
                None => unsafe { std::env::remove_var(OUTPUT_METADATA_ENV_VAR) },
            }
            Self { previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(prev) = &self.previous {
                unsafe { std::env::set_var(OUTPUT_METADATA_ENV_VAR, prev) };
            } else {
                unsafe { std::env::remove_var(OUTPUT_METADATA_ENV_VAR) };
            }
        }
    }
}
