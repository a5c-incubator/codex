use crate::auth::AuthProvider;
use crate::auth::add_auth_headers;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::provider::WireApi;
use crate::telemetry::run_with_request_telemetry;
use codex_client::HttpTransport;
use codex_client::RequestTelemetry;
use http::HeaderMap;
use http::Method;
use serde_json::Value;
use std::sync::Arc;

/// Client responsible for registering subagents with Claude-compatible APIs.
pub struct SubagentsClient<T: HttpTransport, A: AuthProvider> {
    transport: T,
    provider: Provider,
    auth: A,
    request_telemetry: Option<Arc<dyn RequestTelemetry>>,
}

impl<T: HttpTransport, A: AuthProvider> SubagentsClient<T, A> {
    /// Creates a new client backed by the provided HTTP transport.
    pub fn new(transport: T, provider: Provider, auth: A) -> Self {
        Self {
            transport,
            provider,
            auth,
            request_telemetry: None,
        }
    }

    /// Associates request telemetry with the client.
    pub fn with_telemetry(mut self, request: Option<Arc<dyn RequestTelemetry>>) -> Self {
        self.request_telemetry = request;
        self
    }

    fn path(&self) -> &'static str {
        match self.provider.wire {
            WireApi::Responses | WireApi::Compact => "responses/register_subagents",
            WireApi::Chat => "subagents/register",
        }
    }

    /// Sends the serialized manifest payload to the provider's registration endpoint.
    pub async fn register(
        &self,
        payload: Arc<Value>,
        extra_headers: HeaderMap,
    ) -> Result<(), ApiError> {
        let builder = || {
            let mut req = self.provider.build_request(Method::POST, self.path());
            req.headers.extend(extra_headers.clone());
            req.body = Some(payload.as_ref().clone());
            add_auth_headers(&self.auth, req)
        };

        run_with_request_telemetry(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            builder,
            |req| self.transport.execute(req),
        )
        .await
        .map(|_| ())
        .map_err(ApiError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use codex_client::Request;
    use codex_client::Response;
    use codex_client::StreamResponse;
    use codex_client::TransportError;
    use http::HeaderMap;
    use http::HeaderValue;
    use http::StatusCode;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Clone, Default)]
    struct CapturingTransport {
        last_request: Arc<Mutex<Option<Request>>>,
    }

    impl CapturingTransport {
        fn new() -> Self {
            Self::default()
        }

        fn last_request(&self) -> Request {
            self.last_request
                .lock()
                .unwrap_or_else(|err| panic!("mutex poisoned: {err}"))
                .as_ref()
                .expect("request recorded")
                .clone()
        }
    }

    #[async_trait]
    impl HttpTransport for CapturingTransport {
        async fn execute(&self, req: Request) -> Result<Response, TransportError> {
            *self
                .last_request
                .lock()
                .unwrap_or_else(|err| panic!("mutex poisoned: {err}")) = Some(req);
            Ok(Response {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            })
        }

        async fn stream(&self, _req: Request) -> Result<StreamResponse, TransportError> {
            Err(TransportError::Build(
                "register should not use streaming".to_string(),
            ))
        }
    }

    #[derive(Clone)]
    struct StaticAuth {
        token: Option<String>,
    }

    impl StaticAuth {
        fn new(token: Option<&str>) -> Self {
            Self {
                token: token.map(ToOwned::to_owned),
            }
        }
    }

    impl AuthProvider for StaticAuth {
        fn bearer_token(&self) -> Option<String> {
            self.token.clone()
        }
    }

    fn provider(name: &str, wire: WireApi) -> Provider {
        Provider {
            name: name.to_string(),
            base_url: "https://example.com/v1".to_string(),
            query_params: None,
            wire,
            headers: HeaderMap::new(),
            retry: crate::provider::RetryConfig {
                max_attempts: 1,
                base_delay: Duration::from_millis(1),
                retry_429: false,
                retry_5xx: true,
                retry_transport: true,
            },
            stream_idle_timeout: Duration::from_secs(1),
        }
    }

    fn header(name: &str, value: &str) -> HeaderValue {
        HeaderValue::from_str(value).unwrap_or_else(|err| panic!("invalid header {name}: {err}"))
    }

    #[tokio::test]
    async fn posts_responses_path_for_responses_wire() {
        let transport = CapturingTransport::new();
        let client = SubagentsClient::new(
            transport.clone(),
            provider("responses", WireApi::Responses),
            StaticAuth::new(Some("token")),
        );

        let payload = Arc::new(json!({ "subagents": [] }));
        let mut headers = HeaderMap::new();
        headers.insert("x-extra-header", header("x-extra-header", "1"));

        client
            .with_telemetry(None)
            .register(payload.clone(), headers.clone())
            .await
            .expect("request succeeds");

        let recorded = transport.last_request();
        assert!(
            recorded.url.ends_with("/responses/register_subagents"),
            "unexpected url: {}",
            recorded.url
        );
        assert_eq!(
            recorded
                .headers
                .get("x-extra-header")
                .expect("header missing"),
            &header("x-extra-header", "1")
        );
        assert_eq!(recorded.body.as_ref(), Some(payload.as_ref()));
        assert_eq!(
            recorded
                .headers
                .get(http::header::AUTHORIZATION)
                .expect("auth header missing"),
            &header(http::header::AUTHORIZATION.as_str(), "Bearer token")
        );
    }

    #[tokio::test]
    async fn posts_chat_path_for_chat_wire() {
        let transport = CapturingTransport::new();
        let client = SubagentsClient::new(
            transport.clone(),
            provider("chat", WireApi::Chat),
            StaticAuth::new(None),
        );

        let payload = Arc::new(json!({ "subagents": [{"id":"alpha"}] }));

        client
            .with_telemetry(None)
            .register(payload.clone(), HeaderMap::new())
            .await
            .expect("request succeeds");

        let recorded = transport.last_request();
        assert!(
            recorded.url.ends_with("/subagents/register"),
            "unexpected url: {}",
            recorded.url
        );
        assert_eq!(recorded.body.as_ref(), Some(payload.as_ref()));
        assert!(
            !recorded.headers.contains_key(http::header::AUTHORIZATION),
            "did not expect auth header"
        );
    }
}
