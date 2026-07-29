use std::collections::BTreeMap;
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    UnlessTrusted,
    #[default]
    OnRequest,
    Never,
}

impl ApprovalPolicy {
    pub fn requires_approval(
        self,
        kind: crate::approval::ApprovalKind,
        risk: crate::approval::RiskLevel,
    ) -> bool {
        match self {
            Self::Never => false,
            Self::OnRequest => risk == crate::approval::RiskLevel::Elevated,
            Self::UnlessTrusted => {
                kind == crate::approval::ApprovalKind::Patch
                    || risk != crate::approval::RiskLevel::ReadOnly
            }
        }
    }
}

impl QueueMode {
    pub fn take_all(self) -> bool {
        matches!(self, Self::All)
    }
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
    pub streaming: bool,
    pub request_max_retries: usize,
    pub max_retry_delay_seconds: u64,
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
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub approval_policy: ApprovalPolicy,
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
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            approval_policy: ApprovalPolicy::OnRequest,
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
pub struct McpConfig {
    pub servers: BTreeMap<String, McpServerConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub enabled: bool,
    pub startup_timeout_seconds: u64,
    pub tool_timeout_seconds: u64,
    pub max_output_bytes: usize,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            enabled: true,
            startup_timeout_seconds: 10,
            tool_timeout_seconds: 60,
            max_output_bytes: 64 * 1_024,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct SkillsConfig {
    pub enabled: bool,
    pub discover_user: bool,
    pub discover_project: bool,
    pub compatibility_directories: bool,
    pub paths: Vec<PathBuf>,
    pub max_skills: usize,
    pub max_file_bytes: usize,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            discover_user: true,
            discover_project: true,
            compatibility_directories: true,
            paths: Vec::new(),
            max_skills: 128,
            max_file_bytes: 128 * 1_024,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub model: ModelConfig,
    pub agent: AgentConfig,
    pub cache: CacheConfig,
    pub mcp: McpConfig,
    pub skills: SkillsConfig,
}

impl Config {
    pub fn load(workspace: &Path, explicit: Option<&Path>) -> Result<Self> {
        let project = workspace.join(".wecode.toml");
        let (path, automatically_loaded_project) = if let Some(explicit) = explicit {
            (Some(explicit.to_path_buf()), false)
        } else if project.is_file() {
            (Some(project), true)
        } else {
            let user = default_config_path();
            (user.is_file().then_some(user), false)
        };

        let Some(path) = path else {
            return Ok(Self::default());
        };
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let config: Self = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        if automatically_loaded_project && config.mcp.servers.values().any(|server| server.enabled)
        {
            bail!(
                "refusing to auto-start MCP commands from {}; review it, then pass --config {} explicitly or move trusted MCP configuration to {}",
                path.display(),
                path.display(),
                default_config_path().display()
            );
        }
        if automatically_loaded_project && !config.skills.paths.is_empty() {
            bail!(
                "refusing external skill paths from {}; review it, then pass --config {} explicitly or move trusted skill paths to {}",
                path.display(),
                path.display(),
                default_config_path().display()
            );
        }
        Ok(config)
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
        if self.model.request_max_retries > 10 {
            bail!("model.request_max_retries cannot exceed 10");
        }
        if self.model.max_retry_delay_seconds > 300 {
            bail!("model.max_retry_delay_seconds cannot exceed 300");
        }
        if self.agent.max_steps == 0 {
            bail!("agent.max_steps must be greater than zero");
        }
        if self.agent.command_output_bytes < 1_024 {
            bail!("agent.command_output_bytes must be at least 1024");
        }
        if self.mcp.servers.len() > 16 {
            bail!("mcp cannot configure more than 16 servers");
        }
        for (name, server) in &self.mcp.servers {
            validate_mcp_name(name, "server")?;
            if server.command.trim().is_empty() || server.command.len() > 4_096 {
                bail!("mcp server {name:?} command must contain between 1 and 4096 bytes");
            }
            if server.args.len() > 128
                || server
                    .args
                    .iter()
                    .any(|argument| argument.len() > 16 * 1_024)
            {
                bail!("mcp server {name:?} arguments exceed the configured limits");
            }
            if server.env.len() > 128 || server.env.values().any(|value| value.len() > 16 * 1_024) {
                bail!("mcp server {name:?} environment exceeds the configured limits");
            }
            if server.startup_timeout_seconds == 0 || server.startup_timeout_seconds > 300 {
                bail!("mcp server {name:?} startup_timeout_seconds must be between 1 and 300");
            }
            if server.tool_timeout_seconds == 0 || server.tool_timeout_seconds > 1_800 {
                bail!("mcp server {name:?} tool_timeout_seconds must be between 1 and 1800");
            }
            if !(1_024..=16 * 1_024 * 1_024).contains(&server.max_output_bytes) {
                bail!("mcp server {name:?} max_output_bytes must be between 1024 and 16777216");
            }
            for env_name in server.env.keys() {
                if !valid_env_name(env_name) {
                    bail!("mcp server {name:?} has invalid environment variable {env_name:?}");
                }
            }
        }
        if self.skills.paths.len() > 32 {
            bail!("skills.paths cannot contain more than 32 entries");
        }
        if !(1..=512).contains(&self.skills.max_skills) {
            bail!("skills.max_skills must be between 1 and 512");
        }
        if !(4 * 1_024..=1_024 * 1_024).contains(&self.skills.max_file_bytes) {
            bail!("skills.max_file_bytes must be between 4096 and 1048576");
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

fn validate_mcp_name(name: &str, kind: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 32
        || name.contains("__")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        bail!(
            "mcp {kind} name {name:?} must be 1-32 ASCII letters, digits, underscores, or hyphens and cannot contain \"__\""
        );
    }
    Ok(())
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
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
            streaming: true,
            request_max_retries: 3,
            max_retry_delay_seconds: 60,
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
            streaming: true,
            request_max_retries: 3,
            max_retry_delay_seconds: 60,
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
            streaming: true,
            request_max_retries: 3,
            max_retry_delay_seconds: 60,
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
        streaming: true,
        request_max_retries: 3,
        max_retry_delay_seconds: 60,
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

pub fn default_credentials_path() -> PathBuf {
    wecode_home_dir().join("credentials")
}

pub fn default_history_path() -> PathBuf {
    wecode_home_dir().join("history")
}

pub fn wecode_home_dir() -> PathBuf {
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
        assert_eq!(default_credentials_path(), home.join("credentials"));
        assert_eq!(default_history_path(), home.join("history"));
    }

    #[test]
    fn mcp_config_rejects_unsafe_names_and_unbounded_limits() {
        let mut config = Config::default();
        config.mcp.servers.insert(
            "bad.name".into(),
            McpServerConfig {
                command: "server".into(),
                ..Default::default()
            },
        );
        assert!(config.validate().is_err());

        config.mcp.servers.clear();
        config.mcp.servers.insert(
            "safe".into(),
            McpServerConfig {
                command: "server".into(),
                tool_timeout_seconds: 0,
                ..Default::default()
            },
        );
        assert!(config.validate().is_err());
    }

    #[test]
    fn project_mcp_requires_explicit_config_trust() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".wecode.toml");
        std::fs::write(
            &path,
            r#"
[mcp.servers.fixture]
command = "fixture-server"
"#,
        )
        .unwrap();
        let error = Config::load(temp.path(), None).unwrap_err();
        assert!(error.to_string().contains("refusing to auto-start MCP"));
        let explicit = Config::load(temp.path(), Some(&path)).unwrap();
        assert_eq!(explicit.mcp.servers["fixture"].command, "fixture-server");

        std::fs::write(
            &path,
            r#"
[skills]
paths = ["../external-skills"]
"#,
        )
        .unwrap();
        let error = Config::load(temp.path(), None).unwrap_err();
        assert!(error.to_string().contains("refusing external skill paths"));
        assert!(Config::load(temp.path(), Some(&path)).is_ok());
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
