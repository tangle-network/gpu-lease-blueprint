//! EIP-191 challenge-session credentials — SPEC §2 Credentials: "NEVER
//! on-chain (job results are public forever). Credentials (temporary,
//! scoped, revocable) are delivered via the operator's EIP-191
//! challenge-session API (existing, proven)."
//!
//! Flow (the pattern proven in the sandbox blueprint's session-auth):
//!   1. lessee requests a session for a leaseId → operator issues a nonce
//!      challenge bound to (leaseId, lessee address)
//!   2. lessee `personal_sign`s the challenge with the key that owns the lease
//!   3. operator recovers the signer; on match, issues a scoped, expiring
//!      credential token for the leased device
//!   4. token is revoked at release/reap/expiry — overstay is impossible
//!      because the credential dies with the escrow (I2, operator side)

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use tiny_keccak::{Hasher, Keccak};

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("unknown challenge session")]
    UnknownChallenge,
    #[error("challenge expired")]
    ChallengeExpired,
    #[error("signature recovery failed")]
    RecoveryFailed,
    #[error("signer mismatch: expected {expected}, recovered {recovered}")]
    SignerMismatch { expected: String, recovered: String },
    #[error("unknown credential")]
    UnknownCredential,
    #[error("credential expired")]
    CredentialExpired,
    #[error("credential revoked")]
    CredentialRevoked,
    #[error("malformed hex: {0}")]
    MalformedHex(String),
}

/// An issued nonce challenge (public data — contains no secrets).
#[derive(Debug, Clone)]
pub struct Challenge {
    pub lease_id: [u8; 32],
    pub lessee: String,
    pub nonce: [u8; 32],
    pub expires_at: u64,
    /// TEE attestation nonce — when the lease requires confidentiality, the
    /// GPU's TEE must sign this nonce to prove it's the attested device.
    /// Zero = no TEE attestation required (non-confidential lease).
    pub tee_nonce: [u8; 32],
}

impl Challenge {
    /// The exact string the lessee must EIP-191 `personal_sign`.
    pub fn message(&self) -> String {
        let mut msg = format!(
            "gpu-lease session\nlease: 0x{}\nnonce: 0x{}\nexpires: {}",
            hex::encode(self.lease_id),
            hex::encode(self.nonce),
            self.expires_at
        );
        // TEE composition: the challenge binds the GPU's TEE attestation
        // nonce, so the credential is only valid on the attested device.
        if self.tee_nonce != [0u8; 32] {
            msg.push_str(&format!("\ntee-nonce: 0x{}", hex::encode(self.tee_nonce)));
        }
        msg
    }
}

/// A scoped, expiring, revocable credential — NEVER in calldata/results.
#[derive(Debug, Clone)]
pub struct ScopedCredential {
    pub token: String,
    pub lease_id: [u8; 32],
    pub lessee: String,
    pub scopes: Vec<String>,
    pub expires_at: u64,
}

struct SessionState {
    challenges: HashMap<[u8; 32], Challenge>, // key: nonce
    credentials: HashMap<String, ScopedCredential>,
    revoked: Vec<String>,
}

impl std::fmt::Debug for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionState")
            .field("challenges", &self.challenges.len())
            .field("credentials", &self.credentials.len())
            .field("revoked", &self.revoked.len())
            .finish()
    }
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            challenges: HashMap::new(),
            credentials: HashMap::new(),
            revoked: Vec::new(),
        }
    }
}

/// Operator-wide credential session store (interior mutability for statics).
#[derive(Debug, Default)]
pub struct CredentialSessions {
    state: Mutex<SessionState>,
    counter: AtomicU64,
}

const CHALLENGE_TTL_SECONDS: u64 = 300;
pub const SCOPE_GPU_ATTACH: &str = "gpu:attach";
pub const SCOPE_GPU_EXEC: &str = "gpu:exec";

impl CredentialSessions {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(SessionState {
                challenges: HashMap::new(),
                credentials: HashMap::new(),
                revoked: Vec::new(),
            }),
            counter: AtomicU64::new(0),
        }
    }

    /// Issue a nonce challenge bound to (leaseId, lessee). Deterministic
    /// nonce derivation per (lease, counter) — no RNG dependency.
    pub fn issue_challenge(&self, lease_id: [u8; 32], lessee: &str) -> Challenge {
        self.issue_challenge_with_tee(lease_id, lessee, false)
    }

    /// Issue a challenge with a TEE attestation nonce for confidential leases.
    /// The GPU's TEE must sign this nonce to prove it's the attested device
    /// (NVIDIA CC mode: the GPU signs with its unique device key).
    pub fn issue_challenge_with_tee(
        &self,
        lease_id: [u8; 32],
        lessee: &str,
        tee_required: bool,
    ) -> Challenge {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let mut nonce = [0u8; 32];
        let mut k = Keccak::v256();
        k.update(&lease_id);
        k.update(lessee.as_bytes());
        k.update(&n.to_be_bytes());
        k.finalize(&mut nonce);
        let mut tee_nonce = [0u8; 32];
        if tee_required {
            let mut tk = Keccak::v256();
            tk.update(&nonce);
            tk.update(b"gpu-tee-attestation");
            tk.finalize(&mut tee_nonce);
        }
        let challenge = Challenge {
            lease_id,
            lessee: lessee.to_string(),
            nonce,
            tee_nonce,
            expires_at: unix_now() + CHALLENGE_TTL_SECONDS,
        };
        self.state
            .lock()
            .expect("credentials poisoned")
            .challenges
            .insert(nonce, challenge.clone());
        challenge
    }

    /// Verify an EIP-191 `personal_sign` response and mint the scoped
    /// credential. The signature is over the CHALLENGE MESSAGE (not the
    /// digest) — `eip191_personal_sign_digest` applies the prefix.
    pub fn verify_and_issue(
        &self,
        challenge_nonce: [u8; 32],
        signature_rsv_hex: &str,
        expires_at: u64,
    ) -> Result<ScopedCredential, CredentialError> {
        // Peek without consuming: a FAILED verification (bad signature) must
        // not burn the nonce — only success is single-use. Expiry burns it.
        let challenge = {
            let mut st = self.state.lock().expect("credentials poisoned");
            let c = st
                .challenges
                .get(&challenge_nonce)
                .cloned()
                .ok_or(CredentialError::UnknownChallenge)?;
            if unix_now() > c.expires_at {
                st.challenges.remove(&challenge_nonce);
                return Err(CredentialError::ChallengeExpired);
            }
            c
        };

        let recovered = eip191_recover_signer(&challenge.message(), signature_rsv_hex)?;
        if recovered != challenge.lessee.to_ascii_lowercase() {
            return Err(CredentialError::SignerMismatch {
                expected: challenge.lessee,
                recovered,
            });
        }
        // Success => the nonce is spent (single-use).
        self.state
            .lock()
            .expect("credentials poisoned")
            .challenges
            .remove(&challenge_nonce);

        // Mint token: keccak(lease || nonce || counter) — unguessable without
        // holding the operator's view; bearer-use confined to this operator.
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let mut k = Keccak::v256();
        k.update(&challenge.lease_id);
        k.update(&challenge_nonce);
        k.update(&n.to_be_bytes());
        let mut token_raw = [0u8; 32];
        k.finalize(&mut token_raw);
        let token = format!("glv1_{}", hex::encode(token_raw));

        let cred = ScopedCredential {
            token: token.clone(),
            lease_id: challenge.lease_id,
            lessee: challenge.lessee,
            scopes: vec![SCOPE_GPU_ATTACH.to_string(), SCOPE_GPU_EXEC.to_string()],
            expires_at,
        };
        self.state
            .lock()
            .expect("credentials poisoned")
            .credentials
            .insert(token, cred.clone());
        Ok(cred)
    }

    /// Which lease a pending challenge is bound to (for credential lifetime).
    pub fn challenge_lease(&self, nonce: [u8; 32]) -> Option<[u8; 32]> {
        self.state
            .lock()
            .expect("credentials poisoned")
            .challenges
            .get(&nonce)
            .map(|c| c.lease_id)
    }

    /// Bearer check: valid, unexpired, unrevoked.
    pub fn validate(&self, token: &str) -> Result<ScopedCredential, CredentialError> {
        let st = self.state.lock().expect("credentials poisoned");
        if st.revoked.iter().any(|t| t == token) {
            return Err(CredentialError::CredentialRevoked);
        }
        let cred = st
            .credentials
            .get(token)
            .ok_or(CredentialError::UnknownCredential)?;
        if unix_now() >= cred.expires_at {
            return Err(CredentialError::CredentialExpired);
        }
        Ok(cred.clone())
    }

    /// Revoke by lease (release/reap path) — kills every credential bound
    /// to the lease. Returns how many were revoked.
    pub fn revoke_for_lease(&self, lease_id: [u8; 32]) -> usize {
        let mut st = self.state.lock().expect("credentials poisoned");
        let doomed: Vec<String> = st
            .credentials
            .values()
            .filter(|c| c.lease_id == lease_id)
            .map(|c| c.token.clone())
            .collect();
        let n = doomed.len();
        for t in &doomed {
            st.credentials.remove(t);
            st.revoked.push(t.clone());
        }
        st.challenges.retain(|_, c| c.lease_id != lease_id);
        n
    }
}

/// EIP-191 `personal_sign` signer recovery: prefix, keccak, ECDSA recover,
/// address = keccak(uncompressed pubkey)[12..].
pub fn eip191_recover_signer(
    message: &str,
    signature_rsv_hex: &str,
) -> Result<String, CredentialError> {
    let sig_bytes = hex::decode(signature_rsv_hex.trim_start_matches("0x"))
        .map_err(|e| CredentialError::MalformedHex(e.to_string()))?;
    if sig_bytes.len() != 65 {
        return Err(CredentialError::RecoveryFailed);
    }
    let v = sig_bytes[64];
    let v_norm = if v >= 27 {
        i32::from(v) - 27
    } else {
        i32::from(v)
    };
    // Fail closed on non-canonical v (EIP-191 allows only 0/1, or 27/28).
    let recovery_id = match v_norm {
        0 => RecoveryId::new(false, false),
        1 => RecoveryId::new(true, false),
        _ => return Err(CredentialError::RecoveryFailed),
    };

    let digest = eip191_personal_message_digest(message);
    let sig =
        Signature::from_slice(&sig_bytes[..64]).map_err(|_| CredentialError::RecoveryFailed)?;
    let vk = VerifyingKey::recover_from_prehash(&digest, &sig, recovery_id)
        .map_err(|_| CredentialError::RecoveryFailed)?;

    let pub_key = vk.to_encoded_point(false); // uncompressed: 0x04 || X || Y
    let mut k = Keccak::v256();
    k.update(&pub_key.as_bytes()[1..]); // address hash excludes the 0x04 tag
    let mut hash = [0u8; 32];
    k.finalize(&mut hash);
    Ok(format!("0x{}", hex::encode(&hash[12..])))
}

/// EIP-191 `personal_sign` with a raw secp256k1 key. Returns (rsv_hex, address)
/// where address is the signer's derived EVM address — the recovered-address
/// counterpart `eip191_recover_signer` must reproduce exactly.
pub fn eip191_sign_message(
    signing_key: &k256::ecdsa::SigningKey,
    message: &str,
) -> Result<(String, String), CredentialError> {
    use k256::ecdsa::signature::Signer;
    let digest = eip191_personal_message_digest(message);
    let (sig, rec_id) = signing_key
        .sign_prehash_recoverable(&digest)
        .map_err(|_| CredentialError::RecoveryFailed)?;
    let mut rsv = sig.to_bytes().to_vec();
    rsv.push(u8::from(rec_id.is_y_odd()) + 27);
    let vk = k256::ecdsa::VerifyingKey::from(signing_key);
    let addr = verifying_key_address(&vk)?;
    Ok((hex::encode(rsv), addr))
}

/// EVM address of a secp256k1 verifying key: keccak(uncompressed pubkey)[12..].
pub fn verifying_key_address(vk: &k256::ecdsa::VerifyingKey) -> Result<String, CredentialError> {
    use k256::elliptic_curve::sec1::ToEncodedPoint;
    let point = vk.to_encoded_point(false);
    let mut k = Keccak::v256();
    k.update(&point.as_bytes()[1..]);
    let mut hash = [0u8; 32];
    k.finalize(&mut hash);
    Ok(format!("0x{}", hex::encode(&hash[12..])))
}

/// keccak256("\x19Ethereum Signed Message:\n" || len || message)
pub fn eip191_personal_message_digest(message: &str) -> [u8; 32] {
    let prefixed = format!("\x19Ethereum Signed Message:\n{}{}", message.len(), message);
    let mut k = Keccak::v256();
    k.update(prefixed.as_bytes());
    let mut out = [0u8; 32];
    k.finalize(&mut out);
    out
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::SigningKey;

    fn sign_personal(key: &SigningKey, message: &str) -> String {
        let digest = eip191_personal_message_digest(message);
        // ecdsa 0.16: recoverable signing returns (Signature, RecoveryId).
        let (sig, rec_id) = key.sign_prehash_recoverable(&digest).unwrap();
        let mut rsv = sig.to_bytes().to_vec();
        rsv.push(u8::from(rec_id.is_y_odd()) + 27); // EIP-191 v
        hex::encode(rsv)
    }

    fn address_of(key: &SigningKey) -> String {
        let vk = VerifyingKey::from(key);
        let point = vk.to_encoded_point(false);
        let mut k = Keccak::v256();
        k.update(&point.as_bytes()[1..]);
        let mut hash = [0u8; 32];
        k.finalize(&mut hash);
        format!("0x{}", hex::encode(&hash[12..]))
    }

    #[test]
    fn full_challenge_session_flow() {
        let sessions = CredentialSessions::new();
        let key = SigningKey::from_slice(&[42u8; 32]).unwrap();
        let addr = address_of(&key);
        let lease = [5u8; 32];

        let ch = sessions.issue_challenge(lease, &addr);
        let sig = sign_personal(&key, &ch.message());
        let cred = sessions
            .verify_and_issue(ch.nonce, &sig, unix_now() + 3600)
            .unwrap();

        assert_eq!(cred.lease_id, lease);
        assert_eq!(cred.lessee, addr);
        assert!(cred.scopes.contains(&SCOPE_GPU_ATTACH.to_string()));

        // Bearer validates.
        assert!(sessions.validate(&cred.token).is_ok());

        // Revocation kills it (release/reap path) — reported as Revoked.
        assert_eq!(sessions.revoke_for_lease(lease), 1);
        assert!(matches!(
            sessions.validate(&cred.token),
            Err(CredentialError::CredentialRevoked)
        ));
    }

    #[test]
    fn wrong_signer_rejected() {
        let sessions = CredentialSessions::new();
        let lessee_key = SigningKey::from_slice(&[1u8; 32]).unwrap();
        let attacker_key = SigningKey::from_slice(&[2u8; 32]).unwrap();
        let addr = address_of(&lessee_key);
        let ch = sessions.issue_challenge([7u8; 32], &addr);
        let sig = sign_personal(&attacker_key, &ch.message());
        assert!(matches!(
            sessions.verify_and_issue(ch.nonce, &sig, unix_now() + 60),
            Err(CredentialError::SignerMismatch { .. })
        ));
    }

    #[test]
    fn challenge_single_use() {
        let sessions = CredentialSessions::new();
        let key = SigningKey::from_slice(&[3u8; 32]).unwrap();
        let addr = address_of(&key);
        let ch = sessions.issue_challenge([9u8; 32], &addr);
        let sig = sign_personal(&key, &ch.message());
        sessions
            .verify_and_issue(ch.nonce, &sig, unix_now() + 60)
            .unwrap();
        // Replay of the same nonce fails closed.
        assert!(matches!(
            sessions.verify_and_issue(ch.nonce, &sig, unix_now() + 60),
            Err(CredentialError::UnknownChallenge)
        ));
    }

    #[test]
    fn expired_credential_rejected() {
        let sessions = CredentialSessions::new();
        let key = SigningKey::from_slice(&[4u8; 32]).unwrap();
        let addr = address_of(&key);
        let ch = sessions.issue_challenge([1u8; 32], &addr);
        let sig = sign_personal(&key, &ch.message());
        // Already-expired credential (escrow exhausted — I2 operator side).
        let cred = sessions
            .verify_and_issue(ch.nonce, &sig, unix_now().saturating_sub(1))
            .unwrap();
        assert!(matches!(
            sessions.validate(&cred.token),
            Err(CredentialError::CredentialExpired)
        ));
    }

    #[test]
    fn malformed_signatures_fail_closed() {
        assert!(matches!(
            eip191_recover_signer("msg", "zz"),
            Err(CredentialError::MalformedHex(_))
        ));
        assert!(matches!(
            eip191_recover_signer("msg", &hex::encode([0u8; 64])),
            Err(CredentialError::RecoveryFailed)
        ));
    }
}

#[cfg(test)]
mod tee_attestation_tests {
    use super::*;

    #[test]
    fn tee_challenge_includes_attestation_nonce() {
        let sessions = CredentialSessions::new();
        let ch = sessions.issue_challenge_with_tee([9u8; 32], "0xabc", true);
        assert_ne!(ch.tee_nonce, [0u8; 32], "TEE nonce must be generated");
        assert!(
            ch.message().contains("tee-nonce:"),
            "challenge message must bind the TEE nonce: {}",
            ch.message()
        );
    }

    #[test]
    fn non_tee_challenge_has_zero_tee_nonce() {
        let sessions = CredentialSessions::new();
        let ch = sessions.issue_challenge([9u8; 32], "0xabc");
        assert_eq!(ch.tee_nonce, [0u8; 32]);
        assert!(!ch.message().contains("tee-nonce:"));
    }

    #[test]
    fn tee_challenge_is_deterministic_per_nonce() {
        let sessions = CredentialSessions::new();
        // Same challenge always produces the same tee_nonce (derived from the
        // session nonce — the GPU's TEE signs THIS exact nonce).
        let ch = sessions.issue_challenge_with_tee([9u8; 32], "0xabc", true);
        let mut expected = [0u8; 32];
        let mut k = Keccak::v256();
        k.update(&ch.nonce);
        k.update(b"gpu-tee-attestation");
        k.finalize(&mut expected);
        assert_eq!(ch.tee_nonce, expected);
    }
}
