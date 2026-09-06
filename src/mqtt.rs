//! Reaching a broker: the part the publisher and the subscriber share.
//!
//! The VTN publishes notifications and a VEN subscribes to them, and both have to turn one
//! `mqtts://host:8883` out of a `notifierBinding` into a connection. Written twice, that is two
//! places to disagree about a default port, a scheme name or which TLS provider is in use — and the
//! disagreement shows up as a VEN that cannot connect to the broker its own VTN published to.
//!
//! So it is written once, here, and neither side parses a URL.

use crate::std_shim::{String, ToString, format};
use std::time::Duration;

use rumqttc::{MqttOptions, Transport};

/// Turn a broker URL into connection options.
///
/// Understands `mqtt`/`tcp` (port 1883) and `mqtts`/`ssl` (port 8883). Anything else is refused by
/// name rather than defaulted, because a typo in a scheme should not silently become plaintext.
pub(crate) fn broker_options(
    url: &str,
    client_id: &str,
    keep_alive: Duration,
) -> Result<MqttOptions, String> {
    let url = url::Url::parse(url).map_err(|e| e.to_string())?;
    let host = url
        .host_str()
        .ok_or_else(|| "broker URL has no host".to_string())?
        .to_string();
    let (default_port, transport) = match url.scheme() {
        "mqtt" | "tcp" => (1883, Transport::Tcp),
        "mqtts" | "ssl" => {
            install_crypto_provider();
            (8883, Transport::tls_with_default_config())
        }
        other => {
            return Err(format!(
                "unsupported broker scheme {other:?}; use mqtt:// or mqtts://"
            ));
        }
    };

    let mut options = MqttOptions::new(
        client_id.to_string(),
        host,
        url.port().unwrap_or(default_port),
    );
    options.set_transport(transport);
    options.set_keep_alive(keep_alive);
    // A clean session would let the broker forget in-flight QoS 1 state across a reconnect, which
    // is state both ends depend on: the publisher for its acknowledgements, the subscriber for the
    // notifications that arrived while it was away.
    options.set_clean_session(false);
    Ok(options)
}

/// Pick the TLS crypto provider, once per process. See [`crate::crypto`].
pub(crate) use crate::crypto::install_crypto_provider;

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(url: &str) -> Result<MqttOptions, String> {
        broker_options(url, "test", Duration::from_secs(30))
    }

    #[test]
    fn schemes_carry_their_default_ports() {
        let plain = opts("mqtt://broker.example.com").unwrap();
        assert_eq!(plain.broker_address(), ("broker.example.com".into(), 1883));
        let tls = opts("mqtts://broker.example.com").unwrap();
        assert_eq!(tls.broker_address(), ("broker.example.com".into(), 8883));
        let explicit = opts("mqtt://broker.example.com:1884").unwrap();
        assert_eq!(
            explicit.broker_address(),
            ("broker.example.com".into(), 1884)
        );
    }

    #[test]
    fn an_unknown_scheme_is_refused_rather_than_assumed() {
        // Silently treating `mqts://` as plaintext would publish a fleet's dispatch schedule in
        // the clear because of a missing letter.
        let err = opts("mqts://broker.example.com").unwrap_err();
        assert!(err.contains("unsupported broker scheme"), "{err}");
        assert!(opts("not a url").is_err());
        assert!(opts("mqtt:///no-host").is_err());
    }

    #[test]
    fn the_session_is_not_clean() {
        // Both ends depend on the broker remembering: the publisher for in-flight acknowledgements,
        // the subscriber for what arrived while it was disconnected.
        assert!(!opts("mqtt://broker.example.com").unwrap().clean_session());
    }
}
