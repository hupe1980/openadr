//! Serving the VTN over TLS, and deciding who may connect at all.
//!
//! Without this the single-binary deployment needs a reverse proxy to be reachable over anything but
//! plaintext — on the machine with the least room for a second process. The specification's own
//! worked discovery record writes `local_url=https://…`, so a local VTN is expected to speak it.
//!
//! **Server TLS** is the ordinary half: a certificate and a key.
//!
//! **A client CA is a network gate, not an authenticator.** It refuses the *connection* of anyone
//! whose certificate that CA did not issue, before a byte of HTTP is parsed. What it deliberately
//! does not do is decide who the caller *is*: that stays in
//! [`Authenticator`](super::auth::Authenticator), where every other authorization decision lives.
//! The peer's chain reaches the application as [`PeerCertificates`] — DER, unparsed, because what a
//! subject *means* is a deployment's decision and guessing at a Common Name is how one ends up
//! authenticating on a string that was never meant to be one.
//!
//! Fluvius' NetFlex profile wants the pair: mutual TLS at the edge, no OAuth2 `[NetFlex §5.4]`.
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use openadr::vtn::tls::TlsConfig;
//! # let vtn: openadr::vtn::Vtn = todo!();
//! let tls = TlsConfig::from_pem_files("cert.pem", "key.pem")?
//!     .with_client_ca_file("clients-ca.pem")?;
//! vtn.serve_tls("0.0.0.0:8443", tls).await?;
//! # Ok(()) }
//! ```

use std::path::Path;
use std::sync::Arc;

// The PEM reader is `rustls-pki-types`' own, not `rustls-pemfile`: the latter has been an archived
// thin wrapper around exactly this code since August 2025 (RUSTSEC-2025-0134), and depending on a
// wrapper for a parser both crates already ship is a supply-chain entry with nothing behind it.
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

/// Why a TLS listener could not be built or started.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// A PEM file could not be read.
    #[error("could not read {path}: {source}")]
    Io {
        /// The file that could not be read.
        path: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A PEM file held nothing of the kind expected.
    #[error("{0}")]
    Pem(String),
    /// `rustls` refused the configuration.
    #[error("TLS configuration refused: {0}")]
    Config(String),
}

fn read(path: &Path) -> Result<Vec<u8>, TlsError> {
    std::fs::read(path).map_err(|source| TlsError::Io {
        path: path.display().to_string(),
        source,
    })
}

/// The certificate chain a peer presented, if the listener asked for one.
///
/// Placed in every request's extensions by [`Vtn::serve_tls`](crate::vtn::Vtn::serve_tls). DER,
/// unparsed, leaf first — this crate has no
/// X.509 parser and will not pretend to: what a subject *means* is a deployment's decision.
///
/// Absent when the listener requested no client certificate, and absent when the peer was allowed to
/// connect without one — so its presence is evidence and its absence is not.
#[derive(Debug, Clone)]
pub struct PeerCertificates(pub Arc<Vec<CertificateDer<'static>>>);

/// A server certificate, its key, and optionally the CA that client certificates must come from.
#[derive(Clone)]
pub struct TlsConfig {
    chain: Vec<CertificateDer<'static>>,
    key: Arc<PrivateKeyDer<'static>>,
    client_ca: Vec<CertificateDer<'static>>,
    require_client_certificate: bool,
}

impl std::fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsConfig")
            .field("chain_length", &self.chain.len())
            .field("key", &"<redacted>")
            .field("client_ca_certificates", &self.client_ca.len())
            .field(
                "require_client_certificate",
                &self.require_client_certificate,
            )
            .finish()
    }
}

impl TlsConfig {
    /// Read a certificate chain and a private key from PEM files.
    ///
    /// The chain is leaf first, as every tool that writes one emits it. The key may be PKCS#8,
    /// PKCS#1 or SEC1; which one it is, is not something an operator should have to know.
    pub fn from_pem_files(
        certificate_chain: impl AsRef<Path>,
        private_key: impl AsRef<Path>,
    ) -> Result<Self, TlsError> {
        let cert_path = certificate_chain.as_ref();
        let key_path = private_key.as_ref();
        Self::from_pem(&read(cert_path)?, &read(key_path)?).map_err(|e| match e {
            TlsError::Pem(message) => TlsError::Pem(format!(
                "{message} (certificate {}, key {})",
                cert_path.display(),
                key_path.display()
            )),
            other => other,
        })
    }

    /// The same, from PEM already in memory.
    pub fn from_pem(certificate_chain: &[u8], private_key: &[u8]) -> Result<Self, TlsError> {
        let chain = certificates(certificate_chain)?;
        if chain.is_empty() {
            return Err(TlsError::Pem(
                "the certificate file holds no CERTIFICATE block".into(),
            ));
        }
        let key = PrivateKeyDer::from_pem_slice(private_key).map_err(|e| {
            TlsError::Pem(format!(
                "the key file holds no usable PRIVATE KEY block (PKCS#8, PKCS#1 and SEC1 are all \
                 read): {e}"
            ))
        })?;
        Ok(Self {
            chain,
            key: Arc::new(key),
            client_ca: Vec::new(),
            require_client_certificate: true,
        })
    }

    /// Refuse any connection whose client certificate this CA did not issue.
    pub fn with_client_ca_file(self, path: impl AsRef<Path>) -> Result<Self, TlsError> {
        let path = path.as_ref();
        let pem = read(path)?;
        self.with_client_ca(&pem).map_err(|e| match e {
            TlsError::Pem(message) => TlsError::Pem(format!("{message} ({})", path.display())),
            other => other,
        })
    }

    /// The same, from PEM already in memory.
    pub fn with_client_ca(mut self, pem: &[u8]) -> Result<Self, TlsError> {
        let ca = certificates(pem)?;
        if ca.is_empty() {
            return Err(TlsError::Pem(
                "the client CA file holds no CERTIFICATE block".into(),
            ));
        }
        self.client_ca = ca;
        Ok(self)
    }

    /// Ask for a client certificate but serve a peer that presents none.
    ///
    /// For a migration, and for nothing else. A listener in this state is not a gate: every request
    /// still has to be authorized by a credential, and the certificate is only evidence when it
    /// happens to be there.
    pub fn allowing_anonymous_clients(mut self) -> Self {
        self.require_client_certificate = false;
        self
    }

    /// Whether this listener asks peers for a certificate at all.
    pub fn asks_for_client_certificates(&self) -> bool {
        !self.client_ca.is_empty()
    }

    /// Build the `rustls` server configuration.
    ///
    /// TLS 1.2 and 1.3 only, which is what the compiled provider offers; there is no switch for
    /// anything older because there is nothing older to switch on.
    pub fn server_config(&self) -> Result<rustls::ServerConfig, TlsError> {
        crate::crypto::install_crypto_provider();

        let builder = rustls::ServerConfig::builder();
        let builder = if self.client_ca.is_empty() {
            builder.with_no_client_auth()
        } else {
            let mut roots = rustls::RootCertStore::empty();
            for certificate in &self.client_ca {
                roots.add(certificate.clone()).map_err(|e| {
                    TlsError::Config(format!("a client CA certificate was refused: {e}"))
                })?;
            }
            let roots = Arc::new(roots);
            let verifier = if self.require_client_certificate {
                rustls::server::WebPkiClientVerifier::builder(roots).build()
            } else {
                rustls::server::WebPkiClientVerifier::builder(roots)
                    .allow_unauthenticated()
                    .build()
            }
            .map_err(|e| TlsError::Config(format!("the client verifier was refused: {e}")))?;
            builder.with_client_cert_verifier(verifier)
        };

        let mut config = builder
            .with_single_cert(self.chain.clone(), self.key.clone_key())
            .map_err(|e| TlsError::Config(format!("the certificate and key were refused: {e}")))?;
        // HTTP/2 first: an event with a year of quarter-hourly intervals is one large body, and a
        // fleet of VENs polling is many small ones. Both are better multiplexed.
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(config)
    }
}

fn certificates(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TlsError::Pem(format!("unreadable certificate: {e}")))
}

/// Serve a router over TLS until `shutdown` resolves.
///
/// Written as an accept loop rather than handed to `axum::serve` because the peer's certificate is
/// known only *after* the handshake, and it has to reach the request that follows it. A connection
/// that fails to handshake is dropped and logged at debug: on a public listener that is a scan, and
/// one line per scan is a log nobody reads.
pub(super) async fn serve(
    listener: tokio::net::TcpListener,
    router: axum::Router,
    config: rustls::ServerConfig,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use tower::Service as _;

    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => accepted?,
            () = &mut shutdown => {
                tracing::info!("shutting down");
                return Ok(());
            }
        };

        let acceptor = acceptor.clone();
        let router = router.clone();
        tokio::spawn(async move {
            let stream = match acceptor.accept(stream).await {
                Ok(stream) => stream,
                Err(e) => {
                    tracing::debug!(%peer, error = %e, "TLS handshake failed");
                    return;
                }
            };

            // The peer's chain, captured once here rather than looked up per request: it is a
            // property of the connection, and every request on it shares one.
            let certificates = stream
                .get_ref()
                .1
                .peer_certificates()
                .map(|chain| PeerCertificates(Arc::new(chain.to_vec())));

            let service = hyper::service::service_fn(move |mut request: hyper::Request<_>| {
                if let Some(certificates) = certificates.clone() {
                    request.extensions_mut().insert(certificates);
                }
                router.clone().call(request)
            });

            if let Err(e) = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(TokioIo::new(stream), service)
                .await
            {
                tracing::debug!(%peer, error = %e, "connection ended");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_certificate_file_with_no_certificate_is_named_as_such() {
        let e = TlsConfig::from_pem(b"not a pem file", b"neither is this").unwrap_err();
        assert!(
            matches!(&e, TlsError::Pem(m) if m.contains("no CERTIFICATE")),
            "{e}"
        );
    }

    #[test]
    fn a_missing_file_names_itself() {
        let e =
            TlsConfig::from_pem_files("/nonexistent/cert.pem", "/nonexistent/key.pem").unwrap_err();
        assert!(
            matches!(&e, TlsError::Io { path, .. } if path.contains("cert.pem")),
            "{e}"
        );
    }
}
