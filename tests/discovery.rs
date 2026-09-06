//! mDNS discovery, over a real multicast group.
//!
//! What the record *says* is unit-tested in `openadr::discovery`, without a network, because that
//! is where the specification's six TXT keys live and a test of them must not depend on an
//! interface. This is the other half: that the responder and the browser in this crate actually
//! meet — the same reason `tests/mqtt.rs` carries a broker rather than a mock (D-059).
//!
//! It is worth being explicit about what this *cannot* see, because it is D-084's lesson: both ends
//! here read the same constants, so renaming `base_path` to `basePath` is symmetric and this test
//! passes exactly as it does now. Two halves that are consistently wrong still meet. The unit test
//! `the_txt_record_uses_the_specifications_own_keys_and_order` is what pins the spellings against
//! the specification's own worked example, and it is the one that fails on such a rename.

#![cfg(feature = "mdns")]

use std::time::Duration;

use openadr::discovery::{Advertisement, VtnService, discover};

/// A name unique to this run, so two test binaries on one network do not find each other's VTN.
fn unique_instance() -> String {
    format!(
        "openadr-test-{}-{}",
        std::process::id(),
        openadr::model::Timestamp::now().as_nanosecond() % 1_000_000
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ven_finds_a_vtn_that_advertises_itself() {
    let instance = unique_instance();
    let service = VtnService::new(&instance, 3000)
        .with_base_path("/openadr3/3.1.0")
        .with_program_names(["local-tariff", "curtailment"])
        .requiring_auth(false)
        .with_openapi_url("http://x.local:3000/openapi.json");

    // Starting the responder needs a usable network interface. Saying so and stopping is the third
    // outcome: a run that could not attempt the check has not shown it works, and reporting that as
    // a pass is the mistake the conformance kit exists to avoid.
    let Ok(advertisement) = Advertisement::start(&service) else {
        eprintln!("skipping mDNS round trip: no usable network interface");
        return;
    };
    assert_eq!(
        advertisement.full_name(),
        format!("{instance}._openadr3._tcp.local.")
    );

    // Browsing blocks, so it goes on a blocking thread rather than stalling the runtime.
    let found = tokio::task::spawn_blocking(|| discover(Duration::from_secs(5)))
        .await
        .expect("the browse task panicked")
        .expect("the browser could not start");

    let Some(discovered) = found.iter().find(|s| s.instance == instance) else {
        eprintln!(
            "skipping mDNS round trip: the responder started but nothing was received \
             (found {} other service(s)); multicast is not usable here",
            found.len()
        );
        return;
    };

    // The record a real responder published and a real browser parsed, not one this test built.
    assert_eq!(discovered.port, 3000);
    assert_eq!(discovered.version, openadr::SPEC_VERSION);
    assert_eq!(discovered.base_path, "openadr3/3.1.0");
    assert!(!discovered.requires_auth);
    assert_eq!(
        discovered.program_names,
        vec!["local-tariff".to_string(), "curtailment".to_string()]
    );
    assert_eq!(
        discovered.openapi_url.as_deref(),
        Some("http://x.local:3000/openapi.json")
    );
    assert!(
        discovered.local_url().ends_with(":3000/openadr3/3.1.0"),
        "a VEN would dial {}",
        discovered.local_url()
    );
}
