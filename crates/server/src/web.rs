//! HTTP: the webhook endpoint and the read-only pages. No login, nothing
//! private: the pages show counts and the command list only.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::Notify;

use lumenfx_engine::templates::COMMANDS;

use crate::db::{self, NewEvent};
use crate::webhook::{thread_of, verify_signature};

pub type RateFn = Arc<dyn Fn() -> Vec<(u64, i64)> + Send + Sync>;

pub struct AppState {
    pub pool: PgPool,
    pub webhook_secret: Vec<u8>,
    pub wake: Arc<Notify>,
    pub dry_run: bool,
    pub slug: String,
    pub started: DateTime<Utc>,
    pub rate: RateFn,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/github-hook", post(hook))
        .route("/status", get(status))
        .route("/help", get(help))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(ready))
        .route(
            "/",
            get(|| async {
                Html(page(
                    "lumenfx bot",
                    "<p><a href=\"/help\">Commands</a> | <a href=\"/status\">Status</a></p>",
                ))
            }),
        )
        .with_state(state)
}

async fn hook(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let h = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    };
    if !verify_signature(&st.webhook_secret, &body, h("x-hub-signature-256")) {
        return (StatusCode::UNAUTHORIZED, "bad signature").into_response();
    }
    let event = h("x-github-event");
    let delivery = h("x-github-delivery");
    if event.is_empty() || delivery.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing event or delivery header").into_response();
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, "body is not JSON").into_response();
    };
    let inserted = db::insert_event(
        &st.pool,
        NewEvent {
            delivery_id: delivery,
            event,
            action: payload["action"].as_str(),
            repo: payload["repository"]["full_name"].as_str(),
            number: thread_of(event, &payload),
            installation_id: payload["installation"]["id"].as_i64(),
            payload: &payload,
        },
    )
    .await;
    match inserted {
        Ok(true) => {
            st.wake.notify_waiters();
            (StatusCode::ACCEPTED, "stored").into_response()
        }
        Ok(false) => (StatusCode::OK, "already stored").into_response(),
        Err(e) => {
            tracing::error!(error = %e, %delivery, "could not store delivery");
            (StatusCode::SERVICE_UNAVAILABLE, "could not store").into_response()
        }
    }
}

async fn ready(State(st): State<Arc<AppState>>) -> Response {
    match sqlx::query("SELECT 1").execute(&st.pool).await {
        Ok(_) => "ok".into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "database unreachable").into_response(),
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>{}</title>\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <style>body{{font:15px/1.5 system-ui,sans-serif;max-width:52rem;margin:2rem auto;padding:0 1rem}}\
         table{{border-collapse:collapse}}td,th{{border:1px solid #ccc;padding:.3rem .6rem;text-align:left;vertical-align:top}}\
         code{{background:#f3f3f3;padding:0 .2rem}}</style></head><body><h1>{}</h1>{}</body></html>",
        esc(title),
        esc(title),
        body
    )
}

fn when(t: Option<DateTime<Utc>>) -> String {
    t.map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "never".into())
}

async fn status(State(st): State<Arc<AppState>>) -> Response {
    let stats = match db::stats(&st.pool).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "status page");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Html(page(
                    "lumenfx bot status",
                    "<p>The database is unreachable.</p>",
                )),
            )
                .into_response();
        }
    };
    let reconcile = db::get_meta(&st.pool, "last_reconcile")
        .await
        .ok()
        .flatten()
        .and_then(|v| crate::snapshot::time(&v));
    let mut b = String::new();
    b.push_str("<table>");
    let row = |b: &mut String, k: &str, v: &str| {
        b.push_str(&format!("<tr><th>{}</th><td>{}</td></tr>", esc(k), esc(v)))
    };
    row(
        &mut b,
        "Mode",
        if st.dry_run {
            "dry run: plans are logged, nothing is written to GitHub"
        } else {
            "live"
        },
    );
    row(&mut b, "Running since", &when(Some(st.started)));
    row(&mut b, "Last delivery received", &when(stats.last_received));
    row(&mut b, "Last event processed", &when(stats.last_processed));
    row(&mut b, "Events waiting", &stats.pending.to_string());
    row(
        &mut b,
        "Events failing (5 or more attempts)",
        &stats.failed.to_string(),
    );
    row(&mut b, "Timers scheduled", &stats.jobs_pending.to_string());
    row(&mut b, "Last reconcile sweep", &when(reconcile));
    let rates = (st.rate)();
    if rates.is_empty() {
        row(&mut b, "GitHub rate limit remaining", "no request made yet");
    }
    for (inst, left) in rates {
        row(
            &mut b,
            &format!("GitHub rate limit remaining (installation {inst})"),
            &left.to_string(),
        );
    }
    b.push_str("</table><h2>Repositories</h2>");
    if stats.configs.is_empty() {
        b.push_str("<p>No repository config read yet.</p>");
    } else {
        b.push_str("<table><tr><th>Repository</th><th><code>.github/lumenfx.toml</code></th></tr>");
        for (repo, valid, errors) in &stats.configs {
            let state = if *valid {
                "valid".to_string()
            } else {
                format!("invalid: {}", errors.join("; "))
            };
            b.push_str(&format!(
                "<tr><td>{}</td><td>{}</td></tr>",
                esc(repo),
                esc(&state)
            ));
        }
        b.push_str("</table>");
    }
    Html(page("lumenfx bot status", &b)).into_response()
}

async fn help(State(st): State<Arc<AppState>>) -> Response {
    let mut b = format!(
        "<p>Every command starts with the mention: write <code>@{slug} &lt;command&gt;</code> on its own line \
         in a pull request or issue comment. A bare word such as <code>ready</code> does nothing. \
         Commands inside code, quotes, or HTML are ignored. \
         A command that worked gets a thumbs-up reaction; one that produced output or failed gets a reply.</p>\
         <table><tr><th>Command</th><th>Where</th><th>Who</th><th>Effect</th></tr>",
        slug = esc(&st.slug)
    );
    for c in COMMANDS {
        b.push_str(&format!(
            "<tr><td><code>@{} {}</code></td><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(&st.slug),
            esc(c.usage),
            esc(c.where_),
            esc(c.who),
            esc(&c.effect.replace('`', ""))
        ));
    }
    b.push_str(
        "</table><p>The bot follows <code>.github/lumenfx.toml</code> on each repository's default branch. \
         A repository without that file gets answers to <code>help</code> and nothing else.</p>",
    );
    (
        [(header::CACHE_CONTROL, "max-age=300")],
        Html(page("lumenfx bot commands", &b)),
    )
        .into_response()
}
