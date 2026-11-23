//! A downstream application chooses its own provider and trusts its private VTN.
use std::sync::Arc;

use openadr::{
    client::{BusinessLogic, Client, ClientError, VirtualEndNode},
    model::{Interval, ProgramRequest, ReportResource, Timestamp, Value, ValuesMap},
    ven::{DueReport, Meter, VenConfig, VenError, VenRuntime},
    vtn::{Vtn, auth::StaticTokenAuth, store::MemoryStorage, tls::TlsConfig},
};
use serde_json::json;

struct UsageMeter;

#[async_trait::async_trait]
impl Meter for UsageMeter {
    async fn read(&self, due: &DueReport) -> Result<Vec<ReportResource>, VenError> {
        Ok(vec![ReportResource {
            resource_name: "meter".parse().unwrap(),
            interval_period: None,
            intervals: vec![Interval::new(
                due.interval_ids[0],
                vec![ValuesMap::new(
                    due.payload_type.clone(),
                    vec![Value::Integer(42)],
                )],
            )],
        }])
    }
}

#[tokio::test]
async fn aws_lc_ven_reports_over_verified_https() {
    // 032-tls-provider/SR-001. Run in its own binary so no other test installs a provider.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .unwrap();
    let installed = rustls::crypto::CryptoProvider::get_default().unwrap();

    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca = rcgen::CertificateParams::default();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign];
    let ca_pem = ca.self_signed(&ca_key).unwrap().pem();
    let issuer = rcgen::Issuer::new(ca, ca_key);
    let key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".into()])
        .unwrap()
        .signed_by(&key, &issuer)
        .unwrap();
    let tls = TlsConfig::from_pem(leaf.pem().as_bytes(), key.serialize_pem().as_bytes())
        .unwrap()
        .server_config()
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "https://{}{}",
        listener.local_addr().unwrap(),
        openadr::DEFAULT_BASE_PATH
    );
    let vtn = Vtn::builder()
        .storage(MemoryStorage::shared())
        .authenticator(Arc::new(
            StaticTokenAuth::new("https://unused.invalid/token")
                .with_business_logic("bl-token", "bl".parse().unwrap())
                .with_ven("ven-token", "ven".parse().unwrap()),
        ))
        .build();
    let server = tokio::spawn(async move { vtn.serve_tls_on(listener, tls).await.unwrap() });

    assert!(matches!(
        Client::<VirtualEndNode>::builder(&url)
            .unwrap()
            .trust_root_pem(b"no certificate"),
        Err(ClientError::EmptyCaBundle)
    ));
    let untrusted = Client::<VirtualEndNode>::builder(&url)
        .unwrap()
        .build()
        .unwrap();
    assert!(
        matches!(untrusted.events().list().await, Err(ClientError::Transport(e)) if e.is_connect())
    );
    let bl = Client::<BusinessLogic>::builder(&url)
        .unwrap()
        .trust_root_pem(ca_pem.as_bytes())
        .unwrap()
        .bearer_token("bl-token")
        .build()
        .unwrap();
    let ven = Client::<VirtualEndNode>::builder(&url)
        .unwrap()
        .trust_root_pem(ca_pem.as_bytes())
        .unwrap()
        .bearer_token("ven-token")
        .build()
        .unwrap();
    let program = bl
        .programs()
        .create(&ProgramRequest::new("metering".parse().unwrap()))
        .await
        .unwrap();
    // The first interval is ready to report. The second keeps the event in the active poll.
    let start = Timestamp::now()
        .checked_sub(std::time::Duration::from_secs(120))
        .unwrap();
    let request = serde_json::from_value(json!({
        "programID": program.id,
        "intervalPeriod": {"start": start, "duration": "PT1M"},
        "intervals": [
            {"id": 0, "payloads": [{"type": "IMPORT_CAPACITY_LIMIT", "values": [60]}]},
            {"id": 1, "intervalPeriod": {"start": Timestamp::now(), "duration": "PT1H"},
             "payloads": [{"type": "IMPORT_CAPACITY_LIMIT", "values": [60]}]}
        ],
        "reportDescriptors": [{"payloadType": "USAGE", "startInterval": 0, "numIntervals": 1,
                               "frequency": 1, "repeat": 1, "units": "KWH"}]
    }))
    .unwrap();
    let event = bl.events().create(&request).await.unwrap();
    let runtime = VenRuntime::with_meter(ven, VenConfig::new("meter".parse().unwrap()), UsageMeter);
    runtime.register().await.unwrap();
    assert_eq!(
        runtime.sync().await.unwrap().added.as_slice(),
        std::slice::from_ref(&event.id)
    );
    let filed = runtime.submit_due_reports().await.unwrap();
    assert_eq!(filed.len(), 1);
    let stored = bl.reports().get(&filed[0].id).await.unwrap();
    assert_eq!(stored.content.event_id, event.id);
    let values = &stored.content.resources[0].intervals[0].payloads[0];
    assert_eq!(values.value_type.as_str(), "USAGE");
    assert_eq!(values.values, [Value::Integer(42)]);
    assert!(Arc::ptr_eq(
        installed,
        rustls::crypto::CryptoProvider::get_default().unwrap()
    ));
    server.abort();
}
