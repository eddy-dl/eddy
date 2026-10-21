use rusqlite::{Connection, Result, params};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

static DB_CONNECTION: OnceLock<Mutex<Connection>> = OnceLock::new();

/// Initializes the database schemas.
pub fn init<P: AsRef<Path>>(app_dir: P) -> Result<()> {
    let db_path = app_dir.as_ref().join("downloads.db");

    // Ensure parent directories exist
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let conn = Connection::open(db_path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    // The share popup runs in its own process and writes here too.
    conn.busy_timeout(std::time::Duration::from_secs(3))?;

    // Backend config table (for the raw, functional paths)
    conn.execute(
        "CREATE TABLE IF NOT EXISTS backend_config (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
        [],
    )?;

    // Ledger for tracking active and completed downloads
    conn.execute(
        "CREATE TABLE IF NOT EXISTS downloads (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_url TEXT NOT NULL,
            file_name TEXT NOT NULL,
            file_path TEXT NOT NULL,
            status TEXT NOT NULL, -- e.g., 'pending', 'downloading', 'completed', 'failed'
            bytes_downloaded INTEGER DEFAULT 0,
            total_bytes INTEGER DEFAULT 0,
            added_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )",
        [],
    )?;

    // Links waiting for the user to look at them. Shared between the main app
    // and the share popup (a separate process), so a link added in one shows
    // up in the other.
    conn.execute(
        "CREATE TABLE IF NOT EXISTS pending_links (
            url TEXT PRIMARY KEY,
            added_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )",
        [],
    )?;

    let _ = DB_CONNECTION.set(Mutex::new(conn));
    Ok(())
}

/// Saves the functional/raw download directory used by the file system
pub fn set_raw_download_path(raw_path: &str) -> Result<()> {
    let mutex = DB_CONNECTION.get().expect("DB not initialized");
    let conn = mutex.lock().unwrap();
    let mut stmt = conn.prepare(
        "INSERT INTO backend_config (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )?;
    stmt.execute(params!["raw_download_path", raw_path])?;
    Ok(())
}

/// Retrieves the raw download path for backend file operations
pub fn get_raw_download_path() -> Result<Option<String>> {
    let mutex = DB_CONNECTION.get().expect("DB not initialized");
    let conn = mutex.lock().unwrap();
    let mut stmt = conn.prepare("SELECT value FROM backend_config WHERE key = ?1")?;

    let mut rows = stmt.query(params!["raw_download_path"])?;
    if let Some(row) = rows.next()? {
        let value: String = row.get(0)?;
        Ok(Some(value))
    } else {
        Ok(None)
    }
}

/// Runs `f` with the connection, or does nothing if the database couldn't be
/// opened (the app then works without remembering pending links).
fn with_conn<T: Default>(f: impl FnOnce(&Connection) -> Result<T>) -> T {
    let Some(mutex) = DB_CONNECTION.get() else {
        return T::default();
    };
    let conn = mutex.lock().unwrap_or_else(|e| e.into_inner());
    f(&conn).unwrap_or_else(|e| {
        eprintln!("database error: {e}");
        T::default()
    })
}

/// Remembers a pending link (no-op if it's already there).
pub fn add_pending(url: &str) {
    with_conn(|conn| {
        conn.execute("INSERT OR IGNORE INTO pending_links (url) VALUES (?1)", params![url])?;
        Ok(())
    })
}

/// Forgets a pending link once it's been started or skipped.
pub fn remove_pending(url: &str) {
    with_conn(|conn| {
        conn.execute("DELETE FROM pending_links WHERE url = ?1", params![url])?;
        Ok(())
    })
}

/// Every pending link, oldest first.
pub fn pending_links() -> Vec<String> {
    with_conn(|conn| {
        let mut stmt = conn.prepare("SELECT url FROM pending_links ORDER BY added_at, rowid")?;
        let urls = stmt.query_map([], |row| row.get(0))?.collect::<Result<Vec<String>>>()?;
        Ok(urls)
    })
}

/// The download folder the user picked, if any (None when the database isn't
/// open, unlike `get_raw_download_path`).
pub fn raw_download_path() -> Option<String> {
    with_conn(|conn| {
        let mut stmt = conn.prepare("SELECT value FROM backend_config WHERE key = ?1")?;
        let mut rows = stmt.query(params!["raw_download_path"])?;
        Ok(match rows.next()? {
            Some(row) => Some(row.get(0)?),
            None => None,
        })
    })
}
