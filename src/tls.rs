//! Mutual TLS configuration shared by the web listener and federation client.
//!
//! A LAN discovery record is intentionally unauthenticated multicast data. The
//! certificate authority configured here is therefore the trust anchor: a
//! discovered address is contacted only through HTTPS, and no bearer token is
//! sent until its server certificate has validated against that authority.

use std::{
    fs,
    io::{BufReader, Cursor},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use axum_server::tls_rustls::RustlsConfig;
use rustls::{RootCertStore, ServerConfig, server::WebPkiClientVerifier};
use tokio_rustls::TlsConnector;

use crate::config::TlsConfig;

/// Selects the process-wide cryptographic provider before any Rustls client or
/// server configuration is created.
///
/// `reqwest` and the web listener can enable different Rustls providers through
/// their transitive dependencies. Making the choice explicit prevents startup
/// from panicking when both are linked.
///
/// # Errors
///
/// Returns an error only when another incompatible provider was installed
/// before atmux started its TLS subsystem.
pub fn install_crypto_provider() -> Result<()> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| {
            anyhow::anyhow!("an incompatible Rustls crypto provider was already installed")
        })
}

/// Builds the HTTPS server configuration.
///
/// Every client must present a certificate signed by the configured CA.
/// Browsers reach the public service through the gateway, which presents its
/// dedicated client certificate; federation clients present their node
/// identity from [`TlsConfig`].
///
/// # Errors
///
/// Returns an error when a configured certificate, key, or CA file is missing
/// or cannot form a valid Rustls server configuration.
pub fn server_config(tls: &TlsConfig) -> Result<RustlsConfig> {
    let certificates = read_certificates(&tls.cert_file)?;
    let private_key = read_private_key(&tls.key_file)?;
    let roots = read_root_store(&tls.ca_file)?;
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .context("failed to configure the atmux client-certificate verifier")?;
    let mut config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certificates, private_key)
        .context("failed to load the atmux TLS certificate and private key")?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(RustlsConfig::from_config(Arc::new(config)))
}

/// Builds a certificate-validating TLS connector that presents this node's
/// client certificate to its peers.
///
/// # Errors
///
/// Returns an error when certificate material cannot be read or parsed, or a
/// TLS client cannot be constructed.
pub fn client(tls: &TlsConfig) -> Result<TlsConnector> {
    let roots = read_root_store(&tls.ca_file)?;
    let certificates = read_certificates(&tls.cert_file)?;
    let private_key = read_private_key(&tls.key_file)?;
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(certificates, private_key)
        .context("failed to load the atmux TLS client certificate and private key")?;
    Ok(TlsConnector::from(Arc::new(config)))
}

fn read_certificates(
    path: &std::path::Path,
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read atmux TLS certificate {}", path.display()))?;
    let certificates = rustls_pemfile::certs(&mut Cursor::new(bytes))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to parse the atmux TLS certificate")?;
    if certificates.is_empty() {
        bail!("atmux TLS certificate file contains no certificates");
    }
    Ok(certificates)
}

fn read_private_key(path: &std::path::Path) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let file = fs::File::open(path)
        .with_context(|| format!("failed to read atmux TLS private key {}", path.display()))?;
    rustls_pemfile::private_key(&mut BufReader::new(file))
        .context("failed to parse the atmux TLS private key")?
        .context("atmux TLS private-key file contains no private key")
}

fn read_root_store(path: &std::path::Path) -> Result<RootCertStore> {
    let certificates = read_certificates(path)?;
    let mut roots = RootCertStore::empty();
    let (added, _) = roots.add_parsable_certificates(certificates);
    if added == 0 {
        bail!("atmux TLS CA file contains no usable certificates");
    }
    Ok(roots)
}
