use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use tempfile::TempDir;
use turso_core::{ConnectOpts, SessionRole};
use turso_pg_client::{error_message, BackendEvent, ConnParams, Error, PgConn};
use turso_pg_server::TursoPgServer;

#[test]
fn transaction_of_one_client_is_not_visible_to_another() {
    let server = TestServer::start();
    let mut client1 = server.connect();
    let mut client2 = server.connect();
    query(&mut client1, "CREATE TABLE t (x int)");

    query(&mut client1, "BEGIN");
    query(&mut client1, "INSERT INTO t VALUES (1)");

    assert_eq!(query(&mut client2, "SELECT count(*) FROM t"), vec!["0"]);
    // COMMIT outside a transaction block succeeds like in PostgreSQL, and
    // must not end the other client's open transaction.
    query(&mut client2, "COMMIT");
    assert_eq!(query(&mut client2, "SELECT count(*) FROM t"), vec!["0"]);
    query(&mut client1, "COMMIT");
    assert_eq!(query(&mut client2, "SELECT count(*) FROM t"), vec!["1"]);
}

#[test]
fn search_path_of_one_client_does_not_change_another() {
    let server = TestServer::start();
    let mut client1 = server.connect();
    let mut client2 = server.connect();
    query(&mut client1, "CREATE TABLE t (x text)");
    query(&mut client1, "INSERT INTO t VALUES ('public')");
    query(&mut client1, "CREATE SCHEMA s");
    query(&mut client1, "CREATE TABLE s.t (x text)");
    query(&mut client1, "INSERT INTO s.t VALUES ('s')");

    query(&mut client1, "SET search_path TO s");

    assert_eq!(query(&mut client1, "SELECT x FROM t"), vec!["s"]);
    assert_eq!(query(&mut client2, "SELECT x FROM t"), vec!["public"]);
}

#[test]
fn schema_created_by_one_client_is_visible_to_another() {
    let server = TestServer::start();
    let mut client1 = server.connect();
    let mut client2 = server.connect();
    query(&mut client2, "SELECT 1");

    query(&mut client1, "CREATE SCHEMA s");
    query(&mut client1, "CREATE TABLE s.t (x int)");
    query(&mut client1, "INSERT INTO s.t VALUES (1)");

    assert_eq!(query(&mut client2, "SELECT x FROM s.t"), vec!["1"]);
}

#[test]
fn schema_dropped_by_one_client_is_gone_for_another() {
    let server = TestServer::start();
    let mut client1 = server.connect();
    let mut client2 = server.connect();
    query(&mut client1, "CREATE SCHEMA s");
    query(&mut client1, "CREATE TABLE s.t (x int)");
    assert_eq!(query(&mut client2, "SELECT count(*) FROM s.t"), vec!["0"]);

    query(&mut client1, "DROP SCHEMA s CASCADE");

    query_error(&mut client2, "SELECT count(*) FROM s.t");
}

#[test]
fn startup_refuses_parameters_tursopg_does_not_support() {
    let server = TestServer::start();
    for (startup, sqlstate, message) in [
        (
            &[("TimeZone", "Europe/Helsinki")][..],
            "0A000",
            r#"parameter "TimeZone" does not support value "Europe/Helsinki"; supported: "UTC", "Etc/UTC""#,
        ),
        (
            &[("search_path", "public")],
            "0A000",
            r#"the startup parameter "search_path" is not supported; use SET after connecting"#,
        ),
        (
            &[("options", "")],
            "0A000",
            r#"the startup parameter "options" is not supported; use SET after connecting"#,
        ),
        (
            &[("replication", "false")],
            "0A000",
            "replication connections are not supported",
        ),
        (
            &[("_pq_.foo", "bar")],
            "0A000",
            r#"protocol extension "_pq_.foo" is not supported"#,
        ),
        // Startup parameter names are case-sensitive, so "User" is not "user".
        (
            &[("User", "postgres")],
            "0A000",
            r#"parameter "User" is not supported"#,
        ),
        (
            &[("app..user_id", "7")],
            "42602",
            r#"invalid configuration parameter name "app..user_id""#,
        ),
        (
            &[("TimeZone", "UTC"), ("timezone", "Etc/UTC")],
            "0A000",
            r#"giving the startup parameter "timezone" more than once is not supported"#,
        ),
    ] {
        let Err(Error::StartupError(fields)) = server.connect_with(startup) else {
            panic!("{startup:?}: expected the server to refuse the connection");
        };
        assert_eq!(
            (
                fields[&b'S'].as_str(),
                fields[&b'C'].as_str(),
                error_message(&fields)
            ),
            ("FATAL", sqlstate, message),
            "{startup:?}"
        );
    }
}

#[test]
fn startup_accepts_spellings_of_supported_values() {
    let server = TestServer::start();
    // asyncpg sends client_encoding 'utf-8', with the quotes.
    for (name, value, shown) in [
        ("TimeZone", "utc", "UTC"),
        ("client_encoding", "'utf-8'", "UTF8"),
        ("x.y", "z", "z"),
    ] {
        let mut client = server.connect_with(&[(name, value)]).unwrap();
        assert_eq!(
            query(&mut client, &format!("SHOW {name}")),
            [shown],
            "{name}={value}"
        );
    }
}

#[test]
fn startup_reports_parameter_values() {
    let server = TestServer::start();
    let client = server
        .connect_with(&[("TimeZone", "utc"), ("application_name", "app")])
        .unwrap();
    let reported = client.startup_parameters();
    for (name, value) in [
        ("TimeZone", "UTC"),
        ("application_name", "app"),
        ("client_encoding", "UTF8"),
        ("DateStyle", "ISO, MDY"),
        ("IntervalStyle", "postgres"),
        ("standard_conforming_strings", "on"),
    ] {
        assert_eq!(reported[name], value, "{name}");
    }
}

#[test]
fn reset_returns_to_startup_values() {
    let server = TestServer::start();
    let mut client = server
        .connect_with(&[("TimeZone", "utc"), ("app.user_id", "7")])
        .unwrap();
    for reset in ["RESET TimeZone", "RESET ALL"] {
        query(&mut client, "SET TimeZone = 'Etc/UTC'");
        query(&mut client, reset);
        assert_eq!(query(&mut client, "SHOW TimeZone"), ["UTC"], "{reset}");
    }
    query(&mut client, "SET app.user_id = '8'");
    assert_eq!(
        query(&mut client, "SELECT set_config('app.user_id', NULL, false)"),
        ["7"]
    );
}

struct TestServer {
    port: u16,
    _dir: TempDir,
}

impl TestServer {
    fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let db_file = dir.path().join("test.db").to_string_lossy().to_string();
        let (_io, db) = turso_pg::open_database(
            &db_file,
            None,
            turso_pg::OpenFlags::default(),
            turso_pg::DatabaseOpts::new(),
        )
        .unwrap();
        let server = TursoPgServer::new(
            "127.0.0.1:0".to_string(),
            db_file,
            db,
            ConnectOpts::new(SessionRole::Superuser),
            Arc::new(AtomicUsize::new(0)),
        );
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || runtime.block_on(server.serve(listener)));
        Self { port, _dir: dir }
    }

    fn connect(&self) -> PgConn {
        self.connect_with(&[]).unwrap()
    }

    /// Connects with `startup` as extra startup parameters.
    fn connect_with(&self, startup: &[(&str, &str)]) -> turso_pg_client::Result<PgConn> {
        let params = ConnParams {
            host: "127.0.0.1".to_string(),
            port: self.port,
            user: "postgres".to_string(),
            password: None,
            database: "main".to_string(),
        };
        PgConn::connect(&params, startup)
    }
}

/// Runs `sql` and returns the first column of each row.
fn query(client: &mut PgConn, sql: &str) -> Vec<String> {
    let mut values = Vec::new();
    for event in client.simple_query(sql).unwrap() {
        match event {
            BackendEvent::DataRow(columns) => {
                values.push(columns[0].clone().unwrap_or_default());
            }
            BackendEvent::ErrorResponse(fields) => {
                panic!("{sql}: {}", error_message(&fields));
            }
            _ => {}
        }
    }
    values
}

/// Runs `sql`, which must fail, and returns the error message.
fn query_error(client: &mut PgConn, sql: &str) -> String {
    for event in client.simple_query(sql).unwrap() {
        if let BackendEvent::ErrorResponse(fields) = event {
            return error_message(&fields).to_string();
        }
    }
    panic!("{sql}: expected an error");
}
