// Application configuration for krust server.
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct KrustConfig {
    /// Host address to bind to. Default: "127.0.0.1" (localhost only).
    /// Set to "0.0.0.0" to allow LAN access.
    #[serde(default = "default_host")]
    pub(crate) host: String,

    /// Port to listen on. Default: 3000.
    #[serde(default = "default_port")]
    pub(crate) port: u16,

    /// Allowed WebSocket origins. Requests with an Origin header not in this
    /// list will be rejected. Defaults include localhost and Grit UI.
    #[serde(default = "default_allowed_origins")]
    pub(crate) allowed_origins: Vec<String>,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    3000
}

fn default_allowed_origins() -> Vec<String> {
    vec![
        "http://localhost:3000".to_string(),
        "http://127.0.0.1:3000".to_string(),
        "http://localhost:5000".to_string(),
    ]
}

impl Default for KrustConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            allowed_origins: default_allowed_origins(),
        }
    }
}

impl KrustConfig {
    /// Load configuration from the standard config file location, with
    /// environment variable overrides.
    pub(crate) fn load() -> Self {
        let mut config = Self::from_file().unwrap_or_default();

        // Environment variables override config file
        if let Ok(host) = env::var("HOST") {
            config.host = host;
        }
        if let Ok(port) = env::var("PORT") {
            if let Ok(p) = port.parse::<u16>() {
                config.port = p;
            }
        }

        config
    }

    /// Get the config file path.
    fn config_path() -> Option<PathBuf> {
        // $XDG_CONFIG_HOME/bitshift/krust/config.json
        if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
            let mut path = PathBuf::from(xdg);
            path.push("bitshift/krust/config.json");
            return Some(path);
        }
        // $HOME/.config/bitshift/krust/config.json
        if let Ok(home) = env::var("HOME") {
            let mut path = PathBuf::from(home);
            path.push(".config/bitshift/krust/config.json");
            return Some(path);
        }
        None
    }

    /// Load configuration from file, if it exists.
    fn from_file() -> Option<Self> {
        let path = Self::config_path()?;
        if !path.exists() {
            return None;
        }
        let content = fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Return the socket address string for binding.
    pub(crate) fn bind_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Check if an origin is allowed.
    pub(crate) fn is_origin_allowed(&self, origin: &str) -> bool {
        self.allowed_origins.iter().any(|o| o == origin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_correct() {
        let config = KrustConfig::default();
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 3000);
        assert_eq!(
            config.allowed_origins,
            vec![
                "http://localhost:3000",
                "http://127.0.0.1:3000",
                "http://localhost:5000"
            ]
        );
    }

    #[test]
    fn config_allows_explicit_host_and_port() {
        let config = KrustConfig {
            host: "0.0.0.0".to_string(),
            port: 8080,
            allowed_origins: vec!["https://example.com".to_string()],
        };
        assert_eq!(config.bind_addr(), "0.0.0.0:8080");
        assert!(config.is_origin_allowed("https://example.com"));
        assert!(!config.is_origin_allowed("http://evil.com"));
    }
}