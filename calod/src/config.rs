//! Startup configuration resolution — the single source of truth for how the
//! server builds its effective [`CalodConfig`].
//!
//! Precedence (highest wins):
//!
//! 1. CLI flags (`--address`, `--port`, `--password`, `--max-connections`,
//!    `--tcp-backlog`, `--tcp-keepalive`, `--reuse-address`, `--allow-data-loss`)
//! 2. Environment variables (prefix `CALOD_`, see below)
//! 3. TOML config file (`--config <path>`, default `./calod.toml` if present)
//! 4. Built-in defaults ([`CalodConfig::default`])
//!
//! # Environment variable scheme
//!
//! Variables are prefixed with `CALOD_`; nested sections are joined with a
//! double underscore `__`, matching the TOML structure. Booleans accept
//! `true/false`, `1/0`, `yes/no`, `on/off`. Supported variables:
//!
//! - `CALOD_LISTEN_ADDR` (e.g. `127.0.0.1:6379`), `CALOD_MAX_CONNECTIONS`,
//!   `CALOD_LOG_LEVEL`, `CALOD_METRICS_ENABLED`, `CALOD_SHARDS`,
//!   `CALOD_SHARD_CAPACITY`, `CALOD_MAX_MEMORY_MB`, `CALOD_EVICTION_POLICY`
//!   (`noeviction|lru-exact|lru-approximated|lfu|random|ttl`),
//!   `CALOD_ALLOW_DATA_LOSS`
//! - `CALOD_PERSISTENCE__ENABLE_SNAPSHOT`, `CALOD_PERSISTENCE__SNAPSHOT_DIR`,
//!   `CALOD_PERSISTENCE__SNAPSHOT_INTERVAL_SECS`,
//!   `CALOD_PERSISTENCE__SNAPSHOT_MIN_CHANGES`, `CALOD_PERSISTENCE__ENABLE_AOF`,
//!   `CALOD_PERSISTENCE__AOF_DIR`, `CALOD_PERSISTENCE__AOF_FSYNC_STRATEGY`
//!   (`always|everysec|no`), `CALOD_PERSISTENCE__AOF_REWRITE_ENABLED`,
//!   `CALOD_PERSISTENCE__AOF_REWRITE_PERCENTAGE`,
//!   `CALOD_PERSISTENCE__AOF_REWRITE_MIN_SIZE_MB`
//! - `CALOD_API__ENABLED`, `CALOD_API__LISTEN_ADDR`, `CALOD_API__DATA_DIR`,
//!   `CALOD_API__DURABILITY` (`always|everysec|no`; defaults to the AOF fsync
//!   strategy)
//! - `CALOD_SECURITY__PASSWORD`, `CALOD_SECURITY__TLS_ENABLED`,
//!   `CALOD_SECURITY__TLS_CERT_PATH`, `CALOD_SECURITY__TLS_KEY_PATH`,
//!   `CALOD_SECURITY__TLS_CA_PATH`, `CALOD_SECURITY__TLS_CLIENT_AUTH`,
//!   `CALOD_SECURITY__TLS_PLAINTEXT_FALLBACK`
//! - `CALOD_ADVANCED__TCP_BACKLOG`, `CALOD_ADVANCED__TCP_KEEPALIVE_SECS`,
//!   `CALOD_ADVANCED__REUSE_ADDRESS`
//!
//! # TOML file format
//!
//! The file mirrors [`CalodConfig`]; every field is optional (missing fields
//! fall back to the built-in defaults):
//!
//! ```toml
//! listen_addr = "127.0.0.1:6379"
//! max_connections = 10000
//! max_memory_mb = 1024
//! allow_data_loss = false
//!
//! [persistence]
//! enable_snapshot = true
//! snapshot_dir = "./data"
//! enable_aof = true
//! aof_dir = "./data"
//! aof_fsync_strategy = "everysec"
//!
//! [security]
//! password = "secret"
//!
//! [advanced]
//! tcp_backlog = 511
//! tcp_keepalive_secs = 300
//! reuse_address = false
//! ```

use std::fmt;
use std::path::PathBuf;

use calod_core::store::config::{CalodConfig, EvictionPolicy, FsyncStrategy};

/// Default config file path, used when `--config` is not given and the file exists.
pub const DEFAULT_CONFIG_FILE: &str = "./calod.toml";

/// Error produced while resolving the startup configuration.
#[derive(Debug)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ConfigError {}

/// CLI flag overrides. `None` means "flag not given" so lower-precedence
/// sources still apply. Boolean flags are only ever set to `true` (they cannot
/// undo a `true` from env/file).
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    /// `--config <path>`: explicit TOML config file path.
    pub config_path: Option<PathBuf>,
    pub address: Option<String>,
    pub port: Option<u16>,
    pub password: Option<String>,
    pub max_connections: Option<usize>,
    pub tcp_backlog: Option<u32>,
    pub tcp_keepalive: Option<u64>,
    pub reuse_address: bool,
    pub allow_data_loss: bool,
}

/// The effective configuration plus a summary of where its values came from.
#[derive(Debug)]
pub struct ResolvedConfig {
    pub config: CalodConfig,
    /// Config file actually loaded, if any.
    pub file: Option<PathBuf>,
    /// Number of values overridden by environment variables.
    pub env_overrides: usize,
    /// Number of values overridden by CLI flags.
    pub cli_overrides: usize,
}

/// Resolve the effective startup configuration following the precedence chain
/// documented in this module. `env` reads environment variables; production
/// passes `|name| std::env::var(name).ok()`, tests pass a map-backed closure.
pub fn resolve_config(
    cli: &CliOverrides,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<ResolvedConfig, ConfigError> {
    let path = cli.config_path.clone().or_else(|| {
        let default = PathBuf::from(DEFAULT_CONFIG_FILE);
        default.exists().then_some(default)
    });

    let content =
        match &path {
            Some(p) => Some(std::fs::read_to_string(p).map_err(|e| {
                ConfigError(format!("cannot read config file {}: {}", p.display(), e))
            })?),
            None => None,
        };

    let mut resolved = resolve_from_str(cli, content.as_deref(), env)?;
    resolved.file = path;
    Ok(resolved)
}

/// Core resolution: parse the optional TOML content, then apply env and CLI
/// overlays in precedence order. Split from file IO so tests stay hermetic.
fn resolve_from_str(
    cli: &CliOverrides,
    file_content: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<ResolvedConfig, ConfigError> {
    let mut config = match file_content {
        Some(text) => toml::from_str::<CalodConfig>(text)
            .map_err(|e| ConfigError(format!("invalid config file: {}", e)))?,
        None => CalodConfig::default(),
    };

    let env_overrides = apply_env_overrides(&mut config, env)?;
    let cli_overrides = apply_cli_overrides(&mut config, cli)?;

    Ok(ResolvedConfig {
        config,
        file: None,
        env_overrides,
        cli_overrides,
    })
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_eviction_policy(value: &str) -> Option<EvictionPolicy> {
    match value.to_ascii_lowercase().as_str() {
        "noeviction" => Some(EvictionPolicy::NoEviction),
        "lru-exact" => Some(EvictionPolicy::LruExact),
        "lru-approximated" => Some(EvictionPolicy::LruApproximated),
        "lfu" => Some(EvictionPolicy::Lfu),
        "random" => Some(EvictionPolicy::Random),
        "ttl" => Some(EvictionPolicy::Ttl),
        _ => None,
    }
}

fn parse_fsync_strategy(value: &str) -> Option<FsyncStrategy> {
    match value.to_ascii_lowercase().as_str() {
        "always" => Some(FsyncStrategy::Always),
        "everysec" => Some(FsyncStrategy::Everysec),
        "no" => Some(FsyncStrategy::No),
        _ => None,
    }
}

/// Apply a `FromStr`-parseable env var to `slot`. Returns 1 when the var was
/// set (and parsed), 0 when unset; a set-but-unparseable value is an error.
fn env_parse<T>(
    env: &dyn Fn(&str) -> Option<String>,
    name: &str,
    slot: &mut T,
) -> Result<usize, ConfigError>
where
    T: std::str::FromStr,
    T::Err: fmt::Display,
{
    match env(name) {
        Some(v) => {
            *slot = v
                .parse()
                .map_err(|e| ConfigError(format!("invalid value for {}='{}': {}", name, v, e)))?;
            Ok(1)
        }
        None => Ok(0),
    }
}

fn env_bool(
    env: &dyn Fn(&str) -> Option<String>,
    name: &str,
    slot: &mut bool,
) -> Result<usize, ConfigError> {
    match env(name) {
        Some(v) => {
            *slot = parse_bool(&v).ok_or_else(|| {
                ConfigError(format!(
                    "invalid boolean for {}='{}' (expected true/false, 1/0, yes/no, on/off)",
                    name, v
                ))
            })?;
            Ok(1)
        }
        None => Ok(0),
    }
}

fn env_string(
    env: &dyn Fn(&str) -> Option<String>,
    name: &str,
    slot: &mut String,
) -> Result<usize, ConfigError> {
    match env(name) {
        Some(v) => {
            *slot = v;
            Ok(1)
        }
        None => Ok(0),
    }
}

fn env_opt_string(
    env: &dyn Fn(&str) -> Option<String>,
    name: &str,
    slot: &mut Option<String>,
) -> Result<usize, ConfigError> {
    match env(name) {
        Some(v) => {
            *slot = Some(v);
            Ok(1)
        }
        None => Ok(0),
    }
}

fn env_path(
    env: &dyn Fn(&str) -> Option<String>,
    name: &str,
    slot: &mut PathBuf,
) -> Result<usize, ConfigError> {
    match env(name) {
        Some(v) => {
            *slot = PathBuf::from(v);
            Ok(1)
        }
        None => Ok(0),
    }
}

fn env_opt_path(
    env: &dyn Fn(&str) -> Option<String>,
    name: &str,
    slot: &mut Option<PathBuf>,
) -> Result<usize, ConfigError> {
    match env(name) {
        Some(v) => {
            *slot = Some(PathBuf::from(v));
            Ok(1)
        }
        None => Ok(0),
    }
}

fn apply_env_overrides(
    config: &mut CalodConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<usize, ConfigError> {
    let mut n = 0;

    n += env_parse(env, "CALOD_LISTEN_ADDR", &mut config.listen_addr)?;
    n += env_parse(env, "CALOD_MAX_CONNECTIONS", &mut config.max_connections)?;
    n += env_string(env, "CALOD_LOG_LEVEL", &mut config.log_level)?;
    n += env_bool(env, "CALOD_METRICS_ENABLED", &mut config.metrics_enabled)?;
    n += env_parse(env, "CALOD_SHARDS", &mut config.shards)?;
    n += env_parse(env, "CALOD_SHARD_CAPACITY", &mut config.shard_capacity)?;
    n += env_parse(env, "CALOD_MAX_MEMORY_MB", &mut config.max_memory_mb)?;
    n += env_bool(env, "CALOD_ALLOW_DATA_LOSS", &mut config.allow_data_loss)?;
    if let Some(v) = env("CALOD_EVICTION_POLICY") {
        config.eviction_policy = parse_eviction_policy(&v).ok_or_else(|| {
            ConfigError(format!("invalid value for CALOD_EVICTION_POLICY='{}'", v))
        })?;
        n += 1;
    }

    let p = &mut config.persistence;
    n += env_bool(
        env,
        "CALOD_PERSISTENCE__ENABLE_SNAPSHOT",
        &mut p.enable_snapshot,
    )?;
    n += env_path(env, "CALOD_PERSISTENCE__SNAPSHOT_DIR", &mut p.snapshot_dir)?;
    n += env_parse(
        env,
        "CALOD_PERSISTENCE__SNAPSHOT_INTERVAL_SECS",
        &mut p.snapshot_interval_secs,
    )?;
    n += env_parse(
        env,
        "CALOD_PERSISTENCE__SNAPSHOT_MIN_CHANGES",
        &mut p.snapshot_min_changes,
    )?;
    n += env_bool(env, "CALOD_PERSISTENCE__ENABLE_AOF", &mut p.enable_aof)?;
    n += env_path(env, "CALOD_PERSISTENCE__AOF_DIR", &mut p.aof_dir)?;
    if let Some(v) = env("CALOD_PERSISTENCE__AOF_FSYNC_STRATEGY") {
        p.aof_fsync_strategy = parse_fsync_strategy(&v).ok_or_else(|| {
            ConfigError(format!(
                "invalid value for CALOD_PERSISTENCE__AOF_FSYNC_STRATEGY='{}'",
                v
            ))
        })?;
        n += 1;
    }
    n += env_bool(
        env,
        "CALOD_PERSISTENCE__AOF_REWRITE_ENABLED",
        &mut p.aof_rewrite_enabled,
    )?;
    n += env_parse(
        env,
        "CALOD_PERSISTENCE__AOF_REWRITE_PERCENTAGE",
        &mut p.aof_rewrite_percentage,
    )?;
    n += env_parse(
        env,
        "CALOD_PERSISTENCE__AOF_REWRITE_MIN_SIZE_MB",
        &mut p.aof_rewrite_min_size_mb,
    )?;

    let api = &mut config.api;
    n += env_bool(env, "CALOD_API__ENABLED", &mut api.enabled)?;
    n += env_parse(env, "CALOD_API__LISTEN_ADDR", &mut api.listen_addr)?;
    n += env_opt_path(env, "CALOD_API__DATA_DIR", &mut api.data_dir)?;
    if let Some(v) = env("CALOD_API__DURABILITY") {
        api.durability = Some(parse_fsync_strategy(&v).ok_or_else(|| {
            ConfigError(format!("invalid value for CALOD_API__DURABILITY='{}'", v))
        })?);
        n += 1;
    }

    let s = &mut config.security;
    n += env_opt_string(env, "CALOD_SECURITY__PASSWORD", &mut s.password)?;
    n += env_bool(env, "CALOD_SECURITY__TLS_ENABLED", &mut s.tls_enabled)?;
    n += env_opt_path(env, "CALOD_SECURITY__TLS_CERT_PATH", &mut s.tls_cert_path)?;
    n += env_opt_path(env, "CALOD_SECURITY__TLS_KEY_PATH", &mut s.tls_key_path)?;
    n += env_opt_path(env, "CALOD_SECURITY__TLS_CA_PATH", &mut s.tls_ca_path)?;
    n += env_bool(
        env,
        "CALOD_SECURITY__TLS_CLIENT_AUTH",
        &mut s.tls_client_auth,
    )?;
    n += env_bool(
        env,
        "CALOD_SECURITY__TLS_PLAINTEXT_FALLBACK",
        &mut s.tls_plaintext_fallback,
    )?;

    let a = &mut config.advanced;
    n += env_parse(env, "CALOD_ADVANCED__TCP_BACKLOG", &mut a.tcp_backlog)?;
    n += env_parse(
        env,
        "CALOD_ADVANCED__TCP_KEEPALIVE_SECS",
        &mut a.tcp_keepalive_secs,
    )?;
    n += env_bool(env, "CALOD_ADVANCED__REUSE_ADDRESS", &mut a.reuse_address)?;

    Ok(n)
}

fn apply_cli_overrides(config: &mut CalodConfig, cli: &CliOverrides) -> Result<usize, ConfigError> {
    let mut n = 0;

    if let Some(address) = &cli.address {
        let ip = address.parse().map_err(|_| {
            ConfigError(format!(
                "invalid --address '{}' (expected an IP address)",
                address
            ))
        })?;
        config.listen_addr.set_ip(ip);
        n += 1;
    }
    if let Some(port) = cli.port {
        config.listen_addr.set_port(port);
        n += 1;
    }
    if let Some(password) = &cli.password {
        config.security.password = Some(password.clone());
        n += 1;
    }
    if let Some(max_connections) = cli.max_connections {
        config.max_connections = max_connections;
        n += 1;
    }
    if let Some(tcp_backlog) = cli.tcp_backlog {
        config.advanced.tcp_backlog = tcp_backlog;
        n += 1;
    }
    if let Some(tcp_keepalive) = cli.tcp_keepalive {
        config.advanced.tcp_keepalive_secs = tcp_keepalive;
        n += 1;
    }
    if cli.reuse_address {
        config.advanced.reuse_address = true;
        n += 1;
    }
    if cli.allow_data_loss {
        config.allow_data_loss = true;
        n += 1;
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn defaults_are_safe_when_nothing_is_configured() {
        let resolved = resolve_from_str(&CliOverrides::default(), None, &env_from(&[]))
            .expect("resolve defaults");
        let cfg = &resolved.config;

        assert_eq!(
            cfg.listen_addr,
            "127.0.0.1:6379".parse().unwrap(),
            "default bind must be loopback"
        );
        assert!(!cfg.allow_data_loss);
        assert!(cfg.security.password.is_none());
        assert_eq!(resolved.env_overrides, 0);
        assert_eq!(resolved.cli_overrides, 0);
    }

    #[test]
    fn file_overrides_defaults_and_partial_files_keep_the_rest() {
        let file = r#"
            listen_addr = "0.0.0.0:7000"
            max_memory_mb = 256

            [persistence]
            enable_aof = true
            aof_fsync_strategy = "always"
        "#;

        let resolved = resolve_from_str(&CliOverrides::default(), Some(file), &env_from(&[]))
            .expect("resolve file");

        assert_eq!(resolved.config.listen_addr, "0.0.0.0:7000".parse().unwrap());
        assert_eq!(resolved.config.max_memory_mb, 256);
        assert!(resolved.config.persistence.enable_aof);
        assert!(matches!(
            resolved.config.persistence.aof_fsync_strategy,
            FsyncStrategy::Always
        ));
        // Untouched fields keep built-in defaults.
        assert_eq!(resolved.config.max_connections, 10000);
        assert!(resolved.config.persistence.enable_snapshot);
    }

    #[test]
    fn env_overrides_file_and_counts_applied_vars() {
        let file = r#"listen_addr = "10.0.0.1:7000""#;
        let env = env_from(&[
            ("CALOD_LISTEN_ADDR", "192.168.1.1:7100"),
            ("CALOD_MAX_MEMORY_MB", "512"),
            ("CALOD_SECURITY__PASSWORD", "s3cret"),
        ]);

        let resolved =
            resolve_from_str(&CliOverrides::default(), Some(file), &env).expect("resolve env");

        assert_eq!(
            resolved.config.listen_addr,
            "192.168.1.1:7100".parse().unwrap()
        );
        assert_eq!(resolved.config.max_memory_mb, 512);
        assert_eq!(resolved.config.security.password.as_deref(), Some("s3cret"));
        assert_eq!(resolved.env_overrides, 3);
    }

    #[test]
    fn cli_overrides_env_and_file() {
        let file = r#"listen_addr = "10.0.0.1:7000""#;
        let env = env_from(&[
            ("CALOD_LISTEN_ADDR", "192.168.1.1:7100"),
            ("CALOD_ADVANCED__TCP_BACKLOG", "2048"),
        ]);
        let cli = CliOverrides {
            port: Some(7200),
            tcp_backlog: Some(4096),
            allow_data_loss: true,
            ..CliOverrides::default()
        };

        let resolved = resolve_from_str(&cli, Some(file), &env).expect("resolve cli");

        // CLI --port overrides the port from env; env still owns the IP.
        assert_eq!(
            resolved.config.listen_addr,
            "192.168.1.1:7200".parse().unwrap()
        );
        assert_eq!(resolved.config.advanced.tcp_backlog, 4096);
        assert!(resolved.config.allow_data_loss);
        assert_eq!(resolved.cli_overrides, 3);
    }

    #[test]
    fn cli_address_and_port_compose_with_lower_precedence_sources() {
        let file = r#"listen_addr = "10.0.0.1:7000""#;
        let cli = CliOverrides {
            address: Some("127.0.0.1".to_string()),
            ..CliOverrides::default()
        };

        let resolved = resolve_from_str(&cli, Some(file), &env_from(&[])).expect("resolve");

        assert_eq!(
            resolved.config.listen_addr,
            "127.0.0.1:7000".parse().unwrap()
        );
    }

    #[test]
    fn invalid_file_env_or_cli_values_are_errors_not_fallbacks() {
        assert!(resolve_from_str(
            &CliOverrides::default(),
            Some("not = [toml"),
            &env_from(&[])
        )
        .is_err());

        let bad_env = env_from(&[("CALOD_MAX_CONNECTIONS", "lots")]);
        assert!(resolve_from_str(&CliOverrides::default(), None, &bad_env).is_err());

        let bad_bool = env_from(&[("CALOD_ALLOW_DATA_LOSS", "maybe")]);
        assert!(resolve_from_str(&CliOverrides::default(), None, &bad_bool).is_err());

        let cli = CliOverrides {
            address: Some("not-an-ip".to_string()),
            ..CliOverrides::default()
        };
        assert!(resolve_from_str(&cli, None, &env_from(&[])).is_err());
    }
}
