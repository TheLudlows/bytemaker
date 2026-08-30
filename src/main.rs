//! main.rs — binary entry (REPL).
//!
//! Bootstraps env/config → init tracing → `IO::console()` → logo + banner →
//! build `Agent` → `start_cron_runtime` → `run_interactive` → `shutdown`.

use std::sync::Arc;

use bytemaker::agent::{Agent, AgentConfig};
use bytemaker::config::Config;
use bytemaker::error::AgentError;
use dotenv::dotenv;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), AgentError> {
    // ch5: `bytemaker eval ...` 子命令。必须在 Config::from_env() 之前分发——
    // `--replay` 离线回放不要求 OPENAI_API_KEY（docs/5.evals.md 验收 #3）。
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("eval") {
        return bytemaker::eval::run_cli(&argv[2..]).await;
    }

    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_file(true)
        .with_line_number(true)
        .init();

    let env_cfg = Config::from_env()?;
    let api_key = env_cfg.api_key.clone();
    let base_url = env_cfg.base_url.clone();
    let model = env_cfg.model.clone();
    let cwd = env_cfg.workdir.clone();
    let skills_dir = env_cfg.skills_dir.clone();

    // Create the I/O combo (Coordinator is an internal detail).
    let io = Arc::new(bytemaker::io::IO::console());

    // logo: show immediately after creating I/O.
    io.output.logo();

    io.output.banner("Enter a question, press Enter to send. Type q to quit.\n");
    io.output.banner(&format!(
        "base_url: {}, model: {}, key: {}",
        base_url, model, "***"
    ));
    let cfg = AgentConfig {
        api_key,
        base_url,
        model,
        workdir: cwd.clone(),
        skills_dir: skills_dir.clone(),
        io: Arc::clone(&io),
    };
    let agent = Agent::new(cfg).await?;
    agent.start_cron_runtime().await?;
    io.output.banner(&format!(
        "Loaded {} skill(s) from {}",
        agent.skills_len(),
        skills_dir.display()
    ));

    agent.run_interactive().await?;
    agent.shutdown().await;

    Ok(())
}
