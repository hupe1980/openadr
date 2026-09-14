//! Encrypting the credentials the VTN holds on somebody else's behalf.
//!
//! A subscription carries a `bearerToken`: the credential the *subscriber* wants presented when the
//! VTN calls their endpoint. The VTN does not own it, cannot derive it, and must be able to send it
//! — so it is encryption, not hashing, and the plaintext has to come back.
//!
//! ## Why field encryption rather than "encrypt the disk"
//!
//! Full-disk encryption is the deployment's job and is not this. It answers one threat — somebody
//! carries the disk away — and none of the four that actually reach a borrowed credential: a
//! database backup, a read replica, a support dump, and a read primitive in some other part of the
//! system. A credential belonging to a third party should not be recoverable from any of them, which
//! is why webhook providers that hold subscriber secrets encrypt the field rather than the volume.
//!
//! ## Where it applies, and where it deliberately does not
//!
//! Two places put this value on disk and both are sealed: the `subscription` row's document, and the
//! `outbox` row's `bearer_token` column, which the fan-out derives from it. [`MemoryStorage`] seals
//! nothing, because "at rest" is the whole claim and an in-memory map is not at rest — sealing it
//! would be ceremony that makes the guarantee *look* uniform while protecting nothing.
//!
//! The webhook **signing key** is not here. It is one value on
//! [`WebhookConfig`](crate::vtn::notify::WebhookConfig), supplied per process by `--webhook-key`,
//! and it never reaches storage at all — so there is nothing at rest to seal.
//!
//! [`MemoryStorage`]: super::MemoryStorage

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};

use super::StorageError;

/// The scheme marker. A stored value that does not start with this is plaintext.
///
/// It is a *version* and not merely a flag: a second scheme is a second prefix, and a reader that
/// meets one it does not know says so instead of returning a credential it decrypted wrongly.
const V1: &str = "enc:v1:";

/// Nonce length for AES-256-GCM, in bytes.
const NONCE: usize = 12;

/// Seals and unseals the credentials a VTN stores for somebody else.
///
/// [`Sealer::plain`] is the identity, for a deployment that has not configured a key and for the
/// in-memory backend. It is not a silent default: `openadr vtn` warns at start-up when a durable
/// backend is storing subscriber credentials unsealed.
#[derive(Clone, Default)]
pub enum Sealer {
    /// Store as given.
    #[default]
    Plain,
    /// Seal with AES-256-GCM.
    Aead(std::sync::Arc<Aead>),
}

impl core::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never render the key, not even its length.
        f.write_str(match self {
            Sealer::Plain => "Sealer::Plain",
            Sealer::Aead(_) => "Sealer::Aead(<key>)",
        })
    }
}

/// An AES-256-GCM key and the randomness that gives each sealing its own nonce.
///
/// No `Debug`: the whole point of [`Sealer`]'s hand-written one is that no derive anywhere can
/// start rendering key material into a log line.
#[allow(missing_debug_implementations)]
pub struct Aead {
    key: LessSafeKey,
    rng: SystemRandom,
}

impl Sealer {
    /// A sealer that stores values as given.
    pub fn plain() -> Self {
        Self::Plain
    }

    /// A sealer built from exactly 32 bytes of key material.
    ///
    /// The key is the operator's to manage. There is no derivation from a passphrase here on
    /// purpose: a key-derivation function has parameters, and parameters that live in this crate
    /// are parameters an operator cannot change when the advice about them changes.
    pub fn with_key(key: &[u8]) -> Result<Self, StorageError> {
        let unbound = UnboundKey::new(&AES_256_GCM, key).map_err(|_| {
            StorageError::Unavailable(format!(
                "a storage encryption key must be exactly {} bytes; got {}",
                AES_256_GCM.key_len(),
                key.len()
            ))
        })?;
        Ok(Self::Aead(std::sync::Arc::new(Aead {
            key: LessSafeKey::new(unbound),
            rng: SystemRandom::new(),
        })))
    }

    /// Whether this sealer actually encrypts.
    pub fn is_sealing(&self) -> bool {
        matches!(self, Sealer::Aead(_))
    }

    /// Seal a value for storage.
    pub fn seal(&self, plaintext: &str) -> Result<String, StorageError> {
        let Sealer::Aead(aead) = self else {
            return Ok(plaintext.to_string());
        };
        let mut nonce = [0u8; NONCE];
        aead.rng
            .fill(&mut nonce)
            .map_err(|_| StorageError::Unavailable("no randomness for a nonce".into()))?;

        let mut buffer = plaintext.as_bytes().to_vec();
        aead.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::empty(),
                &mut buffer,
            )
            .map_err(|_| StorageError::Unavailable("could not seal a stored credential".into()))?;

        let mut out = Vec::with_capacity(NONCE + buffer.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&buffer);
        Ok(format!("{V1}{}", base64(&out)))
    }

    /// Read a stored value back.
    ///
    /// A value with no scheme prefix is returned as it stands. That is deliberate and it is the
    /// only leniency here: it is what lets a deployment turn encryption *on* against a database
    /// that already has rows, without a migration and without every existing subscription becoming
    /// undeliverable. It is not a downgrade risk — anybody who can write the row can already
    /// replace the credential outright.
    ///
    /// A value that *does* carry the prefix must decrypt. A wrong key is an error rather than a
    /// silent pass-through, because the alternative is presenting ciphertext to a subscriber as
    /// though it were their token.
    pub fn unseal(&self, stored: &str) -> Result<String, StorageError> {
        let Some(encoded) = stored.strip_prefix(V1) else {
            return Ok(stored.to_string());
        };
        let Sealer::Aead(aead) = self else {
            return Err(StorageError::Unavailable(
                "this row holds a sealed credential and no storage encryption key is configured"
                    .into(),
            ));
        };
        let raw = unbase64(encoded)
            .ok_or_else(|| StorageError::Unavailable("a sealed credential is not base64".into()))?;
        if raw.len() < NONCE + AES_256_GCM.tag_len() {
            return Err(StorageError::Unavailable(
                "a sealed credential is too short to be one".into(),
            ));
        }
        let (nonce, body) = raw.split_at(NONCE);
        let nonce: [u8; NONCE] = nonce.try_into().expect("split at NONCE");
        let mut buffer = body.to_vec();
        let plaintext = aead
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::empty(),
                &mut buffer,
            )
            .map_err(|_| {
                StorageError::Unavailable(
                    "a sealed credential did not decrypt; the storage encryption key has changed"
                        .into(),
                )
            })?;
        String::from_utf8(plaintext.to_vec())
            .map_err(|_| StorageError::Unavailable("a sealed credential is not UTF-8".into()))
    }

    /// Seal an optional value, which is the shape both call sites have.
    pub fn seal_opt(&self, value: Option<&str>) -> Result<Option<String>, StorageError> {
        value.map(|v| self.seal(v)).transpose()
    }

    /// Unseal an optional value.
    pub fn unseal_opt(&self, value: Option<String>) -> Result<Option<String>, StorageError> {
        value.map(|v| self.unseal(&v)).transpose()
    }
}

fn base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unbase64(s: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealer() -> Sealer {
        Sealer::with_key(&[7u8; 32]).unwrap()
    }

    #[test]
    fn a_sealed_value_round_trips() {
        let s = sealer();
        let sealed = s.seal("super-secret-token").unwrap();
        assert_ne!(sealed, "super-secret-token");
        assert!(sealed.starts_with(V1));
        assert_eq!(s.unseal(&sealed).unwrap(), "super-secret-token");
    }

    #[test]
    fn the_ciphertext_does_not_contain_the_plaintext() {
        let sealed = sealer().seal("super-secret-token").unwrap();
        assert!(
            !sealed.contains("super-secret-token"),
            "the point of the exercise"
        );
    }

    #[test]
    fn sealing_the_same_value_twice_gives_different_ciphertext() {
        // A per-value nonce. Without it, equal tokens are visibly equal in the database, which
        // tells a reader which subscribers share a credential.
        let s = sealer();
        assert_ne!(s.seal("same").unwrap(), s.seal("same").unwrap());
    }

    #[test]
    fn a_wrong_key_is_an_error_rather_than_a_wrong_answer() {
        let sealed = sealer().seal("token").unwrap();
        let other = Sealer::with_key(&[9u8; 32]).unwrap();
        let err = other.unseal(&sealed).unwrap_err();
        assert!(err.to_string().contains("key has changed"));
    }

    #[test]
    fn a_tampered_ciphertext_is_refused() {
        // AEAD: the tag covers the body, so a flipped bit is a refusal and not a corrupted token.
        let s = sealer();
        let sealed = s.seal("token").unwrap();
        let mut raw = unbase64(sealed.strip_prefix(V1).unwrap()).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        let tampered = format!("{V1}{}", base64(&raw));
        assert!(s.unseal(&tampered).is_err());
    }

    #[test]
    fn plaintext_passes_through_both_ways_when_no_key_is_configured() {
        let s = Sealer::plain();
        assert_eq!(s.seal("token").unwrap(), "token");
        assert_eq!(s.unseal("token").unwrap(), "token");
        assert!(!s.is_sealing());
    }

    #[test]
    fn a_row_written_before_a_key_was_configured_still_reads() {
        // Turning encryption on must not make every existing subscription undeliverable.
        assert_eq!(
            sealer().unseal("legacy-plaintext").unwrap(),
            "legacy-plaintext"
        );
    }

    #[test]
    fn a_sealed_row_is_refused_rather_than_returned_when_the_key_is_gone() {
        // The opposite direction: an operator who removes the key must not be handed ciphertext
        // and told it is a token.
        let sealed = sealer().seal("token").unwrap();
        let err = Sealer::plain().unseal(&sealed).unwrap_err();
        assert!(err.to_string().contains("no storage encryption key"));
    }

    #[test]
    fn a_key_of_the_wrong_length_is_refused_by_name() {
        let err = Sealer::with_key(&[0u8; 16]).unwrap_err();
        assert!(err.to_string().contains("exactly 32 bytes"));
    }

    #[test]
    fn the_key_never_appears_in_debug_output() {
        let rendered = format!("{:?}", sealer());
        assert_eq!(rendered, "Sealer::Aead(<key>)");
    }

    #[test]
    fn an_optional_value_seals_and_unseals_as_one() {
        let s = sealer();
        assert_eq!(s.seal_opt(None).unwrap(), None);
        let sealed = s.seal_opt(Some("t")).unwrap();
        assert_eq!(s.unseal_opt(sealed).unwrap(), Some("t".to_string()));
    }
}
