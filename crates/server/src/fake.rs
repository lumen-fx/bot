//! An in-memory GitHub for tests and replay: GETs are served from recorded
//! documents, and the writes the bot makes are applied to those documents
//! and recorded, so a second evaluation sees the first one's effect.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::github::{Api, ApiFactory, HttpError, Method};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Write {
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub body: Value,
    /// The HTTP status of a write GitHub refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<u16>,
}

#[derive(Default)]
struct Inner {
    docs: BTreeMap<String, Value>,
    graphql: BTreeMap<String, Value>,
    writes: Vec<Write>,
    next_id: u64,
    /// Store writes that lose a race: another writer appends first.
    store_races: u32,
}

#[derive(Clone)]
pub struct FakeGitHub {
    inner: Arc<Mutex<Inner>>,
    /// Login and ID of the bot account, for comments it creates.
    bot_login: String,
    bot_slug: String,
    /// Timestamp given to created comments.
    now: String,
}

impl FakeGitHub {
    pub fn new(
        docs: BTreeMap<String, Value>,
        graphql: BTreeMap<String, Value>,
        bot_slug: &str,
        now: &str,
    ) -> Self {
        FakeGitHub {
            inner: Arc::new(Mutex::new(Inner {
                docs,
                graphql,
                writes: Vec::new(),
                next_id: 9_000_000,
                store_races: 0,
            })),
            bot_login: format!("{bot_slug}[bot]"),
            bot_slug: bot_slug.to_string(),
            now: now.to_string(),
        }
    }

    pub fn writes(&self) -> Vec<Write> {
        self.inner.lock().unwrap().writes.clone()
    }

    pub fn clear_writes(&self) {
        self.inner.lock().unwrap().writes.clear();
    }

    pub fn doc(&self, path: &str) -> Option<Value> {
        self.inner.lock().unwrap().docs.get(path).cloned()
    }

    /// Replace a document; `null` removes it.
    pub fn set_doc(&self, path: &str, v: Value) {
        let mut inner = self.inner.lock().unwrap();
        if v.is_null() {
            inner.docs.remove(path);
        } else {
            inner.docs.insert(path.to_string(), v);
        }
    }

    /// The next `n` writes to a contents path find that someone else wrote
    /// the file first: the fake appends a signature of its own and answers
    /// 409, as GitHub does when the blob SHA no longer matches.
    pub fn race_store_writes(&self, n: u32) {
        self.inner.lock().unwrap().store_races = n;
    }

    pub fn graphql_doc(&self, key: &str) -> Option<Value> {
        self.inner.lock().unwrap().graphql.get(key).cloned()
    }

    pub fn set_graphql(&self, key: &str, v: Value) {
        self.inner
            .lock()
            .unwrap()
            .graphql
            .insert(key.to_string(), v);
    }

    fn lookup(&self, path: &str) -> Result<Value> {
        let inner = self.inner.lock().unwrap();
        let bare = path.split('?').next().unwrap_or(path);
        inner
            .docs
            .get(path)
            .or_else(|| inner.docs.get(bare))
            .cloned()
            .ok_or_else(|| {
                HttpError {
                    status: 404,
                    body: format!("fake: no document at {path}"),
                }
                .into()
            })
    }

    fn apply(&self, method: Method, path: &str, body: &Option<Value>) -> Result<Value> {
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        let id = inner.next_id;
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let body = body.clone().unwrap_or(Value::Null);
        let bot = json!({ "login": self.bot_login, "id": 1, "type": "Bot" });
        match (method, parts.as_slice()) {
            (Method::Post, ["repos", o, r, "issues", n, "labels"]) => {
                let names: Vec<String> = body["labels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|l| l.as_str().map(String::from))
                    .collect();
                for doc in [
                    format!("/repos/{o}/{r}/pulls/{n}"),
                    format!("/repos/{o}/{r}/issues/{n}"),
                ] {
                    if let Some(d) = inner.docs.get_mut(&doc) {
                        let labels = d["labels"].as_array_mut().expect("labels array");
                        for name in &names {
                            if !labels.iter().any(|l| l["name"] == *name) {
                                labels.push(json!({ "name": name }));
                            }
                        }
                    }
                }
                Ok(json!([]))
            }
            (Method::Delete, ["repos", o, r, "issues", n, "labels", name]) => {
                let name = urlencoding::decode(name)?.into_owned();
                for doc in [
                    format!("/repos/{o}/{r}/pulls/{n}"),
                    format!("/repos/{o}/{r}/issues/{n}"),
                ] {
                    if let Some(d) = inner.docs.get_mut(&doc) {
                        d["labels"]
                            .as_array_mut()
                            .expect("labels array")
                            .retain(|l| l["name"] != *name);
                    }
                }
                Ok(json!([]))
            }
            (Method::Post, ["repos", o, r, "issues", n, "comments"]) => {
                let c = json!({
                    "id": id,
                    "user": bot,
                    "body": body["body"],
                    "created_at": self.now,
                    "html_url": format!("https://github.com/{o}/{r}/pull/{n}#issuecomment-{id}"),
                });
                inner
                    .docs
                    .entry(format!("/repos/{o}/{r}/issues/{n}/comments"))
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                    .expect("comments array")
                    .push(c.clone());
                Ok(c)
            }
            (Method::Patch, ["repos", _, _, "issues", "comments", cid]) => {
                let cid: u64 = cid.parse()?;
                for (k, d) in inner.docs.iter_mut() {
                    if !k.ends_with("/comments") {
                        continue;
                    }
                    if let Some(c) = d
                        .as_array_mut()
                        .and_then(|a| a.iter_mut().find(|c| c["id"].as_u64() == Some(cid)))
                    {
                        c["body"] = body["body"].clone();
                        return Ok(c.clone());
                    }
                }
                Err(HttpError {
                    status: 404,
                    body: "no such comment".into(),
                }
                .into())
            }
            (Method::Put, ["repos", o, r, "issues", n, "lock"]) => {
                for doc in [
                    format!("/repos/{o}/{r}/pulls/{n}"),
                    format!("/repos/{o}/{r}/issues/{n}"),
                ] {
                    if let Some(d) = inner.docs.get_mut(&doc) {
                        d["locked"] = json!(true);
                    }
                }
                Ok(Value::Null)
            }
            (Method::Put, ["repos", o, r, "contents", rest @ ..]) => {
                let path = format!("/repos/{o}/{r}/contents/{}", rest.join("/"));
                let current = inner
                    .docs
                    .get(&path)
                    .and_then(|d| d["sha"].as_str())
                    .map(String::from);
                if inner.store_races > 0 {
                    inner.store_races -= 1;
                    let mut file = current_file(inner.docs.get(&path));
                    file["signedContributors"]
                        .as_array_mut()
                        .expect("signedContributors")
                        .push(json!({
                            "name": "racer", "id": 4242, "comment_id": 1,
                            "created_at": "2026-10-22T00:00:00Z", "repoId": 1, "pullRequestNo": 1,
                        }));
                    let text = serde_json::to_string_pretty(&file)?;
                    inner
                        .docs
                        .insert(path, file_doc(&format!("race{id}"), &text));
                    return Err(conflict());
                }
                if body["sha"].as_str() != current.as_deref() {
                    return Err(conflict());
                }
                let content = body["content"].as_str().unwrap_or("");
                let text =
                    String::from_utf8(base64::engine::general_purpose::STANDARD.decode(content)?)?;
                let sha = format!("blob{id}");
                inner.docs.insert(path, file_doc(&sha, &text));
                Ok(json!({ "content": { "sha": sha } }))
            }
            (Method::Post, ["repos", _, _, "issues", "comments", _, "reactions"]) => {
                Ok(json!({ "id": id }))
            }
            (Method::Post, ["repos", o, r, "check-runs"]) => {
                let sha = body["head_sha"].as_str().unwrap_or_default().to_string();
                let run = json!({
                    "id": id,
                    "name": body["name"],
                    "head_sha": sha,
                    "status": "completed",
                    "conclusion": body["conclusion"],
                    "app": { "slug": self.bot_slug },
                    "output": body["output"],
                });
                let doc = inner
                    .docs
                    .entry(format!("/repos/{o}/{r}/commits/{sha}/check-runs"))
                    .or_insert_with(|| json!({ "check_runs": [] }));
                let runs = doc["check_runs"].as_array_mut().expect("check_runs");
                runs.retain(|c| {
                    !(c["name"] == body["name"] && c["app"]["slug"] == self.bot_slug.as_str())
                });
                runs.push(run.clone());
                Ok(run)
            }
            (Method::Patch, ["repos", _, _, "check-runs", cid]) => {
                let cid: u64 = cid.parse()?;
                for (k, d) in inner.docs.iter_mut() {
                    if !k.ends_with("/check-runs") {
                        continue;
                    }
                    if let Some(c) = d["check_runs"]
                        .as_array_mut()
                        .and_then(|a| a.iter_mut().find(|c| c["id"].as_u64() == Some(cid)))
                    {
                        c["conclusion"] = body["conclusion"].clone();
                        c["output"] = body["output"].clone();
                        return Ok(c.clone());
                    }
                }
                Err(HttpError {
                    status: 404,
                    body: "no such check run".into(),
                }
                .into())
            }
            _ => Ok(Value::Null),
        }
    }
}

fn conflict() -> anyhow::Error {
    HttpError {
        status: 409,
        body: "fake: the file changed".into(),
    }
    .into()
}

/// A contents document for a file.
pub fn file_doc(sha: &str, text: &str) -> Value {
    json!({
        "sha": sha,
        "size": text.len(),
        "content": base64::engine::general_purpose::STANDARD.encode(text),
    })
}

/// The JSON a contents document holds; an empty store when there is none.
pub fn current_file(doc: Option<&Value>) -> Value {
    doc.and_then(|d| d["content"].as_str())
        .and_then(|c| base64::engine::general_purpose::STANDARD.decode(c).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({ "signedContributors": [] }))
}

impl Api for FakeGitHub {
    async fn get(&self, path: &str) -> Result<Value> {
        self.lookup(path)
    }

    async fn get_all(&self, path: &str, field: Option<&str>) -> Result<Vec<Value>> {
        let v = match self.lookup(path) {
            Ok(v) => v,
            Err(e) if crate::github::is_not_found(&e) => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        let items = match field {
            Some(f) => v.get(f).cloned().unwrap_or(Value::Null),
            None => v,
        };
        Ok(items.as_array().cloned().unwrap_or_default())
    }

    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let inner = self.inner.lock().unwrap();
        let key = format!("{}#{}", graphql_name(query), variables["number"]);
        Ok(inner.graphql.get(&key).cloned().unwrap_or(Value::Null))
    }

    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let out = self.apply(method, path, &body);
        let refused = out
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<HttpError>().map(|h| h.status));
        self.inner.lock().unwrap().writes.push(Write {
            method: format!("{method:?}").to_uppercase(),
            path: path.to_string(),
            body: body.unwrap_or(Value::Null),
            refused,
        });
        out
    }
}

/// `query PullState(...)` -> `PullState`.
fn graphql_name(q: &str) -> &str {
    q.trim_start()
        .strip_prefix("query")
        .unwrap_or(q)
        .trim_start()
        .split(|c: char| !c.is_alphanumeric())
        .next()
        .unwrap_or("")
}

impl ApiFactory for FakeGitHub {
    type Api = FakeGitHub;

    fn for_installation(&self, _installation_id: u64) -> FakeGitHub {
        self.clone()
    }

    fn for_store(&self, _installation_id: u64, _repo: &str) -> FakeGitHub {
        self.clone()
    }

    async fn installations(&self) -> Result<Vec<(u64, Vec<String>)>> {
        Ok(Vec::new())
    }
}
