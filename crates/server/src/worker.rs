//! Workers: take stored events in order, serialize per thread with a
//! Postgres advisory lock, and run snapshot -> evaluate -> execute.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::Notify;

use lumenfx_engine::plan::Plan;
use lumenfx_engine::snapshot::BotIdentity;

use crate::cla;
use crate::db::{self, EventRow, NewEvent};
use crate::executor::Executor;
use crate::github::{Api, ApiFactory};
use crate::snapshot::{Builder, COVERAGE_CHECK, load_config};

pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(Utc::now)
}

/// Attempts before an event is given up on and left with its error.
pub const MAX_ATTEMPTS: i32 = 5;

pub struct Worker<F: ApiFactory> {
    pub pool: PgPool,
    pub factory: Arc<F>,
    pub bot: BotIdentity,
    pub dry_run: bool,
    pub clock: Clock,
    /// Seconds an event waits before it is taken, so bursts coalesce.
    pub settle_secs: f64,
    pub wake: Arc<Notify>,
}

/// Where an event points: a thread, or something to resolve first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Thread(String, u64),
    /// Commit-level events: resolve the SHA to its pull requests.
    Commit {
        repo: String,
        sha: String,
        prs: Vec<u64>,
    },
    /// Re-evaluate every open thread of a repository.
    Sweep(String),
    /// A merge group waits for its required checks.
    MergeGroup {
        repo: String,
        head_sha: String,
        head_ref: String,
    },
    /// Nothing to evaluate.
    Ignore(&'static str),
}

/// What a delivery is about, from its payload alone. `bot_login` and
/// `bot_slug` identify the bot's own activity, which never wakes it.
pub fn target(event: &str, payload: &Value, bot_login: &str, bot_slug: &str) -> Target {
    let repo = payload["repository"]["full_name"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let action = payload["action"].as_str().unwrap_or("");
    if payload["sender"]["login"].as_str() == Some(bot_login) {
        return Target::Ignore("the bot's own activity");
    }
    let prs = |v: &Value| -> Vec<u64> {
        v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p["number"].as_u64())
            .collect()
    };
    match event {
        "pull_request"
        | "pull_request_review"
        | "pull_request_review_comment"
        | "pull_request_review_thread" => match payload["pull_request"]["number"].as_u64() {
            Some(n) => Target::Thread(repo, n),
            None => Target::Ignore("no pull request number"),
        },
        "issue_comment" | "issues" => match payload["issue"]["number"].as_u64() {
            Some(n) => Target::Thread(repo, n),
            None => Target::Ignore("no issue number"),
        },
        "check_run" => {
            let run = &payload["check_run"];
            if run["app"]["slug"].as_str() == Some(bot_slug) {
                return Target::Ignore("the bot's own check run");
            }
            let name = run["name"].as_str().unwrap_or("");
            let failed = action == "completed"
                && !matches!(
                    run["conclusion"].as_str(),
                    Some("success" | "neutral" | "skipped")
                );
            if failed || name == COVERAGE_CHECK {
                Target::Commit {
                    repo,
                    sha: run["head_sha"].as_str().unwrap_or("").to_string(),
                    prs: prs(&run["pull_requests"]),
                }
            } else {
                Target::Ignore("check run that a check suite will cover")
            }
        }
        "check_suite" => {
            let suite = &payload["check_suite"];
            if suite["app"]["slug"].as_str() == Some(bot_slug) {
                return Target::Ignore("the bot's own check suite");
            }
            if action != "completed" {
                return Target::Ignore("check suite not completed");
            }
            Target::Commit {
                repo,
                sha: suite["head_sha"].as_str().unwrap_or("").to_string(),
                prs: prs(&suite["pull_requests"]),
            }
        }
        "status" => Target::Commit {
            repo,
            sha: payload["sha"].as_str().unwrap_or("").to_string(),
            prs: Vec::new(),
        },
        "workflow_run" => {
            let run = &payload["workflow_run"];
            // A held fork run shows as action_required.
            if run["conclusion"].as_str() == Some("action_required")
                || run["status"].as_str() == Some("action_required")
            {
                Target::Commit {
                    repo,
                    sha: run["head_sha"].as_str().unwrap_or("").to_string(),
                    prs: prs(&run["pull_requests"]),
                }
            } else {
                Target::Ignore("workflow run covered by its check suite")
            }
        }
        "merge_group" if action == "checks_requested" => {
            let g = &payload["merge_group"];
            Target::MergeGroup {
                repo,
                head_sha: g["head_sha"].as_str().unwrap_or("").to_string(),
                head_ref: g["head_ref"].as_str().unwrap_or("").to_string(),
            }
        }
        "job" => match (payload["repo"].as_str(), payload["number"].as_u64()) {
            (Some(r), Some(n)) => Target::Thread(r.to_string(), n),
            _ => Target::Ignore("malformed job"),
        },
        "sweep" => match payload["repo"].as_str() {
            Some(r) => Target::Sweep(r.to_string()),
            None => Target::Ignore("malformed sweep"),
        },
        "installation_repositories" | "installation" => Target::Ignore("installation change"),
        _ => Target::Ignore("event not handled"),
    }
}

impl<F: ApiFactory> Worker<F> {
    /// Process one event if one is ready. Returns whether it did.
    pub async fn run_once(&self) -> Result<bool> {
        let Some(ev) = db::claim_event(&self.pool, self.settle_secs).await? else {
            return Ok(false);
        };
        let id = ev.delivery_id.clone();
        match self.process(&ev).await {
            Ok(()) => {}
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::warn!(delivery = %id, attempts = ev.attempts, error = %msg, "event failed");
                if ev.attempts >= MAX_ATTEMPTS {
                    db::mark_processed(&self.pool, &id, Some(&msg)).await?;
                } else {
                    let backoff = 10.0 * 2f64.powi(ev.attempts);
                    db::release_event(&self.pool, &id, backoff, Some(&msg)).await?;
                }
            }
        }
        Ok(true)
    }

    /// Worker loop; `n` of these run side by side.
    pub async fn run(self: Arc<Self>) {
        loop {
            match self.run_once().await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => tracing::error!(error = %e, "worker"),
            }
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
    }

    async fn process(&self, ev: &EventRow) -> Result<()> {
        let installation = ev.installation_id.unwrap_or(0) as u64;
        match target(&ev.event, &ev.payload, &self.bot.login, &self.bot.slug) {
            Target::Ignore(why) => {
                tracing::debug!(delivery = %ev.delivery_id, event = %ev.event, %why, "ignored");
                db::mark_processed(&self.pool, &ev.delivery_id, None).await
            }
            Target::Thread(repo, n) => {
                let Some(lock) = db::try_lock(&self.pool, &repo, n as i64).await? else {
                    // Another worker has this thread; come back shortly.
                    return db::release_event(&self.pool, &ev.delivery_id, 1.0, None).await;
                };
                let started = Utc::now();
                let result = self.evaluate_thread(installation, &repo, n).await;
                if result.is_ok() {
                    db::coalesce(&self.pool, &repo, n as i64, started).await?;
                    db::mark_processed(&self.pool, &ev.delivery_id, None).await?;
                }
                lock.unlock().await?;
                result?;
                if ev.action.as_deref() == Some("closed") {
                    for dep in db::blocked_on(&self.pool, &repo, n as i64).await? {
                        self.enqueue(
                            &format!("blocked:{}:{dep}", ev.delivery_id),
                            &repo,
                            dep as u64,
                            installation,
                        )
                        .await?;
                    }
                }
                Ok(())
            }
            Target::Commit { repo, sha, prs } => {
                let prs = if prs.is_empty() {
                    self.prs_for_commit(installation, &repo, &sha).await?
                } else {
                    prs
                };
                for n in prs {
                    self.enqueue(&format!("{}:pr{n}", ev.delivery_id), &repo, n, installation)
                        .await?;
                }
                db::mark_processed(&self.pool, &ev.delivery_id, None).await
            }
            Target::MergeGroup {
                repo,
                head_sha,
                head_ref,
            } => {
                self.merge_group(installation, &repo, &head_sha, &head_ref)
                    .await?;
                db::mark_processed(&self.pool, &ev.delivery_id, None).await
            }
            Target::Sweep(repo) => {
                let api = self.factory.for_installation(installation);
                let open = api
                    .get_all(&format!("/repos/{repo}/issues?state=open"), None)
                    .await?;
                for item in open {
                    if let Some(n) = item["number"].as_u64() {
                        self.enqueue(&format!("{}:{n}", ev.delivery_id), &repo, n, installation)
                            .await?;
                    }
                }
                db::mark_processed(&self.pool, &ev.delivery_id, None).await
            }
        }
    }

    /// Publish `cla` on a merge group in a gated repository: success only
    /// when every pull request in the group passed it on its queued head.
    pub async fn merge_group(
        &self,
        installation: u64,
        repo: &str,
        head_sha: &str,
        head_ref: &str,
    ) -> Result<()> {
        let api = self.factory.for_installation(installation);
        let repo_doc = api.get(&format!("/repos/{repo}")).await?;
        let default_branch = repo_doc["default_branch"].as_str().unwrap_or("main");
        let config = load_config(&api, &self.pool, repo, default_branch).await?;
        if !config.effective().is_some_and(|c| c.raw.cla.enabled) {
            return Ok(());
        }
        let members = cla::group_members(&api, repo, head_sha, head_ref, &self.bot.slug).await?;
        let run = lumenfx_engine::cla::merge_group_check(head_sha, &members);
        let existing = api
            .get_all(
                &format!(
                    "/repos/{repo}/commits/{head_sha}/check-runs?check_name={}",
                    run.name
                ),
                Some("check_runs"),
            )
            .await?
            .into_iter()
            .filter(|r| r["app"]["slug"].as_str() == Some(self.bot.slug.as_str()))
            .max_by_key(|r| r["id"].as_u64().unwrap_or(0));
        if let Some(r) = &existing
            && r["conclusion"].as_str() == run.conclusion.as_deref()
            && r["output"]["summary"].as_str() == Some(run.summary.as_str())
        {
            return Ok(());
        }
        let plan_id = format!("{repo}@{head_sha}");
        tracing::info!(%repo, %head_sha, check = %serde_json::to_string(&run).unwrap_or_default(), dry_run = self.dry_run, "merge group");
        Executor {
            api: &api,
            store_api: None,
            pool: &self.pool,
            dry_run: self.dry_run,
            installation_id: installation,
        }
        .check_run(
            repo,
            0,
            existing.and_then(|r| r["id"].as_u64()),
            &run,
            &plan_id,
        )
        .await
    }

    async fn prs_for_commit(&self, installation: u64, repo: &str, sha: &str) -> Result<Vec<u64>> {
        if sha.is_empty() {
            return Ok(Vec::new());
        }
        let api = self.factory.for_installation(installation);
        let found: Vec<u64> = api
            .get_all(&format!("/repos/{repo}/commits/{sha}/pulls"), None)
            .await?
            .iter()
            .filter(|p| p["state"].as_str() == Some("open"))
            .filter_map(|p| p["number"].as_u64())
            .collect();
        if !found.is_empty() {
            return Ok(found);
        }
        // Fork heads are not always associated with the commit; search finds
        // open pull requests by head SHA.
        let q = format!("repo:{repo} is:pr is:open {sha}");
        let v = api
            .get(&format!("/search/issues?q={}", urlencoding::encode(&q)))
            .await?;
        Ok(v["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| i["number"].as_u64())
            .collect())
    }

    /// A synthetic event that evaluates one thread.
    async fn enqueue(
        &self,
        delivery_id: &str,
        repo: &str,
        n: u64,
        installation: u64,
    ) -> Result<()> {
        let payload = json!({ "repo": repo, "number": n });
        db::insert_event(
            &self.pool,
            NewEvent {
                delivery_id,
                event: "job",
                action: None,
                repo: Some(repo),
                number: Some(n as i64),
                installation_id: Some(installation as i64),
                payload: &payload,
            },
        )
        .await?;
        self.wake.notify_waiters();
        Ok(())
    }

    /// Snapshot, evaluate, execute one thread. Returns the plan.
    pub async fn evaluate_thread(
        &self,
        installation: u64,
        repo: &str,
        number: u64,
    ) -> Result<Plan> {
        let api = self.factory.for_installation(installation);
        let now = (self.clock)();
        let snapshot = Builder {
            api: &api,
            pool: &self.pool,
            bot: &self.bot,
            now,
            config_override: None,
        }
        .build(repo, number)
        .await
        .with_context(|| format!("snapshot of {repo}#{number}"))?;
        let plan = lumenfx_engine::evaluate(&snapshot);
        let plan_id = format!("{repo}#{number}@{}", now.timestamp_millis());
        tracing::info!(
            %repo, number, plan = %serde_json::to_string(&plan).unwrap_or_default(),
            dry_run = self.dry_run, "plan"
        );
        let store_api = plan
            .cla_signatures
            .first()
            .map(|sig| self.factory.for_store(installation, &sig.store));
        Executor {
            api: &api,
            store_api: store_api.as_ref(),
            pool: &self.pool,
            dry_run: self.dry_run,
            installation_id: installation,
        }
        .apply(&snapshot, &plan, &plan_id)
        .await?;
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bot_never_wakes_itself() {
        let p = json!({ "sender": { "login": "lumenfx-bot[bot]" }, "pull_request": { "number": 3 }, "repository": { "full_name": "o/r" } });
        assert!(matches!(
            target("pull_request", &p, "lumenfx-bot[bot]", "lumenfx-bot"),
            Target::Ignore(_)
        ));
        let p = json!({ "action": "completed", "check_run": { "app": { "slug": "lumenfx-bot" }, "name": "docs" }, "repository": { "full_name": "o/r" } });
        assert!(matches!(
            target("check_run", &p, "x[bot]", "lumenfx-bot"),
            Target::Ignore(_)
        ));
    }

    #[test]
    fn only_check_runs_that_matter_wake_it() {
        let run = |name: &str, conclusion: &str| {
            json!({ "action": "completed", "repository": { "full_name": "o/r" },
                    "check_run": { "name": name, "conclusion": conclusion, "head_sha": "abc", "app": { "slug": "github-actions" },
                                   "pull_requests": [ { "number": 7 } ] } })
        };
        let b = ("lumenfx-bot[bot]", "lumenfx-bot");
        assert!(matches!(
            target("check_run", &run("fmt", "success"), b.0, b.1),
            Target::Ignore(_)
        ));
        assert_eq!(
            target("check_run", &run("fmt", "failure"), b.0, b.1),
            Target::Commit {
                repo: "o/r".into(),
                sha: "abc".into(),
                prs: vec![7]
            }
        );
        assert!(matches!(
            target("check_run", &run("codecov/patch", "success"), b.0, b.1),
            Target::Commit { .. }
        ));
        assert!(matches!(
            target("check_run", &run("cla", "success"), b.0, b.1),
            Target::Ignore(_)
        ));
    }

    #[test]
    fn a_merge_group_asks_for_its_checks() {
        let p = json!({ "action": "checks_requested", "repository": { "full_name": "o/r" },
                        "merge_group": { "head_sha": "g1", "head_ref": "refs/heads/gh-readonly-queue/main/pr-5-b0" } });
        assert_eq!(
            target("merge_group", &p, "b[bot]", "b"),
            Target::MergeGroup {
                repo: "o/r".into(),
                head_sha: "g1".into(),
                head_ref: "refs/heads/gh-readonly-queue/main/pr-5-b0".into()
            }
        );
        let p = json!({ "action": "destroyed", "repository": { "full_name": "o/r" }, "merge_group": {} });
        assert!(matches!(
            target("merge_group", &p, "b[bot]", "b"),
            Target::Ignore(_)
        ));
    }

    #[test]
    fn threads_from_issue_and_pr_events() {
        let p = json!({ "action": "created", "issue": { "number": 4 }, "repository": { "full_name": "o/r" }, "sender": { "login": "u" } });
        assert_eq!(
            target("issue_comment", &p, "b[bot]", "b"),
            Target::Thread("o/r".into(), 4)
        );
        let p = json!({ "action": "submitted", "pull_request": { "number": 5 }, "repository": { "full_name": "o/r" } });
        assert_eq!(
            target("pull_request_review", &p, "b[bot]", "b"),
            Target::Thread("o/r".into(), 5)
        );
    }
}
