//! An isolated process exercises provider-free startup.
#![cfg(all(feature = "ven", not(feature = "tls-ring")))]

use openadr::client::{Client, ClientError, VirtualEndNode};

#[test]
fn missing_provider_is_an_actionable_builder_error() {
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    let result = Client::<VirtualEndNode>::builder("https://vtn.invalid")
        .unwrap()
        .build();
    let error = result.unwrap_err();
    assert!(matches!(error, ClientError::MissingCryptoProvider(_)));
    assert!(error.to_string().contains("install_default"));
}
