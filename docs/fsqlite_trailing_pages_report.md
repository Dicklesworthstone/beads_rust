# frankensqlite: trailing zero pages rejected where SQLite accepts them

Prepared 2026-07-25 for filing upstream against
[frankensqlite](https://github.com/Dicklesworthstone/frankensqlite). Tracked in
beads_rust as `beads_rust-ymik`.

## Summary

When whole zero pages are appended past the logical end of a SQLite database
image, fsqlite reports the file as corrupt in two different ways. SQLite accepts
the same file, because it reads the page count from the database header (page 1,
offset 28) and ignores any bytes beyond it.

| Operation | SQLite 3.x | fsqlite 0.1.12 / 0.1.13 / 0.1.18 |
|---|---|---|
| `PRAGMA integrity_check` | `ok` | `database disk image is malformed: page 3 is never used` |
| `VACUUM` | succeeds, rewrites to 8192 bytes | `database disk image is malformed: database image header page count 2 does not match file length page count 34` |

Both behaviours reproduce identically on **0.1.12, 0.1.13 and 0.1.18**, and in
both `journal_mode=DELETE` and `journal_mode=WAL`. This is a long-standing
divergence, not a recent regression.

## Why it matters

A database with trailing bytes is not exotic. It arises from an interrupted
copy, a truncated restore, a filesystem that rounds up an allocation, or — as in
our case — a test fixture that deliberately appends slack to exercise a
compaction path. SQLite treats such a file as healthy and `VACUUM` is the
canonical way to reclaim the slack.

Under fsqlite the operator is told the image is malformed and then cannot run
the one command that would normalize it, because `VACUUM` refuses on the same
grounds. That is a dead end: the file is reported broken, and the documented
repair is unavailable.

Note also that the two messages disagree about what is wrong. `integrity_check`
says a *specific page* is unreferenced; `VACUUM` says the *header page count*
disagrees with the file length. The second is the accurate description of the
condition.

## Reproducer

Self-contained, no beads_rust involvement. `Cargo.toml`:

```toml
[package]
name = "fsqlite-trailing-pages-repro"
version = "0.1.0"
edition = "2021"

[dependencies]
fsqlite = "0.1.18"   # also reproduced with 0.1.13 and 0.1.12
```

`src/main.rs`:

```rust
use fsqlite::Connection;
use std::fs::OpenOptions;
use std::io::Write;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: repro <scratch-dir>");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let db = format!("{dir}/repro.db");
    let _ = std::fs::remove_file(&db);

    // 1. Build an ordinary, valid database.
    {
        let conn = Connection::open(db.clone()).expect("open");
        conn.execute("PRAGMA journal_mode=WAL").expect("wal");
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .expect("create");
        for i in 0..200 {
            conn.execute(&format!("INSERT INTO t (id, v) VALUES ({i}, 'row {i}')"))
                .expect("insert");
        }
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").ok();
    }
    integrity(" 1. integrity_check, clean file     ", &db);
    vacuum(   " 2. VACUUM, clean file             ", &db);

    // 2. Append whole zero pages past the logical end of the image.
    let (page_size, appended_pages) = (4096u64, 32u64);
    {
        let mut f = OpenOptions::new().append(true).open(&db).expect("open append");
        f.write_all(&vec![0u8; (page_size * appended_pages) as usize]).expect("append");
        f.flush().expect("flush");
    }
    integrity(" 3. integrity_check, trailing pages", &db);
    vacuum(   " 4. VACUUM, trailing pages         ", &db);
}

fn integrity(label: &str, db: &str) {
    let conn = Connection::open(db.to_string()).expect("open");
    match conn.query("PRAGMA integrity_check") {
        Ok(rows) => println!("{label}: {rows:?}"),
        Err(e) => println!("{label}: ERROR: {e}"),
    }
}

fn vacuum(label: &str, db: &str) {
    let conn = Connection::open(db.to_string()).expect("open");
    match conn.execute("VACUUM") {
        Ok(_) => println!("{label}: OK -> {} bytes", std::fs::metadata(db).unwrap().len()),
        Err(e) => println!("{label}: ERROR: {e}"),
    }
}
```

Observed output (identical for 0.1.12, 0.1.13, 0.1.18):

```
 1. integrity_check, clean file     : [Row { values: [Text("ok")] }]
 2. VACUUM, clean file             : OK -> 8192 bytes
 3. integrity_check, trailing pages: [Row { values: [Text("database disk image is malformed: page 3 is never used")] }]
 4. VACUUM, trailing pages         : ERROR: database disk image is malformed: database image header page count 2 does not match file length page count 34
```

The same file under stock SQLite:

```console
$ sqlite3 repro.db 'PRAGMA integrity_check; PRAGMA page_count; PRAGMA page_size;'
ok
2
4096
$ sqlite3 repro.db 'VACUUM;' && wc -c < repro.db
8192
```

## Where it comes from

`integrity_check` walks `1..=total_pages` looking for pages that no b-tree or
freelist claims, and `total_pages` is the pager's published `db_size`
(`fsqlite-core/src/connection.rs`, the `page {} is never used` site — line 46471
in 0.1.13, 47621 in 0.1.18; the surrounding logic is unchanged between them).
When `db_size` is derived from the file length rather than the header's page
count, every appended page reads as leaked.

The `VACUUM` refusal is explicit: `exact_database_page_count()` computes
`file_size / page_size` and `database_image_receipt_for_open_file()` then
rejects the image when `header.page_count != page_count`
(`fsqlite-pager-0.1.18/src/pager.rs:5096` and `:5145`). fsqlite 0.1.13 has no
function of that name but reaches the same verdict, so the check exists there in
another form.

## Suggested direction

Treat the header's page count as authoritative for the logical extent of the
database, as SQLite does, and ignore bytes past `page_count * page_size`.
Trailing slack is not evidence of corruption.

If rejecting it is deliberate, then at minimum `VACUUM` should be exempt — it is
the operation that would fix the condition, it rewrites the image from the
logical page set anyway, and refusing it leaves no in-engine path back to a
file the engine will accept.

## Note for beads_rust

`br doctor` runs an orthogonal `PRAGMA integrity_check` through the system
`sqlite3` binary alongside its own, and reports both verdicts side by side
(`sqlite.integrity_check` vs `sqlite3.integrity_check`). That is what made this
divergence visible and is worth keeping.

One beads_rust-side behaviour remains unexplained and is **not** covered by the
reproducer above: `br` 0.2.16 (fsqlite 0.1.12) successfully VACUUMs a bloated
`.beads/beads.db`, while `br` 0.2.19 (fsqlite 0.1.18) fails on a byte-identical
fixture, even though `fix_db_bloat_via_vacuum_if_warned` is unchanged between
those tags and the standalone reproducer shows no version difference. The next
step is to bisect fsqlite 0.1.13 → 0.1.18 against br's `SqliteStorage::open`
path specifically, rather than a raw `Connection::open` as used here.
