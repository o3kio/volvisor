//! # volvisor-tls
//!
//! Native TLS (rustls) configuration shared by the Volvisor daemons.
//!
//! The crate deliberately owns only the rustls configuration loading: the
//! Volume API and witness daemons serve through `axum-server`'s rustls
//! acceptor, and the outbound witness/peer clients build a
//! `hyper-rustls` connector from the same [`ClientTls`] settings. Keeping
//! PEM parsing and verifier construction here means the two daemons cannot
//! drift into different trust rules.
//!
//! ## Trust model
//!
//! - **Server identity** is an explicitly configured certificate/key pair.
//!   There is no generated ephemeral identity: an operator who has not
//!   provisioned a certificate cannot accidentally serve TLS with a
//!   throwaway key.
//! - **Outbound verification** uses the configured private CA bundle when
//!   one is set, otherwise the Mozilla root program (`webpki-roots`).
//!   Certificate hostname/IP verification is always performed; there is no
//!   "accept any certificate" switch.
//! - **mTLS** is opt-in per server (a client CA bundle) and per client (a
//!   certificate/key pair). When a server has a client CA, rustls requires
//!   and verifies a client certificate; a client without one fails the
//!   handshake.
//!
//! Private keys are loaded into rustls and never formatted or logged by
//! this crate.

#![deny(unsafe_code)]
// Tests may use expect/unwrap for invariant assertions; production code may not.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub use rustls;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};

/// Errors loading or building a rustls configuration.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// A configured file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The file that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// A certificate bundle contained no certificates.
    #[error("no certificates found in {path}")]
    NoCertificates {
        /// The empty certificate file.
        path: PathBuf,
    },
    /// A key file contained no private key.
    #[error("no private key found in {path}")]
    NoPrivateKey {
        /// The keyless file.
        path: PathBuf,
    },
    /// A PEM item was malformed.
    #[error("invalid PEM material in {path}: {detail}")]
    InvalidPem {
        /// The offending file.
        path: PathBuf,
        /// The parser's detail.
        detail: String,
    },
    /// The certificate/key pair is inconsistent or unsupported.
    #[error("invalid certificate/key configuration: {0}")]
    InvalidConfiguration(String),
    /// rustls refused the configuration.
    #[error("rustls configuration failed: {0}")]
    Rustls(#[from] rustls::Error),
}

/// Server-side TLS settings: this daemon's certificate/key and the optional
/// client CA that turns the listener into an mTLS endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerTls {
    /// PEM certificate chain served to clients.
    pub cert_file: PathBuf,
    /// PEM private key matching the first certificate.
    pub key_file: PathBuf,
    /// Optional PEM CA bundle. When set, every client must present a
    /// certificate issued by one of these roots (mTLS).
    pub client_ca_file: Option<PathBuf>,
}

impl ServerTls {
    /// Load the certificate, key and optional client CA and build the
    /// rustls server configuration.
    ///
    /// # Errors
    /// [`TlsError`] when a file is unreadable, contains no usable
    /// certificate/key, or rustls refuses the verifier or key pair.
    pub fn build(&self) -> Result<Arc<ServerConfig>, TlsError> {
        let certificates = load_certificates(&self.cert_file)?;
        let key = load_private_key(&self.key_file)?;
        let builder = ServerConfig::builder();
        let mut config = if let Some(ca_file) = &self.client_ca_file {
            let mut roots = RootCertStore::empty();
            for certificate in load_certificates(ca_file)? {
                roots.add(certificate)?;
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .map_err(|error| TlsError::InvalidConfiguration(error.to_string()))?;
            builder
                .with_client_cert_verifier(verifier)
                .with_single_cert(certificates, key)?
        } else {
            builder
                .with_no_client_auth()
                .with_single_cert(certificates, key)?
        };
        // The daemons serve HTTP/1.1 (axum's configured surface); advertising
        // only what is actually served keeps an h2-capable client from
        // negotiating a protocol the server will not speak.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }
}

/// Outbound TLS settings: trust roots for peer certificates and an optional
/// client identity for mTLS.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientTls {
    /// Optional PEM CA bundle. Unset uses the Mozilla root program.
    pub ca_file: Option<PathBuf>,
    /// Optional PEM client certificate chain (mTLS).
    pub cert_file: Option<PathBuf>,
    /// Optional PEM client private key (mTLS).
    pub key_file: Option<PathBuf>,
}

impl ClientTls {
    /// Load the configured roots and optional client identity and build a
    /// rustls client configuration.
    ///
    /// # Errors
    /// [`TlsError`] when a file is unreadable/empty, when only one of
    /// `cert_file`/`key_file` is set, or when rustls refuses the pair.
    pub fn build(&self) -> Result<Arc<ClientConfig>, TlsError> {
        let mut roots = RootCertStore::empty();
        if let Some(ca_file) = &self.ca_file {
            for certificate in load_certificates(ca_file)? {
                roots.add(certificate)?;
            }
        } else {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        let builder = ClientConfig::builder().with_root_certificates(roots);
        let config = match (&self.cert_file, &self.key_file) {
            (Some(cert_file), Some(key_file)) => {
                let certificates = load_certificates(cert_file)?;
                let key = load_private_key(key_file)?;
                builder.with_client_auth_cert(certificates, key)?
            }
            (None, None) => builder.with_no_client_auth(),
            _ => {
                return Err(TlsError::InvalidConfiguration(
                    "client certificate and key must be configured together \
                     (tls.client.cert_file and tls.client.key_file)"
                        .to_owned(),
                ));
            }
        };
        Ok(Arc::new(config))
    }

    /// The default trust configuration (Mozilla roots, no client identity).
    ///
    /// This is infallible by construction and is what the HTTP-only dev/test
    /// constructors use; a deployment that needs a private CA or mTLS calls
    /// [`ClientTls::build`] with its configured files instead.
    #[must_use]
    pub fn webpki_default() -> Arc<ClientConfig> {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    }
}

/// Read every certificate in a PEM bundle.
///
/// # Errors
/// [`TlsError::Read`] for I/O failures, [`TlsError::NoCertificates`] for an
/// empty bundle and [`TlsError::InvalidPem`] for malformed items.
pub fn load_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let bytes = std::fs::read(path).map_err(|source| TlsError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = std::io::Cursor::new(bytes);
    let mut certificates = Vec::new();
    for item in rustls_pemfile::certs(&mut reader) {
        certificates.push(item.map_err(|error| TlsError::InvalidPem {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?);
    }
    if certificates.is_empty() {
        return Err(TlsError::NoCertificates {
            path: path.to_path_buf(),
        });
    }
    Ok(certificates)
}

/// Read the first private key in a PEM file.
///
/// # Errors
/// [`TlsError::Read`] for I/O failures, [`TlsError::NoPrivateKey`] for a file
/// without a supported key and [`TlsError::InvalidPem`] for malformed items.
pub fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let bytes = std::fs::read(path).map_err(|source| TlsError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = std::io::Cursor::new(bytes);
    rustls_pemfile::private_key(&mut reader)
        .map_err(|error| TlsError::InvalidPem {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?
        .ok_or_else(|| TlsError::NoPrivateKey {
            path: path.to_path_buf(),
        })
}
