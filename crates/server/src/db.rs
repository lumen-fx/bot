//! Postgres: the delivery log, the per-thread state, the bot's comments and
//! labels, timers, and the audit log.

use std::collections::BTreeSet;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Postgres, Row, pool::PoolConnection};

use lumenfx_engine::snapshot::EntityState;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

pub async fn connect(url: &str) -> Result<PgPool> {
    Ok(PgPoolOptions::new()
        .max_connections(16)
        .connect(url)
        .await?)
}

pub async fn migrate(pool: &PgPool) -> Result<()> {
    MIGRATOR.run(pool).await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub delivery_id: String,
    pub event: String,
    pub action: Option<String>,
    pub repo: Option<String>,
    pub number: Option<i64>,
    pub installation_id: Option<i64>,
    pub payload: Value,
    pub received_at: DateTime<Utc>,
    pub attempts: i32,
}

pub struct NewEvent<'a> {
    pub delivery_id: &'a str,
    pub event: &'a str,
    pub action: Option<&'a str>,
    pub repo: Option<&'a str>,
    pub number: Option<i64>,
    pub installation_id: Option<i64>,
    pub payload: &'a Value,
}

/// Store a delivery. Returns false when the delivery ID was already stored
/// (a redelivery).
pub async fn insert_event(pool: &PgPool, e: NewEvent<'_>) -> Result<bool> {
    let r = sqlx::query(
        "INSERT INTO events (delivery_id, event, action, repo, number, installation_id, payload)
         VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (delivery_id) DO NOTHING",
    )
    .bind(e.delivery_id)
    .bind(e.event)
    .bind(e.action)
    .bind(e.repo)
    .bind(e.number)
    .bind(e.installation_id)
    .bind(e.payload)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Claim the oldest unprocessed event that has settled for `settle_secs`.
pub async fn claim_event(pool: &PgPool, settle_secs: f64) -> Result<Option<EventRow>> {
    let row = sqlx::query(
        "UPDATE events SET locked_until = now() + interval '5 minutes', attempts = attempts + 1
         WHERE delivery_id = (
           SELECT delivery_id FROM events
           WHERE processed_at IS NULL AND next_attempt_at <= now()
             AND (locked_until IS NULL OR locked_until < now())
             AND received_at <= now() - make_interval(secs => $1)
           ORDER BY received_at LIMIT 1 FOR UPDATE SKIP LOCKED)
         RETURNING delivery_id, event, action, repo, number, installation_id, payload, received_at, attempts",
    )
    .bind(settle_secs)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| EventRow {
        delivery_id: r.get("delivery_id"),
        event: r.get("event"),
        action: r.get("action"),
        repo: r.get("repo"),
        number: r.get("number"),
        installation_id: r.get("installation_id"),
        payload: r.get("payload"),
        received_at: r.get("received_at"),
        attempts: r.get("attempts"),
    }))
}

/// Put a claimed event back to be retried after `delay_secs`.
pub async fn release_event(
    pool: &PgPool,
    id: &str,
    delay_secs: f64,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE events SET locked_until = NULL, next_attempt_at = now() + make_interval(secs => $2),
         error = COALESCE($3, error) WHERE delivery_id = $1",
    )
    .bind(id)
    .bind(delay_secs)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_processed(pool: &PgPool, id: &str, error: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE events SET processed_at = now(), locked_until = NULL, error = $2 WHERE delivery_id = $1")
        .bind(id)
        .bind(error)
        .execute(pool)
        .await?;
    Ok(())
}

/// Mark every pending event for a thread received up to `upto` as done: one
/// evaluation from a fresh snapshot answers all of them.
pub async fn coalesce(pool: &PgPool, repo: &str, number: i64, upto: DateTime<Utc>) -> Result<u64> {
    let r = sqlx::query(
        "UPDATE events SET processed_at = now(), locked_until = NULL
         WHERE processed_at IS NULL AND repo = $1 AND number = $2 AND received_at <= $3",
    )
    .bind(repo)
    .bind(number)
    .bind(upto)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

/// A per-thread lock held on its own connection for the whole evaluation.
/// Dropped without `unlock`, the connection is closed rather than returned
/// to the pool, so the session lock can never outlive its holder.
pub struct EntityLock {
    conn: Option<PoolConnection<Postgres>>,
    repo: String,
    number: i64,
}

pub async fn try_lock(pool: &PgPool, repo: &str, number: i64) -> Result<Option<EntityLock>> {
    let mut conn = pool.acquire().await?;
    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext($1), $2::int)")
        .bind(repo)
        .bind(number as i32)
        .fetch_one(&mut *conn)
        .await?;
    Ok(got.then(|| EntityLock {
        conn: Some(conn),
        repo: repo.to_string(),
        number,
    }))
}

impl EntityLock {
    pub async fn unlock(mut self) -> Result<()> {
        if let Some(mut conn) = self.conn.take() {
            sqlx::query("SELECT pg_advisory_unlock(hashtext($1), $2::int)")
                .bind(&self.repo)
                .bind(self.number as i32)
                .execute(&mut *conn)
                .await?;
        }
        Ok(())
    }
}

impl Drop for EntityLock {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // Closing the session releases its advisory locks.
            drop(conn.detach());
        }
    }
}

pub const TRIAGE_KEY: &str = "triage";

pub async fn load_entity(pool: &PgPool, repo: &str, number: i64) -> Result<EntityState> {
    let v: Option<Value> = sqlx::query_scalar(
        "SELECT data FROM entity_data WHERE repo = $1 AND number = $2 AND key = $3",
    )
    .bind(repo)
    .bind(number)
    .bind(TRIAGE_KEY)
    .fetch_optional(pool)
    .await?;
    Ok(v.map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default())
}

pub async fn save_entity(pool: &PgPool, repo: &str, number: i64, st: &EntityState) -> Result<()> {
    put_data(pool, repo, number, TRIAGE_KEY, &serde_json::to_value(st)?).await
}

pub async fn get_data(pool: &PgPool, repo: &str, number: i64, key: &str) -> Result<Option<Value>> {
    Ok(sqlx::query_scalar(
        "SELECT data FROM entity_data WHERE repo = $1 AND number = $2 AND key = $3",
    )
    .bind(repo)
    .bind(number)
    .bind(key)
    .fetch_optional(pool)
    .await?)
}

pub async fn put_data(pool: &PgPool, repo: &str, number: i64, key: &str, v: &Value) -> Result<()> {
    sqlx::query(
        "INSERT INTO entity_data (repo, number, key, data) VALUES ($1, $2, $3, $4)
         ON CONFLICT (repo, number, key) DO UPDATE SET data = EXCLUDED.data, updated_at = now()",
    )
    .bind(repo)
    .bind(number)
    .bind(key)
    .bind(v)
    .execute(pool)
    .await?;
    Ok(())
}

/// Threads whose `blocked #N` names `n`.
pub async fn blocked_on(pool: &PgPool, repo: &str, n: i64) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT number FROM entity_data WHERE repo = $1 AND key = $2 AND (data->>'blocked_by')::bigint = $3",
    )
    .bind(repo)
    .bind(TRIAGE_KEY)
    .bind(n)
    .fetch_all(pool)
    .await?)
}

pub struct CommentRecord {
    pub status_comment_id: Option<u64>,
    pub posted: BTreeSet<(String, String)>,
    pub last_day: u32,
}

pub async fn comments(pool: &PgPool, repo: &str, number: i64) -> Result<CommentRecord> {
    let rows = sqlx::query(
        "SELECT purpose, key, comment_id, created_at FROM bot_comments WHERE repo = $1 AND number = $2",
    )
    .bind(repo)
    .bind(number)
    .fetch_all(pool)
    .await?;
    let day_ago = Utc::now() - chrono::Duration::hours(24);
    let mut rec = CommentRecord {
        status_comment_id: None,
        posted: BTreeSet::new(),
        last_day: 0,
    };
    for r in rows {
        let purpose: String = r.get("purpose");
        let key: String = r.get("key");
        let id: i64 = r.get("comment_id");
        let at: DateTime<Utc> = r.get("created_at");
        if at > day_ago {
            rec.last_day += 1;
        }
        if purpose == "status" {
            rec.status_comment_id = Some(id as u64);
        }
        rec.posted.insert((purpose, key));
    }
    Ok(rec)
}

pub async fn record_comment(
    pool: &PgPool,
    repo: &str,
    number: i64,
    purpose: &str,
    key: &str,
    comment_id: u64,
    body_sha256: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO bot_comments (repo, number, purpose, key, comment_id, body_sha256)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (repo, number, purpose, key) DO UPDATE
           SET comment_id = EXCLUDED.comment_id, body_sha256 = EXCLUDED.body_sha256,
               created_at = now(), updated_at = now()",
    )
    .bind(repo)
    .bind(number)
    .bind(purpose)
    .bind(key)
    .bind(comment_id as i64)
    .bind(body_sha256)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn touch_comment(
    pool: &PgPool,
    repo: &str,
    number: i64,
    purpose: &str,
    body_sha256: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE bot_comments SET body_sha256 = $4, updated_at = now() WHERE repo = $1 AND number = $2 AND purpose = $3",
    )
    .bind(repo)
    .bind(number)
    .bind(purpose)
    .bind(body_sha256)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn owned_label(
    pool: &PgPool,
    repo: &str,
    number: i64,
    label: &str,
    present: bool,
) -> Result<()> {
    if present {
        sqlx::query("INSERT INTO owned_labels (repo, number, label) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
    } else {
        sqlx::query("DELETE FROM owned_labels WHERE repo = $1 AND number = $2 AND label = $3")
    }
    .bind(repo)
    .bind(number)
    .bind(label)
    .execute(pool)
    .await?;
    Ok(())
}

pub struct LogEntry<'a> {
    pub repo: &'a str,
    pub number: i64,
    pub action: &'a str,
    pub target: &'a str,
    pub rule: &'a str,
    pub plan_id: &'a str,
    pub dry_run: bool,
}

pub async fn log_action(pool: &PgPool, e: LogEntry<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO actions_log (repo, number, action, target, rule, plan_id, dry_run)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(e.repo)
    .bind(e.number)
    .bind(e.action)
    .bind(e.target)
    .bind(e.rule)
    .bind(e.plan_id)
    .bind(e.dry_run)
    .execute(pool)
    .await?;
    Ok(())
}

/// Schedule a re-evaluation. The same (name, time) twice is one job.
pub async fn schedule_job(
    pool: &PgPool,
    name: &str,
    at: DateTime<Utc>,
    metadata: &Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO jobs (name, scheduled_at, metadata) VALUES ($1, $2, $3)
         ON CONFLICT (name, scheduled_at) DO NOTHING",
    )
    .bind(name)
    .bind(at)
    .bind(metadata)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug)]
pub struct JobRow {
    pub id: i64,
    pub name: String,
    pub metadata: Value,
}

/// Take every due job, marking it executed.
pub async fn take_due_jobs(pool: &PgPool, now: DateTime<Utc>) -> Result<Vec<JobRow>> {
    let rows = sqlx::query(
        "UPDATE jobs SET executed_at = now() WHERE id IN (
           SELECT id FROM jobs WHERE executed_at IS NULL AND scheduled_at <= $1
           ORDER BY scheduled_at LIMIT 200 FOR UPDATE SKIP LOCKED)
         RETURNING id, name, metadata",
    )
    .bind(now)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| JobRow {
            id: r.get("id"),
            name: r.get("name"),
            metadata: r.get("metadata"),
        })
        .collect())
}

pub struct CachedConfig {
    pub blob_sha: String,
    pub valid: bool,
    pub errors: Vec<String>,
    pub last_good_text: Option<String>,
}

pub async fn config_cache(pool: &PgPool, repo: &str) -> Result<Option<CachedConfig>> {
    let r =
        sqlx::query("SELECT blob_sha, valid, errors, last_good_text FROM configs WHERE repo = $1")
            .bind(repo)
            .fetch_optional(pool)
            .await?;
    Ok(r.map(|r| CachedConfig {
        blob_sha: r.get("blob_sha"),
        valid: r.get("valid"),
        errors: serde_json::from_value(r.get("errors")).unwrap_or_default(),
        last_good_text: r.get("last_good_text"),
    }))
}

pub async fn put_config(
    pool: &PgPool,
    repo: &str,
    blob_sha: &str,
    text: &str,
    errors: &[String],
) -> Result<()> {
    let valid = errors.is_empty();
    sqlx::query(
        "INSERT INTO configs (repo, blob_sha, text, valid, errors, last_good_text, checked_at)
         VALUES ($1, $2, $3, $4, $5, CASE WHEN $4 THEN $3 END, now())
         ON CONFLICT (repo) DO UPDATE SET blob_sha = EXCLUDED.blob_sha, text = EXCLUDED.text,
           valid = EXCLUDED.valid, errors = EXCLUDED.errors, checked_at = now(),
           last_good_text = CASE WHEN EXCLUDED.valid THEN EXCLUDED.text ELSE configs.last_good_text END",
    )
    .bind(repo)
    .bind(blob_sha)
    .bind(text)
    .bind(valid)
    .bind(serde_json::to_value(errors)?)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn forget_config(pool: &PgPool, repo: &str) -> Result<()> {
    sqlx::query("DELETE FROM configs WHERE repo = $1")
        .bind(repo)
        .execute(pool)
        .await?;
    Ok(())
}

/// Newest `synchronize` delivery for a pull request: when the bot last saw a
/// push.
pub async fn last_push_seen(
    pool: &PgPool,
    repo: &str,
    number: i64,
) -> Result<Option<DateTime<Utc>>> {
    Ok(sqlx::query_scalar(
        "SELECT max(received_at) FROM events
         WHERE repo = $1 AND number = $2 AND event = 'pull_request' AND action = 'synchronize'",
    )
    .bind(repo)
    .bind(number)
    .fetch_one(pool)
    .await?)
}

pub async fn set_meta(pool: &PgPool, key: &str, v: &Value) -> Result<()> {
    sqlx::query(
        "INSERT INTO meta (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
    )
    .bind(key)
    .bind(v)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_meta(pool: &PgPool, key: &str) -> Result<Option<Value>> {
    Ok(sqlx::query_scalar("SELECT value FROM meta WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await?)
}

/// Delivery payloads are kept 30 days.
pub async fn prune(pool: &PgPool) -> Result<u64> {
    let r = sqlx::query("DELETE FROM events WHERE processed_at IS NOT NULL AND received_at < now() - interval '30 days'")
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

pub struct Stats {
    pub pending: i64,
    pub failed: i64,
    pub last_processed: Option<DateTime<Utc>>,
    pub last_received: Option<DateTime<Utc>>,
    pub jobs_pending: i64,
    pub configs: Vec<(String, bool, Vec<String>)>,
}

pub async fn stats(pool: &PgPool) -> Result<Stats> {
    let r = sqlx::query(
        "SELECT
           (SELECT count(*) FROM events WHERE processed_at IS NULL) AS pending,
           (SELECT count(*) FROM events WHERE processed_at IS NULL AND attempts >= 5) AS failed,
           (SELECT max(processed_at) FROM events) AS last_processed,
           (SELECT max(received_at) FROM events) AS last_received,
           (SELECT count(*) FROM jobs WHERE executed_at IS NULL) AS jobs_pending",
    )
    .fetch_one(pool)
    .await?;
    let configs = sqlx::query("SELECT repo, valid, errors FROM configs ORDER BY repo")
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| {
            (
                r.get::<String, _>("repo"),
                r.get::<bool, _>("valid"),
                serde_json::from_value(r.get("errors")).unwrap_or_default(),
            )
        })
        .collect();
    Ok(Stats {
        pending: r.get("pending"),
        failed: r.get("failed"),
        last_processed: r.get("last_processed"),
        last_received: r.get("last_received"),
        jobs_pending: r.get("jobs_pending"),
        configs,
    })
}
