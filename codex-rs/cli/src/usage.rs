use codex_common::CliConfigOverrides;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_login::AuthMode;
use codex_login::CodexAuth;

use codex_chatgpt::guardrails::format_reset_duration;
use codex_chatgpt::guardrails::get_guardrail_usage;

#[derive(Debug, clap::Parser)]
pub struct UsageCommand {
    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,
}

pub async fn run_usage_command(cli_config_overrides: CliConfigOverrides) -> anyhow::Result<()> {
    let config = load_config_or_exit(cli_config_overrides);

    match CodexAuth::from_codex_home(&config.codex_home, config.preferred_auth_method) {
        Ok(Some(auth)) => {
            let plan = auth
                .get_plan_type()
                .unwrap_or_else(|| "unknown".to_string());
            match auth.mode {
                AuthMode::ApiKey => {
                    println!(
                        "Plan: {plan}\nUsage information is not available for API key auth.\nVisit your OpenAI dashboard for usage details."
                    );
                }
                AuthMode::ChatGPT => {
                    if let Some(usage) = get_guardrail_usage(&config).await? {
                        println!("Plan: {plan}");
                        println!("ChatGPT guardrail usage:\n");
                        if let Some(ref five) = usage.five_hour {
                            let used = five.used_seconds.unwrap_or(0);
                            let limit = five.limit_seconds.unwrap_or(5 * 3600);
                            let rem = limit.saturating_sub(used);
                            let reset = five
                                .resets_in_seconds
                                .map(format_reset_duration)
                                .unwrap_or_else(|| "unknown".to_string());
                            println!("5-hour window:");
                            println!(
                                "  • Used: {} min {} sec (remaining: {} min)",
                                used / 60,
                                used % 60,
                                rem / 60
                            );
                            println!("  • Limit: {} min", limit / 60);
                            println!("  • Resets: in {reset}\n");
                        }
                        if let Some(ref week) = usage.weekly {
                            let used = week.used_seconds.unwrap_or(0);
                            let limit = week.limit_seconds.unwrap_or(7 * 5 * 3600);
                            let rem = limit.saturating_sub(used);
                            let reset = week
                                .resets_in_seconds
                                .map(format_reset_duration)
                                .unwrap_or_else(|| "unknown".to_string());
                            println!("Weekly window:");
                            println!(
                                "  • Used: {} h {} min (remaining: {} h)",
                                used / 3600,
                                (used % 3600) / 60,
                                rem / 3600
                            );
                            println!("  • Limit: {} h", limit / 3600);
                            println!("  • Resets: in {reset}");
                        }
                        if usage.five_hour.is_none() && usage.weekly.is_none() {
                            println!(
                                "Usage information is currently unavailable.\nNote: Guardrail usage and reset times will appear here when supported."
                            );
                        }
                    } else {
                        println!(
                            "Usage information is currently unavailable.\nPlan: {plan}\nNote: Guardrail usage and reset times will appear here when supported."
                        );
                    }
                }
            }
            Ok(())
        }
        Ok(None) => {
            println!("Not logged in. Usage information requires authentication.\nRun: codex login");
            Ok(())
        }
        Err(e) => {
            println!(
                "Unable to determine authentication status.\nReason: {e}\nUsage information is currently unavailable."
            );
            Ok(())
        }
    }
}

fn load_config_or_exit(cli_config_overrides: CliConfigOverrides) -> Config {
    let cli_overrides = match cli_config_overrides.parse_overrides() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error parsing -c overrides: {e}");
            std::process::exit(1);
        }
    };

    let config_overrides = ConfigOverrides::default();
    match Config::load_with_cli_overrides(cli_overrides, config_overrides) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Error loading configuration: {e}");
            std::process::exit(1);
        }
    }
}
