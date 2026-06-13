use std::sync::Arc;
use tokio::time::{sleep, Duration};

use crate::{db, s3::S3Client};

const INTERVAL_SECS: u64 = 24 * 60 * 60;

pub async fn run_expiry_loop(pool: sqlx::PgPool, s3: Arc<S3Client>) {
    loop {
        sleep(Duration::from_secs(INTERVAL_SECS)).await;
        if let Err(e) = expire_objects(&pool, &s3).await {
            tracing::error!(err = %e, "lifecycle expiry run failed");
        }
    }
}

async fn expire_objects(pool: &sqlx::PgPool, s3: &S3Client) -> anyhow::Result<()> {
    let expired = db::find_expired_objects(pool).await?;
    if expired.is_empty() {
        return Ok(());
    }

    tracing::info!(count = expired.len(), "lifecycle: expiring objects");

    // Delete from S3 first — if S3 fails we keep the metadata and retry tomorrow.
    s3.delete_many(&expired)
        .await
        .map_err(|e| anyhow::anyhow!("S3 batch delete failed: {e}"))?;

    let deleted = db::delete_objects_by_pairs(pool, &expired).await?;
    tracing::info!(deleted, "lifecycle: objects expired");
    Ok(())
}
