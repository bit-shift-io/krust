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

    /// Allowed WebSocket origins, used only while `host` is loopback. With a
    /// non-loopback bind every origin is accepted — see `is_origin_allowed`.
    /// Defaults include localhost and Grit UI.
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
    /// Load configuration from the standard config file location.
    ///
    /// The file is the only source of `host` and `port`. It used to be
    /// overridden by `HOST`/`PORT`, but those are ordinary shell variables
    /// (`HOST` is set by default in zsh), so Grit's settings page would rewrite
    /// the file and krust would silently ignore it.
    pub(crate) fn load() -> Self {
        Self::from_file().unwrap_or_default()
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

    /// True when the configured host is not loopback, i.e. LAN access is on.
    pub(crate) fn is_lan(&self) -> bool {
        !matches!(self.host.as_str(), "127.0.0.1" | "localhost" | "::1")
    }

    /// Check if an origin is allowed.
    ///
    /// Once the daemon is bound non-loopback it is deliberately reachable from
    /// the network, where the client's origin is simply another machine and a
    /// same-origin comparison has nothing left to check — so every origin is
    /// accepted. Loopback keeps the strict allow-list.
    pub(crate) fn is_origin_allowed(&self, origin: &str) -> bool {
        if self.is_lan() {
            return true;
        }
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
    fn environment_does_not_override_the_config_file() {
        // `HOST` is set by default in zsh, so an environment override would make
        // Grit's settings page rewrite the file and have krust ignore it.
        std::env::set_var("HOST", "10.99.99.99");
        std::env::set_var("PORT", "59999");
        let config = KrustConfig::load();
        assert_ne!(config.host, "10.99.99.99", "HOST must not override the file");
        assert_ne!(config.port, 59999, "PORT must not override the file");
        std::env::remove_var("HOST");
        std::env::remove_var("PORT");
    }

    #[test]
    fn config_allows_explicit_host_and_port() {
        let config = KrustConfig {
            host: "127.0.0.1".to_string(),
            port: 8080,
            allowed_origins: vec!["https://example.com".to_string()],
        };
        assert_eq!(config.bind_addr(), "127.0.0.1:8080");
        assert!(config.is_origin_allowed("https://example.com"));
        assert!(!config.is_origin_allowed("http://evil.com"));
    }

    #[test]
    fn non_loopback_host_allows_any_origin() {
        // LAN access is opt-in, and once krust answers on the network a
        // same-origin check has nothing left to compare against.
        let config = KrustConfig {
            host: "0.0.0.0".to_string(),
            ..KrustConfig::default()
        };
        assert!(config.is_origin_allowed("http://192.168.1.9:3000"));
        assert!(config.is_origin_allowed("http://evil.example"));
    }

    #[test]
    fn loopback_host_keeps_the_origin_allow_list() {
        let config = KrustConfig::default();
        assert!(!config.is_origin_allowed("http://192.168.1.9:3000"));
        assert!(config.is_origin_allowed("http://localhost:3000"));
    }
}