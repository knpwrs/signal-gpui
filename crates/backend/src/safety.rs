//! Safety numbers, as Signal Desktop computes them (`ts/util/safetyNumber`):
//! libsignal's fingerprint over both ACIs and identity keys, version 2,
//! 5200 iterations. Both sides see the same 60 digits; the QR code holds
//! the scannable fingerprint, which the other side's app compares with its
//! own.

use anyhow::anyhow;
use presage::libsignal_service::protocol::{Aci, Fingerprint, IdentityKey};

const ITERATIONS: u32 = 5200;
/// Service-id (ACI) fingerprints.
const VERSION: u32 = 2;

/// The 60 digits and the QR code's bytes.
pub(crate) fn compute(ours: (Aci, &IdentityKey), theirs: (Aci, &IdentityKey)) -> anyhow::Result<(String, Vec<u8>)> {
    // An ACI's binary form is its UUID's 16 bytes, as Desktop's `uuidToBytes`.
    let ours_id = ours.0.service_id_binary();
    let theirs_id = theirs.0.service_id_binary();
    let fingerprint = Fingerprint::new(VERSION, ITERATIONS, &ours_id, ours.1, &theirs_id, theirs.1)
        .map_err(|e| anyhow!("{e}"))?;
    let digits = fingerprint.display_string().map_err(|e| anyhow!("{e}"))?;
    let qr = fingerprint.scannable.serialize().map_err(|e| anyhow!("{e}"))?;
    Ok((digits, qr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::protocol::IdentityKeyPair;

    #[test]
    fn both_sides_see_the_same_number() {
        let mut rng = rand::rng();
        let (alice, bob) = (Aci::from_uuid_bytes([0xa1; 16]), Aci::from_uuid_bytes([0xb0; 16]));
        let (alice_key, bob_key) = (IdentityKeyPair::generate(&mut rng), IdentityKeyPair::generate(&mut rng));
        let (a, a_qr) = compute((alice, alice_key.identity_key()), (bob, bob_key.identity_key())).unwrap();
        let (b, b_qr) = compute((bob, bob_key.identity_key()), (alice, alice_key.identity_key())).unwrap();
        assert_eq!(a.len(), 60);
        assert!(a.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(a, b);
        // Each side's QR is its own view; scanning compares them.
        let scanned = presage::libsignal_service::protocol::ScannableFingerprint::deserialize(&b_qr).unwrap();
        let mine = presage::libsignal_service::protocol::ScannableFingerprint::deserialize(&a_qr).unwrap();
        assert!(mine.compare(&b_qr).unwrap());
        assert!(scanned.compare(&a_qr).unwrap());
        // A new key changes it.
        let other = IdentityKeyPair::generate(&mut rng);
        let (c, _) = compute((alice, alice_key.identity_key()), (bob, other.identity_key())).unwrap();
        assert_ne!(a, c);
    }
}
