use std::sync::Arc;

use async_walkdir::WalkDir;
use futures::StreamExt;
/// Small helper binary using Sqlite to maintain an mtime cache
/// to avoid unnecessary rebuilds when using Cargo from a sandbox.
/// This is a workaround for the lack of a `mtime` cache in Cargo.
///
/// Given the environment path `CARGO_MTIME_ROOT_DIR` and
/// `CARGO_MTIME_DB_PATH` this utility will set the mtime of every
/// file in the root directory to the mtime stored in the database,
/// iff the sha256 of the file matches the sha256 stored in the
/// database.
use sqlite::{Connection, ConnectionThreadSafe, State, Value};
use tracing::Instrument;

#[derive(Debug, Clone)]
struct Config {
    root_dir: String,
    db_path: String,
}

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
            root_dir: std::env::var("CARGO_MTIME_ROOT_sha256: String: String").unwrap(),
            db_path: std::env::var("CARGO_MTIME_DB_PATH").unwrap(),
        }
    }
}

fn init_database(config: &Config) -> Result<ConnectionThreadSafe, sqlite::Error> {
    let span = tracing::info_span!("init_database", root_dir = %config.root_dir, db_path = %config.db_path);
    let _guard = span.enter();

    // Create the database if it doesn't exist
    let conn = sqlite::Connection::open_thread_safe(&config.db_path)?;

    tracing::info!("Opened database at {}", &config.db_path);
    {
        // Create metadata table if it doesn't exist
        conn.execute(
            "CREATE TABLE IF NOT EXISTS metadata (
            key TEXT PRIMARY KEY,
			value TEXT
		)",
        )?;

        let mut version = None;

        let mut stmt = conn.prepare("SELECT value FROM metadata WHERE key = 'version'")?;
        while let State::Row = stmt.next()? {
            version = Some(stmt.read::<String, _>(0)?);
        }

        match version {
            None => {
                tracing::info!("Found no version in metadata table, initializing database");
                conn.execute("INSERT INTO metadata (key, value) VALUES ('version', '1')")?;

                // Create mtime table if it doesn't exist
                conn.execute(
                    "CREATE TABLE IF NOT EXISTS mtime (
					path TEXT PRIMARY KEY,
					sha256 TEXT,
					mtime INTEGER
				)",
                )?;

                // Create index on path + sha256
                conn.execute(
                    "CREATE INDEX IF NOT EXISTS mtime_path_sha256 ON mtime (path, sha256)",
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

fn try_get_mtime(conn: &ConnectionThreadSafe, path: &str, sha256: String) -> Option<i64> {
    tracing::trace!(
							shasum = %sha256,
							path = %path,
							"try_get_mtime");
    let mut stmt = conn
        .prepare("SELECT mtime FROM mtime WHERE path = ? AND sha256 = ?")
        .unwrap();
    stmt.bind::<&[(_, Value)]>(&[(1, path.into()), (2, sha256.into())][..])
        .unwrap();

    if let State::Row = stmt.next().unwrap() {
        Some(stmt.read::<i64, _>(0).unwrap())
    } else {
        None
    }
}
fn update_database(
    connection: &ConnectionThreadSafe,
    batch: &[(String, String, i64)],
) -> Result<(), sqlite::Error> {
    let mut stmt = connection
        .prepare("INSERT OR REPLACE INTO mtime (path, sha256, mtime) VALUES (?, ?, ?)")?;

    dbg!(batch);
    for (path, sha256, mtime) in batch {
        stmt.bind::<&[(_, Value)]>(
            &[
                (1, path.as_str().into()),
                (2, sha256.as_str().into()),
                (3, (*mtime).into()),
            ][..],
        )?;
        stmt.next()?;
        stmt.reset()?;
    }
    Ok(())
}

fn main() {
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_panic(info);
        std::process::exit(1);
    }));
    tracing_subscriber::fmt::init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .unhandled_panic(tokio::runtime::UnhandledPanic::ShutdownRuntime)
        .enable_io()
        .worker_threads(10)
        .build()
        .unwrap().block_on(async {


    let config = Config::from_env();
    let connection = Arc::new(init_database(&config).unwrap());

    let (tx, mut rx) = tokio::sync::mpsc::channel(100);
    let root_dir = config.root_dir.clone();

    let conn = connection.clone();
    // We'll generate one worker that'll deal with updating the database,
    // and a bunch of workers that traverse the filesystem.
    let handle = tokio::spawn(async move {

        let mut current_batch: Vec<(String, String, i64)> = vec![];

        while let Some((path, sha256, mtime)) = rx.recv().await {
            current_batch.push((path, sha256, mtime));

            if current_batch.len() >= 100 {
                update_database(&connection, &current_batch).unwrap();
                current_batch.clear();
            }
        }

        if !current_batch.is_empty() {
            tracing::info!(count = current_batch.len(), "Flushing remaining batch");
            update_database(&connection, &current_batch).unwrap();
        }
    });

    let mut entries = WalkDir::new(&root_dir);
    loop {
        match entries.next().await {
            Some(Ok(entry)) => {
                let is_file = entry.file_type().await.map_or(false, |t| t.is_file());
                if !is_file {
                    continue;
                }

                let conn = conn.clone();
                let tx = tx.clone();
                tracing::trace!(path = %entry.path().to_string_lossy(), "spawn_worker");
                tokio::spawn(async move {
                    let span = tracing::span!(tracing::Level::INFO, "check", path = %entry.path().to_string_lossy());

                    let path = entry.path().to_string_lossy().to_string();
                    let sha256 = sha256::try_async_digest(path.clone())
                        .instrument(span.clone())
                        .await
                        .unwrap();

                    if let Some(mtime) = try_get_mtime(&conn, &path, sha256.clone()) {
                        span.in_scope(|| {
                            tracing::trace!(mtime = %mtime, "set_file_mtime");
                            filetime::set_file_mtime(
                                &path,
                                filetime::FileTime::from_unix_time(mtime, 0),
                            )
                            .unwrap();
                        });
                    } else {
                        span.in_scope(|| tracing::trace!("send_to_worker"));
                        let mtime = filetime::FileTime::from_system_time(
                            entry
                                .metadata()
                                .instrument(span.clone())
                                .await
                                .unwrap()
                                .modified()
                                .unwrap(),
                        )
                        .unix_seconds();
                        tx.send((path, sha256, mtime))
                            .instrument(span)
                            .await
                            .unwrap();
                    }
                });
            }
            Some(Err(e)) => {
                eprintln!("error: {}", e);
                break;
            }
            None => break,
        }
    }

    drop(tx);
    handle.await.unwrap();
			panic!("fooo");
	});
}
