//! Applies a Plan: writes only what differs from the snapshot, records every
//! write in the audit log, and stores the thread's new state. In dry run the
//! GitHub writes are logged and skipped; the bot's own bookkeeping is still
//! stored, so turning dry run off later does not replay old commands.

use anyhow::Result;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use lumenfx_engine::plan::{CheckRun, Plan};
use lumenfx_engine::snapshot::{Item, Snapshot};
use lumenfx_engine::templates::STATUS_MARKER;

use crate::cla;
use crate::db::{self, LogEntry};
use crate::github::{Api, Method};

pub struct Executor<'a, A: Api> {
    pub api: &'a A,
    /// Writes the CLA signature store; needed only when the plan signs.
    pub store_api: Option<&'a A>,
    pub pool: &'a PgPool,
    pub dry_run: bool,
    pub installation_id: u64,
}

fn sha(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

impl<A: Api> Executor<'_, A> {
    async fn log(
        &self,
        s: &Snapshot,
        plan_id: &str,
        action: &str,
        target: &str,
        rule: &str,
    ) -> Result<()> {
        self.log_at(&s.repo, s.number, plan_id, action, target, rule)
            .await
    }

    async fn log_at(
        &self,
        repo: &str,
        number: u64,
        plan_id: &str,
        action: &str,
        target: &str,
        rule: &str,
    ) -> Result<()> {
        tracing::info!(
            %repo, number, %action, %target, %rule, dry_run = self.dry_run,
            "{}", if self.dry_run { "would write" } else { "write" }
        );
        db::log_action(
            self.pool,
            LogEntry {
                repo,
                number: number as i64,
                action,
                target,
                rule,
                plan_id,
                dry_run: self.dry_run,
            },
        )
        .await
    }

    async fn write(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        if self.dry_run {
            return Ok(serde_json::Value::Null);
        }
        self.api.send(method, path, body).await
    }

    pub async fn apply(&self, s: &Snapshot, plan: &Plan, plan_id: &str) -> Result<()> {
        let repo = &s.repo;
        let n = s.number;
        let ni = n as i64;
        let current = match &s.item {
            Item::Pr(pr) => &pr.labels,
            Item::Issue(i) => &i.labels,
        };

        // Signatures first: if the store cannot be written, nothing else
        // goes out, and the event is retried with the same plan.
        for sig in &plan.cla_signatures {
            if !self.dry_run {
                let api = self
                    .store_api
                    .ok_or_else(|| anyhow::anyhow!("no store writer for a plan that signs"))?;
                cla::append(api, self.pool, sig).await?;
            }
            self.log(s, plan_id, "cla-sign", &sig.name, "11.2").await?;
        }

        let adds: Vec<&str> = plan
            .labels
            .iter()
            .filter(|c| c.present && !current.contains(&c.label))
            .map(|c| c.label.as_str())
            .collect();
        if !adds.is_empty() {
            self.write(
                Method::Post,
                &format!("/repos/{repo}/issues/{n}/labels"),
                Some(json!({ "labels": adds })),
            )
            .await?;
        }
        for c in &plan.labels {
            if c.present && !current.contains(&c.label) {
                self.log(s, plan_id, "label-add", &c.label, &c.rule).await?;
                db::owned_label(self.pool, repo, ni, &c.label, true).await?;
            } else if !c.present && current.contains(&c.label) {
                self.write(
                    Method::Delete,
                    &format!(
                        "/repos/{repo}/issues/{n}/labels/{}",
                        urlencoding::encode(&c.label)
                    ),
                    None,
                )
                .await?;
                self.log(s, plan_id, "label-remove", &c.label, &c.rule)
                    .await?;
                db::owned_label(self.pool, repo, ni, &c.label, false).await?;
            }
        }

        if let Some(sc) = &plan.status_comment {
            let existing = s
                .store
                .status_comment_id
                .and_then(|id| s.comments.iter().find(|c| c.id == id))
                .or_else(|| {
                    s.comments.iter().find(|c| {
                        c.user.login.eq_ignore_ascii_case(&s.bot.login)
                            && c.body.starts_with(STATUS_MARKER)
                    })
                });
            match existing {
                Some(c) if c.body.trim_end() == sc.body.trim_end() => {}
                Some(c) => {
                    self.write(
                        Method::Patch,
                        &format!("/repos/{repo}/issues/comments/{}", c.id),
                        Some(json!({ "body": sc.body })),
                    )
                    .await?;
                    self.log(s, plan_id, "status-edit", &c.id.to_string(), "8.1")
                        .await?;
                    if s.store.status_comment_id == Some(c.id) {
                        db::touch_comment(self.pool, repo, ni, "status", &sha(&sc.body)).await?;
                    } else {
                        db::record_comment(self.pool, repo, ni, "status", "", c.id, &sha(&sc.body))
                            .await?;
                    }
                }
                None if sc.create => {
                    let v = self
                        .write(
                            Method::Post,
                            &format!("/repos/{repo}/issues/{n}/comments"),
                            Some(json!({ "body": sc.body })),
                        )
                        .await?;
                    self.log(s, plan_id, "status-create", "", "8.1").await?;
                    if let Some(id) = v["id"].as_u64() {
                        db::record_comment(self.pool, repo, ni, "status", "", id, &sha(&sc.body))
                            .await?;
                    }
                }
                None => {}
            }
        }

        for c in &plan.comments {
            if s.store.posted.contains(&(c.purpose.clone(), c.key.clone())) {
                continue;
            }
            let v = self
                .write(
                    Method::Post,
                    &format!("/repos/{repo}/issues/{n}/comments"),
                    Some(json!({ "body": c.body })),
                )
                .await?;
            self.log(
                s,
                plan_id,
                "comment",
                &c.purpose,
                &format!("8.2 {}", c.purpose),
            )
            .await?;
            if !self.dry_run {
                db::record_comment(
                    self.pool,
                    repo,
                    ni,
                    &c.purpose,
                    &c.key,
                    v["id"].as_u64().unwrap_or(0),
                    &sha(&c.body),
                )
                .await?;
            }
        }

        for r in &plan.reactions {
            self.write(
                Method::Post,
                &format!("/repos/{repo}/issues/comments/{}/reactions", r.comment_id),
                Some(json!({ "content": r.content })),
            )
            .await?;
            self.log(
                s,
                plan_id,
                "reaction",
                &r.comment_id.to_string(),
                "7 acknowledge",
            )
            .await?;
        }

        for cr in &plan.check_runs {
            let existing = match &s.item {
                Item::Pr(pr) => pr.own_checks.get(&cr.name).map(|c| c.id),
                Item::Issue(_) => None,
            };
            self.check_run(repo, n, existing, cr, plan_id).await?;
        }

        if plan.lock {
            self.write(
                Method::Put,
                &format!("/repos/{repo}/issues/{n}/lock"),
                Some(json!({})),
            )
            .await?;
            self.log(s, plan_id, "lock", "", "11.4 lock after merge")
                .await?;
        }

        db::save_entity(self.pool, repo, ni, &plan.state).await?;

        for j in &plan.jobs {
            db::schedule_job(
                self.pool,
                &format!("eval:{repo}#{n}:{}", j.reason),
                j.at,
                &json!({ "repo": repo, "number": n, "installation_id": self.installation_id, "reason": j.reason }),
            )
            .await?;
        }
        Ok(())
    }

    /// Create or update one check run. `number` is the thread it is
    /// logged under; 0 for a merge group.
    pub async fn check_run(
        &self,
        repo: &str,
        number: u64,
        existing: Option<u64>,
        cr: &CheckRun,
        plan_id: &str,
    ) -> Result<()> {
        let mut body = json!({
            "name": cr.name,
            "head_sha": cr.head_sha,
            "output": { "title": cr.title, "summary": cr.summary },
        });
        match &cr.conclusion {
            Some(c) => {
                body["status"] = json!("completed");
                body["conclusion"] = json!(c);
            }
            None => body["status"] = json!("in_progress"),
        }
        match existing {
            Some(id) => {
                self.write(
                    Method::Patch,
                    &format!("/repos/{repo}/check-runs/{id}"),
                    Some(body),
                )
                .await?
            }
            None => {
                self.write(
                    Method::Post,
                    &format!("/repos/{repo}/check-runs"),
                    Some(body),
                )
                .await?
            }
        };
        let rule = if cr.name == lumenfx_engine::cla::CLA_CHECK {
            "11.4"
        } else {
            "17.3 / 5"
        };
        self.log_at(
            repo,
            number,
            plan_id,
            "check-run",
            &format!(
                "{} {}",
                cr.name,
                cr.conclusion.as_deref().unwrap_or("in_progress")
            ),
            rule,
        )
        .await
    }
}
