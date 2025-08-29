use chrono::Local;
use codex_common::CliConfigOverrides;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_login::AuthMode;
use codex_login::CodexAuth;

pub async fn run_usage(cli_config_overrides: CliConfigOverrides) -> anyhow::Result<()> {
    let config = load_config(cli_config_overrides)?;

    match CodexAuth::from_codex_home(&config.codex_home, config.preferred_auth_method) {
        Ok(Some(auth)) => match codex_login::usage::fetch_usage(&auth).await {
            Ok(info) => {
                print_usage(info);
            }
            Err(e) => {
                eprintln!("Usage data unavailable: {e}");
            }
        },
        Ok(None) => {
            eprintln!("Not logged in — usage data unavailable");
        }
        Err(e) => {
            eprintln!("Failed to load auth: {e}");
        }
    }

    Ok(())
}

fn load_config(cli_config_overrides: CliConfigOverrides) -> anyhow::Result<Config> {
    let overrides_vec = cli_config_overrides
        .parse_overrides()
        .map_err(anyhow::Error::msg)?;
    let config_overrides = ConfigOverrides::default();
    let config = Config::load_with_cli_overrides(overrides_vec, config_overrides)?;
    Ok(config)
}

fn fmt_ts(ts: chrono::DateTime<chrono::Utc>) -> String {
    let local = ts.with_timezone(&Local);
    format!(
        "{} (local {})",
        ts.to_rfc3339(),
        local.format("%Y-%m-%d %H:%M:%S %Z")
    )
}

fn print_usage(info: codex_login::usage::UsageInfo) {
    println!("/usage");
    println!("\n📈 Guardrail Usage");
    match info.plan.as_deref() {
        Some(plan) => println!("  • Plan: {plan}"),
        None => {}
    }
    if let Some(five) = info.five_hour {
        println!(
            "  • 5-hour: {} / {} minutes",
            five.used_minutes, five.limit_minutes
        );
        if let Some(reset_at) = five.reset_at {
            println!("    • Resets at: {}", fmt_ts(reset_at));
        }
    } else {
        println!("  • 5-hour: unavailable");
    }
    if let Some(week) = info.weekly {
        println!(
            "  • Weekly: {} / {} minutes",
            week.used_minutes, week.limit_minutes
        );
        if let Some(reset_at) = week.reset_at {
            println!("    • Resets at: {}", fmt_ts(reset_at));
        }
    } else {
        println!("  • Weekly: unavailable");
    }

    match info.source {
        codex_login::usage::UsageSource::ChatGptWeb => {}
        codex_login::usage::UsageSource::OpenAiApiKey => {}
        codex_login::usage::UsageSource::Unknown => {
            println!("\n  • Note: usage source unknown; values may be unavailable.");
        }
    }
}
