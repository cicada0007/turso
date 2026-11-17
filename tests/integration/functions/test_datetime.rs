use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc,
};

use crate::common::ExecRows;
use turso_core::{
    Clock, Completion, ConnectOpts, Database, File, MemoryIO, MonotonicInstant, OpenFlags,
    SessionRole, WallClockInstant, IO,
};
use turso_sqlite::SqliteDialect;

struct TestClockIo {
    inner: MemoryIO,
    unix_secs: AtomicI64,
    secs_added_per_read: i64,
}

impl Clock for TestClockIo {
    fn current_time_monotonic(&self) -> MonotonicInstant {
        self.inner.current_time_monotonic()
    }

    fn current_time_wall_clock(&self) -> WallClockInstant {
        WallClockInstant {
            secs: self
                .unix_secs
                .fetch_add(self.secs_added_per_read, Ordering::SeqCst),
            micros: 123_456,
        }
    }
}

impl IO for TestClockIo {
    fn open_file(
        &self,
        path: &str,
        flags: OpenFlags,
        direct: bool,
    ) -> turso_core::Result<Arc<dyn File>> {
        self.inner.open_file(path, flags, direct)
    }

    fn remove_file(&self, path: &str) -> turso_core::Result<()> {
        self.inner.remove_file(path)
    }

    fn step(&self) -> turso_core::Result<()> {
        self.inner.step()
    }

    fn drain_completions(&self, completions: &[Completion]) -> turso_core::Result<()> {
        self.inner.drain_completions(completions)
    }

    fn cancel(&self, completions: &[Completion]) -> turso_core::Result<()> {
        self.inner.cancel(completions)
    }
}

#[test]
fn current_time_comes_from_the_io_clock() {
    let io = Arc::new(TestClockIo {
        inner: MemoryIO::new(),
        unix_secs: AtomicI64::new(1_000_000_000),
        secs_added_per_read: 0,
    });
    let db = Database::open_file(io.clone(), ":memory:", Arc::new(SqliteDialect)).unwrap();
    let conn = db
        .connect(ConnectOpts::new(SessionRole::Superuser))
        .unwrap();

    let rows: Vec<(String, String, String, String, f64, String)> = conn.exec_rows(
        "SELECT date('now'), time(), datetime('now'), strftime('%s', 'now'), \
         unixepoch('now', 'subsec'), timediff('now', '2001-09-08 01:46:40')",
    );
    assert_eq!(
        rows,
        vec![(
            "2001-09-09".to_string(),
            "01:46:40".to_string(),
            "2001-09-09 01:46:40".to_string(),
            "1000000000".to_string(),
            1_000_000_000.123,
            "+0000-00-01 00:00:00.123".to_string(),
        )]
    );

    let rows: Vec<(f64,)> = conn.exec_rows("SELECT julianday('now')");
    assert_eq!(rows, vec![(2_452_161.574_075_497_7,)]);

    let mut stmt = conn
        .prepare("SELECT CURRENT_DATE, CURRENT_TIME, CURRENT_TIMESTAMP")
        .unwrap();
    let read_row = |stmt: &mut turso_core::Statement| {
        let mut row = Vec::new();
        stmt.run_with_row_callback(|r| {
            row = (0..3).map(|i| r.get::<String>(i).unwrap()).collect();
            Ok(())
        })
        .unwrap();
        stmt.reset().unwrap();
        row
    };
    assert_eq!(
        read_row(&mut stmt),
        vec!["2001-09-09", "01:46:40", "2001-09-09 01:46:40"]
    );
    io.unix_secs.store(1_000_086_400, Ordering::SeqCst);
    assert_eq!(
        read_row(&mut stmt),
        vec!["2001-09-10", "01:46:40", "2001-09-10 01:46:40"]
    );
}

#[test]
fn current_time_does_not_change_within_one_step() {
    let io = Arc::new(TestClockIo {
        inner: MemoryIO::new(),
        unix_secs: AtomicI64::new(1_000_000_000),
        secs_added_per_read: 1,
    });
    let db = Database::open_file(io.clone(), ":memory:", Arc::new(SqliteDialect)).unwrap();
    let conn = db
        .connect(ConnectOpts::new(SessionRole::Superuser))
        .unwrap();

    let rows: Vec<(i64, i64, String, String)> = conn.exec_rows(
        "SELECT unixepoch(), unixepoch('now'), datetime(), \
         strftime('%Y-%m-%d %H:%M:%S', 'now')",
    );
    assert_eq!(
        rows,
        vec![(
            1_000_000_000,
            1_000_000_000,
            "2001-09-09 01:46:40".to_string(),
            "2001-09-09 01:46:40".to_string(),
        )]
    );

    let rows: Vec<(i64,)> = conn.exec_rows(
        "SELECT count(DISTINCT unixepoch('now', column1 || ' seconds') - column1) \
         FROM (VALUES (1), (2), (3))",
    );
    assert_eq!(rows, vec![(1,)]);

    let rows: Vec<(i64, i64)> = conn.exec_rows(
        "SELECT unixepoch(), unixepoch('now', column1 || ' seconds') - column1 \
         FROM (VALUES (1), (2), (3))",
    );
    assert_eq!(
        rows,
        vec![
            (1_000_000_002, 1_000_000_002),
            (1_000_000_002, 1_000_000_003),
            (1_000_000_002, 1_000_000_004),
        ]
    );
}

/// The trigger reads the time first, after row 1 is updated. Row 2's new value
/// then reads it in the statement that fired the trigger, in the same step, so
/// both see one time.
#[test]
fn a_time_a_trigger_reads_first_is_the_statement_time() {
    let io = Arc::new(TestClockIo {
        inner: MemoryIO::new(),
        unix_secs: AtomicI64::new(1_000_000_000),
        secs_added_per_read: 1,
    });
    let db = Database::open_file(io.clone(), ":memory:", Arc::new(SqliteDialect)).unwrap();
    let conn = db
        .connect(ConnectOpts::new(SessionRole::Superuser))
        .unwrap();
    conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (1, 0), (2, 0)").unwrap();
    conn.execute("CREATE TABLE log(at INTEGER)").unwrap();
    conn.execute(
        "CREATE TRIGGER tr AFTER UPDATE ON t WHEN new.id = 1 BEGIN \
         INSERT INTO log VALUES (unixepoch()); END",
    )
    .unwrap();

    // The `'now', id || ' seconds'` argument keeps row 2's read in its own row
    // instead of once before the first row.
    conn.execute(
        "UPDATE t SET a = CASE id WHEN 1 THEN 0 \
         ELSE unixepoch('now', id || ' seconds') - id END",
    )
    .unwrap();

    let logged: Vec<(i64,)> = conn.exec_rows("SELECT at FROM log");
    assert_eq!(logged, vec![(1_000_000_000,)]);
    let updated: Vec<(i64,)> = conn.exec_rows("SELECT a FROM t WHERE id = 2");
    assert_eq!(updated, vec![(1_000_000_000,)]);
}

#[test]
fn a_trigger_that_reads_no_time_does_not_read_the_clock() {
    let io = Arc::new(TestClockIo {
        inner: MemoryIO::new(),
        unix_secs: AtomicI64::new(1_000_000_000),
        secs_added_per_read: 1,
    });
    let db = Database::open_file(io.clone(), ":memory:", Arc::new(SqliteDialect)).unwrap();
    let conn = db
        .connect(ConnectOpts::new(SessionRole::Superuser))
        .unwrap();
    conn.execute("CREATE TABLE t(a INTEGER)").unwrap();
    conn.execute("CREATE TABLE log(a INTEGER)").unwrap();
    conn.execute("CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES (new.a); END")
        .unwrap();
    let before = io.unix_secs.load(Ordering::SeqCst);

    conn.execute("INSERT INTO t VALUES (1), (2)").unwrap();

    assert_eq!(io.unix_secs.load(Ordering::SeqCst), before);
}
