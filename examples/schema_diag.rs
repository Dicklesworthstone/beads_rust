//! Diagnostic: dump exactly what fsqlite's introspection reports for the
//! issues/dependencies indexes and run the runtime index contract checker.
//! Read-only against a COPY of the live database.
use beads_rust::franken_sync::Connection;

fn rows(conn: &Connection, sql: &str) -> Vec<Vec<String>> {
    match conn.query(sql) {
        Ok(rs) => rs
            .iter()
            .map(|r| {
                (0..r.values().len())
                    .map(|i| {
                        r.get(i)
                            .map(|v| {
                                if let Some(t) = v.as_text() {
                                    t.to_string()
                                } else if let Some(n) = v.as_integer() {
                                    n.to_string()
                                } else {
                                    "NULL".into()
                                }
                            })
                            .unwrap_or_else(|| "ERR".into())
                    })
                    .collect()
            })
            .collect(),
        Err(e) => vec![vec![format!("QUERY ERROR: {e}")]],
    }
}

fn main() {
    let db = std::env::args().nth(1).expect("usage: schema_diag <db-copy>");
    let conn = Connection::open(&db).expect("open");

    println!("==== versions ====");
    for r in rows(&conn, "PRAGMA user_version") {
        println!("  user_version {r:?}");
    }
    for r in rows(&conn, "PRAGMA schema_version") {
        println!("  schema_version {r:?}");
    }

    println!("==== runtime index contract diagnostics ====");
    print!("{}", beads_rust::storage::schema::runtime_index_diagnostics(&conn));
}
