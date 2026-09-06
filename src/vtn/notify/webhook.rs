//! The webhook transport.
//!
//! `GET /notifiers` reports `WEBHOOK: true` when a transport is installed, and this is the
//! transport it means.
//!
//! The specification's webhook chapter is mostly a threat model, and the threat is the VTN itself:
//! a subscriber names a URL, and the VTN then makes requests to it from inside the operator's
//! network. So the interesting parts here are the refusals.
//!
//! * **The echo challenge** proves the subscriber controls the endpoint before the VTN will ever
//!   post to it `[Def §Webhooks]`.
//! * **The address is checked where the socket is opened**, not beside it: names resolve through
//!   [`GuardedResolver`], so the answer that is refused is the answer the connection would have
//!   used. Resolving separately and letting the HTTP client resolve again inspects one answer and
//!   connects to another, which is DNS rebinding with a check in front of it.
//! * **Redirects are not followed.** A redirect is the same attack with an extra hop.
//! * **Payloads are signed over an instant as well as a body** ([`crate::webhook`]), so a receiver
//!   can tell a genuine notification both from a forged one and from a *replayed* one. A signature
//!   over the body alone is valid for ever, and a dispatch instruction that can be replayed is a
//!   dispatch instruction.
//!
//! Retrying and giving up are *not* here: they belong to the dispatcher, which holds the durable
//! attempt count. A transport that retried internally would hold its outbox lease open for the whole
//! sequence and would forget the count on restart.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use async_trait::async_trait;

use super::{CallbackPolicy, CallbackRejected, Channel, Delivery, DeliveryFailure, Notifier};
use crate::core::{Clock, SystemClock};

pub use crate::webhook::{ATTEMPT_HEADER, SIGNATURE_HEADER, TIMESTAMP_HEADER};

/// How the transport behaves.
#[derive(Debug, Clone)]
pub struct WebhookConfig {
    /// Per-request timeout. The specification suggests 10–30 seconds.
    pub timeout: StdDuration,
    /// Secret used to sign payloads. Absent means unsigned.
    pub signing_key: Option<String>,
    /// Which callback URLs are acceptable.
    ///
    /// The same policy the API applies when a subscription is created, so a URL cannot be accepted
    /// at subscription time and refused for ever afterwards at delivery time.
    pub policy: CallbackPolicy,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            timeout: StdDuration::from_secs(15),
            signing_key: None,
            policy: CallbackPolicy::default(),
        }
    }
}

/// Why a delivery did not happen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebhookError {
    /// The callback URL was unusable or unsafe.
    #[error(transparent)]
    Rejected(#[from] CallbackRejected),
    /// The endpoint answered, but not with success.
    #[error("callback returned {status}")]
    Status {
        /// The status the endpoint returned.
        status: u16,
    },
    /// The endpoint could not be reached.
    #[error("callback unreachable: {0}")]
    Unreachable(String),
    /// The endpoint did not return the challenge, so it is not under the subscriber's control.
    #[error("the endpoint did not echo the challenge")]
    EchoFailed,
    /// The delivery belongs to another transport.
    #[error("this delivery names no callback URL, so it is not a webhook's to make")]
    NotOurs,
}

impl WebhookError {
    /// Whether another attempt could plausibly succeed.
    ///
    /// The status classification is the interesting half. A `4xx` is the receiver saying the request
    /// itself is wrong, and repeating it cannot make it right — except for `408` and `429`, which
    /// are explicitly "not now" rather than "not ever".
    pub fn is_retriable(&self) -> bool {
        match self {
            // A malformed or non-HTTPS URL will still be malformed in five minutes. A private
            // address, though, can be a DNS answer that changes back.
            Self::Rejected(CallbackRejected::Malformed(_) | CallbackRejected::NotHttps)
            | Self::Rejected(CallbackRejected::NoHost) => false,
            Self::Rejected(_) => true,
            Self::Status { status } => {
                !(400..500).contains(status) || *status == 408 || *status == 429
            }
            Self::Unreachable(_) => true,
            Self::EchoFailed => false,
            // Routing does not improve with time.
            Self::NotOurs => false,
        }
    }
}

/// Posts notifications to subscriber callbacks.
#[derive(Debug)]
pub struct WebhookNotifier {
    http: reqwest::Client,
    config: WebhookConfig,
    /// Stamps the instant a delivery is signed over. Injected so a test can assert what a receiver
    /// would see rather than race the wall clock.
    clock: Arc<dyn Clock>,
}

impl WebhookNotifier {
    /// Build a transport.
    pub fn new(config: WebhookConfig) -> Result<Self, WebhookError> {
        // `reqwest` is built without a crypto provider of its own, so the process default has to
        // exist before a client does. See `crate::crypto`.
        crate::crypto::install_crypto_provider();
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            // A redirect to a private address is the same attack with an extra hop.
            .redirect(reqwest::redirect::Policy::none())
            // The guard belongs in the connector: this is the resolution the socket uses.
            .dns_resolver(Arc::new(GuardedResolver {
                allow_private: config.policy.allow_private_addresses,
            }))
            .user_agent(concat!("openadr-vtn/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| WebhookError::Unreachable(e.to_string()))?;
        Ok(Self {
            http,
            config,
            clock: Arc::new(SystemClock),
        })
    }

    /// Take the signing instant from this clock.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Build a shared transport with default settings.
    pub fn shared(config: WebhookConfig) -> Result<Arc<Self>, WebhookError> {
        Ok(Arc::new(Self::new(config)?))
    }

    /// The echo challenge: a `GET` with a random `echo` parameter that the endpoint must return.
    ///
    /// Reached through [`Notifier::verify_callback`], which is what `POST /subscriptions` calls.
    async fn echo_challenge(&self, callback_url: &str) -> Result<(), WebhookError> {
        let url = self.check_url(callback_url)?;
        let challenge = random_token();

        let mut with_echo = url.clone();
        with_echo.query_pairs_mut().append_pair("echo", &challenge);

        let mut response = self
            .http
            .get(with_echo)
            .send()
            .await
            .map_err(|e| WebhookError::Unreachable(e.to_string()))?;
        if !response.status().is_success() {
            return Err(WebhookError::Status {
                status: response.status().as_u16(),
            });
        }
        // Read the answer a chunk at a time and stop. `text()` would buffer whatever the endpoint
        // chooses to send, on a path an unauthenticated-to-us stranger picks the URL for: the
        // subscriber names the callback and the VTN then fetches it, so an endless body is a
        // memory exhaustion the subscription endpoint hands out for free. The challenge is 32 hex
        // characters, so anything past the cap is already not it.
        let mut body = Vec::with_capacity(ECHO_BODY_LIMIT);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| WebhookError::Unreachable(e.to_string()))?
        {
            if body.len() + chunk.len() > ECHO_BODY_LIMIT {
                return Err(WebhookError::EchoFailed);
            }
            body.extend_from_slice(&chunk);
        }
        let body = String::from_utf8(body).map_err(|_| WebhookError::EchoFailed)?;
        if body.trim() != challenge {
            return Err(WebhookError::EchoFailed);
        }
        Ok(())
    }

    /// Re-check the callback URL immediately before using it.
    ///
    /// The API checked the same thing when the subscription was created, but a policy can be
    /// tightened between then and now, and a literal address is caught here without a resolver.
    /// What the *name* resolves to is [`GuardedResolver`]'s job.
    fn check_url(&self, callback_url: &str) -> Result<url::Url, WebhookError> {
        Ok(self.config.policy.check_literal(callback_url)?)
    }

    /// The signature headers for a body, or none when the VTN signs nothing.
    ///
    /// Both headers or neither: a signature whose timestamp did not travel with it cannot be
    /// verified, because the instant is part of what was signed.
    fn sign(&self, body: &[u8]) -> Option<(String, String)> {
        let key = self.config.signing_key.as_ref()?;
        let at = self.clock.now();
        Some((
            crate::webhook::sign(key.as_bytes(), at, body),
            at.as_second().to_string(),
        ))
    }

    /// Make exactly one attempt.
    ///
    /// One, not several: the dispatcher holds the durable attempt count and decides when to try
    /// again. Looping here would hold the outbox lease open for the whole sequence and would lose
    /// the count on restart.
    async fn post(&self, delivery: &Delivery, attempt: u32) -> Result<(), WebhookError> {
        let Some(callback_url) = delivery.route.callback_url() else {
            // Unreachable through `Notifiers`, which routes by channel. Refused rather than
            // shrugged off: returning `Ok` here is how a broker notification once came to be
            // recorded as delivered by the transport that cannot publish one.
            return Err(WebhookError::NotOurs);
        };

        let url = self.check_url(callback_url)?;
        let body = serde_json::to_vec(&delivery.notification)
            .map_err(|e| WebhookError::Unreachable(e.to_string()))?;

        let mut request = self
            .http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(ATTEMPT_HEADER, attempt.to_string())
            .body(body.clone());
        if let Some(token) = delivery.route.bearer_token() {
            request = request.bearer_auth(token);
        }
        if let Some((signature, at)) = self.sign(&body) {
            request = request
                .header(SIGNATURE_HEADER, signature)
                .header(TIMESTAMP_HEADER, at);
        }

        match request.send().await {
            Ok(response) if response.status().is_success() => Ok(()),
            Ok(response) => Err(WebhookError::Status {
                status: response.status().as_u16(),
            }),
            Err(e) => Err(WebhookError::Unreachable(e.to_string())),
        }
    }
}

#[async_trait]
impl Notifier for WebhookNotifier {
    fn handles(&self, channel: Channel) -> bool {
        channel == Channel::Webhook
    }

    async fn deliver(&self, delivery: &Delivery, attempt: u32) -> Result<(), DeliveryFailure> {
        // The count comes from the dispatcher, which is the only thing that holds it durably.
        match self.post(delivery, attempt).await {
            Ok(()) => {
                tracing::debug!(
                    subscription = ?delivery.subscription_id,
                    object = %delivery.notification.object.object_type(),
                    "notification delivered"
                );
                Ok(())
            }
            Err(e) => {
                tracing::debug!(
                    subscription = ?delivery.subscription_id,
                    error = %e,
                    retriable = e.is_retriable(),
                    "delivery attempt failed"
                );
                Err(if e.is_retriable() {
                    DeliveryFailure::retriable(e.to_string())
                } else {
                    DeliveryFailure::permanent(e.to_string())
                })
            }
        }
    }

    async fn verify_callback(&self, callback_url: &str) -> Result<(), DeliveryFailure> {
        match self.echo_challenge(callback_url).await {
            Ok(()) => Ok(()),
            Err(e) => Err(if e.is_retriable() {
                DeliveryFailure::retriable(e.to_string())
            } else {
                DeliveryFailure::permanent(e.to_string())
            }),
        }
    }

    fn name(&self) -> &'static str {
        "webhook"
    }
}

/// The DNS resolver the webhook client uses.
///
/// `reqwest` asks this for every hostname it is about to connect to, and the addresses it returns
/// are the ones the socket is opened against. Filtering here is therefore not a check *before* the
/// connection but a check *of* it: there is no second resolution to disagree with.
#[derive(Debug)]
struct GuardedResolver {
    allow_private: bool,
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let allow_private = self.allow_private;
        Box::pin(async move {
            let host = name.as_str().to_string();
            // Port 0: the caller supplies the real one; only the addresses matter here.
            let resolved: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if resolved.is_empty() {
                return Err(Box::new(CallbackRejected::Unresolvable {
                    host,
                    detail: "resolved to no addresses".into(),
                })
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            if !allow_private && let Some(bad) = resolved.iter().find(|a| super::is_private(a.ip()))
            {
                // One private answer poisons the set: a name that resolves to both a public and a
                // private address is a rebinding attempt, not a multi-homed server.
                return Err(Box::new(CallbackRejected::PrivateAddress {
                    host: format!("{host} ({})", bad.ip()),
                })
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(resolved.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Largest echo response the VTN will read.
///
/// The challenge is 32 hex characters; a kilobyte is room for a trailing newline and a
/// well-meaning wrapper, and nothing else.
const ECHO_BODY_LIMIT: usize = 1024;

/// A random challenge or identifier.
fn random_token() -> String {
    use rand::Rng as _;
    let bytes: [u8; 16] = rand::rng().random();
    bytes.iter().fold(String::with_capacity(32), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transport_re_checks_the_callback_before_using_it() {
        let notifier = WebhookNotifier::new(WebhookConfig::default()).unwrap();
        for url in ["http://example.com/hook", "https://localhost/hook"] {
            let err = notifier.check_url(url).unwrap_err();
            assert!(matches!(err, WebhookError::Rejected(_)), "{url}: {err:?}");
        }
    }

    #[test]
    fn a_signed_delivery_is_verifiable_by_the_receiver() {
        // The end-to-end property the scheme exists for: what the transport puts in the two
        // headers is exactly what `openadr::webhook` accepts. They were separate implementations
        // once, and the receiver half is the half nobody runs.
        let at: crate::model::Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
        let with_key = |key: &str| {
            WebhookNotifier::new(WebhookConfig {
                signing_key: Some(key.into()),
                ..Default::default()
            })
            .unwrap()
            .with_clock(Arc::new(crate::core::FixedClock::new(at)))
        };

        let (signature, stamp) = with_key("secret").sign(b"body").unwrap();
        assert_eq!(crate::webhook::parse_timestamp(&stamp), Some(at));
        crate::webhook::Signature::parse(&signature)
            .unwrap()
            .verify(
                b"secret",
                b"body",
                at,
                at,
                crate::webhook::DEFAULT_TOLERANCE,
            )
            .expect("a receiver verifies what the transport signed");

        // Stable for one key and body, and different for anything else.
        assert_eq!(signature, with_key("secret").sign(b"body").unwrap().0);
        assert_ne!(signature, with_key("other").sign(b"body").unwrap().0);
        assert_ne!(signature, with_key("secret").sign(b"different").unwrap().0);

        let unsigned = WebhookNotifier::new(WebhookConfig::default()).unwrap();
        assert!(unsigned.sign(b"body").is_none());
    }

    #[test]
    fn a_client_error_is_permanent_and_a_server_error_is_not() {
        let status = |status| WebhookError::Status { status };
        assert!(!status(400).is_retriable(), "a 400 will always be a 400");
        assert!(!status(404).is_retriable());
        // "Not now", not "not ever".
        assert!(status(408).is_retriable());
        assert!(status(429).is_retriable());
        assert!(status(500).is_retriable());
        assert!(status(503).is_retriable());
        assert!(WebhookError::Unreachable("refused".into()).is_retriable());
        assert!(!WebhookError::Rejected(CallbackRejected::NotHttps).is_retriable());
        // DNS answered privately once; it may not next time.
        assert!(
            WebhookError::Rejected(CallbackRejected::PrivateAddress { host: "h".into() })
                .is_retriable()
        );
    }

    #[tokio::test]
    async fn the_resolver_refuses_a_name_that_answers_privately() {
        use reqwest::dns::Resolve as _;
        use std::str::FromStr as _;

        // `localhost` resolves to a loopback address on every host, which is what makes it a usable
        // stand-in here for a public name whose answer has been rebound to one.
        let name = || reqwest::dns::Name::from_str("localhost").unwrap();
        let guarded = GuardedResolver {
            allow_private: false,
        };
        let err = guarded
            .resolve(name())
            .await
            .err()
            .expect("a loopback answer must be refused inside the connector");
        assert!(err.to_string().contains("loopback"), "{err}");

        // And the escape hatch tests use really does open it.
        let permissive = GuardedResolver {
            allow_private: true,
        };
        assert!(permissive.resolve(name()).await.is_ok());
    }

    #[test]
    fn challenges_do_not_repeat() {
        let a = random_token();
        assert_eq!(a.len(), 32);
        assert_ne!(a, random_token());
    }
}
