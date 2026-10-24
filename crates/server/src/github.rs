//! A thin GitHub client: App JWT -> installation token -> REST and GraphQL.
//!
//! Everything above this module talks to the [`Api`] trait, so tests run the
//! same code against [`crate::fake::FakeGitHub`].

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;
use serde_json::{Value, json};

pub const API: &str = "https://api.github.com";
const USER_AGENT: &str = concat!("lumenfx-bot/", env!("CARGO_PKG_VERSION"));

/// An HTTP error from GitHub, kept so callers can tell 404 from the rest.
#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    pub body: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GitHub answered {}: {}", self.status, self.body)
    }
}

impl std::error::Error for HttpError {}

pub fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<HttpError>()
        .is_some_and(|h| h.status == 404)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Post,
    Patch,
    Put,
    Delete,
}

/// The GitHub surface the bot uses, scoped to one installation.
pub trait Api: Send + Sync {
    /// GET one document.
    fn get(&self, path: &str) -> impl Future<Output = Result<Value>> + Send;
    /// GET every page. `field` names the array inside an object response
    /// (`check_runs`, `workflow_runs`); `None` for plain array responses.
    fn get_all(
        &self,
        path: &str,
        field: Option<&str>,
    ) -> impl Future<Output = Result<Vec<Value>>> + Send;
    fn graphql(&self, query: &str, variables: Value) -> impl Future<Output = Result<Value>> + Send;
    /// A write. Never called in dry run.
    fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> impl Future<Output = Result<Value>> + Send;
}

/// Makes an [`Api`] for an installation.
pub trait ApiFactory: Send + Sync + 'static {
    type Api: Api + 'static;
    fn for_installation(&self, installation_id: u64) -> Self::Api;
    /// An [`Api`] that may write the contents of one repository (`owner/name`)
    /// and nothing else: the CLA signature store.
    fn for_store(&self, installation_id: u64, repo: &str) -> Self::Api;
    /// Installations and their repositories, for the reconcile sweep.
    fn installations(&self) -> impl Future<Output = Result<Vec<(u64, Vec<String>)>>> + Send;
}

#[derive(Serialize)]
struct AppClaims {
    iat: i64,
    exp: i64,
    iss: String,
}

/// An installation, and the repository a store writer's token is limited to.
type TokenKey = (u64, Option<String>);

/// The App's credentials and a cache of installation tokens.
pub struct App {
    pub app_id: String,
    key: EncodingKey,
    http: reqwest::Client,
    base: String,
    tokens: Mutex<HashMap<TokenKey, (String, DateTime<Utc>)>>,
    /// x-ratelimit-remaining by installation, from the last response.
    pub rate: Mutex<HashMap<u64, i64>>,
}

impl App {
    pub fn new(app_id: &str, private_key_pem: &str, base: &str) -> Result<Arc<App>> {
        let key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes())
            .context("the GitHub App private key must be an RSA PEM")?;
        Ok(Arc::new(App {
            app_id: app_id.to_string(),
            key,
            http: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            base: base.trim_end_matches('/').to_string(),
            tokens: Mutex::new(HashMap::new()),
            rate: Mutex::new(HashMap::new()),
        }))
    }

    fn jwt(&self) -> Result<String> {
        let now = Utc::now().timestamp();
        let claims = AppClaims {
            iat: now - 60,
            exp: now + 9 * 60,
            iss: self.app_id.clone(),
        };
        Ok(jsonwebtoken::encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &self.key,
        )?)
    }

    /// The permissions the triage worker asks for. The App may hold more;
    /// a token scoped to these limits what a bug in this code can touch.
    fn worker_permissions() -> Value {
        json!({
            "issues": "write",
            "pull_requests": "write",
            "checks": "write",
            "statuses": "read",
            "contents": "read",
            "actions": "read",
            "metadata": "read",
        })
    }

    /// The CLA store writer's token: contents write on the store only.
    fn store_request(repo: &str) -> Value {
        let name = repo.rsplit('/').next().unwrap_or(repo);
        json!({
            "repositories": [name],
            "permissions": { "contents": "write", "metadata": "read" },
        })
    }

    async fn token(&self, installation_id: u64, scope: Option<&str>) -> Result<String> {
        let key = (installation_id, scope.map(String::from));
        if let Some((t, exp)) = self.tokens.lock().unwrap().get(&key)
            && *exp > Utc::now() + chrono::Duration::minutes(5)
        {
            return Ok(t.clone());
        }
        let request = match scope {
            Some(repo) => Self::store_request(repo),
            None => json!({ "permissions": Self::worker_permissions() }),
        };
        let url = format!(
            "{}/app/installations/{installation_id}/access_tokens",
            self.base
        );
        let resp = self
            .http
            .post(url)
            .bearer_auth(self.jwt()?)
            .header("Accept", "application/vnd.github+json")
            .json(&request)
            .send()
            .await?;
        let v: Value = check(resp).await?;
        let token = v["token"]
            .as_str()
            .context("no token in response")?
            .to_string();
        let exp = v["expires_at"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|| Utc::now() + chrono::Duration::minutes(50));
        self.tokens
            .lock()
            .unwrap()
            .insert(key, (token.clone(), exp));
        Ok(token)
    }

    async fn app_get_all(
        &self,
        path: &str,
        field: Option<&str>,
        bearer: &str,
    ) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut url = Some(self.url(path));
        while let Some(u) = url {
            let resp = self
                .http
                .get(&u)
                .bearer_auth(bearer)
                .header("Accept", "application/vnd.github+json")
                .send()
                .await?;
            url = next_link(resp.headers());
            let v: Value = check(resp).await?;
            extend(&mut out, v, field);
        }
        Ok(out)
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{}", self.base, path)
        }
    }
}

async fn check(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    if status.is_success() {
        if status.as_u16() == 204 {
            return Ok(Value::Null);
        }
        let text = resp.text().await?;
        if text.is_empty() {
            return Ok(Value::Null);
        }
        return Ok(serde_json::from_str(&text)?);
    }
    let body = resp.text().await.unwrap_or_default();
    Err(HttpError {
        status: status.as_u16(),
        body: body.chars().take(500).collect(),
    }
    .into())
}

fn next_link(h: &reqwest::header::HeaderMap) -> Option<String> {
    let link = h.get("link")?.to_str().ok()?;
    link.split(',').find_map(|part| {
        let (url, rel) = part.split_once(';')?;
        rel.contains("rel=\"next\"").then(|| {
            url.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
    })
}

fn extend(out: &mut Vec<Value>, v: Value, field: Option<&str>) {
    let items = match field {
        Some(f) => v.get(f).cloned().unwrap_or(Value::Null),
        None => v,
    };
    if let Value::Array(a) = items {
        out.extend(a);
    }
}

/// [`Api`] over HTTPS for one installation.
#[derive(Clone)]
pub struct RestApi {
    app: Arc<App>,
    installation_id: u64,
    /// Set for the store writer: the repository its token is limited to.
    scope: Option<String>,
}

impl RestApi {
    fn note_rate(&self, h: &reqwest::header::HeaderMap) {
        if let Some(r) = h
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
        {
            self.app
                .rate
                .lock()
                .unwrap()
                .insert(self.installation_id, r);
        }
    }

    async fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&Value>,
    ) -> Result<(Value, Option<String>)> {
        let token = self
            .app
            .token(self.installation_id, self.scope.as_deref())
            .await?;
        let mut req = self
            .app
            .http
            .request(method, url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await?;
        self.note_rate(resp.headers());
        let next = next_link(resp.headers());
        Ok((check(resp).await?, next))
    }
}

impl Api for RestApi {
    async fn get(&self, path: &str) -> Result<Value> {
        Ok(self
            .request(reqwest::Method::GET, &self.app.url(path), None)
            .await?
            .0)
    }

    async fn get_all(&self, path: &str, field: Option<&str>) -> Result<Vec<Value>> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut url = Some(self.app.url(&format!("{path}{sep}per_page=100")));
        let mut out = Vec::new();
        let mut pages = 0;
        while let Some(u) = url {
            let (v, next) = self.request(reqwest::Method::GET, &u, None).await?;
            extend(&mut out, v, field);
            url = next;
            pages += 1;
            if pages > 50 {
                return Err(anyhow!("more than 50 pages at {path}"));
            }
        }
        Ok(out)
    }

    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let body = json!({ "query": query, "variables": variables });
        let (v, _) = self
            .request(
                reqwest::Method::POST,
                &self.app.url("/graphql"),
                Some(&body),
            )
            .await?;
        if let Some(errs) = v.get("errors").filter(|e| !e.is_null()) {
            return Err(anyhow!("GraphQL errors: {errs}"));
        }
        Ok(v["data"].clone())
    }

    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let m = match method {
            Method::Post => reqwest::Method::POST,
            Method::Patch => reqwest::Method::PATCH,
            Method::Put => reqwest::Method::PUT,
            Method::Delete => reqwest::Method::DELETE,
        };
        Ok(self.request(m, &self.app.url(path), body.as_ref()).await?.0)
    }
}

impl ApiFactory for Arc<App> {
    type Api = RestApi;

    fn for_installation(&self, installation_id: u64) -> RestApi {
        RestApi {
            app: self.clone(),
            installation_id,
            scope: None,
        }
    }

    fn for_store(&self, installation_id: u64, repo: &str) -> RestApi {
        RestApi {
            app: self.clone(),
            installation_id,
            scope: Some(repo.to_string()),
        }
    }

    async fn installations(&self) -> Result<Vec<(u64, Vec<String>)>> {
        let jwt = self.jwt()?;
        let installs = self
            .app_get_all("/app/installations?per_page=100", None, &jwt)
            .await?;
        let mut out = Vec::new();
        for i in installs {
            let Some(id) = i["id"].as_u64() else { continue };
            let token = self.token(id, None).await?;
            let repos = self
                .app_get_all(
                    "/installation/repositories?per_page=100",
                    Some("repositories"),
                    &token,
                )
                .await?;
            out.push((
                id,
                repos
                    .iter()
                    .filter_map(|r| r["full_name"].as_str().map(String::from))
                    .collect(),
            ));
        }
        Ok(out)
    }
}

/// [`Api`] with a personal or `gh` token, for reading only: `send` always
/// fails. Used by `lumenfx-bot plan` to evaluate a live thread without
/// an App.
#[derive(Clone)]
pub struct ReadOnlyApi {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl ReadOnlyApi {
    pub fn new(token: &str, base: &str) -> Result<Self> {
        Ok(ReadOnlyApi {
            http: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{}", self.base, path)
        }
    }

    async fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&Value>,
    ) -> Result<(Value, Option<String>)> {
        let mut req = self
            .http
            .request(method, url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await?;
        let next = next_link(resp.headers());
        Ok((check(resp).await?, next))
    }
}

impl Api for ReadOnlyApi {
    async fn get(&self, path: &str) -> Result<Value> {
        Ok(self
            .request(reqwest::Method::GET, &self.url(path), None)
            .await?
            .0)
    }

    async fn get_all(&self, path: &str, field: Option<&str>) -> Result<Vec<Value>> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut url = Some(self.url(&format!("{path}{sep}per_page=100")));
        let mut out = Vec::new();
        while let Some(u) = url {
            let (v, next) = self.request(reqwest::Method::GET, &u, None).await?;
            extend(&mut out, v, field);
            url = next;
        }
        Ok(out)
    }

    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let body = json!({ "query": query, "variables": variables });
        let (v, _) = self
            .request(reqwest::Method::POST, &self.url("/graphql"), Some(&body))
            .await?;
        if let Some(errs) = v.get("errors").filter(|e| !e.is_null()) {
            return Err(anyhow!("GraphQL errors: {errs}"));
        }
        Ok(v["data"].clone())
    }

    async fn send(&self, _method: Method, path: &str, _body: Option<Value>) -> Result<Value> {
        Err(anyhow!("read-only client: refused a write to {path}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_link_is_found() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            "link",
            "<https://api.github.com/x?page=2>; rel=\"next\", <https://api.github.com/x?page=5>; rel=\"last\""
                .parse()
                .unwrap(),
        );
        assert_eq!(
            next_link(&h).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
        h.insert(
            "link",
            "<https://api.github.com/x?page=1>; rel=\"prev\""
                .parse()
                .unwrap(),
        );
        assert_eq!(next_link(&h), None);
    }
}
