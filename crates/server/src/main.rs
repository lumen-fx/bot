//! `lumenfx-bot serve | migrate | check-config FILE`.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::sync::Notify;

use lumenfx_bot::github::{API, App, ReadOnlyApi};
use lumenfx_bot::snapshot::Builder;
use lumenfx_bot::worker::{Worker, system_clock};
use lumenfx_bot::{db, scheduler, web};
use lumenfx_engine::config::Config;
use lumenfx_engine::snapshot::BotIdentity;

fn env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is not set"))
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// A secret from `NAME`, or from the file `NAME_FILE` names.
fn secret(name: &str) -> Result<String> {
    if let Ok(v) = std::env::var(name) {
        return Ok(v);
    }
    let path =
        env(&format!("{name}_FILE")).with_context(|| format!("set {name} or {name}_FILE"))?;
    std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str).unwrap_or("serve") {
        "serve" => serve().await,
        "migrate" => {
            let pool = db::connect(&env("DATABASE_URL")?).await?;
            db::migrate(&pool).await?;
            println!("migrations applied");
            Ok(())
        }
        "check-config" => {
            let path = args
                .get(1)
                .context("usage: lumenfx-bot check-config FILE")?;
            let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
            match Config::parse(&text) {
                Ok(_) => {
                    println!("{path}: valid");
                    Ok(())
                }
                Err(errors) => {
                    for e in &errors {
                        eprintln!("{path}: {e}");
                    }
                    bail!("{} error(s)", errors.len())
                }
            }
        }
        "plan" => plan(&args[1..]).await,
        other => bail!(
            "unknown command {other}; use serve, migrate, check-config FILE, or plan OWNER/REPO NUMBER"
        ),
    }
}

/// `plan OWNER/REPO NUMBER [--config FILE]`: read a live thread with
/// `GITHUB_TOKEN`, evaluate it, print the plan. Writes nothing to GitHub;
/// the database (`DATABASE_URL`) only caches what a snapshot reads.
async fn plan(args: &[String]) -> Result<()> {
    let usage = "usage: lumenfx-bot plan OWNER/REPO NUMBER [--config FILE]";
    let repo = args.first().context(usage)?;
    let number: u64 = args.get(1).context(usage)?.parse().context(usage)?;
    let config = match args.iter().position(|a| a == "--config") {
        Some(i) => {
            let path = args.get(i + 1).context(usage)?;
            let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
            Some(Config::parse(&text).map_err(|e| anyhow::anyhow!("{path}: {}", e.join("; ")))?)
        }
        None => None,
    };
    let api = ReadOnlyApi::new(&env("GITHUB_TOKEN")?, &env_or("GITHUB_API_URL", API))?;
    let pool = db::connect(&env("DATABASE_URL")?).await?;
    db::migrate(&pool).await?;
    let slug = env_or("GITHUB_APP_SLUG", "lumenfx-bot");
    let bot = BotIdentity {
        login: format!("{slug}[bot]"),
        slug,
        public_url: env_or("PUBLIC_URL", "https://bot.lumenfx.dev"),
    };
    let snapshot = Builder {
        api: &api,
        pool: &pool,
        bot: &bot,
        now: chrono::Utc::now(),
        config_override: config.as_ref(),
    }
    .build(repo, number)
    .await?;
    let plan = lumenfx_engine::evaluate(&snapshot);
    println!("{}", serde_json::to_string_pretty(&plan)?);
    Ok(())
}

async fn serve() -> Result<()> {
    let dry_run = matches!(
        env_or("LUMENFX_DRY_RUN", "0").as_str(),
        "1" | "true" | "yes"
    );
    let slug = env_or("GITHUB_APP_SLUG", "lumenfx-bot");
    let bot = BotIdentity {
        login: format!("{slug}[bot]"),
        slug: slug.clone(),
        public_url: env_or("PUBLIC_URL", "https://bot.lumenfx.dev"),
    };
    let pool = db::connect(&env("DATABASE_URL")?).await?;
    db::migrate(&pool).await?;
    let app = App::new(
        &env("GITHUB_APP_ID")?,
        &secret("GITHUB_PRIVATE_KEY")?,
        &env_or("GITHUB_API_URL", API),
    )?;
    let webhook_secret = secret("GITHUB_WEBHOOK_SECRET")?.trim().as_bytes().to_vec();
    if webhook_secret.is_empty() {
        bail!("the webhook secret is empty");
    }
    let wake = Arc::new(Notify::new());
    let workers: usize = env_or("WORKERS", "4")
        .parse()
        .context("WORKERS must be a number")?;

    let worker = Arc::new(Worker {
        pool: pool.clone(),
        factory: Arc::new(app.clone()),
        bot,
        dry_run,
        clock: system_clock(),
        settle_secs: 2.0,
        wake: wake.clone(),
    });
    for _ in 0..workers.max(1) {
        tokio::spawn(worker.clone().run());
    }
    tokio::spawn(scheduler::run(
        pool.clone(),
        Arc::new(app.clone()),
        wake.clone(),
    ));

    let rate_app = app.clone();
    let state = Arc::new(web::AppState {
        pool,
        webhook_secret,
        wake,
        dry_run,
        slug,
        started: chrono::Utc::now(),
        rate: Arc::new(move || {
            let mut v: Vec<(u64, i64)> = rate_app
                .rate
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| (*k, *v))
                .collect();
            v.sort();
            v
        }),
    });
    let listen = env_or("LISTEN", "0.0.0.0:8080");
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!(%listen, dry_run, workers, "lumenfx-bot serving");
    axum::serve(listener, web::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
