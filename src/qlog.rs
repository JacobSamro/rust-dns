//! Query logging to columnar Parquet files, queried on demand with DataFusion.
//!
//! Logging is off the DNS hot path: the handler sends a `LogRecord` over a
//! channel, and a background task batches records into zstd-compressed Parquet
//! segments. A size cap deletes the oldest segments. Queries spin up a
//! DataFusion context with a hard memory ceiling, so a heavy query can never
//! starve the resolver.

use crate::config::QlogConfig;
use anyhow::Result;
use datafusion::arrow::array::{
    ArrayRef, StringBuilder, TimestampMillisecondBuilder, UInt32Builder,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use datafusion::arrow::json::ArrayWriter;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::execution::memory_pool::GreedyMemoryPool;
use datafusion::execution::runtime_env::RuntimeEnvBuilder;
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::parquet::basic::{Compression, ZstdLevel};
use datafusion::parquet::file::properties::WriterProperties;
use datafusion::prelude::{ParquetReadOptions, SessionConfig, SessionContext};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

pub struct LogRecord {
    pub ts_ms: i64,
    pub client: String,
    pub domain: String,
    pub qtype: String,
    pub action: &'static str,
    pub latency_ms: u32,
}

pub type LogTx = mpsc::Sender<LogRecord>;
pub type LogRx = mpsc::Receiver<LogRecord>;

/// Bounded; if logging falls behind, the hot path drops records rather than
/// growing memory unbounded under load.
pub const LOG_CHANNEL_CAP: usize = 100_000;

static SEQ: AtomicU64 = AtomicU64::new(0);

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new(
            "ts",
            DataType::Timestamp(TimeUnit::Millisecond, None),
            false,
        ),
        Field::new("client", DataType::Utf8, true),
        Field::new("domain", DataType::Utf8, false),
        Field::new("qtype", DataType::Utf8, false),
        Field::new("action", DataType::Utf8, false),
        Field::new("latency_ms", DataType::UInt32, false),
    ]))
}

fn build_batch(recs: &[LogRecord]) -> Result<RecordBatch> {
    let mut ts = TimestampMillisecondBuilder::with_capacity(recs.len());
    let mut client = StringBuilder::new();
    let mut domain = StringBuilder::new();
    let mut qtype = StringBuilder::new();
    let mut action = StringBuilder::new();
    let mut latency = UInt32Builder::with_capacity(recs.len());
    for r in recs {
        ts.append_value(r.ts_ms);
        client.append_value(&r.client);
        domain.append_value(&r.domain);
        qtype.append_value(&r.qtype);
        action.append_value(r.action);
        latency.append_value(r.latency_ms);
    }
    let arrays: Vec<ArrayRef> = vec![
        Arc::new(ts.finish()),
        Arc::new(client.finish()),
        Arc::new(domain.finish()),
        Arc::new(qtype.finish()),
        Arc::new(action.finish()),
        Arc::new(latency.finish()),
    ];
    Ok(RecordBatch::try_new(schema(), arrays)?)
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

pub async fn run_writer(cfg: QlogConfig, mut rx: LogRx) {
    let dir = PathBuf::from(&cfg.dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("could not create query log dir {}: {e}", dir.display());
        return;
    }
    let flush_rows = cfg.flush_rows.max(1);
    let mut buf: Vec<LogRecord> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs(cfg.flush_secs.max(1)));
    loop {
        tokio::select! {
            maybe = rx.recv() => match maybe {
                Some(r) => {
                    buf.push(r);
                    if buf.len() >= flush_rows {
                        flush(&dir, cfg.max_bytes, &mut buf).await;
                    }
                }
                None => { flush(&dir, cfg.max_bytes, &mut buf).await; break; }
            },
            _ = tick.tick() => flush(&dir, cfg.max_bytes, &mut buf).await,
        }
    }
}

async fn flush(dir: &Path, max_bytes: u64, buf: &mut Vec<LogRecord>) {
    if buf.is_empty() {
        return;
    }
    let recs = std::mem::take(buf);
    let n = recs.len();
    let dir = dir.to_path_buf();
    match tokio::task::spawn_blocking(move || write_segment(&dir, &recs, max_bytes)).await {
        Ok(Ok(())) => tracing::trace!("query log: wrote {n} rows"),
        Ok(Err(e)) => tracing::warn!("query log write failed: {e}"),
        Err(e) => tracing::warn!("query log join error: {e}"),
    }
}

fn write_segment(dir: &Path, recs: &[LogRecord], max_bytes: u64) -> Result<()> {
    let batch = build_batch(recs)?;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let name = format!("qlog-{:014}-{:06}.parquet", now_ms(), seq);
    let final_path = dir.join(&name);
    let tmp_path = dir.join(format!("{name}.tmp"));

    // Write to a temp file, then atomically rename so a query never reads a
    // half-written segment.
    {
        let file = std::fs::File::create(&tmp_path)?;
        let props = WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::default()))
            .build();
        let mut w = ArrowWriter::try_new(file, batch.schema(), Some(props))?;
        w.write(&batch)?;
        w.close()?;
    }
    std::fs::rename(&tmp_path, &final_path)?;

    enforce_retention(dir, max_bytes)?;
    Ok(())
}

/// Delete oldest segments until the directory is under `max_bytes`.
fn enforce_retention(dir: &Path, max_bytes: u64) -> Result<()> {
    let mut files: Vec<(PathBuf, u64)> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|x| x == "parquet")
                .unwrap_or(false)
        })
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.len())))
        .collect();
    // Names are timestamp-prefixed, so lexical order is chronological.
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    for (path, size) in &files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total -= size;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Query (DataFusion)
// ---------------------------------------------------------------------------

/// Run a filtered query and return the rows as a JSON array (bytes).
/// `where_sql` is a complete `WHERE ...` clause (or empty).
pub async fn query(
    dir: &Path,
    mem_limit_mb: u64,
    where_sql: &str,
    limit: usize,
) -> Result<Vec<u8>> {
    if !has_segments(dir) {
        return Ok(b"[]".to_vec());
    }

    let bytes = (mem_limit_mb.max(16) as usize) * 1024 * 1024;
    let rt = RuntimeEnvBuilder::new()
        .with_memory_pool(Arc::new(GreedyMemoryPool::new(bytes)))
        .build_arc()?;
    let ctx = SessionContext::new_with_config_rt(SessionConfig::new(), rt);
    ctx.register_parquet(
        "logs",
        dir.to_string_lossy().as_ref(),
        ParquetReadOptions::default(),
    )
    .await?;

    let sql = format!(
        "SELECT CAST(ts AS VARCHAR) AS ts, client, domain, qtype, action, latency_ms \
         FROM logs {where_sql} ORDER BY ts DESC LIMIT {limit}"
    );
    let df = ctx.sql(&sql).await?;
    let batches = df.collect().await?;

    let mut w = ArrayWriter::new(Vec::new());
    let refs: Vec<&RecordBatch> = batches.iter().collect();
    w.write_batches(&refs)?;
    w.finish()?;
    Ok(w.into_inner())
}

fn has_segments(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .ok()
        .map(|it| {
            it.filter_map(|e| e.ok()).any(|e| {
                e.path()
                    .extension()
                    .map(|x| x == "parquet")
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}
