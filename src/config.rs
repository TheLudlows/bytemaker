//! 环境配置加载（docs §2.4，12-Factor）。密钥与路由从环境变量注入；
//! 缺 `OPENAI_API_KEY` 时启动即错（验收 #1）。

use std::env;
use std::path::PathBuf;

use crate::error::AgentError;

#[derive(Debug, Clone)]
pub struct Config {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub workdir: PathBuf,
    pub skills_dir: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self, AgentError> {
        let api_key = env::var("OPENAI_API_KEY")
            .map_err(|_| AgentError::Validation("OPENAI_API_KEY not set".into()))?;
        if api_key.trim().is_empty() {
            return Err(AgentError::Validation("OPENAI_API_KEY is empty".into()));
        }
        let base_url = env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        let model = env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4.1".to_string());
        let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
        let skills_dir = env::var("SKILLS_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("{}/skills", cwd.to_string_lossy()));
        Ok(Self {
            api_key,
            base_url,
            model,
            workdir: cwd,
            skills_dir: PathBuf::from(&skills_dir),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // These tests mutate process env; serialize them against each other so they don't race.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn from_env_errors_when_api_key_missing() {
        let _g = ENV_LOCK.lock().unwrap();
        env::remove_var("OPENAI_API_KEY");
        let err = Config::from_env().unwrap_err();
        assert!(matches!(err, AgentError::Validation(_)));
    }

    #[test]
    fn from_env_errors_when_api_key_empty() {
        let _g = ENV_LOCK.lock().unwrap();
        env::set_var("OPENAI_API_KEY", "   ");
        let err = Config::from_env().unwrap_err();
        assert!(matches!(err, AgentError::Validation(_)));
        env::remove_var("OPENAI_API_KEY");
    }

    #[test]
    fn from_env_ok_with_defaults() {
        let _g = ENV_LOCK.lock().unwrap();
        env::set_var("OPENAI_API_KEY", "sk-test");
        env::remove_var("OPENAI_BASE_URL");
        env::remove_var("OPENAI_MODEL");
        let cfg = Config::from_env().expect("valid config");
        assert_eq!(cfg.api_key, "sk-test");
        assert_eq!(cfg.base_url, "https://api.openai.com/v1");
        assert_eq!(cfg.model, "gpt-4.1");
        env::remove_var("OPENAI_API_KEY");
    }
}
