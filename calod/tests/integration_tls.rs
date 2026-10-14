//! TLS endpoint integration tests: a runtime-generated CA + server certificate
//! (rcgen), the production `build_tls_acceptor`, and a rustls client speaking
//! RESP over TLS through the same `Connection::process` path as plaintext.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::Duration;

use calod::cld_srv::auth::AuthManager;
use calod::cld_srv::connection::{Connection, ConnectionConfig};
use calod::cld_srv::metrics::MetricsAggregator;
use calod::cld_srv::served_store::ServedCalodStore;
use calod::store::config::SecurityConfig;
use calod::tls::build_tls_acceptor;
use dashmap::DashMap;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Mutex};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use uuid::Uuid;

/// Generate a test CA and a `localhost` server certificate signed by it.
/// Returns (ca_pem, server_cert_pem, server_key_pem).
fn generate_ca_and_server_cert() -> (String, String, String) {
    let ca_key = rcgen::KeyPair::generate().expect("ca key");
    let mut ca_params = rcgen::CertificateParams::new(Vec::new()).expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "calod-test-ca");
    ca_params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
    ];
    let ca_cert = ca_params.self_signed(&ca_key).expect("ca cert");

    let server_key = rcgen::KeyPair::generate().expect("server key");
    let mut server_params =
        rcgen::CertificateParams::new(vec!["localhost".to_string()]).expect("server params");
    server_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "localhost");
    let server_cert = server_params
        .signed_by(&server_key, &ca_cert, &ca_key)
        .expect("server cert");

    (ca_cert.pem(), server_cert.pem(), server_key.serialize_pem())
}

/// Build the production TLS acceptor from PEM material via temp files (the
/// config surface only takes paths).
fn acceptor_from_pem(dir: &Path, cert_pem: &str, key_pem: &str) -> TlsAcceptor {
    let cert_path = dir.join("server.crt");
    let key_path = dir.join("server.key");
    std::fs::write(&cert_path, cert_pem).unwrap();
    std::fs::write(&key_path, key_pem).unwrap();

    let security = SecurityConfig {
        tls_enabled: true,
        tls_cert_path: Some(cert_path),
        tls_key_path: Some(key_path),
        ..SecurityConfig::default()
    };
    build_tls_acceptor(&security)
        .expect("valid cert/key must build an acceptor")
        .expect("tls_enabled yields an acceptor")
}

fn client_connector(ca_pem: &str) -> TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    let mut reader = std::io::BufReader::new(ca_pem.as_bytes());
    for cert in rustls_pemfile::certs(&mut reader) {
        roots
            .add(cert.expect("ca pem must parse"))
            .expect("add root");
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

/// Spawn a TLS-only test server: every accepted connection must complete a
/// TLS handshake before reaching `Connection::process` — mirroring main.rs.
async fn spawn_tls_server(acceptor: TlsAcceptor) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        let store = Arc::new(ServedCalodStore::new().await);
        let active = Arc::new(DashMap::with_hasher(ahash::RandomState::new()));

        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accept = listener.accept() => {
                    let Ok((stream, peer)) = accept else { continue; };
                    let acceptor = acceptor.clone();
                    let store = store.clone();
                    let active: Arc<DashMap<Uuid, Weak<Mutex<Connection>>, ahash::RandomState>> =
                        active.clone();

                    tokio::spawn(async move {
                        let Ok(tls_stream) = acceptor.accept(stream).await else {
                            return; // failed handshake: dropped, never plaintext
                        };
                        let metrics = Arc::new(MetricsAggregator::new());
                        let auth = Arc::new(AuthManager::new(false));
                        let conn = Connection::new(
                            store,
                            ConnectionConfig::default(),
                            metrics,
                            auth,
                        );
                        let conn_arc = Arc::new(Mutex::new(conn));
                        let id = Uuid::new_v4();
                        let connection = {
                            let mut guard = conn_arc.lock().await;
                            guard.set_client_id(id);
                            guard.set_peer_address(peer);
                            guard.clone()
                        };
                        active.insert(id, Arc::downgrade(&conn_arc));

                        let process_fut =
                            connection.process(tls_stream, active.clone(), conn_arc.clone(), None);
                        let _ = tokio::time::timeout(Duration::from_secs(5), process_fut).await;
                        active.remove(&id);
                    });
                }
            }
        }
    });

    (addr, shutdown_tx)
}

async fn send_command<S>(stream: &mut S, cmd: &str) -> String
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.write_all(cmd.as_bytes()).await.expect("write");
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
        .await
        .expect("read timeout")
        .expect("read");
    String::from_utf8_lossy(&buf[..n]).to_string()
}

fn test_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("calod-tls-{}-{}", name, Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn tls_ping_set_get_round_trip() {
    let dir = test_dir("round-trip");
    let (ca_pem, cert_pem, key_pem) = generate_ca_and_server_cert();
    let acceptor = acceptor_from_pem(&dir, &cert_pem, &key_pem);
    let (addr, shutdown) = spawn_tls_server(acceptor).await;

    let connector = client_connector(&ca_pem);
    let tcp = TcpStream::connect(addr).await.expect("tcp connect");
    let server_name = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
    let mut tls = connector
        .connect(server_name, tcp)
        .await
        .expect("tls handshake");

    let pong = send_command(&mut tls, "*1\r\n$4\r\nPING\r\n").await;
    assert_eq!(pong, "+PONG\r\n");

    let set = send_command(
        &mut tls,
        "*3\r\n$3\r\nSET\r\n$7\r\ntls-key\r\n$9\r\ntls-value\r\n",
    )
    .await;
    assert_eq!(set, "+OK\r\n");

    let get = send_command(&mut tls, "*2\r\n$3\r\nGET\r\n$7\r\ntls-key\r\n").await;
    assert_eq!(get, "$9\r\ntls-value\r\n");

    drop(tls);
    let _ = shutdown.send(());
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn plaintext_connection_is_rejected_on_tls_only_port() {
    let dir = test_dir("reject");
    let (_ca_pem, cert_pem, key_pem) = generate_ca_and_server_cert();
    let acceptor = acceptor_from_pem(&dir, &cert_pem, &key_pem);
    let (addr, shutdown) = spawn_tls_server(acceptor).await;

    // With the fallback off (the default), a plaintext client on the TLS port
    // gets its handshake rejected: the server replies with a TLS fatal alert
    // (record type 0x15) or drops the connection outright — never a RESP
    // reply.
    let mut tcp = TcpStream::connect(addr).await.expect("tcp connect");
    let _ = tcp.write_all(b"PING\r\n").await;

    let mut buf = vec![0u8; 64];
    let read = tokio::time::timeout(Duration::from_secs(2), tcp.read(&mut buf)).await;
    match read {
        Ok(Ok(0)) | Ok(Err(_)) => {} // connection dropped after failed handshake
        Ok(Ok(n)) => {
            assert_eq!(
                buf[0],
                0x15,
                "expected a TLS alert record, got {:?}",
                String::from_utf8_lossy(&buf[..n])
            );
            // After the fatal alert the server closes the connection.
            let closed = tokio::time::timeout(Duration::from_secs(2), tcp.read(&mut buf)).await;
            assert!(
                matches!(closed, Ok(Ok(0)) | Ok(Err(_))),
                "connection must close after the TLS alert"
            );
        }
        Err(_) => panic!("timed out waiting for the TLS-only port to reject a plaintext client"),
    }

    let _ = shutdown.send(());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn misconfigured_tls_fails_closed_instead_of_falling_back() {
    let dir = test_dir("fail-closed");

    // Unreadable certificate: startup error, never silent plaintext.
    let security = SecurityConfig {
        tls_enabled: true,
        tls_cert_path: Some(dir.join("missing.crt")),
        tls_key_path: Some(dir.join("missing.key")),
        ..SecurityConfig::default()
    };
    assert!(build_tls_acceptor(&security).is_err());

    // Garbage PEM: also a startup error.
    let cert_path = dir.join("bad.crt");
    let key_path = dir.join("bad.key");
    std::fs::write(&cert_path, "not a pem").unwrap();
    std::fs::write(&key_path, "not a pem").unwrap();
    let security = SecurityConfig {
        tls_enabled: true,
        tls_cert_path: Some(cert_path),
        tls_key_path: Some(key_path),
        ..SecurityConfig::default()
    };
    assert!(build_tls_acceptor(&security).is_err());

    std::fs::remove_dir_all(&dir).ok();
}
