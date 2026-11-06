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

    fn project(&self, project_id: &str) -> Aes256Gcm {
        let mut key = [0u8; 32];
        Hkdf::<Sha256>::new(Some(SALT), &self.0)
            .expand(project_id.as_bytes(), &mut key)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key))
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
                Nonce::from_slice(&nonce),
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
        let plain = self
            .project(project_id)
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: name.as_bytes(),
                },
            )
            .ok()?;
        String::from_utf8(plain).ok()
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
}
