//! Encrypts project secrets at rest. The ledger only ever stores what [`SealingKey::seal`]
//! returns; the value is opened again only by the delivery consumer, just before sending.
//!
//! One master key (the `SECRETS_KEY` Worker secret, 32 bytes as hex) derives a key per
//! project with HKDF-SHA256, so one project's ciphertext can't be opened as another's. Each
//! value is AES-256-GCM sealed with a random nonce, and the secret's name is the associated
//! data, so a value can't be swapped onto another name either.
//!
//! ```text
//! sealed = "v1:" base64(nonce[12] || ciphertext || tag[16])
//! ```
//!
//! While the key is being rotated, a [`Keyring`] also holds the previous key
//! (`SECRETS_KEY_PREVIOUS`), which opens but never seals. The format names no key: AES-GCM
//! authenticates, so trying the current key and then the previous one is exact. See
//! `docs/adr/0001-rotating-secrets-key.md`.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hkdf::Hkdf;
use sha2::Sha256;

const PREFIX: &str = "v1:";
const SALT: &[u8] = b"runlater/secrets/v1";
pub const NONCE_BYTES: usize = 12;

pub struct SealingKey([u8; 32]);

impl SealingKey {
    /// From the hex-encoded 32-byte master key.
    pub fn parse(hex_key: &str) -> Option<Self> {
        hex::decode(hex_key.trim()).ok()?.try_into().ok().map(Self)
    }

    fn fingerprint(&self) -> String {
        let mut out = [0u8; 8];
        Hkdf::<Sha256>::new(Some(SALT), &self.0)
            .expand(b"fingerprint", &mut out)
            .expect("8 bytes is a valid HKDF-SHA256 output length");
        hex::encode(out)
    }

    fn project(&self, project_id: &str) -> Aes256Gcm {
        let mut key = Key::<Aes256Gcm>::default();
        Hkdf::<Sha256>::new(Some(SALT), &self.0)
            .expand(project_id.as_bytes(), &mut key)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Aes256Gcm::new(&key)
    }

    /// `nonce` must be fresh random bytes for every call.
    pub fn seal(
        &self,
        project_id: &str,
        name: &str,
        value: &str,
        nonce: [u8; NONCE_BYTES],
    ) -> String {
        let sealed = self
            .project(project_id)
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: value.as_bytes(),
                    aad: name.as_bytes(),
                },
            )
            .expect("AES-GCM sealing does not fail for short messages");
        let mut out = nonce.to_vec();
        out.extend_from_slice(&sealed);
        format!("{PREFIX}{}", STANDARD.encode(out))
    }

    /// None if it was sealed for another project or name, under another key, or tampered with.
    pub fn open(&self, project_id: &str, name: &str, sealed: &str) -> Option<String> {
        let raw = STANDARD.decode(sealed.strip_prefix(PREFIX)?).ok()?;
        if raw.len() < NONCE_BYTES {
            return None;
        }
        let (nonce, ciphertext) = raw.split_at(NONCE_BYTES);
        let nonce = Nonce::try_from(nonce).ok()?;
        let plain = self
            .project(project_id)
            .decrypt(
                &nonce,
                Payload {
                    msg: ciphertext,
                    aad: name.as_bytes(),
                },
            )
            .ok()?;
        String::from_utf8(plain).ok()
    }
}

/// The current key, and during a rotation the previous one.
pub struct Keyring {
    current: SealingKey,
    previous: Option<SealingKey>,
}

/// What resealing one value found.
#[derive(Debug, PartialEq, Eq)]
pub enum Resealed {
    /// The current key opens it; nothing to do.
    Current,
    /// Only the previous key opened it; here it is sealed again under the current key.
    Again(String),
    /// No key opens it.
    Unreadable,
}

impl Keyring {
    pub fn new(current: SealingKey, previous: Option<SealingKey>) -> Self {
        Self { current, previous }
    }

    /// While a previous key is set, a name for this pair of keys, so the gateway's sweep can
    /// tell one rotation (or an undo) from the next. Derived one-way from the keys; it reveals
    /// nothing about them. None when there is no previous key.
    pub fn rotation(&self) -> Option<String> {
        let previous = self.previous.as_ref()?;
        Some(format!(
            "{}{}",
            self.current.fingerprint(),
            previous.fingerprint()
        ))
    }

    /// Always under the current key. `nonce` must be fresh random bytes for every call.
    pub fn seal(
        &self,
        project_id: &str,
        name: &str,
        value: &str,
        nonce: [u8; NONCE_BYTES],
    ) -> String {
        self.current.seal(project_id, name, value, nonce)
    }

    /// Under the current key, or failing that the previous one.
    pub fn open(&self, project_id: &str, name: &str, sealed: &str) -> Option<String> {
        self.current
            .open(project_id, name, sealed)
            .or_else(|| self.previous.as_ref()?.open(project_id, name, sealed))
    }

    /// Seals again under the current key a value only the previous key opens.
    pub fn reseal(
        &self,
        project_id: &str,
        name: &str,
        sealed: &str,
        nonce: [u8; NONCE_BYTES],
    ) -> Resealed {
        if self.current.open(project_id, name, sealed).is_some() {
            return Resealed::Current;
        }
        match self
            .previous
            .as_ref()
            .and_then(|p| p.open(project_id, name, sealed))
        {
            Some(value) => Resealed::Again(self.current.seal(project_id, name, &value, nonce)),
            None => Resealed::Unreadable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    #[test]
    fn seals_and_opens_only_for_the_same_project_name_and_key() {
        let key = SealingKey::parse(KEY).unwrap();
        let sealed = key.seal("prj_a", "GH", "ghp_secret", [7; NONCE_BYTES]);
        assert!(sealed.starts_with("v1:"));
        assert!(!sealed.contains("ghp_secret"));
        assert_eq!(
            key.open("prj_a", "GH", &sealed).as_deref(),
            Some("ghp_secret")
        );
        assert_eq!(key.open("prj_b", "GH", &sealed), None, "another project");
        assert_eq!(key.open("prj_a", "OTHER", &sealed), None, "another name");
        let other = SealingKey::parse(&KEY.replace("00", "ff")).unwrap();
        assert_eq!(other.open("prj_a", "GH", &sealed), None, "another key");

        let mut raw = STANDARD.decode(&sealed[3..]).unwrap();
        *raw.last_mut().unwrap() ^= 1;
        let tampered = format!("v1:{}", STANDARD.encode(raw));
        assert_eq!(key.open("prj_a", "GH", &tampered), None);
        assert_eq!(key.open("prj_a", "GH", "v1:"), None);
        assert_eq!(key.open("prj_a", "GH", "garbage"), None);
    }

    /// Secrets already in the ledger must still open after any crypto upgrade. Sealed by the
    /// code as it first shipped, and opened separately with Node's crypto to confirm the format.
    #[test]
    fn opens_a_value_sealed_by_the_released_format() {
        const SEALED: &str = "v1:AQIDBAUGBwgJCgsMdZDKCgeVMo35Ijj3xxEI/F9rHzcusZa9J0LJ09IBuqT4";
        const PROJECT: &str = "prj_0123456789abcdef01234567";
        let key = SealingKey::parse(KEY).unwrap();
        assert_eq!(
            key.open(PROJECT, "GITHUB_TOKEN", SEALED).as_deref(),
            Some("ghp_example-value")
        );
        let nonce = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        assert_eq!(
            key.seal(PROJECT, "GITHUB_TOKEN", "ghp_example-value", nonce),
            SEALED
        );
    }

    #[test]
    fn fresh_nonces_give_different_ciphertexts() {
        let key = SealingKey::parse(KEY).unwrap();
        let a = key.seal("p", "N", "same", [1; NONCE_BYTES]);
        let b = key.seal("p", "N", "same", [2; NONCE_BYTES]);
        assert_ne!(a, b);
    }

    #[test]
    fn needs_a_32_byte_hex_key() {
        assert!(SealingKey::parse("abcd").is_none());
        assert!(SealingKey::parse("zz").is_none());
        assert!(SealingKey::parse(&format!(" {KEY}\n")).is_some());
    }

    /// A nonce that differs on every call and every run, as a real one must; the std hasher's
    /// per-process random seed is enough for a test.
    fn fresh_nonce() -> [u8; NONCE_BYTES] {
        use std::hash::{BuildHasher, RandomState};
        let state = RandomState::new();
        let mut nonce = [0u8; NONCE_BYTES];
        nonce[..8].copy_from_slice(&state.hash_one(0u8).to_le_bytes());
        nonce[8..].copy_from_slice(&state.hash_one(1u8).to_le_bytes()[..4]);
        nonce
    }

    #[test]
    fn a_keyring_opens_under_either_key_and_reseals_under_the_current_one() {
        let old = || SealingKey::parse(KEY).unwrap();
        let new = || SealingKey::parse(&KEY.replace("00", "ee")).unwrap();
        let sealed_old = old().seal("p", "N", "v", fresh_nonce());
        let sealed_new = new().seal("p", "N", "v", fresh_nonce());

        let rotating = Keyring::new(new(), Some(old()));
        let rotation = rotating.rotation().unwrap();
        assert_eq!(rotation.len(), 32);
        assert_ne!(
            Keyring::new(old(), Some(new())).rotation().unwrap(),
            rotation,
            "an undo is a new rotation"
        );
        assert!(!rotation.contains(&KEY[..16]));
        assert_eq!(rotating.open("p", "N", &sealed_old).as_deref(), Some("v"));
        assert_eq!(rotating.open("p", "N", &sealed_new).as_deref(), Some("v"));
        assert_eq!(
            rotating.reseal("p", "N", &sealed_new, fresh_nonce()),
            Resealed::Current
        );
        let Resealed::Again(again) = rotating.reseal("p", "N", &sealed_old, fresh_nonce()) else {
            panic!("expected a reseal");
        };
        assert_eq!(
            new().open("p", "N", &again).as_deref(),
            Some("v"),
            "now under the new key"
        );
        assert_eq!(
            rotating.reseal("q", "N", &sealed_old, fresh_nonce()),
            Resealed::Unreadable
        );
        assert_eq!(
            new()
                .open("p", "N", &rotating.seal("p", "N", "w", fresh_nonce()))
                .as_deref(),
            Some("w"),
            "seals under the new key"
        );

        let settled = Keyring::new(new(), None);
        assert_eq!(settled.rotation(), None);
        assert_eq!(settled.open("p", "N", &sealed_old), None, "old key dropped");
        assert_eq!(
            settled.reseal("p", "N", &sealed_old, fresh_nonce()),
            Resealed::Unreadable
        );
    }
}
