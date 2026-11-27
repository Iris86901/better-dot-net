//! Pure, deterministic, I/O-free recovery state machine.
//!
//! Every transition takes explicit `(now, online, cfg)` inputs so complex timing
//! sequences (debounce, backoff retries, follow-up reminders, cooldown, 24h budget,
//! reconnect, pause/resume) can be verified deterministically without clocks or sockets.
use super::model::{
    DeliveryOutcome, DeliveryState, DispatchTask, EligibilityReason, FollowupStatus,
    FollowupStopReason, Incident, Node, PolicyConfig, PolicyStatus, StatusSnapshot, Token,
};
use anyhow::Result;

#[derive(Debug)]
pub(super) struct TickOutcome {
    pub changed: bool,
    pub dispatch: Option<DispatchTask>,
}

#[derive(Debug)]
pub(super) struct ClaimGrant {
    pub incident_id: String,
    pub expires_in: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReportOutcome {
    Recorded,
    AlreadyRecordedSame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReportError {
    InvalidOrExpiredLease,
    ConflictingOutcome,
}

/// Evaluate the current node state without mutating anything.
/// Shared by MCP `get_recovery_status`, CLI `/v1/recovery/status`, and `claim()`.
pub(super) fn evaluate(
    node_id: &str,
    node: &Node,
    online: bool,
    now: u64,
    cfg: &PolicyConfig,
) -> StatusSnapshot {
    let sub = node.active_subscription(now);
    let claims_in_window = node.claims_in_window(now, cfg);
    let cooldown_until = node.cooldown_until(cfg).filter(|&u| now < u);
    let budget_until = node.budget_reset_at(now, cfg).filter(|&u| now < u);

    let (eligibility_reason, retry_at) = if online {
        (EligibilityReason::Online, None)
    } else if node.paused {
        (EligibilityReason::Paused, None)
    } else if !node.ever_online {
        (EligibilityReason::NeverOnline, None)
    } else if node.binding.is_none() {
        (EligibilityReason::NotPaired, None)
    } else if sub.is_none() {
        (EligibilityReason::NoActiveSubscription, None)
    } else if let Some(incident) = &node.incident {
        if incident.lease.is_some() {
            (EligibilityReason::AlreadyClaimed, None)
        } else if let Some(until) = budget_until {
            let max_until = cooldown_until.map_or(until, |c| c.max(until));
            (EligibilityReason::DailyLimit, Some(max_until))
        } else if let Some(until) = cooldown_until {
            (EligibilityReason::Cooldown, Some(until))
        } else {
            (EligibilityReason::Eligible, None)
        }
    } else if let Some(started) = node.offline_since {
        if now.saturating_sub(started) < cfg.debounce_secs {
            (
                EligibilityReason::Debouncing,
                Some(started.saturating_add(cfg.debounce_secs)),
            )
        } else if let Some(until) = budget_until {
            let max_until = cooldown_until.map_or(until, |c| c.max(until));
            (EligibilityReason::DailyLimit, Some(max_until))
        } else if let Some(until) = cooldown_until {
            (EligibilityReason::Cooldown, Some(until))
        } else {
            (EligibilityReason::NoIncident, None)
        }
    } else {
        (EligibilityReason::NoIncident, None)
    };

    let incident = node.incident.as_ref();
    let followup = evaluate_followup(node, online, now, cfg);

    StatusSnapshot {
        node_id: node_id.to_owned(),
        tunnel_online: online,
        paused: node.paused,
        ever_online: node.ever_online,
        paired: node.binding.is_some(),
        subscription_active: sub.is_some(),
        subscription_expires_at: node.subscription.as_ref().map(|s| s.expires),
        offline_since: node.offline_since,
        incident_id: incident.map(|i| i.id.clone()),
        claimable: eligibility_reason == EligibilityReason::Eligible,
        eligibility_reason,
        retry_at,
        delivery_acknowledged: incident.is_some_and(|i| i.first_delivered_at.is_some()),
        startup_report: incident.and_then(|i| i.report),
        policy: PolicyStatus {
            debounce_secs: cfg.debounce_secs,
            cooldown_secs: cfg.cooldown_secs,
            window_secs: cfg.budget_window_secs,
            max_claims_per_window: cfg.max_claims_per_window,
            claims_in_window,
            lease_ttl_secs: cfg.lease_ttl_secs,
        },
        followup,
    }
}

fn evaluate_followup(node: &Node, online: bool, now: u64, cfg: &PolicyConfig) -> FollowupStatus {
    let enabled = cfg.followup_max_reminders > 0 && cfg.followup_interval_secs > 0;
    let Some(inc) = node.incident.as_ref() else {
        return FollowupStatus {
            enabled,
            wave: 0,
            reminders_sent: 0,
            max_reminders: cfg.followup_max_reminders,
            interval_secs: cfg.followup_interval_secs,
            lifetime_secs: cfg.followup_lifetime_secs,
            delivery_state: None,
            first_delivered_at: None,
            last_delivered_at: None,
            next_reminder_at: None,
            expires_at: None,
            status: if online {
                FollowupStopReason::Online
            } else if node.paused {
                FollowupStopReason::Paused
            } else {
                FollowupStopReason::AwaitingInitialDelivery
            },
            operator_attention: false,
        };
    };

    let expires_at = inc
        .first_delivered_at
        .map(|t| t.saturating_add(cfg.followup_lifetime_secs));
    let mut next_reminder_at = None;

    let status = if online {
        FollowupStopReason::Online
    } else if inc.lease.is_some() {
        FollowupStopReason::Claimed
    } else if node.paused {
        FollowupStopReason::Paused
    } else if let Some(sticky) = inc.followup_stop {
        sticky
    } else if matches!(
        inc.delivery_state,
        DeliveryState::PermanentFailure | DeliveryState::Exhausted
    ) {
        FollowupStopReason::DeliveryFailed
    } else if node.active_subscription(now).is_none() {
        FollowupStopReason::NoActiveSubscription
    } else if inc.first_delivered_at.is_none() || inc.delivery_state == DeliveryState::Pending {
        FollowupStopReason::AwaitingInitialDelivery
    } else if let (Some(exp), Some(last)) = (expires_at, inc.last_delivered_at) {
        if now >= exp {
            FollowupStopReason::Expired
        } else if !enabled || inc.wave >= cfg.followup_max_reminders {
            if now >= last.saturating_add(cfg.followup_interval_secs) {
                FollowupStopReason::Exhausted
            } else {
                FollowupStopReason::Active
            }
        } else {
            let next = last.saturating_add(cfg.followup_interval_secs);
            if next >= exp {
                FollowupStopReason::Expired
            } else {
                next_reminder_at = Some(next);
                FollowupStopReason::Active
            }
        }
    } else {
        FollowupStopReason::AwaitingInitialDelivery
    };

    let operator_attention = matches!(
        status,
        FollowupStopReason::Exhausted
            | FollowupStopReason::Expired
            | FollowupStopReason::DeliveryFailed
    );

    FollowupStatus {
        enabled,
        wave: inc.wave,
        reminders_sent: inc.wave,
        max_reminders: cfg.followup_max_reminders,
        interval_secs: cfg.followup_interval_secs,
        lifetime_secs: cfg.followup_lifetime_secs,
        delivery_state: Some(inc.delivery_state),
        first_delivered_at: inc.first_delivered_at,
        last_delivered_at: inc.last_delivered_at,
        next_reminder_at,
        expires_at,
        status,
        operator_attention,
    }
}

/// Advance the node state machine by one observation tick and optionally schedule one webhook send.
pub(super) fn tick<F>(
    node: &mut Node,
    online: bool,
    now: u64,
    cfg: &PolicyConfig,
    mut gen_id: F,
) -> Result<TickOutcome>
where
    F: FnMut() -> Result<String>,
{
    let mut changed = false;

    if online {
        if !node.ever_online || node.offline_since.is_some() {
            changed = true;
        }
        node.ever_online = true;
        node.offline_since = None;
        // Keep an outstanding claimed lease so Dot can still call report_recovery after reconnect.
        if node.incident.as_ref().is_some_and(|i| i.lease.is_none()) {
            node.incident = None;
            changed = true;
        }
        return Ok(TickOutcome {
            changed,
            dispatch: None,
        });
    }

    if node.paused || !node.ever_online {
        return Ok(TickOutcome {
            changed: false,
            dispatch: None,
        });
    }

    if node.offline_since.is_none() {
        node.offline_since = Some(now);
        node.incident = None;
        return Ok(TickOutcome {
            changed: true,
            dispatch: None,
        });
    }

    let started = node.offline_since.unwrap_or(now);
    if node.incident.is_none()
        && now.saturating_sub(started) >= cfg.debounce_secs
        && node.binding.is_some()
        && node.active_subscription(now).is_some()
        && node.allowed_claim(now, cfg)
    {
        let id = gen_id()?;
        node.incident = Some(Incident {
            active_event_id: id.clone(),
            id,
            created_at: now,
            wave: 0,
            sends: 0,
            next_retry_at: now,
            delivery_state: DeliveryState::Pending,
            first_delivered_at: None,
            last_delivered_at: None,
            lease: None,
            claimed_at: None,
            report: None,
            followup_stop: None,
        });
        changed = true;
    }

    let Some(sub) = node.active_subscription(now).cloned() else {
        return Ok(TickOutcome {
            changed,
            dispatch: None,
        });
    };

    let allowed_claim = node.allowed_claim(now, cfg);
    let Some(inc) = node.incident.as_mut() else {
        return Ok(TickOutcome {
            changed,
            dispatch: None,
        });
    };

    if inc.lease.is_some() {
        return Ok(TickOutcome {
            changed,
            dispatch: None,
        });
    }

    // Plan B: Bounded unclaimed follow-up reminders.
    // Only advance after the current wave received an explicit HTTP 2xx receipt.
    if inc.delivery_state == DeliveryState::Delivered
        && inc.followup_stop.is_none()
        && cfg.followup_max_reminders > 0
        && cfg.followup_interval_secs > 0
        && let (Some(anchor), Some(last)) = (inc.first_delivered_at, inc.last_delivered_at)
    {
        let expires_at = anchor.saturating_add(cfg.followup_lifetime_secs);
        let next_due = last.saturating_add(cfg.followup_interval_secs);
        if now >= expires_at {
            inc.followup_stop = Some(FollowupStopReason::Expired);
            changed = true;
        } else if inc.wave >= cfg.followup_max_reminders {
            if now >= next_due {
                inc.followup_stop = Some(FollowupStopReason::Exhausted);
                changed = true;
            }
        } else if next_due >= expires_at {
            inc.followup_stop = Some(FollowupStopReason::Expired);
            changed = true;
        } else if now >= next_due && allowed_claim {
            inc.wave += 1;
            inc.active_event_id = format!("evt_{}", gen_id()?);
            inc.sends = 0;
            inc.next_retry_at = now;
            inc.delivery_state = DeliveryState::Pending;
            changed = true;
        }
    }

    if inc.delivery_state == DeliveryState::Pending
        && inc.sends < cfg.max_delivery_attempts
        && inc.next_retry_at <= now
    {
        inc.sends += 1;
        inc.next_retry_at = now.saturating_add(30u64 << (inc.sends - 1));
        changed = true;
        return Ok(TickOutcome {
            changed,
            dispatch: Some(DispatchTask {
                subscription: sub,
                incident_id: inc.id.clone(),
                event_id: inc.active_event_id.clone(),
                event_at: now,
                wave: inc.wave,
            }),
        });
    }

    Ok(TickOutcome {
        changed,
        dispatch: None,
    })
}

/// Apply the result of one outbound webhook delivery attempt.
pub(super) fn record_delivery(
    node: &mut Node,
    incident_id: &str,
    event_id: &str,
    now: u64,
    cfg: &PolicyConfig,
    outcome: DeliveryOutcome,
) -> bool {
    let Some(inc) = node
        .incident
        .as_mut()
        .filter(|i| i.id == incident_id && i.active_event_id == event_id)
    else {
        return false;
    };

    match outcome {
        DeliveryOutcome::Acknowledged => {
            inc.delivery_state = DeliveryState::Delivered;
            if inc.first_delivered_at.is_none() {
                inc.first_delivered_at = Some(now);
            }
            inc.last_delivered_at = Some(now);
            true
        }
        DeliveryOutcome::PermanentRefusal => {
            inc.delivery_state = DeliveryState::PermanentFailure;
            inc.sends = cfg.max_delivery_attempts;
            inc.followup_stop = Some(FollowupStopReason::DeliveryFailed);
            true
        }
        DeliveryOutcome::TransientFailure => {
            if inc.sends >= cfg.max_delivery_attempts {
                inc.delivery_state = DeliveryState::Exhausted;
                inc.followup_stop = Some(FollowupStopReason::DeliveryFailed);
                true
            } else {
                false
            }
        }
    }
}

/// Atomically claim an eligible offline incident.
pub(super) fn claim(
    node_id: &str,
    node: &mut Node,
    incident_id: &str,
    online: bool,
    now: u64,
    cfg: &PolicyConfig,
    raw_lease: &str,
) -> std::result::Result<ClaimGrant, EligibilityReason> {
    let status = evaluate(node_id, node, online, now, cfg);
    if !status.claimable {
        return Err(status.eligibility_reason);
    }
    let Some(inc) = node.incident.as_mut().filter(|i| i.id == incident_id) else {
        return Err(EligibilityReason::NoIncident);
    };
    inc.lease = Some(Token::new(
        raw_lease,
        now.saturating_add(cfg.lease_ttl_secs),
    ));
    inc.claimed_at = Some(now);
    if node.attempts == 0 || now.saturating_sub(node.attempt_window) >= cfg.budget_window_secs {
        node.attempt_window = now;
        node.attempts = 0;
    }
    node.attempts += 1;
    node.last_claim = Some(now);
    Ok(ClaimGrant {
        incident_id: inc.id.clone(),
        expires_in: cfg.lease_ttl_secs,
    })
}

/// Record the sanitized local startup report for a claimed lease.
pub(super) fn report(
    node: &mut Node,
    incident_id: &str,
    lease: &str,
    success: bool,
    now: u64,
) -> std::result::Result<ReportOutcome, ReportError> {
    let Some(inc) = node.incident.as_mut().filter(|i| {
        i.id == incident_id && i.lease.as_ref().is_some_and(|tok| tok.matches(lease, now))
    }) else {
        return Err(ReportError::InvalidOrExpiredLease);
    };
    if let Some(prior) = inc.report {
        if prior != success {
            return Err(ReportError::ConflictingOutcome);
        }
        return Ok(ReportOutcome::AlreadyRecordedSame);
    }
    inc.report = Some(success);
    Ok(ReportOutcome::Recorded)
}

/// Pause or resume recovery without breaking OAuth pairing or subscriptions.
pub(super) fn set_paused(node: &mut Node, paused: bool) -> bool {
    if node.paused == paused {
        return false;
    }
    node.paused = paused;
    node.offline_since = None;
    node.incident = None;
    true
}
