//! The socket half: an mDNS responder and a browser.
//!
//! Deliberately thin. Everything that decides *what* is advertised is in
//! [`VtnService`](super::VtnService), which has no network in it and is tested exhaustively; this
//! puts that on a multicast group and takes it off again. A test of a record's contents must not
//! need a network interface, and a test of this module needs one CI will not reliably have.

use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use super::{DiscoveryError, SERVICE_TYPE, VtnService};

fn unavailable(e: impl core::fmt::Display) -> DiscoveryError {
    DiscoveryError::Unavailable(e.to_string())
}

/// A live mDNS advertisement. Dropping it withdraws the service.
///
/// Withdrawal matters more than it sounds: an mDNS record outlives the process that published it
/// for as long as its TTL, so a VTN that stops without unregistering leaves every VEN on the site
/// dialling a port that is no longer open.
pub struct Advertisement {
    daemon: ServiceDaemon,
    full_name: String,
}

impl core::fmt::Debug for Advertisement {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Advertisement")
            .field("full_name", &self.full_name)
            .finish_non_exhaustive()
    }
}

impl Advertisement {
    /// Announce a VTN on the local network.
    ///
    /// Returns as soon as the responder is running; the announcement itself is asynchronous, which
    /// is what mDNS is.
    pub fn start(service: &VtnService) -> Result<Self, DiscoveryError> {
        if service.instance.is_empty() {
            return Err(DiscoveryError::Invalid(
                "a DNS-SD instance name cannot be empty".into(),
            ));
        }
        let daemon = ServiceDaemon::new().map_err(unavailable)?;
        let hostname = if service.hostname.ends_with('.') {
            service.hostname.clone()
        } else {
            format!("{}.", service.hostname)
        };
        let properties: Vec<(String, String)> = service
            .txt_records()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();

        // An empty address list plus `enable_addr_auto` asks the daemon to publish this host's own
        // addresses, which is where the A/AAAA records the specification asks for come from.
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &service.instance,
            &hostname,
            (),
            service.port,
            &properties[..],
        )
        .map_err(|e| DiscoveryError::Invalid(e.to_string()))?
        .enable_addr_auto();

        let full_name = info.get_fullname().to_string();
        daemon.register(info).map_err(unavailable)?;
        tracing::info!(
            service = %full_name,
            url = %service.local_url(),
            "advertising this VTN over mDNS"
        );
        Ok(Self { daemon, full_name })
    }

    /// The fully qualified instance name being advertised.
    pub fn full_name(&self) -> &str {
        &self.full_name
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        // Best effort: the daemon is going away anyway, and a failure here has nobody to tell.
        let _ = self.daemon.unregister(&self.full_name);
        let _ = self.daemon.shutdown();
    }
}

/// Browse for local VTNs for `timeout`, returning what answered.
///
/// The VEN's half `[Def §Discovery]`. It returns *every* VTN that answered rather than picking one:
/// which to enrol with is a decision about the site, and a library that chose would be choosing for
/// an operator who can see the room and cannot see this code.
///
/// An empty result is not an error. A site with no local VTN is the ordinary case for a VEN
/// configured with a cloud URL.
pub fn discover(timeout: Duration) -> Result<Vec<VtnService>, DiscoveryError> {
    let daemon = ServiceDaemon::new().map_err(unavailable)?;
    let receiver = daemon.browse(SERVICE_TYPE).map_err(unavailable)?;
    let deadline = std::time::Instant::now() + timeout;

    let mut found: Vec<VtnService> = Vec::new();
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        let Ok(event) = receiver.recv_timeout(remaining) else {
            break;
        };
        let ServiceEvent::ServiceResolved(info) = event else {
            continue;
        };
        let properties: Vec<(String, String)> = info
            .get_properties()
            .iter()
            .map(|p| (p.key().to_string(), p.val_str().to_string()))
            .collect();
        let instance = info
            .get_fullname()
            .split_once('.')
            .map(|(instance, _)| instance)
            .unwrap_or_else(|| info.get_fullname());
        // A record this crate cannot read is a peer's problem, not a reason to abandon the browse:
        // one malformed advertisement must not hide every other VTN on the network.
        match VtnService::from_txt(instance, info.get_hostname(), info.get_port(), &properties) {
            Ok(service) => {
                if !found.iter().any(|s| s.full_name() == service.full_name()) {
                    found.push(service);
                }
            }
            Err(e) => tracing::debug!(
                service = info.get_fullname(),
                error = %e,
                "ignoring an mDNS record that names no VTN"
            ),
        }
    }
    let _ = daemon.shutdown();
    Ok(found)
}
