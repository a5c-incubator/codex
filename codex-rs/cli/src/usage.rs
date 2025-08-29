use codex_common::CliConfigOverrides;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_login::AuthMode;
use codex_login::CodexAuth;

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
                AuthMode::ApiKey | AuthMode::ChatGPT => {
                    println!(
                        "Usage information is currently unavailable.\nPlan: {plan}\nNote: Guardrail usage and reset times will appear here when supported."
                    );
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
