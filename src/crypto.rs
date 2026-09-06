//! The one decision `rustls` refuses to make for itself.

/// Install `ring` as this process's `rustls` crypto provider, once.
///
/// `rustls` will not choose between compiled-in providers and will not run without one. This crate
/// builds `reqwest` and `rumqttc` in their `…-no-provider` shapes — the alternative drags in
/// `aws-lc-rs`, a second provider nothing calls — and names `ring` here instead `[D-112]`.
///
/// **Every TLS client and listener this crate builds calls this first**, so an ordinary user never
/// has to. It is public for the case that is not ordinary: a program building its *own*
/// `reqwest::Client` or `rustls` configuration in the same process, which would otherwise panic with
/// `No provider set`. Calling it twice, or after somebody else has installed one, does nothing.
///
/// ```no_run
/// openadr::install_crypto_provider();
/// let http = reqwest::Client::builder().build().unwrap();
/// ```
pub fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
