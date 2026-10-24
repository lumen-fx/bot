//! Reads one thread's current state from GitHub and the bot's store, and
//! reduces it to the facts [`lumenfx_engine::snapshot::Snapshot`] carries.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use base64::Engine;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;

use lumenfx_engine::cla::CLA_CHECK;
use lumenfx_engine::command;
use lumenfx_engine::config::{CONFIG_PATH, Config};
use lumenfx_engine::evaluate::{CONFIG_CHECK, DOCS_CHECK};
use lumenfx_engine::snapshot::{
    BotIdentity, ClaStore, Comment, CommitFacts, ConfigState, Issue, Item, OwnCheck, Person,
    PullRequest, Pullsift, PullsiftComment, RequiredCheck, Review, ReviewState, Snapshot,
    StatusFacts, Stored, Thread, ThreadComment, UserFacts, UserRef,
};
use lumenfx_engine::state::{CheckClass, Lifecycle};

use crate::cla::{self, Resolver};
use crate::db;
use crate::github::{Api, is_not_found};

pub const PULLSIFT_LOGIN: &str = "pullsift[bot]";
pub const PULLSIFT_LABEL: &str = "slop-suspect";
pub const PULLSIFT_MARKER: &str = "<!-- pullsift-score -->";
/// The phrase Pullsift's score comment uses for a pull request it lets
/// through.
pub const PULLSIFT_PASS: &str = "below every action threshold";
pub const COVERAGE_CHECK: &str = "codecov/patch";

pub const PULL_STATE_QUERY: &str = r#"query PullState($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      isInMergeQueue
      reviewThreads(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          isResolved
          comments(first: 100) {
            nodes {
              createdAt
              author { __typename login ... on User { databaseId } ... on Bot { databaseId } }
            }
          }
        }
      }
    }
  }
}"#;

pub fn time(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

fn user(v: &Value) -> UserRef {
    UserRef {
        id: v["id"].as_u64().unwrap_or(0),
        login: v["login"].as_str().unwrap_or("").to_string(),
        is_bot: v["type"].as_str() == Some("Bot")
            || v["login"].as_str().is_some_and(|l| l.ends_with("[bot]")),
    }
}

fn labels(v: &Value) -> BTreeSet<String> {
    v["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l["name"].as_str().map(String::from))
        .collect()
}

/// Read the config from a ref. `Ok(None)` when the file does not exist.
pub async fn read_config_text<A: Api>(
    api: &A,
    repo: &str,
    git_ref: &str,
) -> Result<Option<(String, String)>> {
    let path = format!(
        "/repos/{repo}/contents/{CONFIG_PATH}?ref={}",
        urlencoding::encode(git_ref)
    );
    let v = match api.get(&path).await {
        Ok(v) => v,
        Err(e) if is_not_found(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    let sha = v["sha"].as_str().unwrap_or("").to_string();
    let b64: String = v["content"]
        .as_str()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("config content is not base64")?;
    Ok(Some((sha, String::from_utf8_lossy(&bytes).into_owned())))
}

/// The config on the default branch, with the last valid version kept in
/// the database for when the current one breaks.
pub async fn load_config<A: Api>(
    api: &A,
    pool: &PgPool,
    repo: &str,
    default_branch: &str,
) -> Result<ConfigState> {
    let Some((sha, text)) = read_config_text(api, repo, default_branch).await? else {
        db::forget_config(pool, repo).await?;
        return Ok(ConfigState::Missing);
    };
    let parsed = Config::parse(&text);
    let errors = parsed.as_ref().err().cloned().unwrap_or_default();
    let cached = db::config_cache(pool, repo).await?;
    if cached.as_ref().map(|c| c.blob_sha.as_str()) != Some(sha.as_str()) {
        db::put_config(pool, repo, &sha, &text, &errors).await?;
    }
    Ok(match parsed {
        Ok(c) => ConfigState::Valid(Box::new(c)),
        Err(errors) => {
            let last_good = db::config_cache(pool, repo)
                .await?
                .and_then(|c| c.last_good_text)
                .and_then(|t| Config::parse(&t).ok())
                .map(Box::new);
            ConfigState::Invalid { errors, last_good }
        }
    })
}

pub struct Builder<'a, A: Api> {
    pub api: &'a A,
    pub pool: &'a PgPool,
    pub bot: &'a BotIdentity,
    pub now: DateTime<Utc>,
    /// Evaluate with this config instead of the one on the default branch
    /// (the `plan` command's `--config`).
    pub config_override: Option<&'a Config>,
}

impl<A: Api> Builder<'_, A> {
    pub async fn build(&self, repo: &str, number: u64) -> Result<Snapshot> {
        let n = number as i64;
        let issue = self
            .api
            .get(&format!("/repos/{repo}/issues/{number}"))
            .await?;
        let comments: Vec<Comment> = self
            .api
            .get_all(&format!("/repos/{repo}/issues/{number}/comments"), None)
            .await?
            .iter()
            .map(|c| Comment {
                id: c["id"].as_u64().unwrap_or(0),
                user: user(&c["user"]),
                body: c["body"].as_str().unwrap_or("").to_string(),
                created_at: time(&c["created_at"]).unwrap_or(self.now),
                url: c["html_url"].as_str().unwrap_or("").to_string(),
            })
            .collect();

        let entity = db::load_entity(self.pool, repo, n).await?;
        let rec = db::comments(self.pool, repo, n).await?;
        let store = Stored {
            entity,
            status_comment_id: rec.status_comment_id,
            comments_last_day: rec.last_day,
            posted: rec.posted,
        };

        let mut people: BTreeMap<u64, UserRef> = BTreeMap::new();
        let mut note = |u: &UserRef| {
            if u.id != 0 && !u.is_bot {
                people.entry(u.id).or_insert_with(|| u.clone());
            }
        };
        for c in &comments {
            if !command::parse(&c.body, &self.bot.slug).is_empty() {
                note(&c.user);
            }
        }

        let repo_doc = self.api.get(&format!("/repos/{repo}")).await?;
        let default_branch = repo_doc["default_branch"]
            .as_str()
            .unwrap_or("main")
            .to_string();
        let config = match self.config_override {
            Some(c) => ConfigState::Valid(Box::new(c.clone())),
            None => load_config(self.api, self.pool, repo, &default_branch).await?,
        };
        // Without a usable config only `help` is answered, so the expensive
        // reads are skipped.
        let active = config.effective().is_some();
        let cla_cfg = config
            .effective()
            .map(|c| c.raw.cla.clone())
            .filter(|c| c.enabled);
        let mut cla_store = ClaStore::NotRead;

        let item = if issue.get("pull_request").is_some_and(|p| !p.is_null()) {
            let pr = self
                .pull_request(repo, number, &comments, active, cla_cfg.is_some())
                .await?;
            if let Some(c) = &cla_cfg
                && pr.lifecycle == Lifecycle::Open
            {
                // A `sign` or `recheck` not handled yet reads the store
                // afresh rather than from the cache.
                let fresh = comments.iter().any(|c| {
                    command::parse(&c.body, &self.bot.slug).iter().any(|cmd| {
                        matches!(cmd.name.as_str(), "sign" | "recheck")
                            && !store.entity.processed.contains(&(c.id, cmd.key()))
                    })
                });
                cla_store = cla::signers(self.api, self.pool, &c.store, &c.path, fresh).await?;
            }
            note(&pr.author);
            for r in &pr.reviews {
                note(&r.user);
            }
            for t in &pr.threads {
                for c in &t.comments {
                    note(&c.user);
                }
            }
            Item::Pr(Box::new(pr))
        } else {
            let author = user(&issue["user"]);
            note(&author);
            Item::Issue(Issue {
                title: issue["title"].as_str().unwrap_or("").to_string(),
                open: issue["state"].as_str() == Some("open"),
                author,
                labels: labels(&issue),
            })
        };

        let mut users = BTreeMap::new();
        for (id, u) in people {
            let write_access = self.write_access(repo, &u.login).await;
            users.insert(
                id,
                UserFacts {
                    login: u.login,
                    is_bot: u.is_bot,
                    write_access,
                },
            );
        }

        let repo_labels = if !active {
            None
        } else {
            match self
                .api
                .get_all(&format!("/repos/{repo}/labels"), None)
                .await
            {
                Ok(ls) => Some(
                    ls.iter()
                        .filter_map(|l| l["name"].as_str().map(String::from))
                        .collect(),
                ),
                Err(e) => {
                    tracing::warn!(%repo, error = %e, "could not list labels");
                    None
                }
            }
        };

        let blocked_target_closed = match store.entity.blocked_by {
            Some(b) => match self.api.get(&format!("/repos/{repo}/issues/{b}")).await {
                Ok(v) => Some(v["state"].as_str() == Some("closed")),
                Err(e) if is_not_found(&e) => Some(true),
                Err(e) => return Err(e),
            },
            None => None,
        };

        Ok(Snapshot {
            repo: repo.to_string(),
            number,
            now: self.now,
            bot: self.bot.clone(),
            config,
            item,
            comments,
            users,
            repo_labels,
            blocked_target_closed,
            cla_store,
            store,
        })
    }

    async fn write_access(&self, repo: &str, login: &str) -> bool {
        let path = format!(
            "/repos/{repo}/collaborators/{}/permission",
            urlencoding::encode(login)
        );
        match self.api.get(&path).await {
            Ok(v) => {
                let role = v["role_name"]
                    .as_str()
                    .or(v["permission"].as_str())
                    .unwrap_or("");
                matches!(role, "admin" | "maintain" | "write")
            }
            Err(e) => {
                if !is_not_found(&e) {
                    tracing::warn!(%repo, %login, error = %e, "permission lookup failed");
                }
                false
            }
        }
    }

    async fn first_time(&self, repo: &str, number: u64, author: &UserRef) -> Result<bool> {
        if author.is_bot {
            return Ok(false);
        }
        let n = number as i64;
        if let Some(v) = db::get_data(self.pool, repo, n, "author").await?
            && v["id"].as_u64() == Some(author.id)
            && let Some(b) = v["first_time"].as_bool()
        {
            return Ok(b);
        }
        let q = format!("repo:{repo} is:pr is:merged author:{}", author.login);
        let v = match self
            .api
            .get(&format!(
                "/search/issues?q={}&per_page=1",
                urlencoding::encode(&q)
            ))
            .await
        {
            Ok(v) => v,
            Err(e) => {
                // Unknown is not first-time: the welcome waits for the next
                // evaluation rather than greeting a regular by mistake.
                tracing::warn!(%repo, login = %author.login, error = %e, "first-time lookup failed");
                return Ok(false);
            }
        };
        let first = v["total_count"].as_u64() == Some(0);
        db::put_data(
            self.pool,
            repo,
            n,
            "author",
            &json!({ "id": author.id, "first_time": first }),
        )
        .await?;
        Ok(first)
    }

    async fn pull_request(
        &self,
        repo: &str,
        number: u64,
        comments: &[Comment],
        active: bool,
        cla: bool,
    ) -> Result<PullRequest> {
        let pr = self
            .api
            .get(&format!("/repos/{repo}/pulls/{number}"))
            .await?;
        let author = user(&pr["user"]);
        let head_sha = pr["head"]["sha"].as_str().unwrap_or("").to_string();
        let base_ref = pr["base"]["ref"].as_str().unwrap_or("main").to_string();
        let default_branch = pr["base"]["repo"]["default_branch"]
            .as_str()
            .unwrap_or("main")
            .to_string();
        let repo_id = pr["base"]["repo"]["id"].as_u64().unwrap_or(0);
        let locked = pr["locked"].as_bool().unwrap_or(false);
        let lifecycle = if pr["merged"].as_bool() == Some(true) || !pr["merged_at"].is_null() {
            Lifecycle::Merged
        } else if pr["state"].as_str() == Some("closed") {
            Lifecycle::Closed
        } else {
            Lifecycle::Open
        };
        let labels = labels(&pr);
        let mergeable_state = if pr["mergeable"].is_null() {
            None
        } else {
            pr["mergeable_state"].as_str().map(String::from)
        };

        if lifecycle != Lifecycle::Open || !active {
            // Closed threads, and repositories without a usable config, get
            // no evaluation beyond commands.
            return Ok(PullRequest {
                title: pr["title"].as_str().unwrap_or("").to_string(),
                lifecycle,
                draft: pr["draft"].as_bool().unwrap_or(false),
                created_at: time(&pr["created_at"]).unwrap_or(self.now),
                author,
                first_time: false,
                head_sha,
                base_ref,
                default_branch,
                repo_id,
                locked,
                mergeable_state,
                labels,
                files: Vec::new(),
                merge_commits: Vec::new(),
                commits: Vec::new(),
                checks: Vec::new(),
                coverage: None,
                reviews: Vec::new(),
                threads: Vec::new(),
                last_push_at: None,
                pullsift: Pullsift::default(),
                queued: false,
                own_checks: BTreeMap::new(),
                config_edit: None,
            });
        }

        let files: Vec<String> = self
            .api
            .get_all(&format!("/repos/{repo}/pulls/{number}/files"), None)
            .await?
            .iter()
            .filter_map(|f| f["filename"].as_str().map(String::from))
            .collect();
        let commits = self
            .api
            .get_all(&format!("/repos/{repo}/pulls/{number}/commits"), None)
            .await?;
        let merge_commits = commits
            .iter()
            .filter(|c| c["parents"].as_array().is_some_and(|p| p.len() > 1))
            .filter_map(|c| c["sha"].as_str().map(|s| s.chars().take(7).collect()))
            .collect();
        let head_commit_time = commits
            .last()
            .and_then(|c| time(&c["commit"]["committer"]["date"]));
        let commit_facts = if cla {
            self.commit_facts(&commits).await?
        } else {
            Vec::new()
        };

        let required: Vec<String> = match self
            .api
            .get(&format!(
                "/repos/{repo}/rules/branches/{}",
                urlencoding::encode(&base_ref)
            ))
            .await
        {
            Ok(rules) => rules
                .as_array()
                .into_iter()
                .flatten()
                .filter(|r| r["type"] == "required_status_checks")
                .flat_map(|r| {
                    r["parameters"]["required_status_checks"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .filter_map(|c| c["context"].as_str().map(String::from))
                .collect::<BTreeSet<String>>()
                .into_iter()
                .collect(),
            Err(e) if is_not_found(&e) => Vec::new(),
            Err(e) => return Err(e),
        };

        let mut runs = self
            .api
            .get_all(
                &format!("/repos/{repo}/commits/{head_sha}/check-runs?filter=latest"),
                Some("check_runs"),
            )
            .await?;
        runs.sort_by_key(|r| std::cmp::Reverse(r["id"].as_u64().unwrap_or(0)));
        let statuses = match self
            .api
            .get(&format!("/repos/{repo}/commits/{head_sha}/status"))
            .await
        {
            Ok(v) => v["statuses"].as_array().cloned().unwrap_or_default(),
            Err(e) if is_not_found(&e) => Vec::new(),
            Err(e) => return Err(e),
        };
        let held = !self
            .api
            .get_all(
                &format!("/repos/{repo}/actions/runs?head_sha={head_sha}&status=action_required"),
                Some("workflow_runs"),
            )
            .await
            .unwrap_or_default()
            .is_empty();

        let run_named = |name: &str| runs.iter().find(|r| r["name"].as_str() == Some(name));
        let status_named = |name: &str| {
            statuses
                .iter()
                .find(|s| s["context"].as_str() == Some(name))
        };

        let checks = required
            .iter()
            .map(|name| {
                let (class, url) = if let Some(r) = run_named(name) {
                    (run_class(r), r["html_url"].as_str().map(String::from))
                } else if let Some(s) = status_named(name) {
                    (status_class(s), s["target_url"].as_str().map(String::from))
                } else {
                    (CheckClass::Pending, None)
                };
                let class = if class == CheckClass::Pending && held {
                    CheckClass::Held
                } else {
                    class
                };
                RequiredCheck {
                    name: name.clone(),
                    class,
                    url,
                }
            })
            .collect();

        // Codecov publishes `codecov/patch` as a check run; a commit status
        // of that name is read the same way.
        let coverage = run_named(COVERAGE_CHECK)
            .map(|r| StatusFacts {
                state: r["conclusion"]
                    .as_str()
                    .or(r["status"].as_str())
                    .unwrap_or("pending")
                    .to_string(),
                description: r["output"]["title"].as_str().unwrap_or("").to_string(),
                url: r["details_url"].as_str().map(String::from),
            })
            .or_else(|| {
                status_named(COVERAGE_CHECK).map(|s| StatusFacts {
                    state: s["state"].as_str().unwrap_or("").to_string(),
                    description: s["description"].as_str().unwrap_or("").to_string(),
                    url: s["target_url"].as_str().map(String::from),
                })
            });

        let own_checks = runs
            .iter()
            .filter(|r| r["app"]["slug"].as_str() == Some(self.bot.slug.as_str()))
            .filter_map(|r| {
                let name = r["name"].as_str()?;
                (name == DOCS_CHECK || name == CONFIG_CHECK || name == CLA_CHECK).then(|| {
                    (
                        name.to_string(),
                        OwnCheck {
                            id: r["id"].as_u64().unwrap_or(0),
                            conclusion: r["conclusion"].as_str().map(String::from),
                            title: r["output"]["title"].as_str().unwrap_or("").to_string(),
                            summary: r["output"]["summary"].as_str().unwrap_or("").to_string(),
                        },
                    )
                })
            })
            .fold(BTreeMap::new(), |mut m, (k, v)| {
                m.entry(k).or_insert(v);
                m
            });

        let reviews = self
            .api
            .get_all(&format!("/repos/{repo}/pulls/{number}/reviews"), None)
            .await?
            .iter()
            .filter_map(|r| {
                let state = match r["state"].as_str()? {
                    "APPROVED" => ReviewState::Approved,
                    "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
                    "COMMENTED" => ReviewState::Commented,
                    "DISMISSED" => ReviewState::Dismissed,
                    _ => ReviewState::Pending,
                };
                Some(Review {
                    user: user(&r["user"]),
                    state,
                    submitted_at: time(&r["submitted_at"])?,
                })
            })
            .collect();

        let (threads, queued) = self.threads(repo, number).await?;

        let timeline = self
            .api
            .get_all(&format!("/repos/{repo}/issues/{number}/timeline"), None)
            .await?;
        let mut draft_by_pullsift = false;
        let mut force_push: Option<DateTime<Utc>> = None;
        for e in &timeline {
            match e["event"].as_str() {
                Some("convert_to_draft") => {
                    draft_by_pullsift = e["actor"]["login"].as_str() == Some(PULLSIFT_LOGIN);
                }
                Some("ready_for_review") => draft_by_pullsift = false,
                Some("head_ref_force_pushed") => {
                    force_push = force_push.max(time(&e["created_at"]));
                }
                _ => {}
            }
        }
        let draft = pr["draft"].as_bool().unwrap_or(false);
        let seen = db::last_push_seen(self.pool, repo, number as i64).await?;
        let last_push_at = [head_commit_time, force_push, seen]
            .into_iter()
            .flatten()
            .max();

        let pullsift_comment = comments
            .iter()
            .find(|c| c.user.login == PULLSIFT_LOGIN && c.body.contains(PULLSIFT_MARKER))
            .map(|c| PullsiftComment {
                passed: c.body.contains(PULLSIFT_PASS),
                url: c.url.clone(),
            });
        let pullsift = Pullsift {
            flagged: labels.contains(PULLSIFT_LABEL),
            holds_draft: draft && draft_by_pullsift,
            comment: pullsift_comment,
        };

        let config_edit = if files.iter().any(|f| f == CONFIG_PATH) {
            read_config_text(self.api, repo, &head_sha)
                .await?
                .map(|(_, text)| Config::parse(&text).err().unwrap_or_default())
        } else {
            None
        };

        let first_time = self.first_time(repo, number, &author).await?;

        Ok(PullRequest {
            title: pr["title"].as_str().unwrap_or("").to_string(),
            lifecycle,
            draft,
            created_at: time(&pr["created_at"]).unwrap_or(self.now),
            author,
            first_time,
            head_sha,
            base_ref,
            default_branch,
            repo_id,
            locked,
            mergeable_state,
            labels,
            files,
            merge_commits,
            commits: commit_facts,
            checks,
            coverage,
            reviews,
            threads,
            last_push_at,
            pullsift,
            queued,
            own_checks,
            config_edit,
        })
    }

    /// The people each commit names, resolved to accounts. The commit API
    /// resolves authors and committers; trailers go through [`Resolver`].
    async fn commit_facts(&self, commits: &[Value]) -> Result<Vec<CommitFacts>> {
        let mut resolver = Resolver {
            api: self.api,
            pool: self.pool,
            budget: cla::SEARCH_BUDGET,
        };
        let mut out = Vec::new();
        for c in commits {
            let side = |who: &str| {
                let git = &c["commit"][who];
                let account =
                    (!c[who].is_null() && c[who]["id"].as_u64().is_some()).then(|| user(&c[who]));
                (
                    git["email"].as_str().unwrap_or("").to_ascii_lowercase(),
                    Person {
                        name: git["name"].as_str().unwrap_or("").to_string(),
                        account,
                    },
                )
            };
            let (author_email, author) = side("author");
            let (committer_email, committer) = side("committer");
            for (email, p) in [(&author_email, &author), (&committer_email, &committer)] {
                if let Some(u) = &p.account {
                    cla::remember(self.pool, email, u).await?;
                }
            }
            let mut co_authors = Vec::new();
            for (name, email) in cla::co_authors(c["commit"]["message"].as_str().unwrap_or("")) {
                co_authors.push(resolver.person(&name, &email).await?);
            }
            out.push(CommitFacts {
                sha: c["sha"].as_str().unwrap_or("").to_string(),
                author,
                committer,
                co_authors,
            });
        }
        Ok(out)
    }

    async fn threads(&self, repo: &str, number: u64) -> Result<(Vec<Thread>, bool)> {
        let (owner, name) = repo.split_once('/').context("repo is owner/name")?;
        let mut cursor = Value::Null;
        let mut threads = Vec::new();
        let mut queued = false;
        for _ in 0..10 {
            let data = self
                .api
                .graphql(
                    PULL_STATE_QUERY,
                    json!({ "owner": owner, "name": name, "number": number, "cursor": cursor }),
                )
                .await?;
            let pr = &data["repository"]["pullRequest"];
            queued = pr["isInMergeQueue"].as_bool().unwrap_or(false);
            let rt = &pr["reviewThreads"];
            for t in rt["nodes"].as_array().into_iter().flatten() {
                threads.push(Thread {
                    resolved: t["isResolved"].as_bool().unwrap_or(false),
                    comments: t["comments"]["nodes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|c| {
                            let a = &c["author"];
                            Some(ThreadComment {
                                user: UserRef {
                                    id: a["databaseId"].as_u64().unwrap_or(0),
                                    login: a["login"].as_str().unwrap_or("").to_string(),
                                    is_bot: a["__typename"].as_str() == Some("Bot"),
                                },
                                created_at: time(&c["createdAt"])?,
                            })
                        })
                        .collect(),
                });
            }
            if rt["pageInfo"]["hasNextPage"].as_bool() == Some(true) {
                cursor = rt["pageInfo"]["endCursor"].clone();
            } else {
                break;
            }
        }
        Ok((threads, queued))
    }
}

fn run_class(r: &Value) -> CheckClass {
    if r["status"].as_str() != Some("completed") {
        return CheckClass::Pending;
    }
    match r["conclusion"].as_str() {
        Some("success" | "neutral" | "skipped") => CheckClass::Ok,
        _ => CheckClass::RedOwn,
    }
}

fn status_class(s: &Value) -> CheckClass {
    match s["state"].as_str() {
        Some("success") => CheckClass::Ok,
        Some("failure" | "error") => CheckClass::RedOwn,
        _ => CheckClass::Pending,
    }
}
