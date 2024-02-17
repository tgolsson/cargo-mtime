//! Small helper binary using a file-db to maintain an mtime cache
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
use jammdb::DB;
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
        let args = std::env::args().collect::<Vec<_>>();
        let root_dir = args
            .get(1)
            .cloned()
            .unwrap_or_else(|| std::env::var("CARGO_MTIME_ROOT").unwrap())
            .to_string();
        let db_path = args
            .get(2)
            .cloned()
            .unwrap_or_else(|| std::env::var("CARGO_MTIME_DB_PATH").unwrap())
            .to_string();

        Self { root_dir, db_path }
    }
}

fn init_database(config: &Config) -> Result<DB, jammdb::Error> {
    // Create the database if it doesn't exist
    let conn = DB::open(&config.db_path)?;

    {
        let tx = conn.tx(true)?;

        let metadata = tx.get_or_create_bucket("metadata")?;
        let version = metadata.get("version");

        match version {
            None => {
                tx.get_or_create_bucket("mtime")?;
            }
            Some(version) => {
                if version.kv().value() != b"1" {
                    panic!("Unsupported database version: {:?}", version);
                }
            }
        }
        tx.commit()?;
    }

    Ok(conn)
}

fn try_get_mtime(conn: &DB, path: &str, sha256: &str, metrics: &Metrics) -> Option<i64> {
    metrics
        .database_mtime_queries
        .fetch_add(1, Ordering::Relaxed);

    let tx = conn.tx(false).unwrap();

    let bucket = tx.get_bucket("mtime").unwrap();
    let shas = bucket.get_bucket(path).ok()?;
    let res = shas.get(sha256);

    if res.is_some() {
        metrics.database_mtime_hits.fetch_add(1, Ordering::Relaxed);
    } else {
        metrics
            .database_mtime_misses
            .fetch_add(1, Ordering::Relaxed);
    }

    res.map(|mtime| {
        let mtime = mtime.kv().value();
        let mtime = i64::from_be_bytes(mtime.try_into().unwrap());
        mtime
    })
}

fn update_database(
    connection: &DB,
    batch: &[(String, String, i64)],
    metrics: &Metrics,
) -> Result<(), jammdb::Error> {
    let tx = connection.tx(true)?;

    let mtimes = tx.get_bucket("mtime")?;

    for (path, sha256, mtime) in batch {
        let mtime_bytes = mtime.to_be_bytes();

        let shas = mtimes.get_or_create_bucket(path.as_str())?;
        shas.put(sha256.as_str(), mtime_bytes)?;
    }

    tx.commit()?;

    metrics
        .database_mtime_write
        .fetch_add(batch.len(), Ordering::Relaxed);

    Ok(())
}

async fn database_lookup_task(
    mut query_rx: UnboundedReceiver<DatabaseQuery>,
    metrics: Arc<Metrics>,
    read_connection: DB,
) {
    while let Some((path, sha256, response)) = query_rx.recv().await {
        let path: String = path;
        let sha256: String = sha256;
        let mtime = try_get_mtime(&read_connection, &path, &sha256, metrics.as_ref());
        response.send(mtime).unwrap();
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_panic(info);
        std::process::exit(1);
    }));

    let metrics = Arc::new(Metrics::default());

    let config = Config::from_env();
    let connection = init_database(&config).unwrap();
    let read_connection = connection.clone();
    // We'll use a single worker to write to the database, taking requests from all the other workers.
    let (tx, rx) = unbounded_channel();
    let handle = tokio::spawn(gather_submit_db(rx, connection, metrics.clone()));

    // We'll also use a single worker to read from the database, taking requests from all the other workers.

    let (query_tx, query_rx) = unbounded_channel::<DatabaseQuery>();
    let query_handle = tokio::spawn(database_lookup_task(
        query_rx,
        metrics.clone(),
        read_connection,
    ));

    // We'll use a semaphore to limit the number of concurrent file operations, to avoid running out of file descriptors.
    let semaphore = Arc::new(Semaphore::new(768));
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

    dbg!(metrics);
}

async fn gather_submit_db(
    mut rx: UnboundedReceiver<(String, String, i64)>,
    connection: DB,
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
    drop(entry);
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
