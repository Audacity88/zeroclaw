//! Corporate-friendly secure node transport using standard HTTPS + HMAC-SHA256 authentication.

use std::collections::HashMap;

use anyhow::{Result, bail};
use chrono::Utc;
use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use sha2::Sha256;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

const DEFAULT_REPLAY_CACHE_CAPACITY: usize = 16_384;

/// Signs a request payload with HMAC-SHA256.
/// Authenticates a timestamp and canonical nonce alongside the payload.
pub fn sign_request(
    shared_secret: &str,
    payload: &[u8],
    timestamp: i64,
    nonce: &str,
) -> Result<String> {
    parse_canonical_nonce(nonce)?;
    compute_signature(shared_secret, payload, timestamp, nonce)
}

fn compute_signature(
    shared_secret: &str,
    payload: &[u8],
    timestamp: i64,
    nonce: &str,
) -> Result<String> {
    let mac = request_mac(shared_secret, payload, timestamp, nonce)?;
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn request_mac(
    shared_secret: &str,
    payload: &[u8],
    timestamp: i64,
    nonce: &str,
) -> Result<HmacSha256> {
    let mut mac = HmacSha256::new_from_slice(shared_secret.as_bytes()).map_err(|e| {
        ::zeroclaw_log::record!(
            ERROR,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
            "node transport: HMAC-SHA256 init rejected shared_secret"
        );
        anyhow::Error::msg(format!("HMAC key error: {e}"))
    })?;
    mac.update(&timestamp.to_le_bytes());
    mac.update(nonce.as_bytes());
    mac.update(payload);
    Ok(mac)
}

fn verify_signature(
    shared_secret: &str,
    payload: &[u8],
    timestamp: i64,
    nonce: &str,
    signature: &str,
) -> Result<bool> {
    if signature.len() != 64
        || !signature
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Ok(false);
    }

    let mut provided = [0_u8; 32];
    if hex::decode_to_slice(signature, &mut provided).is_err() {
        return Ok(false);
    }

    let mac = request_mac(shared_secret, payload, timestamp, nonce)?;
    Ok(mac.verify_slice(&provided).is_ok())
}

/// Verify a signed request's timestamp, canonical nonce, and HMAC.
///
/// This helper is stateless. Use [`NodeTransport::verify_incoming`] when replay
/// rejection is required.
pub fn verify_request(
    shared_secret: &str,
    payload: &[u8],
    timestamp: i64,
    nonce: &str,
    signature: &str,
    max_age_secs: i64,
) -> Result<bool> {
    verify_request_at(
        shared_secret,
        payload,
        timestamp,
        nonce,
        signature,
        max_age_secs,
        Utc::now().timestamp(),
    )
}

#[allow(clippy::too_many_arguments)]
fn verify_request_at(
    shared_secret: &str,
    payload: &[u8],
    timestamp: i64,
    nonce: &str,
    signature: &str,
    max_age_secs: i64,
    now: i64,
) -> Result<bool> {
    parse_canonical_nonce(nonce)?;
    verify_canonical_request_at(
        shared_secret,
        payload,
        timestamp,
        nonce,
        signature,
        max_age_secs,
        now,
    )
}

#[allow(clippy::too_many_arguments)]
fn verify_canonical_request_at(
    shared_secret: &str,
    payload: &[u8],
    timestamp: i64,
    nonce: &str,
    signature: &str,
    max_age_secs: i64,
    now: i64,
) -> Result<bool> {
    let max_age_secs = u64::try_from(max_age_secs)
        .map_err(|_| anyhow::anyhow!("Maximum request age must be non-negative"))?;
    if now.abs_diff(timestamp) > max_age_secs {
        bail!("Request timestamp too old or too far in future");
    }

    verify_signature(shared_secret, payload, timestamp, nonce, signature)
}

fn parse_canonical_nonce(nonce: &str) -> Result<Uuid> {
    let parsed = Uuid::parse_str(nonce)
        .map_err(|_| anyhow::anyhow!("Invalid nonce: expected a canonical UUID"))?;
    if parsed.hyphenated().to_string() != nonce {
        bail!("Invalid nonce: expected a lowercase hyphenated UUID");
    }
    Ok(parsed)
}

// ── Node transport client ───────────────────────────────────────

struct ReplayCache {
    live_nonces: HashMap<Uuid, i64>,
    capacity: usize,
    earliest_expiry: Option<i64>,
}

impl ReplayCache {
    fn new(capacity: usize) -> Self {
        Self {
            live_nonces: HashMap::new(),
            capacity,
            earliest_expiry: None,
        }
    }

    fn accept(&mut self, nonce: Uuid, expires_at: i64, now: i64) -> Result<()> {
        if self.earliest_expiry.is_some_and(|expiry| expiry < now) {
            let mut next_expiry: Option<i64> = None;
            self.live_nonces.retain(|_, expiry| {
                if *expiry < now {
                    return false;
                }
                next_expiry = Some(next_expiry.map_or(*expiry, |next| next.min(*expiry)));
                true
            });
            self.earliest_expiry = next_expiry;
        }

        if self.live_nonces.contains_key(&nonce) {
            bail!("Request nonce has already been used");
        }
        if self.live_nonces.len() >= self.capacity {
            bail!("Node replay cache is at capacity");
        }

        self.live_nonces.insert(nonce, expires_at);
        self.earliest_expiry = Some(
            self.earliest_expiry
                .map_or(expires_at, |earliest| earliest.min(expires_at)),
        );
        Ok(())
    }
}

pub struct NodeTransport {
    http: reqwest::Client,
    shared_secret: String,
    max_request_age_secs: i64,
    replay_cache: Mutex<ReplayCache>,
}

impl NodeTransport {
    pub fn new(shared_secret: String) -> Self {
        Self::with_limits(shared_secret, 300, DEFAULT_REPLAY_CACHE_CAPACITY)
    }

    fn with_limits(
        shared_secret: String,
        max_request_age_secs: i64,
        replay_cache_capacity: usize,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("HTTP client build"),
            shared_secret,
            max_request_age_secs,
            replay_cache: Mutex::new(ReplayCache::new(replay_cache_capacity)),
        }
    }

    /// Send an authenticated request to a peer node.
    pub async fn send(
        &self,
        node_address: &str,
        endpoint: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let body = serde_json::to_vec(&payload)?;
        let timestamp = Utc::now().timestamp();
        let nonce = uuid::Uuid::new_v4().to_string();
        let signature = sign_request(&self.shared_secret, &body, timestamp, &nonce)?;

        let url = format!("https://{node_address}/api/node-control/{endpoint}");
        let resp = self
            .http
            .post(&url)
            .header("X-ZeroClaw-Timestamp", timestamp.to_string())
            .header("X-ZeroClaw-Nonce", &nonce)
            .header("X-ZeroClaw-Signature", &signature)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await?;

        if !resp.status().is_success() {
            bail!(
                "Node request failed: {} {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
        }

        Ok(resp.json().await?)
    }

    /// Verify an incoming request and reject a reused nonce.
    pub fn verify_incoming(
        &self,
        payload: &[u8],
        timestamp_header: &str,
        nonce_header: &str,
        signature_header: &str,
    ) -> Result<bool> {
        self.verify_incoming_at(
            payload,
            timestamp_header,
            nonce_header,
            signature_header,
            Utc::now().timestamp(),
        )
    }

    fn verify_incoming_at(
        &self,
        payload: &[u8],
        timestamp_header: &str,
        nonce_header: &str,
        signature_header: &str,
        now: i64,
    ) -> Result<bool> {
        let timestamp: i64 = timestamp_header.parse().map_err(|_| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"header": timestamp_header})),
                "node transport: invalid timestamp header"
            );
            anyhow::Error::msg("Invalid timestamp header")
        })?;
        let nonce = parse_canonical_nonce(nonce_header)?;
        let authenticated = verify_canonical_request_at(
            &self.shared_secret,
            payload,
            timestamp,
            nonce_header,
            signature_header,
            self.max_request_age_secs,
            now,
        )?;
        if !authenticated {
            return Ok(false);
        }

        let expires_at = timestamp.saturating_add(self.max_request_age_secs);
        self.replay_cache.lock().accept(nonce, expires_at, now)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SECRET: &str = "test-shared-secret-key";
    const NONCE_A: &str = "00000000-0000-4000-8000-000000000001";
    const NONCE_B: &str = "00000000-0000-4000-8000-000000000002";
    const NONCE_C: &str = "00000000-0000-4000-8000-000000000003";

    #[test]
    fn sign_request_deterministic() {
        let sig1 = sign_request(TEST_SECRET, b"hello", 1_700_000_000, NONCE_A).unwrap();
        let sig2 = sign_request(TEST_SECRET, b"hello", 1_700_000_000, NONCE_A).unwrap();
        assert_eq!(sig1, sig2, "Same inputs must produce the same signature");
        assert_eq!(
            sig1, "38aa64525ffcc81a26ac62add71310d01a1a420b2aed800efcdd6321415c0322",
            "Canonical sender framing must remain wire-compatible"
        );
    }

    #[test]
    fn verify_request_accepts_valid_signature() {
        let now = Utc::now().timestamp();
        let sig = sign_request(TEST_SECRET, b"payload", now, NONCE_A).unwrap();
        let ok = verify_request(TEST_SECRET, b"payload", now, NONCE_A, &sig, 300).unwrap();
        assert!(ok, "Valid signature must pass verification");
    }

    #[test]
    fn verify_request_rejects_tampered_payload() {
        let now = Utc::now().timestamp();
        let sig = sign_request(TEST_SECRET, b"original", now, NONCE_B).unwrap();
        let ok = verify_request(TEST_SECRET, b"tampered", now, NONCE_B, &sig, 300).unwrap();
        assert!(!ok, "Tampered payload must fail verification");
    }

    #[test]
    fn verify_request_rejects_expired_timestamp() {
        let old = Utc::now().timestamp() - 600;
        let sig = sign_request(TEST_SECRET, b"data", old, NONCE_C).unwrap();
        let result = verify_request(TEST_SECRET, b"data", old, NONCE_C, &sig, 300);
        assert!(result.is_err(), "Expired timestamp must be rejected");
    }

    #[test]
    fn verify_request_rejects_wrong_secret() {
        let now = Utc::now().timestamp();
        let sig = sign_request(TEST_SECRET, b"data", now, NONCE_A).unwrap();
        let ok = verify_request("wrong-secret", b"data", now, NONCE_A, &sig, 300).unwrap();
        assert!(!ok, "Wrong secret must fail verification");
    }

    #[test]
    fn node_transport_construction() {
        let transport = NodeTransport::new("secret-key".into());
        assert_eq!(transport.max_request_age_secs, 300);
    }

    #[test]
    fn node_transport_verify_incoming_valid() {
        let transport = NodeTransport::new(TEST_SECRET.into());
        let now = Utc::now().timestamp();
        let payload = b"test-body";
        let nonce = NONCE_A;
        let sig = sign_request(TEST_SECRET, payload, now, nonce).unwrap();

        let ok = transport
            .verify_incoming(payload, &now.to_string(), nonce, &sig)
            .unwrap();
        assert!(ok, "Valid incoming request must pass verification");
    }

    #[test]
    fn node_transport_verify_incoming_bad_timestamp_header() {
        let transport = NodeTransport::new(TEST_SECRET.into());
        let result = transport.verify_incoming(b"body", "not-a-number", "nonce", "sig");
        assert!(result.is_err(), "Non-numeric timestamp header must error");
    }

    #[test]
    fn sign_request_different_nonce_different_signature() {
        let sig1 = sign_request(TEST_SECRET, b"data", 1_700_000_000, NONCE_A).unwrap();
        let sig2 = sign_request(TEST_SECRET, b"data", 1_700_000_000, NONCE_B).unwrap();
        assert_ne!(
            sig1, sig2,
            "Different nonces must produce different signatures"
        );
    }

    #[test]
    fn node_transport_rejects_replayed_request() {
        let transport = NodeTransport::new(TEST_SECRET.into());
        let now = 1_700_000_000;
        let signature = sign_request(TEST_SECRET, b"payload", now, NONCE_A).unwrap();

        assert!(
            transport
                .verify_incoming_at(b"payload", &now.to_string(), NONCE_A, &signature, now)
                .unwrap()
        );
        let replay = transport
            .verify_incoming_at(b"payload", &now.to_string(), NONCE_A, &signature, now)
            .unwrap_err();
        assert!(replay.to_string().contains("already been used"));
    }

    #[test]
    fn node_transport_accepts_exactly_one_concurrent_request() {
        let transport = NodeTransport::new(TEST_SECRET.into());
        let now = 1_700_000_000;
        let timestamp = now.to_string();
        let signature = sign_request(TEST_SECRET, b"payload", now, NONCE_A).unwrap();
        let barrier = std::sync::Barrier::new(8);

        let outcomes = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        transport
                            .verify_incoming_at(b"payload", &timestamp, NONCE_A, &signature, now)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });

        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Ok(true)))
                .count(),
            1
        );
        assert_eq!(
            outcomes.iter().filter(|outcome| outcome.is_err()).count(),
            7
        );
    }

    #[test]
    fn invalid_signature_does_not_reserve_nonce() {
        let transport = NodeTransport::new(TEST_SECRET.into());
        let now = 1_700_000_000;
        let signature = sign_request(TEST_SECRET, b"payload", now, NONCE_A).unwrap();
        let uppercase_signature = signature.to_uppercase();
        let non_hex_signature = "g".repeat(64);

        assert!(
            !transport
                .verify_incoming_at(b"payload", &now.to_string(), NONCE_A, "bad", now)
                .unwrap()
        );
        assert!(
            !transport
                .verify_incoming_at(
                    b"payload",
                    &now.to_string(),
                    NONCE_A,
                    &uppercase_signature,
                    now,
                )
                .unwrap()
        );
        assert!(
            !transport
                .verify_incoming_at(
                    b"payload",
                    &now.to_string(),
                    NONCE_A,
                    &non_hex_signature,
                    now,
                )
                .unwrap()
        );
        assert!(
            transport
                .verify_incoming_at(b"payload", &now.to_string(), NONCE_A, &signature, now)
                .unwrap()
        );
    }

    #[test]
    fn expired_cache_entry_is_pruned() {
        let transport = NodeTransport::with_limits(TEST_SECRET.into(), 300, 1);
        let first_timestamp = 100;
        let first_signature =
            sign_request(TEST_SECRET, b"first", first_timestamp, NONCE_A).unwrap();
        assert!(
            transport
                .verify_incoming_at(
                    b"first",
                    &first_timestamp.to_string(),
                    NONCE_A,
                    &first_signature,
                    first_timestamp,
                )
                .unwrap()
        );

        let exact_expiry = first_timestamp + 300;
        let replay = transport
            .verify_incoming_at(
                b"first",
                &first_timestamp.to_string(),
                NONCE_A,
                &first_signature,
                exact_expiry,
            )
            .unwrap_err();
        assert!(replay.to_string().contains("already been used"));

        let second_timestamp = 401;
        let second_signature =
            sign_request(TEST_SECRET, b"second", second_timestamp, NONCE_B).unwrap();
        assert!(
            transport
                .verify_incoming_at(
                    b"second",
                    &second_timestamp.to_string(),
                    NONCE_B,
                    &second_signature,
                    second_timestamp,
                )
                .unwrap()
        );
    }

    #[test]
    fn full_live_cache_fails_closed_without_eviction() {
        let transport = NodeTransport::with_limits(TEST_SECRET.into(), 300, 1);
        let now = 1_700_000_000;
        let first_signature = sign_request(TEST_SECRET, b"first", now, NONCE_A).unwrap();
        let second_signature = sign_request(TEST_SECRET, b"second", now, NONCE_B).unwrap();

        assert!(
            transport
                .verify_incoming_at(b"first", &now.to_string(), NONCE_A, &first_signature, now)
                .unwrap()
        );
        let full = transport
            .verify_incoming_at(b"second", &now.to_string(), NONCE_B, &second_signature, now)
            .unwrap_err();
        assert!(full.to_string().contains("at capacity"));

        let replay = transport
            .verify_incoming_at(b"first", &now.to_string(), NONCE_A, &first_signature, now)
            .unwrap_err();
        assert!(replay.to_string().contains("already been used"));
    }

    #[test]
    fn verify_request_rejects_noncanonical_nonce_forms() {
        let now = 1_700_000_000;
        let invalid_nonces = [
            "00000000000040008000000000000001",
            "00000000-0000-4000-8000-00000000000A",
            "{00000000-0000-4000-8000-000000000001}",
            "not-a-uuid",
        ];

        for nonce in invalid_nonces {
            let signature = compute_signature(TEST_SECRET, b"payload", now, nonce).unwrap();
            let result =
                verify_request_at(TEST_SECRET, b"payload", now, nonce, &signature, 300, now);
            assert!(result.is_err(), "nonce should be rejected: {nonce}");
        }
    }

    #[test]
    fn invalid_headers_do_not_reserve_nonce() {
        let transport = NodeTransport::with_limits(TEST_SECRET.into(), 300, 1);
        let now = 1_700_000_000;
        let compact_nonce = "00000000000040008000000000000001";
        let compact_signature =
            compute_signature(TEST_SECRET, b"payload", now, compact_nonce).unwrap();

        assert!(
            transport
                .verify_incoming_at(b"payload", "not-a-number", NONCE_A, "bad", now)
                .is_err()
        );
        assert!(
            transport
                .verify_incoming_at(
                    b"payload",
                    &now.to_string(),
                    compact_nonce,
                    &compact_signature,
                    now,
                )
                .is_err()
        );

        let canonical_signature = sign_request(TEST_SECRET, b"payload", now, NONCE_A).unwrap();
        assert!(
            transport
                .verify_incoming_at(
                    b"payload",
                    &now.to_string(),
                    NONCE_A,
                    &canonical_signature,
                    now,
                )
                .unwrap()
        );
    }

    #[test]
    fn verify_request_preserves_timestamp_boundaries() {
        let now = 1_700_000_000;
        let boundary = now - 300;
        let signature = sign_request(TEST_SECRET, b"payload", boundary, NONCE_A).unwrap();
        assert!(
            verify_request_at(
                TEST_SECRET,
                b"payload",
                boundary,
                NONCE_A,
                &signature,
                300,
                now,
            )
            .unwrap()
        );

        let stale = now - 301;
        let signature = sign_request(TEST_SECRET, b"payload", stale, NONCE_B).unwrap();
        assert!(
            verify_request_at(
                TEST_SECRET,
                b"payload",
                stale,
                NONCE_B,
                &signature,
                300,
                now,
            )
            .is_err()
        );
    }
}
