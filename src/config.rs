use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MomConfig {
    #[serde(default = "default_schema_version")]
    pub(crate) schema_version: u32,
    #[serde(default)]
    pub(crate) credentials: CredentialConfig,
    #[serde(default)]
    pub(crate) guest: GuestConfig,
    #[serde(default)]
    pub(crate) auth: AuthConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CredentialConfig {
    #[serde(default)]
    pub(crate) proxy_url: Option<String>,
    #[serde(default)]
    pub(crate) proxy_ca_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuestConfig {
    #[serde(default = "default_hermes_profile")]
    pub(crate) hermes_profile: String,
    #[serde(default = "default_model")]
    pub(crate) model: String,
}

impl Default for GuestConfig {
    fn default() -> Self {
        Self {
            hermes_profile: default_hermes_profile(),
            model: default_model(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthConfig {
    #[serde(default)]
    pub(crate) secret: Option<String>,
    #[serde(default)]
    pub(crate) secret_file: Option<PathBuf>,
    #[serde(default)]
    pub(crate) bootstrap_admin_code: Option<String>,
    #[serde(default)]
    pub(crate) bootstrap_admin_code_file: Option<PathBuf>,
}

impl MomConfig {
    pub(crate) fn hermes_profile(&self) -> &str {
        &self.guest.hermes_profile
    }

    pub(crate) fn model(&self) -> &str {
        &self.guest.model
    }

    pub(crate) fn credential_proxy_url(&self) -> Option<&str> {
        self.credentials.proxy_url.as_deref()
    }

    pub(crate) fn validate_for_guest_config(&self) -> Result<()> {
        let proxy_url = self.credential_proxy_url().unwrap_or("").trim();
        if proxy_url.is_empty() {
            bail!("credentials.proxy_url is required");
        }
        if self.credentials.proxy_ca_path.is_none() {
            bail!("credentials.proxy_ca_path is required");
        }
        Ok(())
    }

    pub(crate) fn validate_referenced_files(&self) -> Result<()> {
        self.validate_for_guest_config()?;
        let ca_path = self
            .credentials
            .proxy_ca_path
            .as_ref()
            .expect("validate_for_guest_config requires proxy_ca_path");
        resolve_required_file(ca_path, "credentials.proxy_ca_path")?;
        Ok(())
    }

    pub(crate) fn validate_for_node(&self) -> Result<()> {
        self.validate_referenced_files()?;
        Ok(())
    }

    pub(crate) fn validate_for_api(&self) -> Result<()> {
        self.auth_secret()?;
        self.bootstrap_admin_code()?;
        Ok(())
    }

    pub(crate) fn auth_secret(&self) -> Result<String> {
        required_config_secret(
            self.auth.secret.as_deref(),
            self.auth.secret_file.as_ref(),
            "auth.secret",
            "auth.secret_file",
        )
    }

    pub(crate) fn bootstrap_admin_code(&self) -> Result<String> {
        required_config_secret(
            self.auth.bootstrap_admin_code.as_deref(),
            self.auth.bootstrap_admin_code_file.as_ref(),
            "auth.bootstrap_admin_code",
            "auth.bootstrap_admin_code_file",
        )
    }

    pub(crate) fn redacted_json(&self) -> serde_json::Value {
        json!({
            "schema_version": self.schema_version,
            "credentials": {
                "proxy_url": self.credentials.proxy_url,
                "proxy_ca_path": self.credentials.proxy_ca_path.as_ref().map(|p| p.display().to_string()),
            },
            "guest": {
                "hermes_profile": self.guest.hermes_profile,
                "model": self.guest.model,
            },
            "auth": {
                "secret": self.auth.secret.as_ref().map(|_| "<redacted>"),
                "secret_file": self.auth.secret_file.as_ref().map(|p| p.display().to_string()),
                "bootstrap_admin_code": self.auth.bootstrap_admin_code.as_ref().map(|_| "<redacted>"),
                "bootstrap_admin_code_file": self.auth.bootstrap_admin_code_file.as_ref().map(|p| p.display().to_string()),
            }
        })
    }
}

pub(crate) fn load_mom_config() -> Result<MomConfig> {
    let path = config_path()?;
    let raw = fs::read_to_string(&path).with_context(|| {
        format!(
            "read Agent Mom config {}; create it or set MOM_CONFIG",
            path.display()
        )
    })?;
    serde_json::from_str(&raw).with_context(|| format!("parse Agent Mom config {}", path.display()))
}

pub(crate) fn config_path() -> Result<PathBuf> {
    match env::var_os("MOM_CONFIG") {
        Some(value) => Ok(PathBuf::from(value)),
        None => Ok(home_dir()?.join(".config").join("mom").join("config.json")),
    }
}

pub(crate) fn resolve_required_file(path: &Path, key: &str) -> Result<PathBuf> {
    let expanded = expand_tilde(path)?;
    expanded.canonicalize().with_context(|| {
        format!(
            "{key} does not point at a readable file: {}",
            expanded.display()
        )
    })
}

fn required_config_secret(
    inline: Option<&str>,
    file: Option<&PathBuf>,
    inline_key: &str,
    file_key: &str,
) -> Result<String> {
    if let Some(value) = inline.map(str::trim).filter(|value| !value.is_empty()) {
        return Ok(value.to_string());
    }
    if let Some(path) = file {
        let path = resolve_required_file(path, file_key)?;
        let value = fs::read_to_string(&path)
            .with_context(|| format!("read {file_key} {}", path.display()))?;
        let value = value.trim();
        if !value.is_empty() {
            return Ok(value.to_string());
        }
        bail!("{file_key} points at an empty file: {}", path.display());
    }
    bail!("{inline_key} or {file_key} is required");
}

pub(crate) fn expand_tilde(path: &Path) -> Result<PathBuf> {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return Ok(home_dir()?.join(rest));
    }
    Ok(path.to_path_buf())
}

pub(crate) fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))
}

fn default_schema_version() -> u32 {
    1
}

fn default_hermes_profile() -> String {
    "main".to_string()
}

fn default_model() -> String {
    "gpt-5.5".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_config(raw: &str) -> MomConfig {
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn parses_structured_config() {
        let config = parse_config(
            r#"{
              "schema_version": 1,
              "credentials": {
                "proxy_url": "http://127.0.0.1:1080",
                "proxy_ca_path": "/tmp/ca.crt"
              },
              "guest": {
                "hermes_profile": "main",
                "model": "openai/gpt-5.5"
              },
              "auth": {
                "secret": "dev-secret",
                "bootstrap_admin_code": "AM-TEST-ADMIN"
              }
            }"#,
        );

        assert_eq!(config.model(), "openai/gpt-5.5");
        assert_eq!(config.auth_secret().unwrap(), "dev-secret");
        assert_eq!(config.bootstrap_admin_code().unwrap(), "AM-TEST-ADMIN");
        assert_eq!(config.credential_proxy_url(), Some("http://127.0.0.1:1080"));
    }

    #[test]
    fn missing_proxy_credentials_are_invalid_for_guest_config() {
        let config = parse_config(
            r#"{
              "schema_version": 1
            }"#,
        );

        assert!(config.validate_for_guest_config().is_err());
    }

    #[test]
    fn rejects_legacy_subscription_auth_keys() {
        let error = serde_json::from_str::<MomConfig>(
            r#"{
              "schema_version": 1,
              "credentials": {
                "mode": "vm-auth-json",
                "codex_auth_path": "/tmp/codex-auth.json"
              }
            }"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }
}
