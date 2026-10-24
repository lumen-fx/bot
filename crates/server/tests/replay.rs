//! Replay suite. Each fixture starts from a world of GitHub documents
//! recorded from a real pull request (`tools/record-world.sh`), changes what
//! the scenario needs, and then plays webhook deliveries through the real
//! receiver, the workers, the evaluator and the executor against the
//! in-memory GitHub. The writes are compared with a golden file, and a final
//! re-evaluation must write nothing.
//!
//! `UPDATE_GOLDEN=1 cargo test --test replay` rewrites the golden files.
//! The suite needs Postgres: `DATABASE_URL` names a server where the test
//! user may create databases.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::Request;
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::Notify;
use tower::ServiceExt;

use lumenfx_bot::fake::{FakeGitHub, Write, current_file, file_doc};
use lumenfx_bot::web::{self, AppState};
use lumenfx_bot::webhook::sign;
use lumenfx_bot::worker::Worker;
use lumenfx_engine::snapshot::BotIdentity;

const SECRET: &[u8] = b"replay-secret";
const STORE_FILE: &str = "/repos/lumen-fx/cla-signatures/contents/signatures/v1/cla.json";
const SLUG: &str = "lumenfx-bot";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    #[allow(dead_code)]
    description: String,
    world: String,
    number: u64,
    now: String,
    /// "example" for examples/lumenfx.toml, inline TOML, or absent for no
    /// config at all.
    #[serde(default)]
    config: Option<String>,
    #[serde(default)]
    dry_run: bool,
    /// The CLA signature file in lumen-fx/cla-signatures; absent: no file.
    #[serde(default)]
    cla_store: Option<Value>,
    /// Store writes that find another writer got there first.
    #[serde(default)]
    store_races: u32,
    /// JSON merge patches over world documents; `null` deletes a document.
    #[serde(default)]
    patch: BTreeMap<String, Value>,
    #[serde(default)]
    graphql: BTreeMap<String, Value>,
    /// Permission and merged-PR count per login.
    #[serde(default)]
    users: BTreeMap<String, UserSpec>,
    steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserSpec {
    id: u64,
    #[serde(default = "read")]
    permission: String,
    #[serde(default)]
    merged_prs: u64,
}

fn read() -> String {
    "read".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
enum Step {
    /// A webhook delivery. The payload is assembled from the world's
    /// current documents in GitHub's shape.
    Deliver {
        event: String,
        action: String,
        #[serde(default)]
        sender: Option<String>,
        /// Thread number when it is not the fixture's.
        #[serde(default)]
        number: Option<u64>,
    },
    /// Someone comments; also delivers `issue_comment` `created`.
    Comment { user: String, body: String },
    /// Edit an existing comment; delivers `issue_comment` `edited`.
    Edit { id: u64, body: String },
    /// Change documents between deliveries.
    Patch { docs: BTreeMap<String, Value> },
    /// Replace the CLA signature file, as a hand edit of the store would.
    Store { file: Value },
    /// Move the clock.
    Advance { hours: i64 },
    /// Fire the timers due at the current clock.
    Timers,
}

fn merge_patch(target: &mut Value, patch: &Value) {
    if let Value::Object(p) = patch {
        if !target.is_object() {
            *target = json!({});
        }
        let t = target.as_object_mut().unwrap();
        for (k, v) in p {
            if v.is_null() {
                t.remove(k);
            } else {
                merge_patch(t.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
    } else {
        *target = patch.clone();
    }
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

struct World {
    fake: FakeGitHub,
    repo: String,
    number: u64,
    clock: Arc<Mutex<DateTime<Utc>>>,
    pool: PgPool,
    worker: Worker<FakeGitHub>,
    router: axum::Router,
    deliveries: u64,
    users: BTreeMap<String, UserSpec>,
}

impl World {
    /// `path` is merge-patched; `path#append:field` appends to the array
    /// at `field` (or to the document itself when `field` is empty);
    /// `graphql:Name#n` patches a recorded GraphQL answer.
    fn apply_patches(&self, docs: &BTreeMap<String, Value>) {
        for (path, p) in docs {
            if let Some(key) = path.strip_prefix("graphql:") {
                let mut d = self.fake.graphql_doc(key).unwrap_or(Value::Null);
                merge_patch(&mut d, p);
                self.fake.set_graphql(key, d);
                continue;
            }
            if let Some((doc, field)) = path.split_once("#append:") {
                let mut d = self.fake.doc(doc).unwrap_or(if field.is_empty() {
                    json!([])
                } else {
                    json!({})
                });
                let arr = if field.is_empty() {
                    &mut d
                } else {
                    d.as_object_mut().unwrap().entry(field).or_insert(json!([]))
                };
                arr.as_array_mut()
                    .unwrap()
                    .extend(p.as_array().unwrap().iter().cloned());
                self.fake.set_doc(doc, d);
                continue;
            }
            if p.is_null() {
                self.fake.set_doc(path, Value::Null);
                continue;
            }
            let mut d = self.fake.doc(path).unwrap_or(Value::Null);
            if p.is_array() {
                d = p.clone();
            } else {
                merge_patch(&mut d, p);
            }
            self.fake.set_doc(path, d);
        }
    }

    fn user(&self, login: &str) -> Value {
        let id = match login {
            "FizzWizZleDazzle" => 126825095,
            "pullsift[bot]" => 2001,
            _ => self.users.get(login).map(|u| u.id).unwrap_or(999),
        };
        let kind = if login.ends_with("[bot]") {
            "Bot"
        } else {
            "User"
        };
        json!({ "login": login, "id": id, "type": kind })
    }

    async fn deliver(&mut self, event: &str, payload: Value) {
        self.deliveries += 1;
        let body = serde_json::to_vec(&payload).unwrap();
        let req = Request::post("/github-hook")
            .header("x-github-event", event)
            .header("x-github-delivery", format!("replay-{}", self.deliveries))
            .header("x-hub-signature-256", sign(SECRET, &body))
            .body(Body::from(body))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 202, "receiver refused the delivery");
        self.drain().await;
    }

    async fn drain(&self) {
        for _ in 0..200 {
            if !self.worker.run_once().await.unwrap() {
                // Events released for retry come back after a delay; make
                // them due now.
                let n = sqlx::query(
                    "UPDATE events SET next_attempt_at = now() WHERE processed_at IS NULL AND next_attempt_at > now()",
                )
                .execute(&self.pool)
                .await
                .unwrap()
                .rows_affected();
                if n == 0 {
                    break;
                }
            }
        }
        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM events WHERE processed_at IS NULL")
                .fetch_one(&self.pool)
                .await
                .unwrap();
        assert_eq!(left, 0, "events left unprocessed");
        let failed: Vec<String> =
            sqlx::query_scalar("SELECT error FROM events WHERE error IS NOT NULL")
                .fetch_all(&self.pool)
                .await
                .unwrap();
        assert!(failed.is_empty(), "events failed: {failed:?}");
    }

    fn payload(&self, event: &str, action: &str, sender: &str, number: u64) -> Value {
        let repo_doc = self.fake.doc(&format!("/repos/{}", self.repo)).unwrap();
        let issue = self
            .fake
            .doc(&format!("/repos/{}/issues/{number}", self.repo))
            .unwrap_or(json!({ "number": number }));
        let mut p = json!({
            "action": action,
            "repository": repo_doc,
            "sender": self.user(sender),
            "installation": { "id": 1 },
        });
        match event {
            "pull_request"
            | "pull_request_review"
            | "pull_request_review_thread"
            | "pull_request_review_comment" => {
                p["number"] = json!(number);
                p["pull_request"] = self
                    .fake
                    .doc(&format!("/repos/{}/pulls/{number}", self.repo))
                    .unwrap_or(json!({ "number": number }));
            }
            "check_run" | "check_suite" => {
                let pr = self
                    .fake
                    .doc(&format!("/repos/{}/pulls/{number}", self.repo))
                    .unwrap();
                let obj = json!({
                    "name": "fmt",
                    "status": "completed",
                    "conclusion": "failure",
                    "head_sha": pr["head"]["sha"],
                    "app": { "slug": "github-actions" },
                    "pull_requests": [ { "number": number } ],
                });
                p[event] = obj;
            }
            "merge_group" => {
                let pr = self
                    .fake
                    .doc(&format!("/repos/{}/pulls/{number}", self.repo))
                    .unwrap();
                let base = pr["base"]["sha"].as_str().unwrap_or("base");
                p["merge_group"] = json!({
                    "head_sha": format!("group{number}"),
                    "head_ref": format!("refs/heads/gh-readonly-queue/main/pr-{number}-{base}"),
                    "base_sha": base,
                    "base_ref": "refs/heads/main",
                });
            }
            "status" => {
                let pr = self
                    .fake
                    .doc(&format!("/repos/{}/pulls/{number}", self.repo))
                    .unwrap();
                p["sha"] = pr["head"]["sha"].clone();
                p["context"] = json!("codecov/patch");
                p["state"] = json!("success");
            }
            _ => p["issue"] = issue,
        }
        p
    }

    async fn step(&mut self, step: &Step) {
        match step {
            Step::Deliver {
                event,
                action,
                sender,
                number,
            } => {
                let n = number.unwrap_or(self.number);
                let pr = self.fake.doc(&format!("/repos/{}/pulls/{n}", self.repo));
                let author = pr
                    .as_ref()
                    .and_then(|p| p["user"]["login"].as_str().map(String::from))
                    .unwrap_or_else(|| "someone".into());
                let sender = sender.clone().unwrap_or(author);
                let payload = self.payload(event, action, &sender, n);
                self.deliver(event, payload).await;
            }
            Step::Comment { user, body } => {
                let path = format!("/repos/{}/issues/{}/comments", self.repo, self.number);
                let mut comments = self.fake.doc(&path).unwrap_or(json!([]));
                let id = 7_000_000 + comments.as_array().unwrap().len() as u64 + self.deliveries;
                let now = *self.clock.lock().unwrap();
                let c = json!({
                    "id": id,
                    "user": self.user(user),
                    "body": body,
                    "created_at": now.to_rfc3339(),
                    "html_url": format!("https://github.com/{}/pull/{}#issuecomment-{id}", self.repo, self.number),
                });
                comments.as_array_mut().unwrap().push(c.clone());
                self.fake.set_doc(&path, comments);
                let mut p = self.payload("issue_comment", "created", user, self.number);
                p["comment"] = c;
                self.deliver("issue_comment", p).await;
            }
            Step::Edit { id, body } => {
                let path = format!("/repos/{}/issues/{}/comments", self.repo, self.number);
                let mut comments = self.fake.doc(&path).unwrap();
                let c = comments
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|c| c["id"].as_u64() == Some(*id))
                    .expect("comment to edit");
                c["body"] = json!(body);
                let c = c.clone();
                self.fake.set_doc(&path, comments);
                let user = c["user"]["login"].as_str().unwrap().to_string();
                let mut p = self.payload("issue_comment", "edited", &user, self.number);
                p["comment"] = c;
                self.deliver("issue_comment", p).await;
            }
            Step::Patch { docs } => self.apply_patches(docs),
            Step::Store { file } => self.fake.set_doc(
                STORE_FILE,
                file_doc("edited", &serde_json::to_string_pretty(file).unwrap()),
            ),
            Step::Advance { hours } => {
                let mut c = self.clock.lock().unwrap();
                *c += Duration::hours(*hours);
            }
            Step::Timers => {
                let now = *self.clock.lock().unwrap();
                lumenfx_bot::scheduler::fire_due_jobs(&self.pool, &Notify::new(), now)
                    .await
                    .unwrap();
                self.drain().await;
            }
        }
    }
}

async fn run_fixture(pool: PgPool, name: &str) {
    let dir = fixtures_dir();
    let fx: Fixture =
        serde_json::from_str(&std::fs::read_to_string(dir.join(format!("{name}.json"))).unwrap())
            .unwrap_or_else(|e| panic!("{name}.json: {e}"));
    let world: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("worlds").join(format!("{}.json", fx.world))).unwrap(),
    )
    .unwrap();
    let docs: BTreeMap<String, Value> = serde_json::from_value(world["docs"].clone()).unwrap();
    let mut graphql: BTreeMap<String, Value> =
        serde_json::from_value(world["graphql"].clone()).unwrap();
    graphql.extend(fx.graphql.clone());
    let repo = docs
        .keys()
        .find_map(|k| {
            let rest = k.strip_prefix("/repos/")?;
            (rest.matches('/').count() == 1).then(|| rest.to_string())
        })
        .unwrap();

    let fake = FakeGitHub::new(docs, graphql, SLUG, &fx.now);
    let now: DateTime<Utc> = DateTime::parse_from_rfc3339(&fx.now)
        .unwrap()
        .with_timezone(&Utc);
    let clock = Arc::new(Mutex::new(now));
    let wake = Arc::new(Notify::new());
    let bot = BotIdentity {
        slug: SLUG.into(),
        login: format!("{SLUG}[bot]"),
        public_url: "https://bot.lumenfx.dev".into(),
    };
    let c2 = clock.clone();
    let worker = Worker {
        pool: pool.clone(),
        factory: Arc::new(fake.clone()),
        bot,
        dry_run: fx.dry_run,
        clock: Arc::new(move || *c2.lock().unwrap()),
        settle_secs: 0.0,
        wake: wake.clone(),
    };
    let router = web::router(Arc::new(AppState {
        pool: pool.clone(),
        webhook_secret: SECRET.to_vec(),
        wake,
        dry_run: fx.dry_run,
        slug: SLUG.into(),
        started: now,
        rate: Arc::new(Vec::new),
    }));
    let mut w = World {
        fake,
        repo: repo.clone(),
        number: fx.number,
        clock,
        pool: pool.clone(),
        worker,
        router,
        deliveries: 0,
        users: fx.users,
    };

    // Config on the default branch.
    if let Some(cfg) = &fx.config {
        let text = if cfg == "example" {
            std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/lumenfx.toml"),
            )
            .unwrap()
        } else {
            cfg.clone()
        };
        let content = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        w.fake.set_doc(
            &format!("/repos/{repo}/contents/.github/lumenfx.toml?ref=main"),
            json!({ "sha": "cfg0", "content": content }),
        );
    }
    for (login, u) in &w.users {
        w.fake.set_doc(
            &format!("/repos/{repo}/collaborators/{login}/permission"),
            json!({ "permission": u.permission, "role_name": u.permission, "user": { "login": login, "id": u.id } }),
        );
        let q = format!("repo:{repo} is:pr is:merged author:{login}");
        w.fake.set_doc(
            &format!("/search/issues?q={}&per_page=1", urlencoding::encode(&q)),
            json!({ "total_count": u.merged_prs, "items": [] }),
        );
    }
    if let Some(file) = &fx.cla_store {
        w.fake.set_doc(
            STORE_FILE,
            file_doc("store0", &serde_json::to_string_pretty(file).unwrap()),
        );
    }
    w.fake.race_store_writes(fx.store_races);
    w.apply_patches(&fx.patch);

    let mut steps_out = Vec::new();
    for (i, step) in fx.steps.iter().enumerate() {
        w.fake.clear_writes();
        w.step(step).await;
        steps_out.push(json!({ "step": i, "writes": w.fake.writes() }));
    }

    // Recompute from scratch: nothing left to write.
    w.fake.clear_writes();
    let plan = w.worker.evaluate_thread(1, &repo, fx.number).await.unwrap();
    let again: Vec<Write> = w.fake.writes();
    assert!(
        again.is_empty(),
        "{name}: a second evaluation wrote {again:#?}"
    );
    if !fx.dry_run {
        assert!(
            plan.writes_nothing(),
            "{name}: second plan not empty: {plan:#?}"
        );
    }

    let labels: Vec<String> = w
        .fake
        .doc(&format!("/repos/{repo}/pulls/{}", fx.number))
        .or_else(|| w.fake.doc(&format!("/repos/{repo}/issues/{}", fx.number)))
        .map(|d| {
            d["labels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| l["name"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default();
    let log: Vec<(String, String, String, bool)> =
        sqlx::query_as("SELECT action, target, rule, dry_run FROM actions_log ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    let mut got = json!({
        "steps": steps_out,
        "final_labels": labels,
        "final_decision": plan.decision,
        "actions_log": log,
    });
    if let Some(doc) = w.fake.doc(STORE_FILE) {
        got["final_cla_store"] = current_file(Some(&doc));
    }

    let golden = dir.join(format!("{name}.golden.json"));
    let text = serde_json::to_string_pretty(&got).unwrap() + "\n";
    if std::env::var_os("UPDATE_GOLDEN").is_some() || !golden.exists() {
        std::fs::write(&golden, &text).unwrap();
        if std::env::var_os("UPDATE_GOLDEN").is_none() {
            panic!("{name}: wrote a new golden file; review it and run again");
        }
    } else {
        let want = std::fs::read_to_string(&golden).unwrap();
        assert_eq!(
            text,
            want,
            "{name}: output differs from {}",
            golden.display()
        );
    }
}

macro_rules! replay {
    ($($name:ident),* $(,)?) => {
        $(
            #[sqlx::test(migrator = "lumenfx_bot::db::MIGRATOR")]
            async fn $name(pool: PgPool) {
                run_fixture(pool, stringify!($name)).await;
            }
        )*

        #[test]
        fn every_fixture_has_a_test() {
            let listed: Vec<&str> = vec![$(stringify!($name)),*];
            for e in std::fs::read_dir(fixtures_dir()).unwrap() {
                let p = e.unwrap().path();
                let n = p.file_name().unwrap().to_str().unwrap().to_string();
                if let Some(stem) = n.strip_suffix(".json").filter(|s| !s.ends_with(".golden")) {
                    assert!(listed.contains(&stem), "fixture {stem}.json has no replay! entry");
                }
            }
        }
    };
}

replay! {
    owner_pr_green,
    no_config_help_only,
    first_time_welcome,
    red_check_then_fixed,
    merge_commits,
    conflict_notice,
    changes_requested,
    cmd_ready,
    cmd_author,
    cmd_blocked,
    cmd_on_issue,
    cmd_label,
    cmd_concern,
    cmd_unknown,
    cmd_quoted_ignored,
    cmd_edited,
    pullsift_hold,
    docs_advisory,
    config_edit_check,
    comment_cap,
    author_nudge,
    draft,
    held_fork_run,
    dry_run,
    cla_all_signed,
    cla_unsigned_author,
    cla_unsigned_trailer,
    cla_unlinked_email,
    cla_sign,
    cla_sign_stranger,
    cla_store_race,
    cla_dry_run,
    cla_recheck,
    cla_store_unreadable,
    cla_merge_group_pass,
    cla_merge_group_fail,
    cla_lock_after_merge,
    cla_ungated,
}
