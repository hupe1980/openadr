//! Shared MQTT 3.1.1 transport for VTN publishers and VEN subscribers.
//!
//! [`crate::mqtt::BrokerAddress`] selects TCP or TLS from the broker URL. [`crate::mqtt::Connection`] supports
//! concurrent QoS 1 publishes and completes each one when the broker sends its PUBACK.
//! OpenADR requires MQTT 3.1.1 or later and recommends MQTT 5.0
//! (`[Def §MQTT]`, `[Notifiers §11.2]`, `[Notifiers §22.9]`).

mod codec;
mod conn;

pub use codec::{CodecError, ConnAck, MAX_PACKET, QoS};
pub use conn::{Connection, ConnectionConfig, Incoming};

use crate::std_shim::{String, ToString, format};

/// Why a broker operation did not happen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MqttError {
    /// TLS requires the embedding application to install a provider.
    #[error(transparent)]
    MissingCryptoProvider(#[from] crate::MissingCryptoProvider),
    /// The broker URL could not be understood.
    #[error("invalid broker URL: {0}")]
    Url(String),
    /// The installed TLS provider cannot support the requested protocol versions.
    #[error("invalid MQTT TLS configuration: {0}")]
    TlsConfig(String),
    /// The broker refused the connection, with its return code.
    #[error("the broker refused the connection: {1} (code {0})")]
    Refused(u8, &'static str),
    /// The connection dropped, or never opened.
    #[error("the broker connection dropped: {0}")]
    Disconnected(String),
    /// The broker sent something that is not MQTT 3.1.1.
    #[error("the broker spoke badly: {0}")]
    Protocol(#[from] CodecError),
    /// The broker did not answer in time.
    #[error("the broker did not answer within {0:?}")]
    Timeout(core::time::Duration),
}

impl MqttError {
    /// Whether another attempt could plausibly succeed.
    ///
    /// Local URL and TLS configuration errors require operator action. A broker that is down comes back, a dropped
    /// connection is what the dispatcher's backoff is for, and even a refusal is retriable: the
    /// commonest cause is an access token that expired, which is true now and false after the
    /// operator renews it (D-102).
    pub fn is_retriable(&self) -> bool {
        !matches!(
            self,
            MqttError::Url(_) | MqttError::MissingCryptoProvider(_) | MqttError::TlsConfig(_)
        )
    }
}

/// A broker's address, resolved from a URL.
///
/// Parsed once, so no code path can disagree about a default port or decide that an unrecognised
/// scheme means plaintext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerAddress {
    host: String,
    port: u16,
    tls: bool,
}

impl core::fmt::Display for BrokerAddress {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let scheme = if self.tls { "mqtts" } else { "mqtt" };
        write!(f, "{scheme}://{}:{}", self.host, self.port)
    }
}

impl BrokerAddress {
    /// Understands `mqtt`/`tcp` (port 1883) and `mqtts`/`ssl` (port 8883).
    ///
    /// Anything else is refused **by name**. A scheme this does not know is far more likely to be a
    /// typo than an intention, and the one thing that must never happen is for it to become
    /// plaintext silently — the broker carries dispatch instructions and, on a per-VEN topic, one
    /// customer's schedule.
    pub fn parse(url: &str) -> Result<Self, MqttError> {
        let parsed = url::Url::parse(url).map_err(|e| MqttError::Url(e.to_string()))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| MqttError::Url("broker URL has no host".to_string()))?
            .to_string();
        let (default_port, tls) = match parsed.scheme() {
            "mqtt" | "tcp" => (1883, false),
            "mqtts" | "ssl" => (8883, true),
            other => {
                return Err(MqttError::Url(format!(
                    "unsupported broker scheme {other:?}; use mqtt:// or mqtts://"
                )));
            }
        };
        Ok(Self {
            host,
            port: parsed.port().unwrap_or(default_port),
            tls,
        })
    }

    /// The host, without brackets for an IPv6 literal.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port, defaulted from the scheme where the URL named none.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Whether the connection is TLS.
    pub fn is_tls(&self) -> bool {
        self.tls
    }

    /// Open a socket to it.
    pub(crate) async fn connect(&self) -> Result<conn::Stream, MqttError> {
        let tcp = tokio::net::TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(|e| MqttError::Disconnected(format!("{self}: {e}")))?;
        // Notifications are small and latency matters more than packet count: a 40-byte PUBACK
        // waiting on Nagle is a round trip added to every QoS 1 publish.
        let _ = tcp.set_nodelay(true);

        if !self.tls {
            return Ok(Box::new(tcp));
        }

        let provider = crate::crypto::install_crypto_provider()?;
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| MqttError::TlsConfig(e.to_string()))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server_name =
            rustls::pki_types::ServerName::try_from(self.host.clone()).map_err(|e| {
                MqttError::Url(format!("{} is not a valid TLS server name: {e}", self.host))
            })?;
        let stream = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
            .connect(server_name, tcp)
            .await
            .map_err(|e| MqttError::Disconnected(format!("TLS handshake with {self}: {e}")))?;
        Ok(Box::new(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemes_carry_their_default_ports() {
        let plain = BrokerAddress::parse("mqtt://broker.example.com").unwrap();
        assert_eq!(
            (plain.host(), plain.port(), plain.is_tls()),
            ("broker.example.com", 1883, false)
        );
        let tls = BrokerAddress::parse("mqtts://broker.example.com").unwrap();
        assert_eq!(
            (tls.host(), tls.port(), tls.is_tls()),
            ("broker.example.com", 8883, true)
        );
    }

    #[test]
    fn an_explicit_port_wins_over_the_scheme_default() {
        let a = BrokerAddress::parse("mqtts://broker.example.com:1884").unwrap();
        assert_eq!(a.port(), 1884);
        assert!(a.is_tls());
    }

    #[test]
    fn tcp_and_ssl_are_the_same_two_transports_under_other_names() {
        assert!(!BrokerAddress::parse("tcp://h").unwrap().is_tls());
        assert!(BrokerAddress::parse("ssl://h").unwrap().is_tls());
    }

    #[test]
    fn an_unknown_scheme_is_refused_by_name_rather_than_defaulted() {
        // `ws://` included: a scheme this does not implement is refused by name rather than
        // silently becoming one it does.
        for url in ["ws://h", "wss://h", "http://h", "mqt://h"] {
            let err = BrokerAddress::parse(url).unwrap_err();
            assert!(matches!(err, MqttError::Url(_)), "{url}");
            assert!(err.to_string().contains("use mqtt:// or mqtts://"), "{url}");
        }
    }

    #[test]
    fn a_url_with_no_host_is_refused() {
        assert!(BrokerAddress::parse("mqtt://").is_err());
    }

    #[test]
    fn an_ipv6_literal_keeps_its_brackets_off_the_host() {
        // `url` returns `[::1]` from `host_str`, which is D-036 in a different crate. The host is
        // handed to `TcpStream::connect`, which wants the address without them.
        let a = BrokerAddress::parse("mqtt://[::1]:1883").unwrap();
        assert_eq!(a.port(), 1883);
        assert!(a.host().contains("::1"));
    }

    #[test]
    fn a_url_that_does_not_parse_is_not_retried_and_everything_else_is() {
        assert!(!MqttError::Url("x".into()).is_retriable());
        assert!(MqttError::Disconnected("x".into()).is_retriable());
        assert!(MqttError::Refused(5, "not authorized").is_retriable());
    }
}
