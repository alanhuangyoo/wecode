use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderFamily {
    #[default]
    OpenAiCompatible,
    Anthropic,
    Gemini,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum WireApi {
    #[default]
    ChatCompletions,
    Responses,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CacheMode {
    Off,
    ReadOnly,
    #[default]
    ReadWrite,
    Refresh,
}

impl CacheMode {
    pub fn can_read(self) -> bool {
        matches!(self, Self::ReadOnly | Self::ReadWrite)
    }

    pub fn can_write(self) -> bool {
        matches!(self, Self::ReadWrite | Self::Refresh)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PromptCacheMode {
    Off,
    #[default]
    Auto,
    Long,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ModelConfig {
    pub provider: String,
    pub family: ProviderFamily,
    pub wire_api: WireApi,
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
    pub api_key_file: Option<PathBuf>,
    pub max_output_tokens: u32,
    pub temperature: Option<f32>,
    pub prompt_cache: PromptCacheMode,
    pub send_prompt_cache_key: bool,
    pub native_tools: bool,
}

impl Default for ModelConfig {
    fn default() -> Self {
        provider_preset("openai", None)
            .expect("the built-in OpenAI provider preset must always exist")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentConfig {
    pub max_steps: usize,
    pub max_format_errors: usize,
    pub wall_time_limit_seconds: u64,
    pub command_timeout_seconds: u64,
    pub command_output_bytes: usize,
    pub context_max_tokens: u64,
    pub context_keep_messages: usize,
    pub verify_retries: usize,
    pub deny_dangerous_commands: bool,
    pub trajectory_directory: PathBuf,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: 40,
            max_format_errors: 3,
            wall_time_limit_seconds: 1_800,
            command_timeout_seconds: 120,
            command_output_bytes: 24_000,
            context_max_tokens: 90_000,
            context_keep_messages: 12,
            verify_retries: 2,
            deny_dangerous_commands: true,
            trajectory_directory: default_state_dir(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct CacheConfig {
    pub mode: CacheMode,
    pub directory: PathBuf,
    pub max_megabytes: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            mode: CacheMode::ReadWrite,
            directory: default_cache_dir().join("responses"),
            max_megabytes: 2_048,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub model: ModelConfig,
    pub agent: AgentConfig,
    pub cache: CacheConfig,
}

impl Config {
    pub fn load(workspace: &Path, explicit: Option<&Path>) -> Result<Self> {
        let path = explicit
            .map(Path::to_path_buf)
            .or_else(|| {
                let project = workspace.join(".wecode.toml");
                project.is_file().then_some(project)
            })
            .or_else(|| {
                let user = default_config_path();
                user.is_file().then_some(user)
            });

        let Some(path) = path else {
            return Ok(Self::default());
        };
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("failed to parse config {}", path.display()))
    }

    pub fn apply_provider(&mut self, provider: &str) -> Result<()> {
        self.model = provider_preset(provider, None)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.model.model.trim().is_empty() {
            bail!("model name cannot be empty");
        }
        if self.model.base_url.trim().is_empty() {
            bail!("model base_url cannot be empty");
        }
        if self.agent.max_steps == 0 {
            bail!("agent.max_steps must be greater than zero");
        }
        if self.agent.command_output_bytes < 1_024 {
            bail!("agent.command_output_bytes must be at least 1024");
        }
        Ok(())
    }

    pub fn api_key(&self) -> Result<Option<String>> {
        if let Ok(value) = env::var("WECODE_API_KEY")
            && !value.is_empty()
        {
            return Ok(Some(value));
        }
        if let Some(path) = &self.model.api_key_file {
            validate_secret_file_permissions(path)?;
            let value = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read API key file {}", path.display()))?;
            let value = value.trim().to_owned();
            if value.is_empty() {
                bail!("API key file {} is empty", path.display());
            }
            return Ok(Some(value));
        }
        if self.model.api_key_env.is_empty() {
            return Ok(None);
        }
        match env::var(&self.model.api_key_env) {
            Ok(value) if !value.is_empty() => Ok(Some(value)),
            _ if is_local_provider(&self.model.provider) => Ok(None),
            _ => bail!(
                "missing API key: set {} or WECODE_API_KEY",
                self.model.api_key_env
            ),
        }
    }

    pub fn command_timeout(&self) -> Duration {
        Duration::from_secs(self.agent.command_timeout_seconds)
    }
}

pub fn provider_preset(name: &str, model_override: Option<String>) -> Result<ModelConfig> {
    let key = name.to_ascii_lowercase();
    let mut config = match key.as_str() {
        "openai" => ModelConfig {
            provider: key,
            family: ProviderFamily::OpenAiCompatible,
            wire_api: WireApi::Responses,
            model: "gpt-5.4".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key_env: "OPENAI_API_KEY".into(),
            api_key_file: None,
            max_output_tokens: 8_192,
            temperature: None,
            prompt_cache: PromptCacheMode::Auto,
            send_prompt_cache_key: true,
            native_tools: true,
        },
        "anthropic" => ModelConfig {
            provider: key,
            family: ProviderFamily::Anthropic,
            wire_api: WireApi::ChatCompletions,
            model: "claude-sonnet-4-6".into(),
            base_url: "https://api.anthropic.com".into(),
            api_key_env: "ANTHROPIC_API_KEY".into(),
            api_key_file: None,
            max_output_tokens: 8_192,
            temperature: None,
            prompt_cache: PromptCacheMode::Long,
            send_prompt_cache_key: false,
            native_tools: true,
        },
        "gemini" | "google" => ModelConfig {
            provider: "gemini".into(),
            family: ProviderFamily::Gemini,
            wire_api: WireApi::ChatCompletions,
            model: "gemini-2.5-pro".into(),
            base_url: "https://generativelanguage.googleapis.com".into(),
            api_key_env: "GEMINI_API_KEY".into(),
            api_key_file: None,
            max_output_tokens: 8_192,
            temperature: None,
            prompt_cache: PromptCacheMode::Auto,
            send_prompt_cache_key: false,
            native_tools: true,
        },
        "openrouter" => openai_compatible(
            key,
            "https://openrouter.ai/api/v1",
            "OPENROUTER_API_KEY",
            "anthropic/claude-sonnet-4.6",
        ),
        "deepseek" => openai_compatible(
            key,
            "https://api.deepseek.com",
            "DEEPSEEK_API_KEY",
            "deepseek-chat",
        ),
        "groq" => openai_compatible(
            key,
            "https://api.groq.com/openai/v1",
            "GROQ_API_KEY",
            "openai/gpt-oss-120b",
        ),
        "xai" => {
            let mut value = openai_compatible(
                key,
                "https://api.x.ai/v1",
                "XAI_API_KEY",
                "grok-code-fast-1",
            );
            value.send_prompt_cache_key = true;
            value
        }
        "mistral" => openai_compatible(
            key,
            "https://api.mistral.ai/v1",
            "MISTRAL_API_KEY",
            "devstral-medium-latest",
        ),
        "ollama" => openai_compatible(key, "http://127.0.0.1:11434/v1", "", "qwen3-coder"),
        "lmstudio" => openai_compatible(key, "http://127.0.0.1:1234/v1", "", "local-model"),
        "vllm" => openai_compatible(key, "http://127.0.0.1:8000/v1", "", "local-model"),
        _ => bail!(
            "unknown provider {name:?}; use openai, anthropic, gemini, openrouter, deepseek, groq, xai, mistral, ollama, lmstudio, or vllm"
        ),
    };
    if let Some(model) = model_override
        && !model.trim().is_empty()
    {
        config.model = model;
    }
    Ok(config)
}

fn openai_compatible(
    provider: String,
    base_url: &str,
    api_key_env: &str,
    model: &str,
) -> ModelConfig {
    ModelConfig {
        provider,
        family: ProviderFamily::OpenAiCompatible,
        wire_api: WireApi::ChatCompletions,
        model: model.into(),
        base_url: base_url.into(),
        api_key_env: api_key_env.into(),
        api_key_file: None,
        max_output_tokens: 8_192,
        temperature: None,
        prompt_cache: PromptCacheMode::Auto,
        send_prompt_cache_key: false,
        native_tools: true,
    }
}

pub fn default_config_path() -> PathBuf {
    wecode_home_dir().join("config.toml")
}

pub fn default_cache_dir() -> PathBuf {
    wecode_home_dir().join("cache")
}

pub fn default_state_dir() -> PathBuf {
    wecode_home_dir().join("sessions")
}

fn wecode_home_dir() -> PathBuf {
    if let Some(path) = env::var_os("WECODE_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    BaseDirs::new()
        .map(|dirs| dirs.home_dir().join(".wecode"))
        .unwrap_or_else(|| std::env::temp_dir().join("wecode"))
}

fn is_local_provider(provider: &str) -> bool {
    matches!(provider, "ollama" | "lmstudio" | "vllm")
}

fn validate_secret_file_permissions(path: &Path) -> Result<()> {
    #[cfg(not(unix))]
    let _ = path;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = std::fs::metadata(path)
            .with_context(|| format!("failed to inspect API key file {}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            bail!(
                "API key file {} is accessible by group or others; run chmod 600 {}",
                path.display(),
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_presets_have_expected_protocols() {
        assert_eq!(
            provider_preset("openai", None).unwrap().wire_api,
            WireApi::Responses
        );
        assert_eq!(
            provider_preset("anthropic", None).unwrap().family,
            ProviderFamily::Anthropic
        );
        assert!(provider_preset("unknown", None).is_err());
    }

    #[test]
    fn cache_modes_expose_correct_capabilities() {
        assert!(CacheMode::ReadWrite.can_read());
        assert!(CacheMode::ReadWrite.can_write());
        assert!(!CacheMode::Refresh.can_read());
        assert!(CacheMode::Refresh.can_write());
    }

    #[test]
    fn user_data_paths_share_the_wecode_home() {
        let home = wecode_home_dir();
        assert_eq!(default_config_path(), home.join("config.toml"));
        assert_eq!(default_cache_dir(), home.join("cache"));
        assert_eq!(default_state_dir(), home.join("sessions"));
    }

    #[cfg(unix)]
    #[test]
    fn secret_file_permissions_must_exclude_group_and_others() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("api-key");
        std::fs::write(&path, "test-key\n").unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        validate_secret_file_permissions(&path).unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let error = validate_secret_file_permissions(&path).unwrap_err();
        assert!(error.to_string().contains("chmod 600"));
    }
}
