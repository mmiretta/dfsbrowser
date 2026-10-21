//! Runtime configuration, read once from environment variables.

use std::env;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Settings {
    /// Address the HTTP server binds to.
    pub bind: String,
    /// Optional HTTP Basic credentials protecting every route.
    pub basic_auth: Option<(String, String)>,
    /// S3 endpoints users may connect to. Empty means "any endpoint".
    pub allowed_endpoints: Vec<String>,
    /// Values pre-filled in the login form.
    pub default_endpoint: String,
    pub default_bucket: String,
    pub default_region: String,
    /// Disable uploads.
    pub read_only: bool,
    /// Lifetime of a browser session.
    pub session_ttl: Duration,
    /// Largest file accepted for upload or assembled in memory for download.
    pub max_file_bytes: u64,
}

impl Settings {
    pub fn from_env() -> anyhow::Result<Self> {
        let user = var("DFS_BROWSER_AUTH_USER");
        let pass = var("DFS_BROWSER_AUTH_PASSWORD");
        let basic_auth = match (user, pass) {
            (Some(u), Some(p)) => Some((u, p)),
            (None, None) => None,
            _ => anyhow::bail!(
                "set both DFS_BROWSER_AUTH_USER and DFS_BROWSER_AUTH_PASSWORD, or neither"
            ),
        };

        let allowed_endpoints = var("DFS_BROWSER_ALLOWED_ENDPOINTS")
            .map(|v| {
                v.split(',')
                    .map(normalize_endpoint)
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            bind: var("DFS_BROWSER_BIND").unwrap_or_else(|| "127.0.0.1:8888".into()),
            basic_auth,
            allowed_endpoints,
            default_endpoint: var("DFS_BROWSER_DEFAULT_ENDPOINT").unwrap_or_default(),
            default_bucket: var("DFS_BROWSER_DEFAULT_BUCKET").unwrap_or_else(|| "dfs".into()),
            default_region: var("DFS_BROWSER_DEFAULT_REGION").unwrap_or_else(|| "us-east-1".into()),
            read_only: var("DFS_BROWSER_READ_ONLY").is_some_and(|v| is_truthy(&v)),
            session_ttl: Duration::from_secs(parse_or("DFS_BROWSER_SESSION_TTL_SECS", 12 * 3600)?),
            max_file_bytes: parse_or("DFS_BROWSER_MAX_FILE_MB", 512)? * 1024 * 1024,
        })
    }

    /// True if `endpoint` may be contacted from this server.
    pub fn endpoint_allowed(&self, endpoint: &str) -> bool {
        self.allowed_endpoints.is_empty()
            || self
                .allowed_endpoints
                .contains(&normalize_endpoint(endpoint))
    }
}

/// Lower-case the endpoint and drop a trailing slash so that
/// `HTTP://minio:9000/` and `http://minio:9000` compare equal.
pub fn normalize_endpoint(s: &str) -> String {
    s.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn parse_or(name: &str, default: u64) -> anyhow::Result<u64> {
    match var(name) {
        Some(v) => v
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("{name} must be a positive integer, got {v:?}")),
        None => Ok(default),
    }
}

fn is_truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_normalization() {
        assert_eq!(
            normalize_endpoint(" HTTP://Minio:9000/ "),
            "http://minio:9000"
        );
    }

    #[test]
    fn allowlist() {
        let mut s = Settings {
            bind: String::new(),
            basic_auth: None,
            allowed_endpoints: vec![],
            default_endpoint: String::new(),
            default_bucket: String::new(),
            default_region: String::new(),
            read_only: false,
            session_ttl: Duration::from_secs(1),
            max_file_bytes: 1,
        };
        assert!(s.endpoint_allowed("http://anything:9000"));
        s.allowed_endpoints = vec!["http://minio:9000".into()];
        assert!(s.endpoint_allowed("http://MINIO:9000/"));
        assert!(!s.endpoint_allowed("http://169.254.169.254"));
    }
}
