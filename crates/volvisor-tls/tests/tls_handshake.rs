//! Real TLS handshake tests against a loopback listener.
//!
//! These exercise the same rustls configuration objects the daemons load:
//! a server `ServerConfig` and a client `ClientConfig`. They are not a
//! multi-host packet capture; the netns evidence under
//! `docs/evidence/transport-tls/` is the transport-cleartext check.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use volvisor_tls::{ClientTls, ServerTls};

struct Pki {
    ca: String,
    server_cert: String,
    server_key: String,
    client_cert: String,
    client_key: String,
}

fn generated_pki() -> Pki {
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("ca params");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca_key = KeyPair::generate().expect("ca key");
    let ca_cert = ca_params.clone().self_signed(&ca_key).expect("ca cert");
    let issuer = Issuer::new(ca_params, ca_key);

    let server_key = KeyPair::generate().expect("server key");
    let mut server_params =
        CertificateParams::new(vec!["localhost".to_owned()]).expect("server params");
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_cert = server_params
        .signed_by(&server_key, &issuer)
        .expect("server cert");

    let client_key = KeyPair::generate().expect("client key");
    let mut client_params =
        CertificateParams::new(vec!["volvisor-client".to_owned()]).expect("client params");
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client_cert = client_params
        .signed_by(&client_key, &issuer)
        .expect("client cert");

    Pki {
        ca: ca_cert.pem(),
        server_cert: server_cert.pem(),
        server_key: server_key.serialize_pem(),
        client_cert: client_cert.pem(),
        client_key: client_key.serialize_pem(),
    }
}

fn write(path: &std::path::Path, contents: &str) {
    std::fs::write(path, contents).expect("write pem");
}

/// Alice talks to Bob over TLS. The server presents a certificate signed by
/// the test CA; the client trusts exactly that CA. The test then exchanges a
/// byte both ways, proving the handshake and record layer are live.
#[tokio::test]
async fn tls_handshake_with_a_private_ca() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pki = generated_pki();
    let ca_path = dir.path().join("ca.pem");
    let cert_path = dir.path().join("server.pem");
    let key_path = dir.path().join("server.key");
    write(&ca_path, &pki.ca);
    write(&cert_path, &pki.server_cert);
    write(&key_path, &pki.server_key);

    let server_config: Arc<ServerConfig> = ServerTls {
        cert_file: cert_path,
        key_file: key_path,
        client_ca_file: None,
    }
    .build()
    .expect("server tls config");
    let client_config = ClientTls {
        ca_file: Some(ca_path),
        cert_file: None,
        key_file: None,
    }
    .build()
    .expect("client tls config");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let acceptor = TlsAcceptor::from(server_config);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut stream = acceptor.accept(stream).await.expect("tls accept");
        let mut request = [0u8; 4];
        stream.read_exact(&mut request).await.expect("read request");
        assert_eq!(&request, b"ping");
        stream.write_all(b"pong").await.expect("write response");
        stream.flush().await.expect("flush");
    });

    let connector = TlsConnector::from(client_config);
    let stream = TcpStream::connect(addr).await.expect("connect");
    let mut stream = connector
        .connect(
            ServerName::try_from("localhost").expect("server name"),
            stream,
        )
        .await
        .expect("client handshake");
    stream.write_all(b"ping").await.expect("write");
    let mut response = [0u8; 4];
    stream
        .read_exact(&mut response)
        .await
        .expect("read response");
    assert_eq!(&response, b"pong");
    server.await.expect("server task");
}

/// mTLS is required on the server. A client with only the CA (no identity)
/// must fail the handshake; the same server accepts the client certificate.
#[tokio::test]
async fn mtls_rejects_a_client_without_a_certificate_and_accepts_one_with_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pki = generated_pki();
    let ca_path = dir.path().join("ca.pem");
    let server_cert_path = dir.path().join("server.pem");
    let server_key_path = dir.path().join("server.key");
    let client_cert_path = dir.path().join("client.pem");
    let client_key_path = dir.path().join("client.key");
    write(&ca_path, &pki.ca);
    write(&server_cert_path, &pki.server_cert);
    write(&server_key_path, &pki.server_key);
    write(&client_cert_path, &pki.client_cert);
    write(&client_key_path, &pki.client_key);

    let server_config = ServerTls {
        cert_file: server_cert_path,
        key_file: server_key_path,
        client_ca_file: Some(ca_path.clone()),
    }
    .build()
    .expect("server tls config");
    let no_client = ClientTls {
        ca_file: Some(ca_path.clone()),
        cert_file: None,
        key_file: None,
    }
    .build()
    .expect("client tls config");
    let with_client = ClientTls {
        ca_file: Some(ca_path),
        cert_file: Some(client_cert_path),
        key_file: Some(client_key_path),
    }
    .build()
    .expect("client tls config");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let acceptor = TlsAcceptor::from(server_config);
    let server = tokio::spawn(async move {
        // First connection: the server rejects it at the TLS layer.
        let (stream, _) = listener.accept().await.expect("accept first");
        assert!(
            acceptor.accept(stream).await.is_err(),
            "a client without a certificate must be rejected"
        );
        // Second connection: mTLS client is accepted.
        let (stream, _) = listener.accept().await.expect("accept second");
        let mut stream = acceptor.accept(stream).await.expect("mtls accept");
        stream.write_all(b"ok").await.expect("write");
    });

    let no_client_connector = TlsConnector::from(no_client);
    let stream = TcpStream::connect(addr).await.expect("connect");
    let rejected = no_client_connector
        .connect(
            ServerName::try_from("localhost").expect("server name"),
            stream,
        )
        .await;
    // Under TLS 1.3 the client can return before the server's client-cert
    // alert arrives, so a successful `connect` is not proof either way: the
    // connection must be unusable (a read fails/EOFes) and the server-side
    // `accept` above must have failed.
    if let Ok(mut stream) = rejected {
        let mut byte = [0u8; 1];
        let usable =
            tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut byte))
                .await
                .is_ok_and(|read| read.is_ok_and(|n| n > 0));
        assert!(!usable, "no client certificate must fail mTLS");
    }

    let with_client_connector = TlsConnector::from(with_client);
    let stream = TcpStream::connect(addr).await.expect("connect");
    let mut stream = with_client_connector
        .connect(
            ServerName::try_from("localhost").expect("server name"),
            stream,
        )
        .await
        .expect("client certificate must be accepted");
    let mut response = [0u8; 2];
    stream.read_exact(&mut response).await.expect("read");
    assert_eq!(&response, b"ok");
    server.await.expect("server task");
}

/// A certificate/key pair that does not match is refused at configuration
/// time, not at the first client handshake.
#[test]
fn mismatched_key_is_refused_when_building_the_server_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pki = generated_pki();
    let other = generated_pki();
    let cert_path = dir.path().join("server.pem");
    let key_path = dir.path().join("server.key");
    write(&cert_path, &pki.server_cert);
    write(&key_path, &other.server_key);
    let error = ServerTls {
        cert_file: cert_path,
        key_file: key_path,
        client_ca_file: None,
    }
    .build()
    .expect_err("mismatched key must be refused");
    assert!(
        error.to_string().contains("key") || error.to_string().contains("certificate"),
        "error explains the pair: {error}"
    );
}

/// A half-configured mTLS client (certificate without key) is refused.
#[test]
fn client_identity_requires_both_certificate_and_key() {
    let error = ClientTls {
        ca_file: None,
        cert_file: Some("/nonexistent/client.pem".into()),
        key_file: None,
    }
    .build()
    .expect_err("half identity must be refused");
    assert!(
        error.to_string().contains("together"),
        "error names the pairing rule: {error}"
    );
}

/// The verified client rejects a server whose certificate is not signed by
/// the configured CA — there is no accept-any mode.
#[tokio::test]
async fn client_rejects_an_untrusted_server_certificate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let trusted = generated_pki();
    let untrusted = generated_pki();
    let trusted_ca = dir.path().join("trusted-ca.pem");
    let server_cert = dir.path().join("server.pem");
    let server_key = dir.path().join("server.key");
    write(&trusted_ca, &trusted.ca);
    write(&server_cert, &untrusted.server_cert);
    write(&server_key, &untrusted.server_key);

    let server_config = ServerTls {
        cert_file: server_cert,
        key_file: server_key,
        client_ca_file: None,
    }
    .build()
    .expect("server config");
    let client_config = ClientTls {
        ca_file: Some(trusted_ca),
        cert_file: None,
        key_file: None,
    }
    .build()
    .expect("client config");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let acceptor = TlsAcceptor::from(server_config);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let _ = acceptor.accept(stream).await;
    });

    let connector = TlsConnector::from(client_config);
    let stream = TcpStream::connect(addr).await.expect("connect");
    let result = connector
        .connect(
            ServerName::try_from("localhost").expect("server name"),
            stream,
        )
        .await;
    assert!(
        result.is_err(),
        "untrusted server certificate must be rejected"
    );
    server.await.expect("server task");
}

/// The default client config is usable without files (webpki roots), while a
/// custom root set still builds.
#[test]
fn default_client_configuration_is_available() {
    let _ = ClientTls::webpki_default();
    let roots = RootCertStore::empty();
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let _ = config;
}
