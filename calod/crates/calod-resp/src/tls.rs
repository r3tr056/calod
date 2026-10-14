//! Server-side TLS for the RESP endpoint (rustls, pure Rust).
//!
//! Fail-closed by construction: when `security.tls_enabled` is set,
//! [`build_tls_acceptor`] returns an error for any misconfiguration (missing
//! or unreadable certificate/key, mismatched pair) — the server refuses to
//! start rather than silently fall back to plaintext.
//!
//! `security.tls_client_auth` / `security.tls_ca_path` are parsed from
//! configuration but currently inert: rustls client-certificate verification
//! is not wired up yet, and setting `tls_client_auth` logs a loud warning at
//! startup instead of changing behavior.

use std::io;
use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tracing::warn;

use calod_core::store::config::SecurityConfig;

/// Build a rustls acceptor from the security config, or `Ok(None)` when TLS
/// is disabled. Any misconfiguration while `tls_enabled` is set is an error —
/// never a silent plaintext fallback.
pub fn build_tls_acceptor(security: &SecurityConfig) -> io::Result<Option<TlsAcceptor>> {
    if !security.tls_enabled {
        return Ok(None);
    }

    if security.tls_client_auth {
        warn!(
            "security.tls_client_auth is set but client-certificate authentication \
             is not implemented yet; the setting is parsed but inert"
        );
    }

    let cert_path = security.tls_cert_path.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "security.tls_enabled is true but security.tls_cert_path is not set",
        )
    })?;
    let key_path = security.tls_key_path.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "security.tls_enabled is true but security.tls_key_path is not set",
        )
    })?;

    let certs = load_certs(cert_path)?;
    if certs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("no PEM certificates found in {}", cert_path.display()),
        ));
    }
    let key = load_private_key(key_path)?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid TLS certificate/key pair: {}", err),
            )
        })?;

    Ok(Some(TlsAcceptor::from(Arc::new(config))))
}

fn load_certs(path: &Path) -> io::Result<Vec<CertificateDer<'static>>> {
    let mut reader = io::BufReader::new(std::fs::File::open(path)?);
    rustls_pemfile::certs(&mut reader).collect()
}

fn load_private_key(path: &Path) -> io::Result<PrivateKeyDer<'static>> {
    let mut reader = io::BufReader::new(std::fs::File::open(path)?);
    rustls_pemfile::private_key(&mut reader)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("no PEM private key found in {}", path.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_disabled_yields_no_acceptor() {
        let security = SecurityConfig::default();
        assert!(build_tls_acceptor(&security).unwrap().is_none());
    }

    #[test]
    fn tls_enabled_without_cert_or_key_fails_closed() {
        let security = SecurityConfig {
            tls_enabled: true,
            ..SecurityConfig::default()
        };
        assert!(build_tls_acceptor(&security).is_err());

        let security = SecurityConfig {
            tls_enabled: true,
            tls_cert_path: Some(Path::new("/nonexistent/cert.pem").to_path_buf()),
            ..SecurityConfig::default()
        };
        assert!(build_tls_acceptor(&security).is_err());
    }
}
