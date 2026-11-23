//! Process-wide TLS provider selection.

/// No application provider was installed and the ring fallback is disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "no rustls crypto provider installed; call CryptoProvider::install_default at startup or enable openadr's tls-ring feature"
)]
pub struct MissingCryptoProvider;

/// Ensure a provider is installed before constructing a TLS transport.
///
/// An application can install its own rustls provider before calling any OpenADR TLS role.
/// With the default `tls-ring` feature, ring fills an empty process default. Without it,
/// this returns an actionable error and the application can install a provider and retry.
pub fn install_crypto_provider()
-> Result<&'static std::sync::Arc<rustls::crypto::CryptoProvider>, MissingCryptoProvider> {
    use rustls::crypto::CryptoProvider;
    #[cfg(feature = "tls-ring")]
    if CryptoProvider::get_default().is_none() {
        // A concurrent installer winning this race also satisfies the postcondition.
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    CryptoProvider::get_default().ok_or(MissingCryptoProvider)
}
