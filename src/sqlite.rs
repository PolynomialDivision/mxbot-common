//! SQLite stores: one connection behind a mutex, used from `spawn_blocking`,
//! and numbered migrations applied once each, in order, every one in its own
//! transaction.
//!
//! Applied migrations are rows of `schema_migrations (version, applied_at)`.
//! A database that a newer image migrated further is refused instead of
//! opened. The SQLite is the one the Matrix SDK's store links (rusqlite at the
//! SDK's version, re-exported as [`rusqlite`]); a bot that wants it compiled
//! in turns on rusqlite's `bundled` itself.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

pub use rusqlite;

/// A bot's migrations in order, `(version, SQL)`. Never edit one that has
/// shipped: add the next.
pub type Migrations = [(i64, &'static str)];

/// How a database is configured beyond what every bot gets (WAL, foreign
/// keys, `synchronous = NORMAL`, a busy timeout of 5 s).
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// `PRAGMA secure_delete`: SQLite overwrites deleted content in the main
    /// database file instead of leaving it in free pages. An extra measure,
    /// not a guarantee of physical erasure: the WAL (until its next
    /// checkpoint), the filesystem, the storage hardware, snapshots and
    /// backups may still hold older data.
    pub secure_delete: bool,
}

/// A database shared by the bot's tasks.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// Open (or create) the database at `path`, configure it and apply the
    /// migrations not applied yet.
    pub async fn open(
        path: &Path,
        migrations: &'static Migrations,
        options: Options,
    ) -> Result<Self> {
        let path = path.to_owned();
        let conn = tokio::task::spawn_blocking(move || open_connection(&path, migrations, options))
            .await??;
        Ok(Self::from_connection(conn))
    }

    /// An already configured and migrated connection (tests: [`memory_connection`]).
    pub fn from_connection(conn: Connection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    /// Run `f` on the connection, off the async runtime.
    pub async fn run<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut conn)
        })
        .await?
    }

    /// Run `f` synchronously (tests and code already off the runtime).
    pub fn with<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut conn)
    }
}

/// [`Db::open`] without the runtime: open, configure, migrate.
pub fn open_connection(
    path: &Path,
    migrations: &Migrations,
    options: Options,
) -> Result<Connection> {
    let mut conn =
        Connection::open(path).with_context(|| format!("Opening SQLite at {}", path.display()))?;
    configure(&conn, options)?;
    migrate(&mut conn, migrations)?;
    Ok(conn)
}

/// A migrated in-memory database with foreign keys on, for tests.
pub fn memory_connection(migrations: &Migrations) -> Result<Connection> {
    let mut conn = Connection::open_in_memory()?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    migrate(&mut conn, migrations)?;
    Ok(conn)
}

/// The pragmas every database gets, and `options`.
pub fn configure(conn: &Connection, options: Options) -> Result<()> {
    let secure_delete = if options.secure_delete {
        "PRAGMA secure_delete = ON;"
    } else {
        ""
    };
    conn.execute_batch(&format!(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         {secure_delete}
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;"
    ))
    .context("Configuring SQLite")
}

/// Apply every migration not applied yet. Returns how many ran.
pub fn migrate(conn: &mut Connection, migrations: &Migrations) -> Result<usize> {
    anyhow::ensure!(
        migrations.windows(2).all(|pair| pair[0].0 < pair[1].0),
        "Migrations must be numbered in increasing order"
    );
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version     INTEGER PRIMARY KEY,
             applied_at  TEXT NOT NULL
         );",
    )?;
    // An older image over a database a newer one migrated: it would write
    // what the schema no longer takes, or misread what it holds.
    let newest: Option<i64> =
        conn.query_row("SELECT max(version) FROM schema_migrations", [], |r| {
            r.get(0)
        })?;
    let known = migrations.last().map_or(0, |(version, _)| *version);
    if let Some(newest) = newest.filter(|n| *n > known) {
        anyhow::bail!(
            "The database is from a newer version of the bot (migration {newest}, this one knows \
             up to {known}). Run the newer image again, or restore a backup from before it."
        );
    }
    let mut applied = 0;
    for (version, sql) in migrations {
        let done: Option<i64> = conn
            .query_row(
                "SELECT version FROM schema_migrations WHERE version = ?1",
                [version],
                |r| r.get(0),
            )
            .optional()?;
        if done.is_some() {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)
            .with_context(|| format!("Applying migration {version}"))?;
        // RFC 3339, UTC, whole seconds, as the bots store their timestamps.
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at)
             VALUES (?1, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))",
            [version],
        )?;
        tx.commit()?;
        tracing::info!(version, "Applied database migration");
        applied += 1;
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIGRATIONS: &Migrations = &[
        (
            1,
            "CREATE TABLE notes (id INTEGER PRIMARY KEY, text TEXT NOT NULL);",
        ),
        (
            2,
            "ALTER TABLE notes ADD COLUMN author TEXT NOT NULL DEFAULT '';",
        ),
        (
            3,
            "CREATE TABLE tags (note_id INTEGER NOT NULL REFERENCES notes (id) ON DELETE CASCADE,
                                tag TEXT NOT NULL);",
        ),
    ];

    fn versions(conn: &Connection) -> Vec<i64> {
        conn.prepare("SELECT version FROM schema_migrations ORDER BY version")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// A database as an image knowing only the first `upto` migrations left it.
    fn at_version(upto: usize) -> Connection {
        let mut conn = memory_connection(&MIGRATIONS[..upto]).unwrap();
        if upto > 0 {
            conn.execute("INSERT INTO notes (id, text) VALUES (1, 'kept')", [])
                .unwrap();
        }
        assert_eq!(migrate(&mut conn, &MIGRATIONS[..upto]).unwrap(), 0);
        conn
    }

    #[tokio::test]
    async fn a_fresh_file_is_configured_and_migrated_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bot.sqlite");
        let options = Options {
            secure_delete: true,
        };
        let db = Db::open(&path, MIGRATIONS, options).await.unwrap();
        let pragmas = db
            .run(|conn| {
                let mut values = Vec::new();
                for pragma in [
                    "journal_mode",
                    "foreign_keys",
                    "secure_delete",
                    "synchronous",
                    "busy_timeout",
                ] {
                    let value: rusqlite::types::Value =
                        conn.query_row(&format!("PRAGMA {pragma}"), [], |r| r.get(0))?;
                    values.push(format!("{value:?}"));
                }
                Ok(values.join(" "))
            })
            .await
            .unwrap();
        assert_eq!(
            pragmas,
            r#"Text("wal") Integer(1) Integer(1) Integer(1) Integer(5000)"#
        );
        db.with(|conn| {
            assert_eq!(versions(conn), vec![1, 2, 3]);
            let applied_at: String =
                conn.query_row("SELECT applied_at FROM schema_migrations", [], |r| r.get(0))?;
            assert_eq!(applied_at.len(), "2026-10-30T12:00:00Z".len());
            assert!(applied_at.ends_with('Z'), "{applied_at}");
            Ok(())
        })
        .unwrap();
        drop(db);
        // Reopening (a restart) applies nothing again.
        let mut conn = open_connection(&path, MIGRATIONS, Options::default()).unwrap();
        assert_eq!(migrate(&mut conn, MIGRATIONS).unwrap(), 0);
    }

    #[test]
    fn every_older_version_upgrades_with_its_data() {
        for upto in 0..MIGRATIONS.len() {
            let mut conn = at_version(upto);
            assert_eq!(
                migrate(&mut conn, MIGRATIONS).unwrap(),
                MIGRATIONS.len() - upto
            );
            assert_eq!(versions(&conn), vec![1, 2, 3]);
            let notes: i64 = conn
                .query_row("SELECT count(*) FROM notes WHERE author = ''", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(notes, i64::from(upto > 0));
        }
    }

    #[test]
    fn an_older_image_refuses_a_database_a_newer_one_migrated() {
        let mut conn = at_version(3);
        conn.execute("INSERT INTO schema_migrations VALUES (4, 'later')", [])
            .unwrap();
        let error = migrate(&mut conn, MIGRATIONS).unwrap_err().to_string();
        assert!(error.contains("newer version"), "{error}");
        assert!(
            error.contains("migration 4, this one knows up to 3"),
            "{error}"
        );
        // An image that knows migration 4 opens it.
        let four: &Migrations = &[MIGRATIONS[0], MIGRATIONS[1], MIGRATIONS[2], (4, "")];
        assert_eq!(migrate(&mut conn, four).unwrap(), 0);
    }

    #[test]
    fn a_failing_migration_leaves_the_database_as_it_was() {
        let mut conn = at_version(1);
        let broken: &Migrations = &[
            MIGRATIONS[0],
            (
                2,
                "ALTER TABLE notes ADD COLUMN author TEXT NOT NULL DEFAULT '';
                 INSERT INTO notes (id, text) VALUES (1, 'twice');",
            ),
        ];
        let error = format!("{:#}", migrate(&mut conn, broken).unwrap_err());
        assert!(error.starts_with("Applying migration 2"), "{error}");
        assert_eq!(versions(&conn), vec![1]);
        let columns: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('notes') WHERE name = 'author'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(columns, 0);
        // The fixed image applies it.
        assert_eq!(migrate(&mut conn, MIGRATIONS).unwrap(), 2);
    }

    #[test]
    fn migrations_out_of_order_are_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        let swapped: &Migrations = &[MIGRATIONS[1], MIGRATIONS[0]];
        assert!(migrate(&mut conn, swapped).is_err());
    }
}
