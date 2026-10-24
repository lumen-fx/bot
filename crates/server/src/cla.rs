//! The CLA's I/O: the signature store in CLA Assistant's format, commit
//! emails resolved to accounts, and the pull requests of a merge group.

use anyhow::{Context, Result, bail};
use base64::Engine;
use chrono::SecondsFormat;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::{PgPool, Row};

use lumenfx_engine::cla::{CLA_CHECK, GroupMember};
use lumenfx_engine::plan::ClaSignature;
use lumenfx_engine::snapshot::{CheckState, ClaStore, Person, UserRef};

use crate::github::{Api, HttpError, Method, is_not_found};

/// How long a read of the store is trusted. The bot is the store's only
/// writer and updates the cache when it writes, so this only bounds how
/// long a hand edit of the file goes unseen; `recheck` reads it at once.
pub const STORE_TTL_SECS: i64 = 3600;
/// Attempts at a store write that lost a race with another writer.
pub const WRITE_ATTEMPTS: u32 = 5;
/// Email searches one evaluation may make (the search API allows 30 a
/// minute).
pub const SEARCH_BUDGET: u32 = 10;

/// The signature file, as CLA Assistant writes it. Unknown fields are kept.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct StoreFile {
    #[serde(rename = "signedContributors", default)]
    pub signed: Vec<Entry>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(rename = "repoId", default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<u64>,
    #[serde(
        rename = "pullRequestNo",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub pull_request_no: Option<u64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// JSON.stringify(file, null, 2), which is what CLA Assistant writes.
pub fn render(file: &StoreFile) -> Result<String> {
    Ok(serde_json::to_string_pretty(file)?)
}

/// Read the file: its blob SHA (`None` when it does not exist yet) and
/// contents.
pub async fn read_file<A: Api>(
    api: &A,
    store: &str,
    path: &str,
) -> Result<(Option<String>, StoreFile)> {
    let v = match api.get(&format!("/repos/{store}/contents/{path}")).await {
        Ok(v) => v,
        Err(e) if is_not_found(&e) => return Ok((None, StoreFile::default())),
        Err(e) => return Err(e),
    };
    let sha = v["sha"]
        .as_str()
        .context("no sha for the signature file")?
        .to_string();
    let mut b64: String = v["content"]
        .as_str()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    // Files over 1 MB come without content; the blob API has it.
    if b64.is_empty() && v["size"].as_u64().unwrap_or(0) > 0 {
        let blob = api.get(&format!("/repos/{store}/git/blobs/{sha}")).await?;
        b64 = blob["content"]
            .as_str()
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("the signature file is not base64")?;
    let file: StoreFile =
        serde_json::from_slice(&bytes).context("the signature file is not CLA Assistant JSON")?;
    Ok((Some(sha), file))
}

async fn cache(pool: &PgPool, store: &str, path: &str, sha: &str, file: &StoreFile) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM cla_signers WHERE store = $1 AND path = $2")
        .bind(store)
        .bind(path)
        .execute(&mut *tx)
        .await?;
    for e in &file.signed {
        sqlx::query(
            "INSERT INTO cla_signers (store, path, github_id, login_at_signing, signed_at, comment_id, store_sha)
             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING",
        )
        .bind(store)
        .bind(path)
        .bind(e.id as i64)
        .bind(&e.name)
        .bind(e.created_at.as_deref().unwrap_or(""))
        .bind(e.comment_id.map(|c| c as i64))
        .bind(sha)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "INSERT INTO cla_store (store, path, blob_sha, read_at) VALUES ($1, $2, $3, now())
         ON CONFLICT (store, path) DO UPDATE SET blob_sha = EXCLUDED.blob_sha, read_at = now()",
    )
    .bind(store)
    .bind(path)
    .bind(sha)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Who signed, from the cache when it is fresh and `fresh` is not asked
/// for, from the store otherwise. A store that cannot be read gives
/// `Unreadable`, never an empty list.
pub async fn signers<A: Api>(
    api: &A,
    pool: &PgPool,
    store: &str,
    path: &str,
    fresh: bool,
) -> Result<ClaStore> {
    if !fresh {
        let cached: Option<String> = sqlx::query_scalar(
            "SELECT blob_sha FROM cla_store WHERE store = $1 AND path = $2
             AND read_at > now() - make_interval(secs => $3)",
        )
        .bind(store)
        .bind(path)
        .bind(STORE_TTL_SECS as f64)
        .fetch_optional(pool)
        .await?;
        if cached.is_some() {
            let ids: Vec<i64> = sqlx::query_scalar(
                "SELECT github_id FROM cla_signers WHERE store = $1 AND path = $2",
            )
            .bind(store)
            .bind(path)
            .fetch_all(pool)
            .await?;
            return Ok(ClaStore::Read(ids.into_iter().map(|i| i as u64).collect()));
        }
    }
    match read_file(api, store, path).await {
        Ok((sha, file)) => {
            cache(pool, store, path, sha.as_deref().unwrap_or(""), &file).await?;
            Ok(ClaStore::Read(file.signed.iter().map(|e| e.id).collect()))
        }
        Err(e) => {
            tracing::warn!(%store, %path, error = %format!("{e:#}"), "signature store unreadable");
            Ok(ClaStore::Unreadable(short_error(&e)))
        }
    }
}

fn short_error(e: &anyhow::Error) -> String {
    match e.downcast_ref::<HttpError>() {
        Some(h) => format!("GitHub answered {}", h.status),
        None => format!("{e}").chars().take(120).collect(),
    }
}

fn is_conflict(e: &anyhow::Error) -> bool {
    e.downcast_ref::<HttpError>()
        .is_some_and(|h| h.status == 409 || (h.status == 422 && h.body.contains("sha")))
}

/// Append a signature: read the file with its blob SHA, add the entry, and
/// write it back conditioned on that SHA. Another writer in between makes
/// GitHub answer 409; then read again and retry. Already present: nothing
/// to write.
pub async fn append<A: Api>(api: &A, pool: &PgPool, sig: &ClaSignature) -> Result<bool> {
    for attempt in 1..=WRITE_ATTEMPTS {
        let (sha, mut file) = read_file(api, &sig.store, &sig.path).await?;
        if file.signed.iter().any(|e| e.id == sig.id) {
            cache(
                pool,
                &sig.store,
                &sig.path,
                sha.as_deref().unwrap_or(""),
                &file,
            )
            .await?;
            return Ok(false);
        }
        file.signed.push(Entry {
            name: sig.name.clone(),
            id: sig.id,
            comment_id: Some(sig.comment_id),
            created_at: Some(sig.created_at.to_rfc3339_opts(SecondsFormat::Secs, true)),
            repo_id: Some(sig.repo_id),
            pull_request_no: Some(sig.pull_request),
            extra: Map::new(),
        });
        let content = base64::engine::general_purpose::STANDARD.encode(render(&file)?);
        let mut body = json!({
            "message": format!("{} signed the CLA in {}#{}", sig.name, sig.repo, sig.pull_request),
            "content": content,
        });
        if let Some(s) = &sha {
            body["sha"] = json!(s);
        }
        match api
            .send(
                Method::Put,
                &format!("/repos/{}/contents/{}", sig.store, sig.path),
                Some(body),
            )
            .await
        {
            Ok(v) => {
                let new_sha = v["content"]["sha"].as_str().unwrap_or("").to_string();
                cache(pool, &sig.store, &sig.path, &new_sha, &file).await?;
                return Ok(true);
            }
            Err(e) if is_conflict(&e) => {
                tracing::info!(store = %sig.store, attempt, "signature store changed underneath; retrying");
            }
            Err(e) => return Err(e),
        }
    }
    bail!(
        "the signature store kept changing; {} attempts at signing {} failed",
        WRITE_ATTEMPTS,
        sig.name
    )
}

/// `Co-authored-by: Name <email>` trailers of a commit message.
pub fn co_authors(message: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in message.lines() {
        let line = line.trim();
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("co-authored-by") {
            continue;
        }
        let rest = rest.trim();
        let (Some(open), true) = (rest.rfind('<'), rest.ends_with('>')) else {
            continue;
        };
        let name = rest[..open].trim().to_string();
        let email = rest[open + 1..rest.len() - 1].trim().to_ascii_lowercase();
        if !email.is_empty() && !out.iter().any(|(_, e)| *e == email) {
            out.push((name, email));
        }
    }
    out
}

/// The account a GitHub noreply address names, without asking anyone:
/// `ID+login@users.noreply.github.com`.
fn noreply(email: &str) -> Option<UserRef> {
    let local = email.strip_suffix("@users.noreply.github.com")?;
    let (id, login) = local.split_once('+')?;
    Some(UserRef {
        id: id.parse().ok()?,
        login: login.to_string(),
        is_bot: login.ends_with("[bot]"),
    })
}

/// Remember an email a commit's author or committer resolved to.
pub async fn remember(pool: &PgPool, email: &str, u: &UserRef) -> Result<()> {
    if email.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO email_accounts (email, account_id, login, is_bot, resolved_at)
         VALUES ($1, $2, $3, $4, now())
         ON CONFLICT (email) DO UPDATE SET account_id = EXCLUDED.account_id, login = EXCLUDED.login,
           is_bot = EXCLUDED.is_bot, resolved_at = now()",
    )
    .bind(email.to_ascii_lowercase())
    .bind(u.id as i64)
    .bind(&u.login)
    .bind(u.is_bot)
    .execute(pool)
    .await?;
    Ok(())
}

/// Resolves trailer emails to accounts: noreply addresses directly, then
/// emails already seen on commits or looked up before, then the user search
/// (public emails only). Positive answers are kept 30 days, negative ones a
/// day. Searches past the budget, and searches that fail, resolve to no
/// account this time and are not cached, so a later evaluation asks again.
pub struct Resolver<'a, A: Api> {
    pub api: &'a A,
    pub pool: &'a PgPool,
    pub budget: u32,
}

impl<A: Api> Resolver<'_, A> {
    pub async fn person(&mut self, name: &str, email: &str) -> Result<Person> {
        Ok(Person {
            name: name.to_string(),
            account: self.account(email).await?,
        })
    }

    async fn account(&mut self, email: &str) -> Result<Option<UserRef>> {
        if let Some(u) = noreply(email) {
            return Ok(Some(u));
        }
        let row = sqlx::query(
            "SELECT account_id, login, is_bot FROM email_accounts WHERE email = $1
             AND resolved_at > now() - CASE WHEN account_id IS NULL THEN interval '1 day' ELSE interval '30 days' END",
        )
        .bind(email)
        .fetch_optional(self.pool)
        .await?;
        if let Some(r) = row {
            let id: Option<i64> = r.get("account_id");
            return Ok(id.map(|id| UserRef {
                id: id as u64,
                login: r.get::<Option<String>, _>("login").unwrap_or_default(),
                is_bot: r.get("is_bot"),
            }));
        }
        if self.budget == 0 {
            tracing::info!(%email, "email search budget spent; resolving later");
            return Ok(None);
        }
        self.budget -= 1;
        let q = format!("{email} in:email");
        let v = match self
            .api
            .get(&format!("/search/users?q={}", urlencoding::encode(&q)))
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(%email, error = %e, "email search failed");
                return Ok(None);
            }
        };
        let found = (v["total_count"].as_u64() == Some(1))
            .then(|| &v["items"][0])
            .and_then(|i| {
                Some(UserRef {
                    id: i["id"].as_u64()?,
                    login: i["login"].as_str()?.to_string(),
                    is_bot: i["type"].as_str() == Some("Bot"),
                })
            });
        sqlx::query(
            "INSERT INTO email_accounts (email, account_id, login, is_bot, resolved_at)
             VALUES ($1, $2, $3, $4, now())
             ON CONFLICT (email) DO UPDATE SET account_id = EXCLUDED.account_id, login = EXCLUDED.login,
               is_bot = EXCLUDED.is_bot, resolved_at = now()",
        )
        .bind(email)
        .bind(found.as_ref().map(|u| u.id as i64))
        .bind(found.as_ref().map(|u| u.login.clone()))
        .bind(found.as_ref().is_some_and(|u| u.is_bot))
        .execute(self.pool)
        .await?;
        Ok(found)
    }
}

pub const MERGE_QUEUE_QUERY: &str = r#"query MergeQueue($owner: String!, $name: String!, $branch: String!) {
  repository(owner: $owner, name: $name) {
    mergeQueue(branch: $branch) {
      entries(first: 100) {
        nodes { position headCommit { oid } pullRequest { number headRefOid } }
      }
    }
  }
}"#;

/// `refs/heads/gh-readonly-queue/main/pr-562-<sha>` -> (`main`, 562).
pub fn parse_group_ref(head_ref: &str) -> Option<(String, u64)> {
    let rest = head_ref
        .trim_start_matches("refs/heads/")
        .strip_prefix("gh-readonly-queue/")?;
    let (base, last) = rest.rsplit_once('/')?;
    let n = last.strip_prefix("pr-")?.split('-').next()?.parse().ok()?;
    Some((base.to_string(), n))
}

/// The pull requests in the merge group headed by `head_sha`, with the
/// queued head of each: every queue entry up to and including the one this
/// group was built for. When the queue no longer lists it, the pull
/// request named in the group's ref.
pub async fn group_members<A: Api>(
    api: &A,
    repo: &str,
    head_sha: &str,
    head_ref: &str,
    bot_slug: &str,
) -> Result<Vec<GroupMember>> {
    let Some((base, named)) = parse_group_ref(head_ref) else {
        return Ok(Vec::new());
    };
    let (owner, name) = repo.split_once('/').context("repo is owner/name")?;
    let data = api
        .graphql(
            MERGE_QUEUE_QUERY,
            json!({ "owner": owner, "name": name, "branch": base }),
        )
        .await?;
    let entries: Vec<(i64, String, u64, String)> =
        data["repository"]["mergeQueue"]["entries"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| {
                Some((
                    e["position"].as_i64()?,
                    e["headCommit"]["oid"].as_str().unwrap_or("").to_string(),
                    e["pullRequest"]["number"].as_u64()?,
                    e["pullRequest"]["headRefOid"].as_str()?.to_string(),
                ))
            })
            .collect();
    let mut heads: Vec<(u64, String)> = match entries.iter().find(|e| e.1 == head_sha) {
        Some(own) => {
            let mut v: Vec<&(i64, String, u64, String)> =
                entries.iter().filter(|e| e.0 <= own.0).collect();
            v.sort_by_key(|e| e.0);
            v.into_iter().map(|e| (e.2, e.3.clone())).collect()
        }
        None => {
            let pr = api.get(&format!("/repos/{repo}/pulls/{named}")).await?;
            vec![(named, pr["head"]["sha"].as_str().unwrap_or("").to_string())]
        }
    };
    heads.dedup();
    let mut out = Vec::new();
    for (number, sha) in heads {
        let cla = own_cla(api, repo, &sha, bot_slug).await?;
        out.push(GroupMember {
            number,
            head_sha: sha,
            cla,
        });
    }
    Ok(out)
}

/// The newest `cla` check run this App published on a commit.
pub async fn own_cla<A: Api>(
    api: &A,
    repo: &str,
    sha: &str,
    bot_slug: &str,
) -> Result<Option<CheckState>> {
    let runs = api
        .get_all(
            &format!("/repos/{repo}/commits/{sha}/check-runs?check_name={CLA_CHECK}"),
            Some("check_runs"),
        )
        .await?;
    Ok(runs
        .iter()
        .filter(|r| r["name"] == CLA_CHECK && r["app"]["slug"].as_str() == Some(bot_slug))
        .max_by_key(|r| r["id"].as_u64().unwrap_or(0))
        .map(|r| {
            if r["status"].as_str() != Some("completed") {
                CheckState::Pending
            } else if r["conclusion"].as_str() == Some("success") {
                CheckState::Success
            } else {
                CheckState::Failure
            }
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailers_are_parsed_and_deduplicated() {
        let msg = "fix: x\n\nbody\n\nCo-authored-by: Bea Baker <Bea@Example.com>\nco-authored-by:Claude <noreply@anthropic.com>\nCo-Authored-By: B <bea@example.com>\nSigned-off-by: A <a@b.c>\nCo-authored-by: broken";
        assert_eq!(
            co_authors(msg),
            vec![
                ("Bea Baker".to_string(), "bea@example.com".to_string()),
                ("Claude".to_string(), "noreply@anthropic.com".to_string()),
            ]
        );
    }

    #[test]
    fn noreply_addresses_name_their_account() {
        assert_eq!(
            noreply("126825095+FizzWizZleDazzle@users.noreply.github.com"),
            Some(UserRef {
                id: 126825095,
                login: "FizzWizZleDazzle".into(),
                is_bot: false
            })
        );
        assert_eq!(
            noreply("49699333+dependabot[bot]@users.noreply.github.com").map(|u| u.is_bot),
            Some(true)
        );
        assert_eq!(noreply("someone@users.noreply.github.com"), None);
        assert_eq!(noreply("a@example.com"), None);
    }

    #[test]
    fn group_refs() {
        assert_eq!(
            parse_group_ref(
                "refs/heads/gh-readonly-queue/main/pr-562-2409434594939b851a82a98737422f3e4ceef08e"
            ),
            Some(("main".into(), 562))
        );
        assert_eq!(
            parse_group_ref("gh-readonly-queue/release/1.x/pr-7-abc"),
            Some(("release/1.x".into(), 7))
        );
        assert_eq!(parse_group_ref("refs/heads/main"), None);
    }

    #[test]
    fn the_file_round_trips_in_cla_assistant_format() {
        let text = "{\n  \"signedContributors\": [\n    {\n      \"name\": \"AeroForger\",\n      \"id\": 313325032,\n      \"comment_id\": 5667357883,\n      \"created_at\": \"2026-09-14T16:37:29Z\",\n      \"repoId\": 1328973863,\n      \"pullRequestNo\": 355\n    }\n  ]\n}";
        let f: StoreFile = serde_json::from_str(text).unwrap();
        assert_eq!(render(&f).unwrap(), text);
        assert_eq!(
            render(&StoreFile::default()).unwrap(),
            "{\n  \"signedContributors\": []\n}"
        );
    }
}
