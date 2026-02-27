//! Disk-backed metrics buffer using SQLite.
//!
//! Collects metrics at 1-second intervals and stores them locally so that
//! data is never lost even when the gateway connection is down. The push
//! task drains unsent rows in batches, and on reconnect the backfill task
//! replays any remaining history.
//!
//! The database lives at the path specified in `metrics.buffer_path`
//! (default `/var/lib/nobase/metrics.db`).

use agent_proto::MetricsPayload;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;
use tracing::{debug, info, warn};

/// Error type for metrics buffer operations (Send + Sync safe).
#[derive(Debug)]
pub struct BufferError(pub String);

impl std::fmt::Display for BufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for BufferError {}

impl From<rusqlite::Error> for BufferError {
    fn from(e: rusqlite::Error) -> Self {
        BufferError(e.to_string())
    }
}

impl From<serde_json::Error> for BufferError {
    fn from(e: serde_json::Error) -> Self {
        BufferError(e.to_string())
    }
}

/// A single buffered metrics row returned from the database.
#[derive(Debug, Clone)]
pub struct BufferedMetric {
    pub id: i64,
    pub timestamp: String,
    pub metrics: MetricsPayload,
}

/// Disk-backed metrics buffer backed by a SQLite database.
///
/// All public methods use an internal `Mutex<Connection>` so the buffer
/// is `Send + Sync` and safe to share via `Arc<MetricsBuffer>`.
pub struct MetricsBuffer {
    conn: Mutex<Connection>,
}

impl MetricsBuffer {
    /// Open (or create) the metrics buffer database at `path`.
    ///
    /// Creates the parent directory if it does not exist.
    pub fn new(path: &str) -> Result<Self, BufferError> {
        // Ensure parent directory exists
        if let Some(parent) = Path::new(path).parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| BufferError(format!("Failed to create directory: {e}")))?;
                info!("Created metrics buffer directory: {}", parent.display());
            }
        }

        let conn = Connection::open(path)?;

        // Performance pragmas for WAL mode (concurrent reads + writes)
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous  = NORMAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA cache_size   = -2000;",
        )?;

        // Create the metrics table if it does not exist
        conn.execute(
            "CREATE TABLE IF NOT EXISTS metrics (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp    TEXT    NOT NULL,
                metrics_json TEXT    NOT NULL,
                sent         INTEGER NOT NULL DEFAULT 0
            )",
            [],
        )?;

        // Index for fast unsent-row queries
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_metrics_sent ON metrics (sent, id)",
            [],
        )?;

        info!("Metrics buffer opened: {path}");

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insert a new metrics data point (marked as unsent).
    pub fn store(&self, timestamp: &str, metrics: &MetricsPayload) -> Result<(), BufferError> {
        let json = serde_json::to_string(metrics)?;
        let conn = self.conn.lock().map_err(|e| BufferError(format!("lock poisoned: {e}")))?;
        conn.execute(
            "INSERT INTO metrics (timestamp, metrics_json, sent) VALUES (?1, ?2, 0)",
            params![timestamp, json],
        )?;
        Ok(())
    }

    /// Fetch the oldest unsent rows, up to `limit`.
    pub fn get_unsent(&self, limit: usize) -> Result<Vec<BufferedMetric>, BufferError> {
        let conn = self.conn.lock().map_err(|e| BufferError(format!("lock poisoned: {e}")))?;
        let mut stmt = conn.prepare(
            "SELECT id, timestamp, metrics_json FROM metrics WHERE sent = 0 ORDER BY id ASC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            let id: i64 = row.get(0)?;
            let timestamp: String = row.get(1)?;
            let json: String = row.get(2)?;
            Ok((id, timestamp, json))
        })?;

        let mut result = Vec::new();
        for row in rows {
            let (id, timestamp, json) = row?;
            match serde_json::from_str::<MetricsPayload>(&json) {
                Ok(metrics) => result.push(BufferedMetric {
                    id,
                    timestamp,
                    metrics,
                }),
                Err(e) => {
                    warn!("Skipping corrupt metrics row {id}: {e}");
                }
            }
        }
        Ok(result)
    }

    /// Mark a set of row IDs as sent.
    pub fn mark_sent(&self, ids: &[i64]) -> Result<(), BufferError> {
        if ids.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().map_err(|e| BufferError(format!("lock poisoned: {e}")))?;

        // Use a single UPDATE with IN clause for efficiency
        let placeholders: Vec<String> = ids.iter().map(|_| "?".to_string()).collect();
        let sql = format!(
            "UPDATE metrics SET sent = 1 WHERE id IN ({})",
            placeholders.join(",")
        );
        let params: Vec<Box<dyn rusqlite::types::ToSql>> =
            ids.iter().map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>).collect();
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        conn.execute(&sql, param_refs.as_slice())?;
        Ok(())
    }

    /// Delete rows older than `retention_hours` (both sent and unsent).
    pub fn cleanup(&self, retention_hours: u64) -> Result<u64, BufferError> {
        let cutoff = chrono::Utc::now()
            - chrono::Duration::hours(retention_hours as i64);
        let cutoff_str = cutoff.to_rfc3339();

        let conn = self.conn.lock().map_err(|e| BufferError(format!("lock poisoned: {e}")))?;
        let deleted = conn.execute(
            "DELETE FROM metrics WHERE timestamp < ?1",
            params![cutoff_str],
        )?;

        if deleted > 0 {
            debug!("Metrics buffer cleanup: removed {deleted} old rows");
        }
        Ok(deleted as u64)
    }

    /// Also cleanup already-sent rows older than 5 minutes to keep the DB lean.
    /// Sent data doesn't need to stick around.
    pub fn cleanup_sent(&self) -> Result<u64, BufferError> {
        let cutoff = chrono::Utc::now() - chrono::Duration::minutes(5);
        let cutoff_str = cutoff.to_rfc3339();

        let conn = self.conn.lock().map_err(|e| BufferError(format!("lock poisoned: {e}")))?;
        let deleted = conn.execute(
            "DELETE FROM metrics WHERE sent = 1 AND timestamp < ?1",
            params![cutoff_str],
        )?;

        if deleted > 0 {
            debug!("Metrics buffer: pruned {deleted} sent rows");
        }
        Ok(deleted as u64)
    }

    /// Count of unsent rows (for diagnostics).
    pub fn unsent_count(&self) -> Result<usize, BufferError> {
        let conn = self.conn.lock().map_err(|e| BufferError(format!("lock poisoned: {e}")))?;
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM metrics WHERE sent = 0", [], |r| r.get(0))?;
        Ok(count as usize)
    }

    /// Total row count (for diagnostics).
    pub fn total_count(&self) -> Result<usize, BufferError> {
        let conn = self.conn.lock().map_err(|e| BufferError(format!("lock poisoned: {e}")))?;
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM metrics", [], |r| r.get(0))?;
        Ok(count as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_proto::*;

    fn test_metrics() -> MetricsPayload {
        MetricsPayload {
            hostname: "test".into(),
            os: OsInfo {
                name: "Linux".into(),
                version: "6.0".into(),
                kernel: "6.0.0".into(),
                arch: "x86_64".into(),
            },
            uptime_secs: 1234,
            cpu: CpuMetrics {
                model: "Test CPU".into(),
                cores: 4,
                usage_percent: 25.0,
                temperature_celsius: Some(50.0),
                mhz: 3000.0,
            },
            memory: MemoryMetrics {
                total_mb: 8192,
                used_mb: 4096,
                available_mb: 4096,
                swap_total_mb: 2048,
                swap_used_mb: 0,
                speed_mhz: None,
            },
            disks: vec![DiskMetrics {
                mount: "/".into(),
                filesystem: "ext4".into(),
                total_gb: 100.0,
                used_gb: 50.0,
                available_gb: 50.0,
                usage_percent: 50.0,
                device: "sda1".into(),
            }],
            network: NetworkMetrics {
                interface: "eth0".into(),
                rx_bytes_per_sec: 1000,
                tx_bytes_per_sec: 500,
                rx_total_bytes: 1000000,
                tx_total_bytes: 500000,
                latency_ms: 0.0,
            },
            gpu: None,
            docker: None,
        }
    }

    #[test]
    fn test_store_and_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let buf = MetricsBuffer::new(path.to_str().unwrap()).unwrap();

        let m = test_metrics();
        let ts = "2026-01-01T00:00:00Z";

        buf.store(ts, &m).unwrap();
        buf.store(ts, &m).unwrap();
        buf.store(ts, &m).unwrap();

        let unsent = buf.get_unsent(10).unwrap();
        assert_eq!(unsent.len(), 3);
        assert_eq!(unsent[0].metrics.hostname, "test");

        assert_eq!(buf.unsent_count().unwrap(), 3);
    }

    #[test]
    fn test_mark_sent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let buf = MetricsBuffer::new(path.to_str().unwrap()).unwrap();

        let m = test_metrics();
        for i in 0..5 {
            buf.store(&format!("2026-01-01T00:00:0{i}Z"), &m).unwrap();
        }

        let unsent = buf.get_unsent(3).unwrap();
        assert_eq!(unsent.len(), 3);

        let ids: Vec<i64> = unsent.iter().map(|r| r.id).collect();
        buf.mark_sent(&ids).unwrap();

        assert_eq!(buf.unsent_count().unwrap(), 2);

        let remaining = buf.get_unsent(10).unwrap();
        assert_eq!(remaining.len(), 2);
    }

    #[test]
    fn test_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let buf = MetricsBuffer::new(path.to_str().unwrap()).unwrap();

        let m = test_metrics();
        // Insert with an old timestamp
        buf.store("2020-01-01T00:00:00Z", &m).unwrap();
        // Insert with a recent timestamp
        buf.store(&chrono::Utc::now().to_rfc3339(), &m).unwrap();

        let deleted = buf.cleanup(24).unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(buf.total_count().unwrap(), 1);
    }
}
