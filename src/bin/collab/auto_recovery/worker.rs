//! Non-blocking background scheduler for tunnel observation, bounded concurrent
//! webhook delivery, Plan B unclaimed reminders, and periodic OAuth sweeping.
use super::super::{App, Failure};
use super::engine;
use super::model::{DeliveryOutcome, DispatchTask, random_hex};
use super::webhook::PermanentDelivery;
use super::{Recovery, get, member, now, online};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;

const TICK_INTERVAL: Duration = Duration::from_secs(5);
const OAUTH_SWEEP_INTERVAL_SECS: u64 = 600;
const MAX_CONCURRENT_DELIVERIES: usize = 8;

pub(super) async fn run(app: App) {
    let Ok(r) = get(&app) else {
        return;
    };
    let mut timer = tokio::time::interval(TICK_INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let delivery_slots = Arc::new(Semaphore::new(MAX_CONCURRENT_DELIVERIES));
    let mut last_sweep = now().unwrap_or_default();

    loop {
        timer.tick().await;
        let Ok(t) = now() else {
            continue;
        };

        if t.saturating_sub(last_sweep) >= OAUTH_SWEEP_INTERVAL_SECS {
            last_sweep = t;
            match r.oauth.sweep_expired().await {
                Ok(removed) if removed > 0 => {
                    tracing::info!(removed, "swept expired OAuth records");
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(?err, "periodic OAuth sweep failed");
                }
            }
        }

        let Ok(ids) = r
            .store
            .access(|s| Ok((s.nodes.keys().cloned().collect::<Vec<_>>(), false)))
            .await
        else {
            continue;
        };

        for node in ids {
            match member(&app, &node).await {
                Err(Failure::Rejected) => {
                    tracing::info!(%node, "revoking recovery for removed project member");
                    let _ = r.revoke(node).await;
                    continue;
                }
                Err(_) => continue,
                Ok(()) => {}
            }

            let up = online(&app, &node);
            let key = node.clone();
            let cfg = r.policy;
            let tick_res = r
                .store
                .access(move |s| {
                    let Some(n) = s.nodes.get_mut(&key) else {
                        return Ok((None, false));
                    };
                    let outcome =
                        engine::tick(n, up, t, &cfg, random_hex).map_err(|_| Failure::Internal)?;
                    Ok((outcome.dispatch, outcome.changed))
                })
                .await;

            if let Ok(Some(task)) = tick_res {
                let Ok(permit) = delivery_slots.clone().try_acquire_owned() else {
                    tracing::warn!(%node, incident_id = %task.incident_id, "webhook delivery pool saturated; will retry on next backoff");
                    continue;
                };
                let app_c = app.clone();
                let r_c = Arc::clone(&r);
                tokio::spawn(async move {
                    let _permit = permit;
                    deliver_one(app_c, r_c, node, task).await;
                });
            }
        }
    }
}

async fn deliver_one(app: App, r: Arc<Recovery>, node: String, task: DispatchTask) {
    if member(&app, &node).await.is_err() || online(&app, &node) {
        return;
    }
    let Ok(t_pre) = now() else {
        return;
    };
    let still_current = {
        let node = node.clone();
        let inc_id = task.incident_id.clone();
        let evt_id = task.event_id.clone();
        let sid = task.subscription.id.clone();
        r.store
            .access(move |s| {
                let ok = s.nodes.get(&node).is_some_and(|n| {
                    !n.paused
                        && n.subscription
                            .as_ref()
                            .is_some_and(|v| v.id == sid && v.expires > t_pre)
                        && n.incident.as_ref().is_some_and(|i| {
                            i.id == inc_id && i.active_event_id == evt_id && i.lease.is_none()
                        })
                });
                Ok((ok, false))
            })
            .await
            .unwrap_or(false)
    };
    if !still_current {
        return;
    }

    let result = r
        .webhook
        .deliver(
            &task.subscription,
            &node,
            &task.incident_id,
            &task.event_id,
            task.event_at,
        )
        .await;

    let outcome = match result {
        Ok(()) => {
            tracing::info!(
                %node,
                incident_id = %task.incident_id,
                event_id = %task.event_id,
                wave = task.wave,
                "recovery wake event acknowledged by webhook receiver"
            );
            DeliveryOutcome::Acknowledged
        }
        Err(err) if err.is::<PermanentDelivery>() => {
            tracing::warn!(
                %node,
                incident_id = %task.incident_id,
                event_id = %task.event_id,
                wave = task.wave,
                "recovery wake webhook permanently refused (410/413)"
            );
            DeliveryOutcome::PermanentRefusal
        }
        Err(err) => {
            tracing::warn!(
                %node,
                incident_id = %task.incident_id,
                event_id = %task.event_id,
                wave = task.wave,
                %err,
                "recovery wake webhook attempt failed"
            );
            DeliveryOutcome::TransientFailure
        }
    };

    let Ok(t_post) = now() else {
        return;
    };
    let cfg = r.policy;
    let inc_id = task.incident_id;
    let evt_id = task.event_id;
    let _ = r
        .store
        .access(move |s| {
            let Some(n) = s.nodes.get_mut(&node) else {
                return Ok(((), false));
            };
            let changed = engine::record_delivery(n, &inc_id, &evt_id, t_post, &cfg, outcome);
            Ok(((), changed))
        })
        .await;
}
