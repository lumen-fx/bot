//! Timers and the reconcile sweep. Due jobs become events; every six hours,
//! and once at startup, every open thread of every installed repository is
//! evaluated again, which heals anything a lost webhook missed.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Notify;

use crate::db::{self, NewEvent};
use crate::github::ApiFactory;

pub const SWEEP_EVERY: Duration = Duration::from_secs(6 * 3600);
const TICK: Duration = Duration::from_secs(10);

/// Turn jobs due at `now` into events. Returns how many.
pub async fn fire_due_jobs(
    pool: &PgPool,
    wake: &Notify,
    now: chrono::DateTime<Utc>,
) -> Result<usize> {
    let jobs = db::take_due_jobs(pool, now).await?;
    for j in &jobs {
        let repo = j.metadata["repo"].as_str();
        let number = j.metadata["number"].as_i64();
        db::insert_event(
            pool,
            NewEvent {
                delivery_id: &format!("job:{}", j.id),
                event: "job",
                action: j.metadata["reason"].as_str(),
                repo,
                number,
                installation_id: j.metadata["installation_id"].as_i64(),
                payload: &json!({ "repo": repo, "number": number, "job": j.name }),
            },
        )
        .await?;
    }
    if !jobs.is_empty() {
        wake.notify_waiters();
    }
    Ok(jobs.len())
}

/// Queue a sweep of every repository of every installation.
pub async fn sweep<F: ApiFactory>(pool: &PgPool, factory: &F, wake: &Notify) -> Result<usize> {
    let stamp = Utc::now().timestamp();
    let mut n = 0;
    for (installation, repos) in factory.installations().await? {
        for repo in repos {
            db::insert_event(
                pool,
                NewEvent {
                    delivery_id: &format!("sweep:{stamp}:{repo}"),
                    event: "sweep",
                    action: None,
                    repo: Some(&repo),
                    number: None,
                    installation_id: Some(installation as i64),
                    payload: &json!({ "repo": repo }),
                },
            )
            .await?;
            n += 1;
        }
    }
    db::set_meta(pool, "last_reconcile", &json!(Utc::now().to_rfc3339())).await?;
    wake.notify_waiters();
    Ok(n)
}

pub async fn run<F: ApiFactory>(pool: PgPool, factory: Arc<F>, wake: Arc<Notify>) {
    let mut last_sweep: Option<std::time::Instant> = None;
    let mut last_prune: Option<std::time::Instant> = None;
    loop {
        if let Err(e) = fire_due_jobs(&pool, &wake, Utc::now()).await {
            tracing::error!(error = %e, "timers");
        }
        if last_sweep.is_none_or(|t| t.elapsed() >= SWEEP_EVERY) {
            match sweep(&pool, factory.as_ref(), &wake).await {
                Ok(n) => {
                    tracing::info!(repositories = n, "reconcile sweep queued");
                    last_sweep = Some(std::time::Instant::now());
                }
                Err(e) => {
                    // Try again in five minutes rather than in six hours.
                    tracing::error!(error = %e, "reconcile sweep");
                    last_sweep = std::time::Instant::now()
                        .checked_sub(SWEEP_EVERY - Duration::from_secs(300));
                }
            }
        }
        if last_prune.is_none_or(|t| t.elapsed() >= Duration::from_secs(86_400)) {
            match db::prune(&pool).await {
                Ok(n) => tracing::info!(deleted = n, "old deliveries pruned"),
                Err(e) => tracing::error!(error = %e, "prune"),
            }
            last_prune = Some(std::time::Instant::now());
        }
        tokio::time::sleep(TICK).await;
    }
}
