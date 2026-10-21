//! The receiver, the delivery log, and the worker's queue handling, against
//! a real Postgres (`DATABASE_URL`).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Notify;
use tower::ServiceExt;

use lumenfx_bot::db;
use lumenfx_bot::fake::FakeGitHub;
use lumenfx_bot::web::{self, AppState};
use lumenfx_bot::webhook::sign;
use lumenfx_bot::worker::Worker;
use lumenfx_engine::snapshot::BotIdentity;

const SECRET: &[u8] = b"s3cret";

fn router(pool: &PgPool, dry_run: bool) -> axum::Router {
    web::router(Arc::new(AppState {
        pool: pool.clone(),
        webhook_secret: SECRET.to_vec(),
        wake: Arc::new(Notify::new()),
        dry_run,
        slug: "lumenfx-bot".into(),
        started: chrono::Utc::now(),
        rate: Arc::new(|| vec![(1, 4999)]),
    }))
}

fn delivery(id: &str, body: &[u8], signature: &str) -> Request<Body> {
    Request::post("/github-hook")
        .header("x-github-event", "issue_comment")
        .header("x-github-delivery", id)
        .header("x-hub-signature-256", signature)
        .body(Body::from(body.to_vec()))
        .unwrap()
}

async fn text(r: axum::response::Response) -> String {
    String::from_utf8(r.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

#[sqlx::test(migrator = "lumenfx_bot::db::MIGRATOR")]
async fn a_bad_signature_is_refused_and_nothing_is_stored(pool: PgPool) {
    let body = br#"{"action":"created","issue":{"number":1},"repository":{"full_name":"o/r"}}"#;
    let r = router(&pool, false)
        .oneshot(delivery("d1", body, &sign(b"wrong", body)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = router(&pool, false)
        .oneshot(delivery("d1", body, ""))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[sqlx::test(migrator = "lumenfx_bot::db::MIGRATOR")]
async fn every_delivery_is_stored_once(pool: PgPool) {
    let body = br#"{"action":"created","issue":{"number":7},"repository":{"full_name":"o/r"},"installation":{"id":42}}"#;
    let sig = sign(SECRET, body);
    let r = router(&pool, false)
        .oneshot(delivery("d1", body, &sig))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let r = router(&pool, false)
        .oneshot(delivery("d1", body, &sig))
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        StatusCode::OK,
        "a redelivery is acknowledged and not stored twice"
    );
    let row: (String, Option<String>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT event, repo, number, installation_id FROM events WHERE delivery_id = 'd1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "issue_comment".into(),
            Some("o/r".into()),
            Some(7),
            Some(42)
        )
    );
    let r = router(&pool, false)
        .oneshot(delivery("d2", b"not json", &sign(SECRET, b"not json")))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "lumenfx_bot::db::MIGRATOR")]
async fn status_and_help_pages_render(pool: PgPool) {
    let r = router(&pool, true)
        .oneshot(Request::get("/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let t = text(r).await;
    assert!(t.contains("dry run"), "{t}");
    assert!(t.contains("Events waiting"));
    assert!(t.contains("4999"));
    let r = router(&pool, false)
        .oneshot(Request::get("/help").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let t = text(r).await;
    for c in lumenfx_engine::templates::COMMANDS {
        let usage = c.usage.replace('"', "&quot;");
        assert!(
            t.contains(&format!("@lumenfx-bot {usage}")),
            "{usage} missing from /help"
        );
    }
    assert!(t.contains("A bare word"));
    let r = router(&pool, false)
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

/// Several pending events for one thread are answered by one evaluation.
#[sqlx::test(migrator = "lumenfx_bot::db::MIGRATOR")]
async fn events_for_one_thread_coalesce(pool: PgPool) {
    let docs = BTreeMap::from([
        (
            "/repos/o/r".to_string(),
            json!({ "default_branch": "main" }),
        ),
        (
            "/repos/o/r/issues/3".to_string(),
            json!({ "number": 3, "state": "open", "title": "t", "user": { "login": "u", "id": 9 }, "labels": [] }),
        ),
    ]);
    let fake = FakeGitHub::new(docs, BTreeMap::new(), "lumenfx-bot", "2026-10-21T00:00:00Z");
    for i in 0..3 {
        db::insert_event(
            &pool,
            db::NewEvent {
                delivery_id: &format!("e{i}"),
                event: "issues",
                action: Some("edited"),
                repo: Some("o/r"),
                number: Some(3),
                installation_id: Some(1),
                payload: &json!({ "action": "edited", "issue": { "number": 3 }, "repository": { "full_name": "o/r" } }),
            },
        )
        .await
        .unwrap();
    }
    let w = Worker {
        pool: pool.clone(),
        factory: Arc::new(fake),
        bot: BotIdentity {
            slug: "lumenfx-bot".into(),
            login: "lumenfx-bot[bot]".into(),
            public_url: "https://bot.lumenfx.dev".into(),
        },
        dry_run: false,
        clock: lumenfx_bot::worker::system_clock(),
        settle_secs: 0.0,
        wake: Arc::new(Notify::new()),
    };
    assert!(w.run_once().await.unwrap());
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE processed_at IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pending, 0, "one evaluation answered all three events");
    assert!(!w.run_once().await.unwrap());
}

/// A thread another worker holds is put back, not evaluated twice at once.
#[sqlx::test(migrator = "lumenfx_bot::db::MIGRATOR")]
async fn a_locked_thread_waits(pool: PgPool) {
    let held = db::try_lock(&pool, "o/r", 5)
        .await
        .unwrap()
        .expect("first lock");
    assert!(db::try_lock(&pool, "o/r", 5).await.unwrap().is_none());
    let other = db::try_lock(&pool, "o/r", 6)
        .await
        .unwrap()
        .expect("another thread");
    held.unlock().await.unwrap();
    assert!(
        db::try_lock(&pool, "o/r", 5)
            .await
            .unwrap()
            .is_some_and(|l| {
                drop(l);
                true
            })
    );
    // A lock dropped without unlock (a panic mid-evaluation) is released
    // with its connection.
    drop(other);
    let again = db::try_lock(&pool, "o/r", 6).await.unwrap();
    assert!(
        again.is_some(),
        "a dropped lock must not leak into the pool"
    );
}
