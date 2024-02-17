//! Small helper binary using Sqlite to maintain an mtime cache
//! to avoid unnecessary rebuilds when using Cargo from a sandbox.
//! This is a workaround for the lack of a `mtime` cache in Cargo.
//!
//! Given the environment path `CARGO_MTIME_ROOT_DIR` and
//! `CARGO_MTIME_DB_PATH` this utility will set the mtime of every
//! file in the root directory to the mtime stored in the database,
//! iff the sha256 of the file matches the sha256 stored in the
//! database.

use async_walkdir::DirEntry;
use async_walkdir::Filtering;
use async_walkdir::WalkDir;
use filetime::FileTime;
use futures::StreamExt;
use rusqlite::Connection;
use rusqlite::ToSql;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::mpsc::unbounded_channel;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

#[derive(Debug, Clone)]
struct Config {
    root_dir: String,
    db_path: String,
}

#[derive(Default, Debug)]
struct Metrics {
    files_processed: AtomicUsize,
    files_was_directory: AtomicUsize,
    files_mtime_restored: AtomicUsize,
    files_mtime_skipped: AtomicUsize,
    files_mtime_updated: AtomicUsize,

    database_mtime_queries: AtomicUsize,
    database_mtime_hits: AtomicUsize,
    database_mtime_misses: AtomicUsize,

    database_mtime_write: AtomicUsize,
}

type DatabaseQuery = (String, String, oneshot::Sender<Option<i64>>);

impl Config {
    fn from_env() -> Self {
        #[cfg(debug_assertions)]
        {
            let args = std::env::args().collect::<Vec<_>>();
            return Self {
                root_dir: args.get(1).unwrap().to_string(),
                db_path: std::env::current_dir()
                    .unwrap()
                    .join("cargo_mtime.db")
                    .to_string_lossy()
                    .to_string(),
            };
        }
        #[cfg(not(debug_assertions))]
        Self {
            root_dir: std::env::var("CARGO_MTIME_ROOT").unwrap(),
            db_path: std::env::var("CARGO_MTIME_DB_PATH").unwrap(),
        }
    }
}

fn init_database(config: &Config) -> Result<Connection, rusqlite::Error> {
    // Create the database if it doesn't exist
    let conn = rusqlite::Connection::open(&config.db_path)?;

    tracing::info!("Opened database at {}", &config.db_path);
    {
        // Create metadata table if it doesn't exist
        conn.execute(
            "CREATE TABLE IF NOT EXISTS metadata (
            key TEXT PRIMARY KEY,
			value TEXT
		)",
            (),
        )?;

        let mut version = None;

        let mut stmt = conn.prepare("SELECT value FROM metadata WHERE key = 'version'")?;
        for v in stmt.query_map([], |row| row.get::<_, String>(0))? {
            version = Some(v?);
        }

        match version {
            None => {
                tracing::info!("Found no version in metadata table, initializing database");
                conn.execute(
                    "INSERT INTO metadata (key, value) VALUES ('version', '1')",
                    (),
                )?;

                // Create mtime table if it doesn't exist
                conn.execute(
                    "CREATE TABLE IF NOT EXISTS mtime (
					path TEXT PRIMARY KEY,
					sha256 TEXT,
					mtime INTEGER
				)",
                    (),
                )?;

                // Create index on path + sha256
                conn.execute(
                    "CREATE INDEX IF NOT EXISTS mtime_path_sha256 ON mtime (path, sha256)",
                    (),
                )?;
            }
            Some(version) => {
                if version != "1" {
                    panic!("Unsupported database version: {}", version);
                }
                tracing::info!("Found version 1 in metadata table, skipping initialization");
            }
        }
    }
    Ok(conn)
}

fn try_get_mtime(conn: &Connection, path: &str, sha256: &str, metrics: &Metrics) -> Option<i64> {
    tracing::trace!(
        shasum = %sha256,
        path = %path,
        "try_get_mtime"
    );

    metrics
        .database_mtime_queries
        .fetch_add(1, Ordering::Relaxed);

    let mut stmt = conn
        .prepare_cached("SELECT mtime FROM mtime WHERE path = ? AND sha256 = ?")
        .unwrap();

    let res = stmt
        .query(&[path, sha256])
        .unwrap()
        .next()
        .unwrap()
        .map(|row| row.get(0));

    if res.is_some() {
        metrics.database_mtime_hits.fetch_add(1, Ordering::Relaxed);
    } else {
        metrics
            .database_mtime_misses
            .fetch_add(1, Ordering::Relaxed);
    }

    res.and_then(std::result::Result::ok)
}

fn update_database(
    connection: &Connection,
    batch: &[(String, String, i64)],
    metrics: &Metrics,
) -> Result<(), rusqlite::Error> {
    let stmt = r#"
INSERT OR REPLACE INTO mtime (path, sha256, mtime) VALUES "#
        .to_string()
        + &batch
            .iter()
            .map(|_| "(?, ?, ?)")
            .collect::<Vec<_>>()
            .join(", ")
        + r#";
"#;
    let mut batch_stmt = connection.prepare(&stmt)?;

    batch_stmt.execute(rusqlite::params_from_iter(batch.iter().flat_map(
        |(path, sha256, mtime)| {
            vec![
                path as &dyn ToSql,
                sha256 as &dyn ToSql,
                mtime as &dyn ToSql,
            ]
        },
    )))?;

    metrics
        .database_mtime_write
        .fetch_add(batch.len(), Ordering::Relaxed);

    Ok(())
}

async fn database_lookup_task(
    mut query_rx: UnboundedReceiver<DatabaseQuery>,
    metrics: Arc<Metrics>,
    read_connection: Connection,
) {
    while let Some((path, sha256, response)) = query_rx.recv().await {
        let path: String = path;
        let sha256: String = sha256;

        let mtime = try_get_mtime(&read_connection, &path, &sha256, metrics.as_ref());
        tracing::trace!(path = %path, sha256 = %sha256, mtime = ?mtime, "query_handle");

        response.send(mtime).unwrap();
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 10)]
async fn main() {
    tracing_subscriber::fmt::init();

    tracing::info!("Installing panic hook");
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_panic(info);
        std::process::exit(1);
    }));

    let metrics = Arc::new(Metrics::default());

    let config = Config::from_env();
    let connection = init_database(&config).unwrap();

    // We'll use a single worker to write to the database, taking requests from all the other workers.
    let (tx, rx) = unbounded_channel();
    let handle = tokio::spawn(gather_submit_db(rx, connection, metrics.clone()));

    // We'll also use a single worker to read from the database, taking requests from all the other workers.
    let read_connection = init_database(&config).unwrap();
    let (query_tx, query_rx) = unbounded_channel::<DatabaseQuery>();
    let query_handle = tokio::spawn(database_lookup_task(
        query_rx,
        metrics.clone(),
        read_connection,
    ));

    // We'll use a semaphore to limit the number of concurrent file operations, to avoid running out of file descriptors.
    let semaphore = Arc::new(Semaphore::new(512));
    let mut entries = WalkDir::new(&config.root_dir).filter(|entry| async move {
        if let Some(true) = entry
            .path()
            .file_name()
            .map(|f| f.to_string_lossy().starts_with('.'))
        {
            return Filtering::IgnoreDir;
        }

        // any directory containing .rustc_info.json is a rustc build directory
        if entry.path().join(".rustc_info.json").exists() {
            return Filtering::IgnoreDir;
        }

        Filtering::Continue
    });

    loop {
        match entries.next().await {
            Some(Ok(entry)) => {
                metrics.files_processed.fetch_add(1, Ordering::Relaxed);

                let is_file = entry.file_type().await.map_or(false, |t| t.is_file());

                if !is_file {
                    metrics.files_was_directory.fetch_add(1, Ordering::Relaxed);
                    continue;
                }

                let tx = tx.clone();
                let query_tx = query_tx.clone();

                let semaphore = semaphore.clone();
                let permit = semaphore.acquire_owned().await.unwrap();
                tokio::spawn(manage_mtime(entry, query_tx, permit, tx, metrics.clone()));
            }
            Some(Err(e)) => {
                eprintln!("error: {}", e);
                break;
            }
            None => break,
        }
    }

    drop(tx);
    drop(query_tx);
    drop(semaphore);

    handle.await.unwrap();
    query_handle.await.unwrap();
}

async fn gather_submit_db(
    mut rx: UnboundedReceiver<(String, String, i64)>,
    connection: Connection,
    metrics: Arc<Metrics>,
) {
    let mut current_batch: Vec<(String, String, i64)> = vec![];

    while let Some((path, sha256, mtime)) = rx.recv().await {
        current_batch.push((path, sha256, mtime));

        if current_batch.len() >= 100 {
            update_database(&connection, &current_batch, metrics.as_ref()).unwrap();
            current_batch.clear();
        }
    }

    if !current_batch.is_empty() {
        update_database(&connection, &current_batch, metrics.as_ref()).unwrap();
    }
}

async fn manage_mtime(
    entry: DirEntry,
    query_tx: UnboundedSender<DatabaseQuery>,
    permit: OwnedSemaphorePermit,
    tx: UnboundedSender<(String, String, i64)>,
    metrics: Arc<Metrics>,
) {
    let path = entry.path();
    let (sha256, metadata) = tokio::join!(sha256::try_async_digest(&path), entry.metadata(),);
    let sha256 = sha256.unwrap();

    let mtime_on_disk =
        FileTime::from_system_time(metadata.unwrap().modified().unwrap()).unix_seconds();

    let string_path = path.to_string_lossy().to_string();
    let (response_tx, response_rx) = oneshot::channel();
    query_tx
        .send((string_path.clone(), sha256.clone(), response_tx))
        .unwrap();

    if let Some(mtime) = response_rx.await.unwrap() {
        if mtime != mtime_on_disk {
            metrics.files_mtime_restored.fetch_add(1, Ordering::Relaxed);
            filetime::set_file_mtime(&path, FileTime::from_unix_time(mtime, 0)).unwrap();
            drop(permit);
        } else {
            metrics.files_mtime_skipped.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        drop(permit);
        metrics.files_mtime_updated.fetch_add(1, Ordering::Relaxed);
        tx.send((string_path, sha256, mtime_on_disk)).unwrap()
    }
}
