//! The tamper-evident hash chain, against a real PostgreSQL.
//!
//! Modeled on Odoo's `account.move` inalterability hash — SHA-256 over the
//! previous hash plus a canonical serialization of the entry and its lines —
//! with one structural advantage the estate's money representation buys: our
//! amounts are integers, so the canonical form is deterministic without
//! Odoo's `float_repr` stabilization.
//!
//! The tamper tests write directly through raw SQL, deliberately bypassing
//! the crate: the chain exists to catch exactly that — an alteration made
//! outside the API, with database credentials.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use double_entry::money::currencies;
use double_entry::{JournalEntry, Line};
use ledger_pg::{PgLedger, ensure_schema, verify_chain};

struct PgInstance {
    dsn: String,
    container: Option<String>,
}

impl Drop for PgInstance {
    fn drop(&mut self) {
        if let Some(id) = &self.container {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", id])
                .output();
        }
    }
}

async fn start_postgres(tenant: &str) -> Option<(PgInstance, PgLedger)> {
    if let Ok(dsn) = std::env::var("TEST_DATABASE_URL") {
        let pool = sqlx::postgres::PgPool::connect(&dsn).await.ok()?;
        ensure_schema(&pool).await.expect("schema");
        let handle = PgLedger::connect(pool, tenant).await.expect("tenant");
        return Some((
            PgInstance {
                dsn,
                container: None,
            },
            handle,
        ));
    }
    let id = format!(
        "ledger-pg-chain-{}-{}",
        tenant.replace('-', ""),
        std::process::id()
    );
    let started = std::process::Command::new("docker")
        .args([
            "run",
            "--rm",
            "-d",
            "--name",
            &id,
            "-e",
            "POSTGRES_PASSWORD=test",
            "-p",
            "54333:5432",
            "postgres:16-alpine",
        ])
        .output()
        .ok()?;
    if !started.status.success() {
        eprintln!("TEST SKIPPED: docker unavailable");
        return None;
    }
    let dsn = "postgres://postgres:test@127.0.0.1:54333/postgres";
    let mut attempt = 0;
    loop {
        if sqlx::postgres::PgPool::connect(dsn).await.is_ok() {
            break;
        }
        attempt += 1;
        if attempt > 60 {
            eprintln!("TEST SKIPPED: postgres not ready");
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let pool = sqlx::postgres::PgPool::connect(dsn)
        .await
        .expect("connects");
    ensure_schema(&pool).await.expect("schema");
    let handle = PgLedger::connect(pool, tenant).await.expect("tenant");
    Some((
        PgInstance {
            dsn: dsn.to_string(),
            container: Some(id),
        },
        handle,
    ))
}

async fn raw_pool(pg: &PgInstance) -> sqlx::PgPool {
    // A second pool for tampering: the whole point is to alter rows the way
    // someone with database credentials would — not through the crate.
    sqlx::postgres::PgPool::connect(&pg.dsn)
        .await
        .expect("tamper pool")
}

fn entry(id: &str, amount: i64) -> JournalEntry {
    JournalEntry::draft(
        id,
        "2026-07-02",
        "2026-07",
        vec![
            Line::debit("1000", double_entry::Amount::minor(amount, currencies::GBP)),
            Line::credit("4000", double_entry::Amount::minor(amount, currencies::GBP)),
        ],
    )
}

/// The container outlives the test body: stop it before the harness's Drop
/// tries again, and close pools so connection-refused noise stays quiet.
async fn cleanup(pg: &PgInstance, pool: &sqlx::PgPool) {
    pool.close().await;
    if let Some(id) = &pg.container {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", id])
            .output();
    }
}

#[tokio::test]
async fn the_chain_verifies_clean_after_normal_posting() {
    let Some((pg, ledger)) = start_postgres("chain-clean").await else {
        return;
    };
    ledger.post(&entry("JE-1", 1_000)).await.expect("posts");
    ledger.post(&entry("JE-2", 2_000)).await.expect("posts");
    ledger.reverse("JE-1", "JE-1R").await.expect("reverses");
    ledger.post(&entry("JE-3", 3_000)).await.expect("posts");

    let verification = verify_chain(&ledger).await.expect("walks");
    assert_eq!(verification.broken_at, None, "no tampering, no break");
    assert_eq!(
        verification.chained, 4,
        "JE-1, its reversal, JE-2 and JE-3 all chained"
    );
    cleanup(&pg, ledger.pool()).await;
}

#[tokio::test]
async fn a_tampered_amount_breaks_the_chain_at_that_version() {
    let Some((pg, ledger)) = start_postgres("chain-tamper").await else {
        return;
    };
    ledger.post(&entry("JE-1", 1_000)).await.expect("posts");
    ledger.post(&entry("JE-2", 2_000)).await.expect("posts");
    ledger.post(&entry("JE-3", 3_000)).await.expect("posts");

    // The alteration someone with database credentials would make: turn the
    // £20.00 sale into £2,000.00, bypassing the API entirely.
    let pool = raw_pool(&pg).await;
    sqlx::query(
        "UPDATE ledger_lines SET amount_units = 200000
         WHERE entry_id = 'JE-2' AND side = 'debit'",
    )
    .execute(&pool)
    .await
    .expect("the tampering applies");

    let verification = verify_chain(&ledger).await.expect("walks");
    let broken = verification.broken_at.expect("the tamper is detected");
    assert_eq!(broken.version, 2, "JE-2 is chain version 2");
    assert_eq!(broken.entry_id, "JE-2");
    assert_eq!(broken.reason, ledger_pg::ChainBreakReason::HashMismatch);
    cleanup(&pg, &pool).await;
}

#[tokio::test]
async fn a_tampered_memo_breaks_it_too_because_the_memo_is_hashed() {
    let Some((pg, ledger)) = start_postgres("chain-memo").await else {
        return;
    };
    let mut e = entry("JE-1", 5_000);
    for line in &mut e.lines {
        line.memo = Some("consulting, INV-071".into());
    }
    ledger.post(&e).await.expect("posts");

    // Rewriting a memo is the quiet alteration: the amounts still add up.
    let pool = raw_pool(&pg).await;
    sqlx::query(
        "UPDATE ledger_lines SET line_memo = 'consulting, INV-999'
         WHERE entry_id = 'JE-1' AND side = 'debit'",
    )
    .execute(&pool)
    .await
    .expect("applies");
    let verification = verify_chain(&ledger).await.expect("walks");
    assert_eq!(
        verification.broken_at.expect("detected").reason,
        ledger_pg::ChainBreakReason::HashMismatch,
        "memos are part of the canonical form"
    );
    cleanup(&pg, &pool).await;
}

#[tokio::test]
async fn a_deleted_entry_is_a_broken_link_not_a_silent_gap() {
    let Some((pg, ledger)) = start_postgres("chain-delete").await else {
        return;
    };
    ledger.post(&entry("JE-1", 1_000)).await.expect("posts");
    ledger.post(&entry("JE-2", 2_000)).await.expect("posts");

    let pool = raw_pool(&pg).await;
    sqlx::query("DELETE FROM ledger_entries WHERE id = 'JE-1'")
        .execute(&pool)
        .await
        .expect("the deletion applies");
    let verification = verify_chain(&ledger).await.expect("walks");
    assert!(
        verification.broken_at.is_some(),
        "JE-2's stored previous_hash now references a missing entry"
    );
    cleanup(&pg, &pool).await;
}
