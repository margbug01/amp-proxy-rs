use std::collections::HashSet;
use std::path::Path;

use serde::Deserialize;

use crate::error::{AppError, Result};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,
    pub port: u16,

    pub api_keys: Vec<String>,

    #[serde(default)]
    pub ampcode: AmpCode,

    #[serde(default)]
    pub debug: DebugConfig,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(default)]
pub struct AmpCode {
    pub upstream_url: String,
    pub upstream_api_key: String,

    pub model_mappings: Vec<ModelMapping>,
    pub force_model_mappings: bool,
    pub custom_providers: Vec<CustomProvider>,
    pub gemini_route_mode: String,
    pub restrict_management_to_localhost: bool,
}

// `restrict-management-to-localhost: true` is the Go-version default and
// must survive an entirely missing `ampcode:` block, so we hand-roll Default
// instead of deriving it.
impl Default for AmpCode {
    fn default() -> Self {
        Self {
            upstream_url: String::new(),
            upstream_api_key: String::new(),
            model_mappings: Vec::new(),
            force_model_mappings: false,
            custom_providers: Vec::new(),
            gemini_route_mode: String::new(),
            restrict_management_to_localhost: true,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub struct ModelMapping {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub regex: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub struct CustomProvider {
    pub name: String,
    pub url: String,
    pub api_key: String,
    pub models: Vec<String>,
    #[serde(default)]
    pub request_overrides: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub responses_translate: bool,
    #[serde(default)]
    pub messages_translate: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub struct DebugConfig {
    #[serde(default)]
    pub access_log_model_peek: bool,
    #[serde(default)]
    pub capture_path_substring: String,
    #[serde(default)]
    pub capture_dir: String,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let raw = std::fs::read_to_string(path.as_ref())?;
        let cfg: Config = serde_yaml::from_str(&raw)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.port == 0 {
            return Err(AppError::Config("port must be 1..=65535".into()));
        }
        if self.api_keys.is_empty() {
            return Err(AppError::Config(
                "api-keys must contain at least one key".into(),
            ));
        }
        for (i, k) in self.api_keys.iter().enumerate() {
            if k.trim().is_empty() {
                return Err(AppError::Config(format!("api-keys[{i}] must not be empty")));
            }
        }

        if !self.ampcode.upstream_url.trim().is_empty() {
            validate_absolute_url(&self.ampcode.upstream_url, "ampcode.upstream-url")?;
        }

        match self.ampcode.gemini_route_mode.trim() {
            "" | "ampcode" | "translate" => {}
            v => {
                return Err(AppError::Config(format!(
                    "ampcode.gemini-route-mode must be empty, ampcode, or translate; got {v:?}"
                )));
            }
        }

        let mut seen_provider_names: HashSet<String> = HashSet::new();
        for (i, p) in self.ampcode.custom_providers.iter().enumerate() {
            let prefix = format!("ampcode.custom-providers[{i}]");
            let name = p.name.trim().to_lowercase();
            if name.is_empty() {
                return Err(AppError::Config(format!("{prefix}.name must not be empty")));
            }
            if !seen_provider_names.insert(name.to_string()) {
                return Err(AppError::Config(format!(
                    "{prefix}.name duplicates an earlier custom provider name"
                )));
            }
            validate_absolute_url(&p.url, &format!("{prefix}.url"))?;
            if p.models.is_empty() {
                return Err(AppError::Config(format!(
                    "{prefix}.models must contain at least one model"
                )));
            }
            for (j, m) in p.models.iter().enumerate() {
                let trimmed = m.trim();
                if trimmed.is_empty() {
                    return Err(AppError::Config(format!(
                        "{prefix}.models[{j}] must not be empty"
                    )));
                }
            }
        }
        Ok(())
    }
}

fn validate_absolute_url(raw: &str, field: &str) -> Result<()> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let u = url::Url::parse(trimmed)
        .map_err(|e| AppError::Config(format!("{field} must be a valid URL: {e}")))?;
    if !u.has_host() {
        return Err(AppError::Config(format!("{field} must be an absolute URL")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let yaml = r#"
host: "127.0.0.1"
port: 8317
api-keys:
  - "abc"
ampcode:
  upstream-url: "https://ampcode.com"
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.port, 8317);
        assert_eq!(cfg.api_keys, vec!["abc".to_string()]);
    }

    #[test]
    fn rejects_empty_api_keys() {
        let yaml = "port: 8317\napi-keys: []\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_bad_gemini_route_mode() {
        let yaml = r#"
port: 8317
api-keys: ["x"]
ampcode:
  gemini-route-mode: "wat"
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn allows_duplicate_provider_models() {
        let yaml = r#"
port: 8317
api-keys: ["x"]
ampcode:
  custom-providers:
    - name: "a"
      url: "https://a.example.com"
      api-key: "k1"
      models: ["gpt-5"]
    - name: "b"
      url: "https://b.example.com"
      api-key: "k2"
      models: ["GPT-5"]
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_duplicate_provider_names() {
        let yaml = r#"
port: 8317
api-keys: ["x"]
ampcode:
  custom-providers:
    - name: "a"
      url: "https://a.example.com"
      api-key: "k1"
      models: ["gpt-5"]
    - name: "a"
      url: "https://b.example.com"
      api-key: "k2"
      models: ["gpt-5-mini"]
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn restrict_management_defaults_true() {
        let yaml = "port: 8317\napi-keys: [\"x\"]\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.ampcode.restrict_management_to_localhost);
    }
}
