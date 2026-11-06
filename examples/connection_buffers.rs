//! SQLx-only reproduction: no skilj runtime, event cache, or projection.
//! Run against a disposable database, outside other benchmarks:
//! DATABASE_URL=postgres://... cargo run --release --example connection_buffers -- 32 2097152
use sqlx::{postgres::PgPoolOptions, Connection, Row};
use std::time::Duration;

fn snapshot(stage: &str) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let field = |name: &str| {
        status
            .lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u64>().ok())
    };
    println!(
        "{}",
        serde_json::json!({"stage":stage,"rss_kib":field("VmRSS:"),"hwm_kib":field("VmHWM:")})
    );
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let count: u32 = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "32".into())
        .parse()?;
    let bytes: usize = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "2097152".into())
        .parse()?;
    let database_url = std::env::var("DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(count)
        .min_connections(count)
        .idle_timeout(None)
        .max_lifetime(None)
        .test_before_acquire(false)
        .connect(&database_url)
        .await?;
    let mut connections = Vec::new();
    for _ in 0..count {
        connections.push(pool.acquire().await?);
    }
    // Hold exactly these connections so the two passes use the same sockets.
    let payload = "x".repeat(bytes);
    snapshot("warm_pool");
    for pass in 1..=4 {
        for connection in &mut connections {
            let rows = sqlx::query("SELECT $1::text")
                .bind(&payload)
                .fetch_all(&mut **connection)
                .await?;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].try_get::<&str, _>(0)?.len(), bytes);
            // Drain the query fully, then drop its results; only the socket survives.
        }
        snapshot(&format!("large_query_pass_{pass}"));
    }
    tokio::time::sleep(Duration::from_secs(15)).await;
    snapshot("idle_with_connections");
    for connection in &mut connections {
        connection.shrink_buffers();
    }
    snapshot("after_shrink_buffers");
    // Some BytesMut allocations stay referenced until the next read. Observe
    // a completed small query too, without closing or replacing any socket.
    for connection in &mut connections {
        sqlx::query("SELECT 1").execute(&mut **connection).await?;
    }
    snapshot("after_small_query");
    tokio::time::sleep(Duration::from_secs(5)).await;
    snapshot("idle_after_shrink");
    drop(connections);
    pool.close().await;
    snapshot("pool_closed");
    Ok(())
}
