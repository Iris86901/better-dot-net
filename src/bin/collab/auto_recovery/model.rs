//! Pure domain model and diagnostic enums for Advanced MCP wake recovery.
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use subtle::ConstantTimeEq;

pub(super) const EVENT_NAME: &str = "bdn.recovery_requested";
pub(super) const DEBOUNCE_SECS: u64 = 15;
pub(super) const COOLDOWN_SECS: u64 = 600;
pub(super) const BUDGET_WINDOW_SECS: u64 = 86400;
pub(super) const MAX_CLAIMS_PER_WINDOW: u8 = 144;
pub(super) const LEASE_TTL_SECS: u64 = 900;
pub(super) const PAIRING_TTL_SECS: u64 = 600;
pub(super) const SUB_MIN_LIFETIME_SECS: u64 = 120;
pub(super) const SUB_MAX_LIFETIME_SECS: u64 = 7 * 86400;
pub(super) const MAX_DELIVERY_ATTEMPTS: u8 = 3;
pub(super) const FOLLOWUP_INTERVAL_SECS: u64 = 900;
pub(super) const FOLLOWUP_MAX_REMINDERS: u8 = 2;
pub(super) const FOLLOWUP_LIFETIME_SECS: u64 = 7200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PolicyConfig {
    pub debounce_secs: u64,
    pub cooldown_secs: u64,
    pub budget_window_secs: u64,
    pub max_claims_per_window: u8,
    pub lease_ttl_secs: u64,
    pub max_delivery_attempts: u8,
    pub followup_interval_secs: u64,
    pub followup_max_reminders: u8,
    pub followup_lifetime_secs: u64,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            debounce_secs: DEBOUNCE_SECS,
            cooldown_secs: COOLDOWN_SECS,
            budget_window_secs: BUDGET_WINDOW_SECS,
            max_claims_per_window: MAX_CLAIMS_PER_WINDOW,
            lease_ttl_secs: LEASE_TTL_SECS,
            max_delivery_attempts: MAX_DELIVERY_ATTEMPTS,
            followup_interval_secs: FOLLOWUP_INTERVAL_SECS,
            followup_max_reminders: FOLLOWUP_MAX_REMINDERS,
            followup_lifetime_secs: FOLLOWUP_LIFETIME_SECS,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Ledger {
    pub nodes: BTreeMap<String, Node>,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub(super) struct Binding {
    pub subject: String,
    pub client: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Token {
    pub hash: String,
    pub expires: u64,
}

impl Token {
    pub(super) fn new(raw: &str, expires: u64) -> Self {
        Self {
            hash: sha256_hex(raw),
            expires,
        }
    }

    pub(super) fn matches(&self, raw: &str, now: u64) -> bool {
        self.expires > now && ct_eq(&self.hash, &sha256_hex(raw))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Subscription {
    pub id: String,
    pub expires: u64,
    pub url: String,
    pub key: Vec<u8>,
    #[serde(default)]
    pub verified_at: u64,
    #[serde(default)]
    pub previous: Option<(u64, Vec<u8>)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DeliveryState {
    Pending,
    Delivered,
    PermanentFailure,
    Exhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum FollowupStopReason {
    Active,
    AwaitingInitialDelivery,
    Online,
    Claimed,
    Paused,
    NoActiveSubscription,
    Exhausted,
    Expired,
    DeliveryFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum EligibilityReason {
    Eligible,
    Online,
    Paused,
    NeverOnline,
    NotPaired,
    NoActiveSubscription,
    Debouncing,
    NoIncident,
    AlreadyClaimed,
    Cooldown,
    DailyLimit,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Incident {
    pub id: String,
    pub created_at: u64,
    pub active_event_id: String,
    pub wave: u8,
    pub sends: u8,
    pub next_retry_at: u64,
    pub delivery_state: DeliveryState,
    pub first_delivered_at: Option<u64>,
    pub last_delivered_at: Option<u64>,
    pub lease: Option<Token>,
    pub claimed_at: Option<u64>,
    pub report: Option<bool>,
    pub followup_stop: Option<FollowupStopReason>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Node {
    pub static_client: String,
    pub binding: Option<Binding>,
    #[serde(default)]
    pub paused: bool,
    pub pairing: Option<Token>,
    pub ever_online: bool,
    pub offline_since: Option<u64>,
    pub last_claim: Option<u64>,
    pub attempt_window: u64,
    pub attempts: u8,
    pub incident: Option<Incident>,
    pub subscription: Option<Subscription>,
}

impl Node {
    pub(super) fn new_pending(static_client: String, pairing: Token, online_now: bool) -> Self {
        Self {
            static_client,
            binding: None,
            paused: false,
            pairing: Some(pairing),
            ever_online: online_now,
            offline_since: None,
            last_claim: None,
            attempt_window: 0,
            attempts: 0,
            incident: None,
            subscription: None,
        }
    }

    pub(super) fn active_subscription(&self, now: u64) -> Option<&Subscription> {
        self.subscription.as_ref().filter(|s| s.expires > now)
    }

    pub(super) fn claims_in_window(&self, now: u64, cfg: &PolicyConfig) -> u8 {
        if self.attempts == 0 || now.saturating_sub(self.attempt_window) >= cfg.budget_window_secs {
            0
        } else {
            self.attempts
        }
    }

    pub(super) fn cooldown_until(&self, cfg: &PolicyConfig) -> Option<u64> {
        self.last_claim.map(|t| t.saturating_add(cfg.cooldown_secs))
    }

    pub(super) fn budget_reset_at(&self, now: u64, cfg: &PolicyConfig) -> Option<u64> {
        (self.claims_in_window(now, cfg) >= cfg.max_claims_per_window)
            .then(|| self.attempt_window.saturating_add(cfg.budget_window_secs))
    }

    pub(super) fn allowed_claim(&self, now: u64, cfg: &PolicyConfig) -> bool {
        let cooldown_ok = self.cooldown_until(cfg).is_none_or(|until| now >= until);
        let budget_ok = self.claims_in_window(now, cfg) < cfg.max_claims_per_window;
        cooldown_ok && budget_ok
    }
}

/// Information captured when a node is removed so OAuth cleanup can run outside the ledger lock.
#[derive(Clone, Debug)]
pub(super) struct RevocationTarget {
    pub static_client: Option<String>,
    pub binding: Option<Binding>,
}

#[derive(Clone, Debug)]
pub(super) struct DispatchTask {
    pub subscription: Subscription,
    pub incident_id: String,
    pub event_id: String,
    pub event_at: u64,
    pub wave: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DeliveryOutcome {
    Acknowledged,
    TransientFailure,
    PermanentRefusal,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct FollowupStatus {
    pub enabled: bool,
    pub wave: u8,
    pub reminders_sent: u8,
    pub max_reminders: u8,
    pub interval_secs: u64,
    pub lifetime_secs: u64,
    pub delivery_state: Option<DeliveryState>,
    pub first_delivered_at: Option<u64>,
    pub last_delivered_at: Option<u64>,
    pub next_reminder_at: Option<u64>,
    pub expires_at: Option<u64>,
    pub status: FollowupStopReason,
    pub operator_attention: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct PolicyStatus {
    pub debounce_secs: u64,
    pub cooldown_secs: u64,
    pub window_secs: u64,
    pub max_claims_per_window: u8,
    pub claims_in_window: u8,
    pub lease_ttl_secs: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct StatusSnapshot {
    pub node_id: String,
    pub tunnel_online: bool,
    pub paused: bool,
    pub ever_online: bool,
    pub paired: bool,
    pub subscription_active: bool,
    pub subscription_expires_at: Option<u64>,
    pub offline_since: Option<u64>,
    pub incident_id: Option<String>,
    pub claimable: bool,
    pub eligibility_reason: EligibilityReason,
    pub retry_at: Option<u64>,
    pub delivery_acknowledged: bool,
    pub startup_report: Option<bool>,
    pub policy: PolicyStatus,
    pub followup: FollowupStatus,
}

pub(super) fn random_hex() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("random source unavailable"))?;
    Ok(hex::encode(bytes))
}

pub(super) fn sha256_hex(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}

pub(super) fn ct_eq(a: &str, b: &str) -> bool {
    bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}
