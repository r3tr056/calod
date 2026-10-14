//! Redis-style `CONFIG GET`/`CONFIG SET` parameter registry.
//!
//! Real clients (redis-py, ioredis, lettuce, redis-cli) probe a handful of
//! `CONFIG GET` parameters on connect and must receive a value rather than an
//! error. This module maps Redis parameter names to values derived from the live
//! [`CalodConfig`], layered with a runtime-mutable [`ConfigOverlay`] that backs
//! `CONFIG SET`. Parameters calod does not model still answer with a sensible
//! Redis-compatible default so handshakes never fail.

use dashmap::DashMap;

use crate::store::config::{CalodConfig, EvictionPolicy, FsyncStrategy};

/// Runtime-mutable overrides applied on top of the static [`CalodConfig`].
/// `CONFIG SET` writes here; `CONFIG GET` reads here first, then falls back to
/// the config-derived default.
#[derive(Default)]
pub struct ConfigOverlay {
    values: DashMap<String, String>,
}

impl ConfigOverlay {
    pub fn new() -> Self {
        Self {
            values: DashMap::new(),
        }
    }

    fn get(&self, name: &str) -> Option<String> {
        self.values.get(name).map(|v| v.clone())
    }

    fn set(&self, name: &str, value: &str) {
        self.values.insert(name.to_string(), value.to_string());
    }
}

/// Every parameter name calod recognizes for `CONFIG GET`/`SET`. Lower-case,
/// Redis spelling (kebab-case).
const KNOWN_PARAMS: &[&str] = &[
    "maxmemory",
    "maxmemory-policy",
    "maxmemory-samples",
    "maxclients",
    "appendonly",
    "appendfsync",
    "save",
    "timeout",
    "databases",
    "tcp-keepalive",
    "tcp-backlog",
    "proto-max-bulk-len",
    "hash-max-listpack-entries",
    "hash-max-listpack-value",
    "list-max-listpack-size",
    "set-max-intset-entries",
    "set-max-listpack-entries",
    "zset-max-listpack-entries",
    "zset-max-listpack-value",
    "bind",
    "port",
    "requirepass",
    "appenddirname",
    "dir",
    "notify-keyspace-events",
];

fn eviction_to_redis(p: &EvictionPolicy) -> &'static str {
    match p {
        EvictionPolicy::NoEviction => "noeviction",
        EvictionPolicy::LruExact | EvictionPolicy::LruApproximated => "allkeys-lru",
        EvictionPolicy::Lfu => "allkeys-lfu",
        EvictionPolicy::Random => "allkeys-random",
        EvictionPolicy::Ttl => "volatile-ttl",
    }
}

fn fsync_to_redis(s: &FsyncStrategy) -> &'static str {
    match s {
        FsyncStrategy::Always => "always",
        FsyncStrategy::Everysec => "everysec",
        FsyncStrategy::No => "no",
    }
}

/// The default value for a known parameter, derived from the static config.
fn default_for(name: &str, cfg: &CalodConfig) -> Option<String> {
    let v = match name {
        "maxmemory" => (cfg.max_memory_mb.saturating_mul(1024 * 1024)).to_string(),
        "maxmemory-policy" => eviction_to_redis(&cfg.eviction_policy).to_string(),
        "maxmemory-samples" => "5".to_string(),
        "maxclients" => cfg.max_connections.to_string(),
        "appendonly" => if cfg.persistence.enable_aof {
            "yes"
        } else {
            "no"
        }
        .to_string(),
        "appendfsync" => fsync_to_redis(&cfg.persistence.aof_fsync_strategy).to_string(),
        "save" => "3600 1 300 100 60 10000".to_string(),
        "timeout" => "0".to_string(),
        "databases" => "16".to_string(),
        "tcp-keepalive" => cfg.advanced.tcp_keepalive_secs.to_string(),
        "tcp-backlog" => cfg.advanced.tcp_backlog.to_string(),
        "proto-max-bulk-len" => (512usize * 1024 * 1024).to_string(),
        "hash-max-listpack-entries" => "128".to_string(),
        "hash-max-listpack-value" => "64".to_string(),
        "list-max-listpack-size" => "128".to_string(),
        "set-max-intset-entries" => "512".to_string(),
        "set-max-listpack-entries" => "128".to_string(),
        "zset-max-listpack-entries" => "128".to_string(),
        "zset-max-listpack-value" => "64".to_string(),
        "bind" => cfg.listen_addr.ip().to_string(),
        "port" => cfg.listen_addr.port().to_string(),
        "requirepass" => cfg.security.password.clone().unwrap_or_default(),
        "appenddirname" => "appendonlydir".to_string(),
        "dir" => cfg.persistence.snapshot_dir.to_string_lossy().into_owned(),
        "notify-keyspace-events" => String::new(),
        _ => return None,
    };
    Some(v)
}

/// Case-insensitive Redis glob match supporting `*` and `?`. Used to match
/// `CONFIG GET` patterns against parameter names.
fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let s: Vec<char> = name.to_lowercase().chars().collect();
    // Iterative wildcard matcher with backtracking on `*`.
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Resolve all `(name, value)` pairs whose name matches `pattern`, overlay first.
pub fn config_get(
    cfg: &CalodConfig,
    overlay: &ConfigOverlay,
    pattern: &str,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for &name in KNOWN_PARAMS {
        if glob_match(pattern, name) {
            let value = overlay
                .get(name)
                .or_else(|| default_for(name, cfg))
                .unwrap_or_default();
            out.push((name.to_string(), value));
        }
    }
    out
}

/// Apply a single `CONFIG SET name value`. Unknown parameters are rejected with
/// a Redis-compatible error string (the wire prefix is added by the caller).
pub fn config_set(overlay: &ConfigOverlay, name: &str, value: &str) -> Result<(), String> {
    let lname = name.to_lowercase();
    if KNOWN_PARAMS.contains(&lname.as_str()) {
        overlay.set(&lname, value);
        Ok(())
    } else {
        Err(format!(
            "Unknown option or number of arguments for CONFIG SET - '{}'",
            name
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_exact_prefix_and_star() {
        assert!(glob_match("maxmemory", "maxmemory"));
        assert!(glob_match("MAXMEMORY", "maxmemory"));
        assert!(glob_match("max*", "maxmemory-policy"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("maxmemory-?olicy", "maxmemory-policy"));
        assert!(!glob_match("maxmemory", "maxclients"));
        assert!(!glob_match("min*", "maxmemory"));
    }

    #[test]
    fn get_returns_default_then_overlay() {
        let cfg = CalodConfig::default();
        let overlay = ConfigOverlay::new();
        let got = config_get(&cfg, &overlay, "maxmemory-policy");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "maxmemory-policy");
        assert_eq!(got[0].1, "allkeys-lru");

        config_set(&overlay, "maxmemory-policy", "noeviction").unwrap();
        let got2 = config_get(&cfg, &overlay, "maxmemory-policy");
        assert_eq!(got2[0].1, "noeviction");
    }

    #[test]
    fn set_unknown_param_errors() {
        let overlay = ConfigOverlay::new();
        assert!(config_set(&overlay, "totally-not-a-param", "x").is_err());
    }

    #[test]
    fn port_reflects_config() {
        let cfg = CalodConfig::default();
        let overlay = ConfigOverlay::new();
        let got = config_get(&cfg, &overlay, "port");
        assert_eq!(got[0].1, "6379");
    }
}
