use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::AuthMode;
use crate::CodexAuth;

#[derive(Debug, Clone)]
pub enum UsageSource {
    ChatGptWeb,
    OpenAiApiKey,
    Unknown,
}

impl Default for UsageSource {
    fn default() -> Self {
        UsageSource::Unknown
    }
}

#[derive(Debug, Clone, Default)]
pub struct WindowUsage {
    pub used_minutes: u32,
    pub limit_minutes: u32,
    pub reset_at: Option<DateTime<Utc>>, // when the window resets
}

#[derive(Debug, Clone, Default)]
pub struct UsageInfo {
    pub plan: Option<String>,
    pub five_hour: Option<WindowUsage>,
    pub weekly: Option<WindowUsage>,
    pub source: UsageSource,
}

#[derive(thiserror::Error, Debug)]
pub enum UsageError {
    #[error("not logged in")]
    NotLoggedIn,
    #[error("{0}")]
    Message(String),
}

/// Best-effort usage fetcher. Returns partial info when possible.
pub async fn fetch_usage(auth: &CodexAuth) -> Result<UsageInfo, UsageError> {
    let mut out = UsageInfo::default();
    out.plan = auth.get_plan_type();

    match auth.mode {
        AuthMode::ChatGPT => {
            // Try the ChatGPT web limits endpoint. This may fail in headless/CI environments.
            match fetch_chatgpt_web_limits(auth).await {
                Ok((five, weekly)) => {
                    out.five_hour = five;
                    out.weekly = weekly;
                    out.source = UsageSource::ChatGptWeb;
                }
                Err(e) => {
                    // Graceful: leave fields None and propagate a message upwards.
                    out.source = UsageSource::Unknown;
                    return Err(UsageError::Message(format!(
                        "could not fetch ChatGPT usage: {e}"
                    )));
                }
            }
        }
        AuthMode::ApiKey => {
            // API key usage details are not currently implemented.
            out.source = UsageSource::OpenAiApiKey;
            return Err(UsageError::Message(
                "API key usage endpoint not implemented".to_string(),
            ));
        }
    }

    Ok(out)
}

async fn fetch_chatgpt_web_limits(
    auth: &CodexAuth,
) -> Result<(Option<WindowUsage>, Option<WindowUsage>), String> {
    // Use the ChatGPT access token from auth.json.
    let token = auth
        .get_token()
        .await
        .map_err(|e| format!("failed to get access token: {e}"))?;

    // Known endpoint used by ChatGPT web app for usage guardrails. This can change.
    let url_candidates = [
        // Example historical endpoints
        "https://chat.openai.com/backend-api/user_limits",
        "https://chat.openai.com/backend-api/usage/limits",
    ];

    let client = reqwest::Client::builder()
        .user_agent("codex-cli/usage")
        .build()
        .map_err(|e| e.to_string())?;

    let mut last_err: Option<String> = None;
    for url in url_candidates.iter() {
        let req = client
            .get(*url)
            .bearer_auth(&token)
            .header("Accept", "application/json");

        match req.send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    last_err = Some(format!("{} returned status {}", url, resp.status()));
                    continue;
                }
                let text = resp.text().await.map_err(|e| e.to_string())?;
                if let Ok(parsed) = serde_json::from_str::<ChatGptLimits>(&text) {
                    let five = parsed
                        .limits
                        .as_ref()
                        .and_then(|l| l.five_hour.as_ref())
                        .map(window_from);
                    let weekly = parsed
                        .limits
                        .as_ref()
                        .and_then(|l| l.weekly.as_ref())
                        .map(window_from);
                    return Ok((five, weekly));
                } else if let Ok(parsed) = serde_json::from_str::<AltChatGptLimits>(&text) {
                    let five = parsed.five_hour.as_ref().map(window_from);
                    let weekly = parsed.weekly.as_ref().map(window_from);
                    return Ok((five, weekly));
                } else {
                    last_err = Some("unexpected response schema".to_string());
                }
            }
            Err(e) => {
                last_err = Some(e.to_string());
            }
        }
    }

    Err(last_err.unwrap_or_else(|| "failed to fetch usage".to_string()))
}

fn window_from(w: &Window) -> WindowUsage {
    WindowUsage {
        used_minutes: w.used_minutes.unwrap_or(0),
        limit_minutes: w.limit_minutes.unwrap_or(0),
        reset_at: w
            .resets_at
            .as_deref()
            .or(w.reset_at.as_deref())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
    }
}

// Primary schema
#[derive(Deserialize)]
struct ChatGptLimits {
    #[allow(dead_code)]
    plan: Option<String>,
    limits: Option<Limits>,
}

#[derive(Deserialize)]
struct Limits {
    #[serde(rename = "5h")]
    five_hour: Option<Window>,
    weekly: Option<Window>,
}

#[derive(Deserialize)]
struct Window {
    #[serde(default)]
    used_minutes: Option<u32>,
    #[serde(default)]
    limit_minutes: Option<u32>,
    #[serde(default)]
    resets_at: Option<String>,
    #[serde(default)]
    reset_at: Option<String>,
}

// Alternate flattened schema
#[derive(Deserialize)]
struct AltChatGptLimits {
    #[serde(rename = "5h")]
    five_hour: Option<Window>,
    weekly: Option<Window>,
}
