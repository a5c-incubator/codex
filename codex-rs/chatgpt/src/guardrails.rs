use codex_core::config::Config;

use crate::chatgpt_client::chatgpt_get_request;

/// Windowed guardrail usage information returned by the ChatGPT backend.
#[derive(Debug, serde::Deserialize, Clone)]
pub struct GuardrailUsageWindow {
    /// Seconds used within this window, if known.
    pub used_seconds: Option<u64>,
    /// Total seconds available for this window, if known.
    pub limit_seconds: Option<u64>,
    /// Seconds until this window resets, if known.
    pub resets_in_seconds: Option<u64>,
}

/// Guardrail usage info across multiple windows (e.g. 5-hour and weekly).
#[derive(Debug, serde::Deserialize, Clone)]
pub struct GuardrailUsage {
    /// Usage for the rolling 5-hour window.
    #[serde(default)]
    pub five_hour: Option<GuardrailUsageWindow>,
    /// Usage for the weekly window.
    #[serde(default)]
    pub weekly: Option<GuardrailUsageWindow>,
}

/// Attempt to fetch guardrail usage information from the ChatGPT backend.
///
/// Returns `Ok(Some(..))` when data is available; `Ok(None)` if the request
/// fails or the endpoint is not available for the current account. Callers
/// should fall back to a friendly placeholder in that case.
pub async fn get_guardrail_usage(config: &Config) -> anyhow::Result<Option<GuardrailUsage>> {
    // Best-effort query; if it fails for any reason, present no usage instead
    // of surfacing an error to the user.
    //
    // Note: Path subject to change upstream; callers rely on the `Option`
    // contract to gracefully handle absence.
    let path = "/wham/guardrails/usage".to_string();
    match chatgpt_get_request::<GuardrailUsage>(config, path).await {
        Ok(resp) => Ok(Some(resp)),
        Err(_e) => Ok(None),
    }
}

/// Utility to format a human-friendly duration like "3 hours 5 minutes".
pub fn format_reset_duration(total_secs: u64) -> String {
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3_600;
    let minutes = (total_secs % 3_600) / 60;
    let mut parts: Vec<String> = Vec::new();
    if days > 0 {
        parts.push(format!("{days} day{}", if days == 1 { "" } else { "s" }));
    }
    if hours > 0 {
        parts.push(format!("{hours} hour{}", if hours == 1 { "" } else { "s" }));
    }
    if minutes > 0 && days == 0 {
        // Only include minutes when less than a day to keep it concise.
        parts.push(format!(
            "{minutes} minute{}",
            if minutes == 1 { "" } else { "s" }
        ));
    }
    if parts.is_empty() {
        return "less than a minute".to_string();
    }
    parts.join(" ")
}
