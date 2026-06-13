use std::net::SocketAddr;
use anyhow::Context;

mod auth;
mod config;
mod db;
mod error;
mod lifecycle;
mod routes;
mod s3;
mod signed;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "valkv_blob=info,tower_http=info".parse().unwrap()),
        )
        .init();

    let config = config::Config::from_env().context("load config")?;
    let port = config.port;

    let pool = sqlx::PgPool::connect(&config.database_url)
        .await
        .context("connect to postgres")?;

    db::init_schema(&pool).await.context("init storage schema")?;

    let s3 = std::sync::Arc::new(s3::S3Client::new(&config).await);
    if let Err(e) = s3.ensure_bucket_exists().await {
        tracing::warn!("S3 bucket check: {}", e);
    }

    // Spawn daily lifecycle expiry sweep in the background.
    tokio::spawn(lifecycle::run_expiry_loop(pool.clone(), s3.clone()));

    let config = std::sync::Arc::new(config);
    let state = routes::AppState { config, db: pool, s3 };
    let app = routes::router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("blob-api listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
