//! Integration tests against a real PostgreSQL.
//!
//! The harness uses `TEST_DATABASE_URL` if set; otherwise it starts a
//! throwaway `postgres:16-alpine` container on a fixed port and tears it down.
//! Without either, the tests print a line and pass — CI boxes without Docker
//! still go green, but they have not run these, and the output says so.

#![allow(clippy::expect_used, clippy::panic)]

use double_entry::money::currencies;
use double_entry::{JournalEntry, Line};
use ledger_pg::{PgLedger, ensure_schema, into_ledger};

/// A running Postgres, disposed on drop.
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

async fn start_postgres() -> Option<PgInstance> {
    if let Ok(dsn) = std::env::var("TEST_DATABASE_URL") {
        return Some(PgInstance {
            dsn,
            container: None,
        });
    }
    let id = format!("ledger-pg-test-{}", std::process::id());
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
            "54329:5432",
            "postgres:16-alpine",
        ])
        .output()
        .ok()?;
    if !started.status.success() {
        eprintln!(
            "TEST SKIPPED: docker unavailable ({})",
            String::from_utf8_lossy(&started.stderr)
        );
        return None;
    }
    // Wait for readiness: a TCP accept is not enough, the db must answer.
    let pool =
        sqlx::postgres::PgPool::connect("postgres://postgres:test@127.0.0.1:54329/postgres").await;
    let mut attempt = 0;
    let mut pool = pool.ok();
    while pool.is_none() && attempt < 60 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        pool = sqlx::postgres::PgPool::connect("postgres://postgres:test@127.0.0.1:54329/postgres")
            .await
            .ok();
        attempt += 1;
    }
    drop(pool);
    Some(PgInstance {
        dsn: "postgres://postgres:test@127.0.0.1:54329/postgres".into(),
        container: Some(id),
    })
}

async fn ledger_for(tenant: &str) -> Option<(PgInstance, PgLedger)> {
    let instance = start_postgres().await?;
    let pool = sqlx::postgres::PgPool::connect(&instance.dsn)
        .await
        .expect("connects once the container answers");
    ensure_schema(&pool).await.expect("schema applies");
    let handle = PgLedger::connect(pool, tenant)
        .await
        .expect("tenant registered");
    Some((instance, handle))
}

fn sale_entry(id: &str) -> JournalEntry {
    JournalEntry::draft(
        id,
        "2026-07-02",
        "2026-07",
        vec![
            Line::debit("1000", double_entry::Amount::minor(12_000, currencies::GBP)),
            Line::credit("4000", double_entry::Amount::minor(12_000, currencies::GBP)),
        ],
    )
}

#[tokio::test]
async fn posting_round_trips_and_balances_survive_the_database() {
    let Some((_pg, ledger)) = ledger_for("round-trip").await else {
        return;
    };
    let v1 = ledger.post(&sale_entry("JE-1")).await.expect("posts");
    assert_eq!(v1, 1);
    let v2 = ledger
        .post(&sale_entry("JE-2"))
        .await
        .expect("the second posts");
    assert_eq!(v2, 2, "the version advances per tenant");

    assert_eq!(
        ledger.balance("1000", "GBP").await.expect("balance"),
        24_000
    );
    // Rebuild an in-memory ledger and let report-kit read it.
    let rebuilt = into_ledger(&ledger).await.expect("rebuilds");
    assert_eq!(
        rebuilt.balance(&double_entry::AccountId::new("1000")),
        24_000
    );
}

#[tokio::test]
async fn an_unbalanced_entry_is_refused_before_it_wins_a_version() {
    let Some((_pg, ledger)) = ledger_for("unbalanced").await else {
        return;
    };
    let bad = JournalEntry::draft(
        "JE-BAD",
        "2026-07-02",
        "2026-07",
        vec![
            Line::debit("1000", double_entry::Amount::minor(10_000, currencies::GBP)),
            Line::credit("4000", double_entry::Amount::minor(9_999, currencies::GBP)),
        ],
    );
    assert!(matches!(
        ledger.post(&bad).await,
        Err(ledger_pg::PgLedgerError::Unbalanced { .. })
    ));
    // The version did not move: nothing was written.
    let good = ledger.post(&sale_entry("JE-GOOD")).await.expect("posts");
    assert_eq!(good, 1, "the refused entry consumed no version");
}

#[tokio::test]
async fn concurrent_posters_serialise_and_lose_no_writes() {
    let Some((_pg, ledger)) = ledger_for("concurrent").await else {
        return;
    };
    let mut handles = Vec::new();
    for index in 0..8 {
        let handle = ledger.clone();
        handles.push(tokio::spawn(async move {
            handle
                .post(&sale_entry(&format!("JE-{index}")))
                .await
                .expect("each poster wins a version eventually")
        }));
    }
    let mut versions = Vec::new();
    for handle in handles {
        versions.push(handle.await.expect("join"));
    }
    versions.sort();
    // Every poster got a distinct version: no lost updates, no duplicates.
    assert_eq!(
        versions,
        vec![1, 2, 3, 4, 5, 6, 7, 8],
        "the CAS serialises posters per tenant"
    );
    assert_eq!(
        ledger.balance("1000", "GBP").await.expect("balance"),
        96_000,
        "eight entries, all counted"
    );
}

#[tokio::test]
async fn tenants_are_independent_even_with_identical_ids() {
    let Some((pg, a)) = ledger_for("tenant-a").await else {
        return;
    };
    let b = PgLedger::connect(pg_pool(&pg).await, "tenant-b")
        .await
        .expect("second tenant");
    // The same entry id in both tenants: legal, because tenant_id keys first.
    a.post(&sale_entry("JE-1")).await.expect("a posts");
    b.post(&sale_entry("JE-1"))
        .await
        .expect("b posts the same id");

    assert_eq!(a.balance("1000", "GBP").await.expect("a"), 12_000);
    assert_eq!(b.balance("1000", "GBP").await.expect("b"), 12_000);

    // And reversing in one tenant does not touch the other.
    a.reverse("JE-1", "JE-1R").await.expect("a reverses");
    assert_eq!(a.balance("1000", "GBP").await.expect("a"), 0);
    assert_eq!(
        b.balance("1000", "GBP").await.expect("b"),
        12_000,
        "b unaffected"
    );
}

async fn pg_pool(pg: &PgInstance) -> sqlx::PgPool {
    sqlx::postgres::PgPool::connect(&pg.dsn)
        .await
        .expect("pool")
}

#[tokio::test]
async fn period_close_refuses_late_postings_atomically() {
    let Some((_pg, ledger)) = ledger_for("periods").await else {
        return;
    };
    ledger.post(&sale_entry("JE-1")).await.expect("posts");
    ledger.close_period("2026-07").await.expect("closes");
    assert!(matches!(
        ledger.post(&sale_entry("JE-2")).await,
        Err(ledger_pg::PgLedgerError::PeriodClosed { .. })
    ));
    // And the refused posting consumed nothing: the ledger's balance is where
    // the close left it.
    assert_eq!(
        ledger.balance("1000", "GBP").await.expect("balance"),
        12_000
    );
}

#[tokio::test]
async fn reversal_is_atomic_and_doubly_refused() {
    let Some((_pg, ledger)) = ledger_for("reversals").await else {
        return;
    };
    ledger.post(&sale_entry("JE-1")).await.expect("posts");
    ledger.reverse("JE-1", "JE-1R").await.expect("reverses");
    assert_eq!(ledger.balance("1000", "GBP").await.expect("balance"), 0);

    // A second reversal of the same entry: refused, the history is not
    // rewritten twice.
    assert!(matches!(
        ledger.reverse("JE-1", "JE-1R2").await,
        Err(ledger_pg::PgLedgerError::AlreadyReversed { .. })
    ));
    // The refused reversal consumed no version.
    let next = ledger
        .post(&sale_entry("JE-2"))
        .await
        .expect("posts after the refusal");
    assert_eq!(next, 3, "JE-1, JE-1R, then this: the refusal consumed none");
}
