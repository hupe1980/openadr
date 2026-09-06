//! mDNS/DNS-SD discovery of a local VTN.
//!
//! `[Def §Discovery and Configuration of Local VTNs]`. A VEN inside a customer site — a heat pump,
//! a charge point, a gateway — **SHOULD** be able to find the VTN on the same network without being
//! told a URL, and a VTN intended for that site **SHOULD** advertise itself. It is the half of the
//! single-binary deployment that SQLite starts and this finishes: one process, one file, and a VEN
//! that finds it.
//!
//! The specification is unusually concrete here — a service type, six TXT keys and their exact
//! spellings — so this module is split the same way the rest of the crate is: the part that decides
//! *what is said* is pure and exhaustively tested ([`VtnService`]), and the part that puts it on a
//! socket is a thin wrapper over `mdns-sd`. A test for "does the TXT record say the right thing"
//! must not need a multicast group.
//!
//! ```no_run
//! # #[cfg(feature = "mdns")]
//! # fn run() -> Result<(), openadr::discovery::DiscoveryError> {
//! use openadr::discovery::{Advertisement, VtnService};
//!
//! // On the VTN: announce this server to the local network.
//! let service = VtnService::new("my-vtn", 3000).with_program_names(["local-tariff"]);
//! let _handle = Advertisement::start(&service)?;
//! # Ok(())
//! # }
//! ```
//!
//! Everything here happens **before** any OpenADR communication: discovery does not change the
//! protocol, and a VEN that was given a URL never needs it.

use crate::std_shim::{String, ToString, Vec, format, vec};

/// The DNS-SD service type OpenADR 3 registers `[Def §Discovery]`.
pub const SERVICE_TYPE: &str = "_openadr3._tcp.local.";

/// TXT keys, spelled exactly as the specification spells them.
///
/// Named constants rather than string literals at each use, because a discovery record whose key is
/// `basePath` instead of `base_path` is a record every other implementation ignores — and it fails
/// by *finding nothing*, which looks identical to an empty network.
pub mod txt {
    /// The OpenADR release the VTN serves, e.g. `3.1.0`.
    pub const VERSION: &str = "version";
    /// The prefix before the OpenADR endpoints, e.g. `openadr3/3.1.0`.
    pub const BASE_PATH: &str = "base_path";
    /// The full URL a VEN should use.
    pub const LOCAL_URL: &str = "local_url";
    /// Comma-separated `programName`s local VENs should use.
    pub const PROGRAM_NAMES: &str = "program_names";
    /// `True` or `False` — whether the VTN requires a credential.
    pub const REQUIRES_AUTH: &str = "requires_auth";
    /// Where the VTN's OpenAPI document is, if it serves one.
    pub const OPENAPI_URL: &str = "openapi_url";
}

/// Why discovery could not be started or read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscoveryError {
    /// The service name or hostname is not usable in a DNS-SD record.
    #[error("invalid service description: {0}")]
    Invalid(String),
    /// The mDNS responder could not be started, or stopped unexpectedly.
    #[error("mDNS is unavailable: {0}")]
    Unavailable(String),
}

/// A VTN as it appears on the local network.
///
/// The pure half: it renders the TXT record a responder publishes and parses the one a browser
/// receives, and it does both without a socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VtnService {
    /// The DNS-SD instance name, e.g. `My_VTN`.
    pub instance: String,
    /// The `.local` hostname, without the trailing dot. Defaults to `{instance}.local`.
    pub hostname: String,
    /// The port the VTN listens on.
    pub port: u16,
    /// The OpenADR release served. Defaults to [`crate::SPEC_VERSION`].
    pub version: String,
    /// The base path, **without** a leading slash — the specification's own example is
    /// `openadr3/3.1.0`.
    pub base_path: String,
    /// Whether the VTN is reached over TLS, which decides the scheme in `local_url`.
    pub tls: bool,
    /// `programName`s a local VEN should follow.
    pub program_names: Vec<String>,
    /// Whether a credential is required.
    pub requires_auth: bool,
    /// Where the VTN's OpenAPI document is, if it serves one.
    pub openapi_url: Option<String>,
}

impl VtnService {
    /// A service description with the specification's defaults.
    pub fn new(instance: impl Into<String>, port: u16) -> Self {
        let instance = instance.into();
        Self {
            hostname: format!("{instance}.local"),
            instance,
            port,
            version: crate::SPEC_VERSION.to_string(),
            base_path: crate::DEFAULT_BASE_PATH.trim_start_matches('/').to_string(),
            // A VTN on a customer site is commonly plain HTTP behind a router, and advertising
            // `https` for one that does not serve it sends every VEN to a port that resets. The
            // specification's own example uses `https`; this is a fact about the deployment rather
            // than a preference, so it is set rather than assumed.
            tls: false,
            program_names: Vec::new(),
            requires_auth: true,
            openapi_url: None,
        }
    }

    /// Set the `.local` hostname to announce.
    pub fn with_hostname(mut self, hostname: impl Into<String>) -> Self {
        self.hostname = hostname.into();
        self
    }

    /// Set the base path, with or without a leading slash.
    pub fn with_base_path(mut self, base_path: &str) -> Self {
        self.base_path = base_path.trim_start_matches('/').to_string();
        self
    }

    /// Advertise `https` in `local_url`.
    pub fn with_tls(mut self) -> Self {
        self.tls = true;
        self
    }

    /// Name the programmes a local VEN should follow.
    pub fn with_program_names<S: Into<String>>(
        mut self,
        names: impl IntoIterator<Item = S>,
    ) -> Self {
        self.program_names = names.into_iter().map(Into::into).collect();
        self
    }

    /// Say whether a credential is required.
    pub fn requiring_auth(mut self, required: bool) -> Self {
        self.requires_auth = required;
        self
    }

    /// Point at an OpenAPI document.
    pub fn with_openapi_url(mut self, url: impl Into<String>) -> Self {
        self.openapi_url = Some(url.into());
        self
    }

    /// The URL a VEN should use, as the `local_url` TXT value.
    ///
    /// `https://{hostname}.local:{port}/{base_path}`, per the specification's own wording — and the
    /// `.local` name rather than an address, because a DHCP lease changes and a name does not.
    pub fn local_url(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        let host = self.hostname.trim_end_matches('.');
        format!("{scheme}://{host}:{}/{}", self.port, self.base_path)
    }

    /// Where this VTN's OpenAPI document is, given where the VTN is.
    ///
    /// Derived from [`VtnService::local_url`] rather than written out beside it, so a change of
    /// scheme, hostname, port or base path moves both. Pass it to
    /// [`VtnService::with_openapi_url`]: the field stays optional because a VTN that serves no
    /// document must advertise no key `[Def §Discovery]`, and a key naming a document nothing
    /// serves is a record that lies.
    pub fn openapi_url(&self) -> String {
        format!("{}/openapi.json", self.local_url().trim_end_matches('/'))
    }

    /// The TXT record, in the specification's key order.
    ///
    /// `openapi_url` is omitted when there is none: a key with an empty value is a claim that the
    /// document is at the empty URL, and a browser cannot tell that from a document at all.
    pub fn txt_records(&self) -> Vec<(&'static str, String)> {
        let mut out = vec![
            (txt::VERSION, self.version.clone()),
            (txt::BASE_PATH, self.base_path.clone()),
            (txt::LOCAL_URL, self.local_url()),
            (txt::PROGRAM_NAMES, self.program_names.join(",")),
            (
                txt::REQUIRES_AUTH,
                // `True`/`False`, capitalised, which is what the specification writes.
                if self.requires_auth { "True" } else { "False" }.to_string(),
            ),
        ];
        if let Some(url) = &self.openapi_url {
            out.push((txt::OPENAPI_URL, url.clone()));
        }
        out
    }

    /// The fully qualified instance name, e.g. `My_VTN._openadr3._tcp.local.`.
    pub fn full_name(&self) -> String {
        format!("{}.{SERVICE_TYPE}", self.instance)
    }

    /// Read a service back from a browser's TXT properties.
    ///
    /// Tolerant on purpose: this parses what *another* implementation advertised, and a missing key
    /// is a peer that did not set it rather than a record to discard. Only `local_url` is
    /// load-bearing, and a record without one is useless — it names no VTN to talk to.
    pub fn from_txt(
        instance: impl Into<String>,
        hostname: impl Into<String>,
        port: u16,
        properties: &[(String, String)],
    ) -> Result<Self, DiscoveryError> {
        let get = |key: &str| {
            properties
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.clone())
        };
        let local_url = get(txt::LOCAL_URL).ok_or_else(|| {
            DiscoveryError::Invalid(format!("the record carries no `{}`", txt::LOCAL_URL))
        })?;
        let base_path = get(txt::BASE_PATH).unwrap_or_else(|| {
            // Derive it from the URL rather than guessing the crate's own default, which would
            // silently point a VEN at a path this peer does not serve.
            local_url
                .split_once("://")
                .and_then(|(_, rest)| rest.split_once('/'))
                .map(|(_, path)| path.to_string())
                .unwrap_or_default()
        });
        Ok(Self {
            instance: instance.into(),
            hostname: hostname.into().trim_end_matches('.').to_string(),
            port,
            version: get(txt::VERSION).unwrap_or_else(|| crate::SPEC_VERSION.to_string()),
            base_path,
            tls: local_url.starts_with("https://"),
            program_names: get(txt::PROGRAM_NAMES)
                .map(|v| {
                    v.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToString::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            // Absent reads as "yes": assuming a VTN is open when it did not say so sends a VEN
            // into a loop of `401`s with no idea why.
            requires_auth: get(txt::REQUIRES_AUTH)
                .map(|v| !v.eq_ignore_ascii_case("false"))
                .unwrap_or(true),
            openapi_url: get(txt::OPENAPI_URL).filter(|u| !u.is_empty()),
        })
    }
}

#[cfg(feature = "mdns")]
mod responder;

#[cfg(feature = "mdns")]
#[cfg_attr(docsrs, doc(cfg(feature = "mdns")))]
pub use responder::{Advertisement, discover};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_txt_record_uses_the_specifications_own_keys_and_order() {
        // A record whose key is `basePath` rather than `base_path` is one every other
        // implementation ignores, and it fails by finding nothing — which looks exactly like an
        // empty network.
        let service = VtnService::new("My_VTN", 883)
            .with_hostname("myvtn.local")
            .with_base_path("/openadr3/3.1.0")
            .with_tls()
            .with_program_names(["local"])
            .requiring_auth(false);

        let records = service.txt_records();
        let keys: Vec<&str> = records.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            vec![
                "version",
                "base_path",
                "local_url",
                "program_names",
                "requires_auth"
            ]
        );

        let value = |key: &str| {
            records
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.as_str())
                .unwrap()
        };
        // The specification's own worked example, verbatim.
        assert_eq!(value("version"), "3.1.0");
        assert_eq!(value("base_path"), "openadr3/3.1.0");
        assert_eq!(value("local_url"), "https://myvtn.local:883/openadr3/3.1.0");
        assert_eq!(value("program_names"), "local");
        assert_eq!(value("requires_auth"), "False");
        assert_eq!(service.full_name(), "My_VTN._openadr3._tcp.local.");
    }

    #[test]
    fn the_openapi_url_follows_the_local_url() {
        // Both keys point into the same VTN. Written out separately they drift, and the failure is
        // a VEN fetching a document over http from a VTN that only speaks TLS.
        let service = VtnService::new("vtn", 8443)
            .with_hostname("myvtn.local")
            .with_base_path("/openadr3/3.1.0")
            .with_tls();
        assert_eq!(
            service.openapi_url(),
            "https://myvtn.local:8443/openadr3/3.1.0/openapi.json"
        );

        // And no double slash when the VTN is mounted at the root.
        let root = VtnService::new("vtn", 3000)
            .with_hostname("myvtn.local")
            .with_base_path("");
        assert_eq!(root.openapi_url(), "http://myvtn.local:3000/openapi.json");
    }

    #[test]
    fn an_absent_openapi_url_is_absent_rather_than_empty() {
        let service = VtnService::new("vtn", 3000);
        assert!(
            service
                .txt_records()
                .iter()
                .all(|(k, _)| *k != txt::OPENAPI_URL),
            "an empty value claims the document is at the empty URL"
        );
        let with = service.with_openapi_url("http://vtn.local:3000/openapi.json");
        assert_eq!(
            with.txt_records().last().map(|(k, _)| *k),
            Some(txt::OPENAPI_URL)
        );
    }

    #[test]
    fn a_record_round_trips_through_its_own_txt() {
        let original = VtnService::new("gateway", 3000)
            .with_base_path("openadr3/3.1.0")
            .with_program_names(["tariff", "curtailment"])
            .requiring_auth(true)
            .with_openapi_url("http://gateway.local:3000/openapi.json");
        let properties: Vec<(String, String)> = original
            .txt_records()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();

        let parsed = VtnService::from_txt("gateway", "gateway.local.", 3000, &properties).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn a_record_without_a_local_url_names_no_vtn() {
        let properties = [("version".to_string(), "3.1.0".to_string())];
        assert!(matches!(
            VtnService::from_txt("x", "x.local", 3000, &properties),
            Err(DiscoveryError::Invalid(_))
        ));
    }

    #[test]
    fn an_unstated_requires_auth_is_read_as_yes() {
        // The safe reading. A VEN that assumes a VTN is open sends unauthenticated requests, gets
        // `401`s, and has nothing to tell its operator; one that assumes a credential is needed
        // asks for one.
        let properties = [(
            "local_url".to_string(),
            "http://vtn.local:3000/openadr3/3.1.0".to_string(),
        )];
        let parsed = VtnService::from_txt("vtn", "vtn.local", 3000, &properties).unwrap();
        assert!(parsed.requires_auth);
        // And the base path is taken from the URL rather than from this crate's default, which the
        // peer may not serve.
        assert_eq!(parsed.base_path, "openadr3/3.1.0");
        assert!(!parsed.tls);
    }

    #[test]
    fn a_peers_capitalisation_does_not_decide_whether_it_is_found() {
        let properties = [
            (
                "LOCAL_URL".to_string(),
                "https://peer.local:8443/oadr".to_string(),
            ),
            ("Requires_Auth".to_string(), "FALSE".to_string()),
        ];
        let parsed = VtnService::from_txt("peer", "peer.local", 8443, &properties).unwrap();
        assert!(!parsed.requires_auth);
        assert!(parsed.tls);
        assert_eq!(parsed.base_path, "oadr");
    }
}
