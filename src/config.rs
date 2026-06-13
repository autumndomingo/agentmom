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
    pub(crate) runtime: RuntimeConfig,
    #[serde(default)]
    pub(crate) credentials: CredentialConfig,
    #[serde(default)]
    pub(crate) guest: GuestConfig,
    #[serde(default)]
    pub(crate) auth: AuthConfig,
    #[serde(default)]
    pub(crate) features: FeatureConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeConfig {
    #[serde(default)]
    pub(crate) snapshot_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CredentialConfig {
    #[serde(default = "default_credential_mode")]
    pub(crate) mode: String,
    #[serde(default)]
    pub(crate) codex_auth_path: PathBuf,
    #[serde(default = "default_opencode_auth_path")]
    pub(crate) opencode_auth_path: PathBuf,
    #[serde(default)]
    pub(crate) proxy_url: Option<String>,
    #[serde(default)]
    pub(crate) proxy_ca_path: Option<PathBuf>,
}

impl Default for CredentialConfig {
    fn default() -> Self {
        Self {
            mode: default_credential_mode(),
            codex_auth_path: PathBuf::new(),
            opencode_auth_path: default_opencode_auth_path(),
            proxy_url: None,
            proxy_ca_path: None,
        }
    }
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
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FeatureConfig {
    #[serde(default)]
    pub(crate) opencode: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialMode {
    VmAuthJson,
    OpenRouterProxy,
}

impl CredentialMode {
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        match raw {
            "vm-auth-json" | "file" => Ok(Self::VmAuthJson),
            "openrouter-proxy" | "proxy" => Ok(Self::OpenRouterProxy),
            _ => {
                bail!(
                    "credentials.mode must be one of: vm-auth-json, openrouter-proxy; got {raw:?}"
                )
            }
        }
    }

    pub(crate) fn uses_guest_auth_files(self) -> bool {
        matches!(self, Self::VmAuthJson)
    }

    pub(crate) fn uses_proxy(self) -> bool {
        matches!(self, Self::OpenRouterProxy)
    }
}

impl MomConfig {
    pub(crate) fn snapshot_name(&self) -> Result<&str> {
        self.runtime
            .snapshot_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("runtime.snapshot_name is required"))
    }

    pub(crate) fn credential_mode(&self) -> Result<CredentialMode> {
        CredentialMode::parse(&self.credentials.mode)
    }

    pub(crate) fn hermes_profile(&self) -> &str {
        &self.guest.hermes_profile
    }

    pub(crate) fn model(&self) -> &str {
        &self.guest.model
    }

    pub(crate) fn credential_proxy_url(&self) -> Option<&str> {
        self.credentials.proxy_url.as_deref()
    }

    pub(crate) fn validate_for_guest_config(&self) -> Result<CredentialMode> {
        let credential_mode = self.credential_mode()?;
        match credential_mode {
            CredentialMode::VmAuthJson => {
                if self.credentials.codex_auth_path.as_os_str().is_empty() {
                    bail!("credentials.mode vm-auth-json requires credentials.codex_auth_path");
                }
            }
            CredentialMode::OpenRouterProxy => {
                let proxy_url = self.credential_proxy_url().unwrap_or("").trim();
                if proxy_url.is_empty() {
                    bail!("credentials.mode openrouter-proxy requires credentials.proxy_url");
                }
                if self.credentials.proxy_ca_path.is_none() {
                    bail!("credentials.mode openrouter-proxy requires credentials.proxy_ca_path");
                }
            }
        }
        Ok(credential_mode)
    }

    pub(crate) fn validate_referenced_files(&self) -> Result<()> {
        match self.validate_for_guest_config()? {
            CredentialMode::VmAuthJson => {
                resolve_required_file(
                    &self.credentials.codex_auth_path,
                    "credentials.codex_auth_path",
                )?;
                resolve_required_file(
                    &self.credentials.opencode_auth_path,
                    "credentials.opencode_auth_path",
                )?;
            }
            CredentialMode::OpenRouterProxy => {
                let ca_path = self
                    .credentials
                    .proxy_ca_path
                    .as_ref()
                    .expect("validate_for_guest_config requires proxy_ca_path");
                resolve_required_file(ca_path, "credentials.proxy_ca_path")?;
            }
        }
        Ok(())
    }

    pub(crate) fn validate_for_node(&self) -> Result<()> {
        self.snapshot_name()?;
        self.validate_referenced_files()?;
        Ok(())
    }

    pub(crate) fn validate_for_api(&self) -> Result<()> {
        self.auth_secret()?;
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

    pub(crate) fn redacted_json(&self) -> serde_json::Value {
        json!({
            "schema_version": self.schema_version,
            "runtime": {
                "snapshot_name": self.runtime.snapshot_name,
            },
            "credentials": {
                "mode": self.credentials.mode,
                "codex_auth_path": redact_path(&self.credentials.codex_auth_path),
                "opencode_auth_path": redact_path(&self.credentials.opencode_auth_path),
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
            },
            "features": {
                "opencode": self.features.opencode,
            }
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ConfigFile {
    Structured(MomConfig),
    Legacy(LegacyMomConfig),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyMomConfig {
    #[serde(default)]
    codex_auth_path: PathBuf,
    #[serde(default = "default_opencode_auth_path")]
    opencode_auth_path: PathBuf,
    #[serde(default = "default_hermes_profile")]
    hermes_profile: String,
    #[serde(default = "default_model")]
    hermes_model: String,
    #[serde(default)]
    snapshot_name: Option<String>,
    #[serde(default = "default_legacy_credential_mode")]
    credential_mode: String,
    #[serde(default)]
    credential_proxy_url: Option<String>,
    #[serde(default)]
    credential_proxy_ca_path: Option<PathBuf>,
}

impl From<LegacyMomConfig> for MomConfig {
    fn from(value: LegacyMomConfig) -> Self {
        Self {
            schema_version: 1,
            runtime: RuntimeConfig {
                snapshot_name: value.snapshot_name,
            },
            credentials: CredentialConfig {
                mode: value.credential_mode,
                codex_auth_path: value.codex_auth_path,
                opencode_auth_path: value.opencode_auth_path,
                proxy_url: value.credential_proxy_url,
                proxy_ca_path: value.credential_proxy_ca_path,
            },
            guest: GuestConfig {
                hermes_profile: value.hermes_profile,
                model: value.hermes_model,
            },
            auth: AuthConfig::default(),
            features: FeatureConfig::default(),
        }
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
    let config: ConfigFile = serde_json::from_str(&raw)
        .with_context(|| format!("parse Agent Mom config {}", path.display()))?;
    Ok(match config {
        ConfigFile::Structured(config) => config,
        ConfigFile::Legacy(config) => config.into(),
    })
}

pub(crate) fn config_path() -> Result<PathBuf> {
    match env::var_os("MOM_CONFIG") {
        Some(value) => Ok(PathBuf::from(value)),
        None => Ok(home_dir()?.join(".config").join("mom").join("config.json")),
    }
}

pub(crate) fn resolve_required_file(path: &PathBuf, key: &str) -> Result<PathBuf> {
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

pub(crate) fn expand_tilde(path: &PathBuf) -> Result<PathBuf> {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return Ok(home_dir()?.join(rest));
    }
    Ok(path.clone())
}

pub(crate) fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))
}

fn default_schema_version() -> u32 {
    1
}

fn default_credential_mode() -> String {
    "openrouter-proxy".to_string()
}

fn default_legacy_credential_mode() -> String {
    "vm-auth-json".to_string()
}

fn default_hermes_profile() -> String {
    "main".to_string()
}

fn default_model() -> String {
    "gpt-5.5".to_string()
}

fn default_opencode_auth_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".local")
        .join("share")
        .join("opencode")
        .join("auth.json")
}

fn redact_path(path: &Path) -> Option<String> {
    if path.as_os_str().is_empty() {
        None
    } else {
        Some(path.display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_config(raw: &str) -> MomConfig {
        match serde_json::from_str::<ConfigFile>(raw).unwrap() {
            ConfigFile::Structured(config) => config,
            ConfigFile::Legacy(config) => config.into(),
        }
    }

    #[test]
    fn parses_structured_config() {
        let config = parse_config(
            r#"{
              "schema_version": 1,
              "runtime": { "snapshot_name": "mom-base-abc123" },
              "credentials": {
                "mode": "openrouter-proxy",
                "proxy_url": "http://127.0.0.1:1080",
                "proxy_ca_path": "/tmp/ca.crt"
              },
              "guest": {
                "hermes_profile": "main",
                "model": "openai/gpt-5.5"
              },
              "auth": {
                "secret": "dev-secret"
              },
              "features": {
                "opencode": true
              }
            }"#,
        );

        assert_eq!(config.snapshot_name().unwrap(), "mom-base-abc123");
        assert_eq!(
            config.credential_mode().unwrap(),
            CredentialMode::OpenRouterProxy
        );
        assert_eq!(config.model(), "openai/gpt-5.5");
        assert_eq!(config.auth_secret().unwrap(), "dev-secret");
        assert!(config.features.opencode);
    }

    #[test]
    fn structured_config_defaults_to_openrouter_proxy() {
        let config = parse_config(
            r#"{
              "schema_version": 1,
              "runtime": { "snapshot_name": "mom-base-default" }
            }"#,
        );

        assert_eq!(
            config.credential_mode().unwrap(),
            CredentialMode::OpenRouterProxy
        );
    }

    #[test]
    fn legacy_flat_config_defaults_to_vm_auth_json() {
        let config = parse_config(
            r#"{
              "snapshot_name": "mom-base-legacy-default",
              "codex_auth_path": "/tmp/codex-auth.json"
            }"#,
        );

        assert_eq!(
            config.credential_mode().unwrap(),
            CredentialMode::VmAuthJson
        );
    }

    #[test]
    fn migrates_legacy_flat_config() {
        let config = parse_config(
            r#"{
              "snapshot_name": "mom-base-legacy",
              "credential_mode": "vm-auth-json",
              "codex_auth_path": "/tmp/codex-auth.json",
              "opencode_auth_path": "/tmp/opencode-auth.json",
              "hermes_profile": "main",
              "hermes_model": "gpt-5.5"
            }"#,
        );

        assert_eq!(config.snapshot_name().unwrap(), "mom-base-legacy");
        assert_eq!(
            config.credential_mode().unwrap(),
            CredentialMode::VmAuthJson
        );
        assert_eq!(
            config.credentials.codex_auth_path,
            PathBuf::from("/tmp/codex-auth.json")
        );
    }
}
