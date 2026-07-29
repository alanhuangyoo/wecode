use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use console::{Style, Term};

use crate::config::{
    Config, WireApi, default_config_path, default_credentials_path, provider_preset,
};

#[derive(Clone, Debug, Default)]
pub struct SetupOptions {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub wire_api: Option<WireApi>,
    pub skip_key: bool,
}

#[derive(Clone, Debug)]
pub struct SetupReport {
    pub config_path: PathBuf,
    pub credentials_path: Option<PathBuf>,
}

pub fn run(options: SetupOptions) -> Result<SetupReport> {
    let config_path = default_config_path();
    let mut config = load_user_config(&config_path)?;
    let guided = options.provider.is_none()
        && options.model.is_none()
        && options.base_url.is_none()
        && options.wire_api.is_none()
        && !options.skip_key;

    if guided {
        let term = Term::stdout();
        term.write_line(&format!(
            "{}",
            Style::new().cyan().bold().apply_to("Configure WeCode")
        ))?;
        term.write_line("Press Enter to keep the current value.\n")?;

        let provider = prompt_value(&term, "Provider", &config.model.provider)?;
        if provider != config.model.provider {
            config.model = provider_preset(&provider, None)?;
        }
        config.model.model = prompt_value(&term, "Model", &config.model.model)?;
        config.model.base_url = prompt_value(&term, "Base URL", &config.model.base_url)?;
        config.model.wire_api = prompt_wire_api(&term, config.model.wire_api)?;
    } else {
        if let Some(provider) = options.provider {
            config.model = provider_preset(&provider, None)?;
        }
        if let Some(model) = options.model {
            config.model.model = model;
        }
        if let Some(base_url) = options.base_url {
            config.model.base_url = base_url;
        }
        if let Some(wire_api) = options.wire_api {
            config.model.wire_api = wire_api;
        }
    }

    config.validate()?;
    let credentials_path = if options.skip_key {
        None
    } else {
        let term = Term::stdout();
        term.write_str("API key (hidden, leave empty to keep the current credential): ")?;
        let api_key = term.read_secure_line()?;
        let api_key = api_key.trim();
        if api_key.is_empty() {
            None
        } else {
            let path = default_credentials_path();
            write_secret(&path, api_key)?;
            config.model.api_key_file = Some(path.clone());
            Some(path)
        }
    };

    write_config(&config_path, &config)?;
    Ok(SetupReport {
        config_path,
        credentials_path,
    })
}

fn load_user_config(path: &Path) -> Result<Config> {
    if !path.is_file() {
        return Ok(Config::default());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("failed to parse config {}", path.display()))
}

fn write_config(path: &Path, config: &Config) -> Result<()> {
    let parent = path.parent().context("config path has no parent")?;
    create_private_directory(parent)?;
    std::fs::write(path, toml::to_string_pretty(config)?)
        .with_context(|| format!("failed to write config {}", path.display()))
}

fn write_secret(path: &Path, api_key: &str) -> Result<()> {
    let parent = path.parent().context("credentials path has no parent")?;
    create_private_directory(parent)?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create credentials {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(api_key.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn create_private_directory(directory: &Path) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn prompt_value(term: &Term, label: &str, current: &str) -> Result<String> {
    term.write_str(&format!("{label} [{current}]: "))?;
    let value = term.read_line()?;
    let value = value.trim();
    if value.is_empty() {
        Ok(current.to_owned())
    } else {
        Ok(value.to_owned())
    }
}

fn prompt_wire_api(term: &Term, current: WireApi) -> Result<WireApi> {
    let current = match current {
        WireApi::ChatCompletions => "chat-completions",
        WireApi::Responses => "responses",
    };
    let value = prompt_value(term, "Wire API (responses/chat-completions)", current)?;
    match value.as_str() {
        "responses" => Ok(WireApi::Responses),
        "chat-completions" | "chat" => Ok(WireApi::ChatCompletions),
        _ => bail!("wire API must be responses or chat-completions"),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn credential_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("credentials");
        write_secret(&path, "test-secret").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "test-secret\n");
    }
}
