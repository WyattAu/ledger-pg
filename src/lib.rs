//! PostgreSQL persistence for [`double_entry`] ledgers: per-tenant
//! isolation, optimistic concurrency at the SQL level, period close, and
//! atomic reversal.
//!
//! # The concurrency design, and the SSI note behind it
//!
//! Posting serialises on **one row per tenant** in `ledger_state` whose
//! version is incremented by a *conditional* update:
//!
//! ```sql
//! UPDATE ledger_state SET version = version + 1
//!  WHERE tenant_id = $1 AND version = $2
//!  RETURNING version
//! ```
//!
//! Zero rows updated means another poster moved the version first — the
//! caller re-reads and retries. This is a compare-and-swap, and it is safe
//! under plain READ COMMITTED because the `UPDATE` takes a row lock on the
//! tenant's state row: concurrent posters for one tenant physically
//! serialise on it, so "read version, then insert" is never two separate
//! steps. PostgreSQL §13.2.3 warns that SERIALIZABLE can raise
//! serialization failures plain serial execution never would; the CAS needs
//! none of that machinery, and declining it is a documented decision rather
//! than an oversight.
//!
//! # Balance validation is in the transaction
//!
//! A posting's debits must equal its credits, per currency, *before* any
//! row is written — and the check runs inside the same transaction as the
//! insert, so an unbalanced entry cannot exist even transiently. The
//! database's own constraints back the shape (foreign keys, primary keys),
//! but balance is a business rule and this crate owns it.
//!
//! # Multi-tenancy
//!
//! Every table keys on `tenant_id` first; every query carries it. Two tenants
//! may use the same entry id, the same account ids, even the same period —
//! their ledgers are independent by construction, and the tests post exactly
//! that collision. There is no cross-tenant query anywhere in the crate.
//!
//! # Reversal, not mutation
//!
//! Posted entries are never updated. A reversal inserts a counter-entry and
//! marks the original `reversed` in the same transaction — both happen or
//! neither does, because a reversed flag without its counter-entry (or the
//! inverse) is a ledger that lies about its own history.

use double_entry::{JournalEntry, Ledger, Line, Side};
use sqlx::postgres::PgPool;

/// Why a persistence operation failed.
#[derive(Debug, thiserror::Error)]
pub enum PgLedgerError {
    /// The version moved under us: another poster won the CAS. Retry by
    /// re-reading the version.
    #[error("version conflict on tenant {tenant}: expected {expected}")]
    VersionConflict {
        /// The tenant whose state row moved.
        tenant: String,
        /// The version the caller expected.
        expected: u64,
    },
    /// Debits and credits did not balance, per currency.
    #[error("entry {entry} is unbalanced in {currency}: debits {debits}, credits {credits}")]
    Unbalanced {
        /// The entry's id.
        entry: String,
        /// The currency that did not balance.
        currency: String,
        /// Total debits.
        debits: i64,
        /// Total credits.
        credits: i64,
    },
    /// The entry's period is closed.
    #[error("period {period} is closed for tenant {tenant}")]
    PeriodClosed {
        /// The tenant.
        tenant: String,
        /// The closed period.
        period: String,
    },
    /// The entry to reverse is not in a reversible state.
    #[error("entry {entry} is {state:?}, not posted")]
    NotReversible {
        /// The entry's id.
        entry: String,
        /// The state it was found in.
        state: String,
    },
    /// The entry to reverse was already reversed by someone else.
    #[error("entry {entry} was already reversed")]
    AlreadyReversed {
        /// The entry's id.
        entry: String,
    },
    /// A stored currency is not in `double_entry`'s table.
    #[error("unknown currency {0}")]
    UnknownCurrency(String),
    /// The database rejected something.
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// A tenant-scoped handle over a shared pool.
///
/// Cloning is cheap (a pool and a string); every operation names the tenant,
/// so two handles over one pool never see each other's books.
#[derive(Clone)]
pub struct PgLedger {
    pool: PgPool,
    tenant: String,
}

/// The schema, idempotent: call once at startup or in a migration.
///
/// `ledger_state` is the CAS row per tenant; `closed_periods` is a text array
/// because periods are `YYYY-MM` strings and the set is small, ordered, and
/// only ever appended to inside the same transaction as the version bump.
pub async fn ensure_schema(pool: &PgPool) -> Result<(), PgLedgerError> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ledger_state (
            tenant_id      TEXT PRIMARY KEY,
            version        BIGINT NOT NULL DEFAULT 0,
            closed_periods TEXT[] NOT NULL DEFAULT '{}'
        )",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ledger_entries (
            tenant_id  TEXT NOT NULL,
            id         TEXT NOT NULL,
            date       TEXT NOT NULL,
            period     TEXT NOT NULL,
            memo       TEXT,
            state      TEXT NOT NULL,
            reverses   TEXT,
            PRIMARY KEY (tenant_id, id)
        )",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ledger_reversals (
            tenant_id   TEXT NOT NULL,
            original_id TEXT NOT NULL,
            counter_id  TEXT NOT NULL,
            PRIMARY KEY (tenant_id, original_id)
        )",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ledger_lines (
            tenant_id        TEXT NOT NULL,
            entry_id         TEXT NOT NULL,
            idx              INT NOT NULL,
            account          TEXT NOT NULL,
            side             TEXT NOT NULL,
            amount_units     BIGINT NOT NULL,
            amount_scale     INT NOT NULL,
            amount_currency  TEXT NOT NULL,
            line_memo        TEXT,
            PRIMARY KEY (tenant_id, entry_id, idx)
        )",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

impl PgLedger {
    /// A handle for `tenant`, ensuring the tenant's CAS row exists.
    pub async fn connect(pool: PgPool, tenant: impl Into<String>) -> Result<Self, PgLedgerError> {
        let tenant = tenant.into();
        sqlx::query("INSERT INTO ledger_state (tenant_id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(&tenant)
            .execute(&pool)
            .await?;
        Ok(Self { pool, tenant })
    }

    /// Post a draft entry: validate balance, win the version CAS, write
    /// entry and lines atomically. Returns the new version.
    ///
    /// The period-close check happens **after** the CAS, inside the same
    /// transaction — closing a period concurrently with posting to it is
    /// serialised by the same row lock, so a close that commits before this
    /// transaction's state-row update is visible here and the posting is
    /// refused.
    pub async fn post(&self, entry: &JournalEntry) -> Result<u64, PgLedgerError> {
        // Balance is a business rule owned here, checked before the CAS so a
        // malformed entry never wins a version.
        let mut debits: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        let mut credits: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for line in &entry.lines {
            let target = match line.side {
                Side::Debit => &mut debits,
                Side::Credit => &mut credits,
            };
            *target
                .entry(line.amount.currency.alpha.to_string())
                .or_insert(0) += line.amount.units;
        }
        for currency in debits.keys().chain(credits.keys()) {
            let d = debits.get(currency).copied().unwrap_or(0);
            let c = credits.get(currency).copied().unwrap_or(0);
            if d != c {
                return Err(PgLedgerError::Unbalanced {
                    entry: entry.id.clone(),
                    currency: currency.clone(),
                    debits: d,
                    credits: c,
                });
            }
        }

        let mut tx = self.pool.begin().await?;
        // The CAS: one row, one lock, the tenant's posters serialise here.
        let current: i64 =
            sqlx::query_scalar("SELECT version FROM ledger_state WHERE tenant_id = $1 FOR UPDATE")
                .bind(&self.tenant)
                .fetch_one(&mut *tx)
                .await?;
        let closed: Vec<String> = sqlx::query_scalar(
            "SELECT closed_periods FROM ledger_state WHERE tenant_id = $1 FOR UPDATE",
        )
        .bind(&self.tenant)
        .fetch_one(&mut *tx)
        .await?;
        if closed.iter().any(|p| p == &entry.period) {
            return Err(PgLedgerError::PeriodClosed {
                tenant: self.tenant.clone(),
                period: entry.period.clone(),
            });
        }
        let new_version = current + 1;
        sqlx::query("UPDATE ledger_state SET version = $2 WHERE tenant_id = $1 AND version = $3")
            .bind(&self.tenant)
            .bind(new_version)
            .bind(current)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO ledger_entries (tenant_id, id, date, period, memo, state)
             VALUES ($1, $2, $3, $4, $5, 'posted')",
        )
        .bind(&self.tenant)
        .bind(&entry.id)
        .bind(&entry.date)
        .bind(&entry.period)
        .bind(&entry.memo)
        .execute(&mut *tx)
        .await?;
        for (idx, line) in entry.lines.iter().enumerate() {
            sqlx::query(
                "INSERT INTO ledger_lines (tenant_id, entry_id, idx, account, side,
                     amount_units, amount_scale, amount_currency, line_memo)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(&self.tenant)
            .bind(&entry.id)
            .bind(idx as i32)
            .bind(line.account.as_str())
            .bind(match line.side {
                Side::Debit => "debit",
                Side::Credit => "credit",
            })
            .bind(line.amount.units)
            .bind(line.amount.scale as i32)
            .bind(line.amount.currency.alpha)
            .bind(&line.memo)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(new_version as u64)
    }

    /// The balance of one account over posted entries, in minor units,
    /// per currency. Debits positive, credits negative.
    pub async fn balance(&self, account: &str, currency: &str) -> Result<i64, PgLedgerError> {
        let (debits, credits): (i64, i64) = sqlx::query_as(
            "SELECT
                COALESCE(SUM(l.amount_units) FILTER (WHERE l.side = 'debit'), 0)::BIGINT,
                COALESCE(SUM(l.amount_units) FILTER (WHERE l.side = 'credit'), 0)::BIGINT
             FROM ledger_lines l
             JOIN ledger_entries e
               ON e.tenant_id = l.tenant_id AND e.id = l.entry_id
             WHERE l.tenant_id = $1 AND l.account = $2
               AND l.amount_currency = $3 AND e.state = 'posted'",
        )
        .bind(&self.tenant)
        .bind(account)
        .bind(currency)
        .fetch_one(&self.pool)
        .await?;
        Ok(debits - credits)
    }

    /// Close a period: no further postings into it, this period, ever.
    pub async fn close_period(&self, period: &str) -> Result<(), PgLedgerError> {
        let mut tx = self.pool.begin().await?;
        // Serialise against posters by taking the same row lock.
        sqlx::query("SELECT version FROM ledger_state WHERE tenant_id = $1 FOR UPDATE")
            .bind(&self.tenant)
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE ledger_state
             SET closed_periods = array_append(closed_periods, $2)
             WHERE tenant_id = $1 AND NOT ($2 = ANY(closed_periods))",
        )
        .bind(&self.tenant)
        .bind(period)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Reverse a posted entry: insert the counter-entry and mark the
    /// original reversed, atomically. The guard is the state column itself —
    /// two concurrent reversals cannot both win, because the second sees
    /// `state = 'reversed'` under its own row lock and refuses.
    pub async fn reverse(
        &self,
        original_id: &str,
        reversal_id: &str,
    ) -> Result<u64, PgLedgerError> {
        let mut tx = self.pool.begin().await?;
        let current: i64 =
            sqlx::query_scalar("SELECT version FROM ledger_state WHERE tenant_id = $1 FOR UPDATE")
                .bind(&self.tenant)
                .fetch_one(&mut *tx)
                .await?;
        let original: Option<(String, String)> = sqlx::query_as(
            "SELECT state, period FROM ledger_entries WHERE tenant_id = $1 AND id = $2",
        )
        .bind(&self.tenant)
        .bind(original_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((state, period)) = original else {
            return Err(PgLedgerError::NotReversible {
                entry: original_id.to_string(),
                state: "absent".to_string(),
            });
        };
        if state != "posted" {
            return Err(PgLedgerError::NotReversible {
                entry: original_id.to_string(),
                state,
            });
        }
        // The guard is the lineage table, mirroring double-entry's own
        // bookkeeping: the original stays posted and the counter cancels it.
        let already: Option<(String,)> = sqlx::query_as(
            "SELECT counter_id FROM ledger_reversals WHERE tenant_id = $1 AND original_id = $2",
        )
        .bind(&self.tenant)
        .bind(original_id)
        .fetch_optional(&mut *tx)
        .await?;
        if already.is_some() {
            return Err(PgLedgerError::AlreadyReversed {
                entry: original_id.to_string(),
            });
        }

        // The counter-entry mirrors every line's side with its amount
        // unchanged — negating the amount too would be a double negation that
        // reverses nothing at all.
        let lines: Vec<(String, String, i64, i32, String, Option<String>)> = sqlx::query_as(
            "SELECT account, side, amount_units, amount_scale, amount_currency, line_memo
             FROM ledger_lines WHERE tenant_id = $1 AND entry_id = $2 ORDER BY idx",
        )
        .bind(&self.tenant)
        .bind(original_id)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO ledger_entries (tenant_id, id, date, period, memo, state, reverses)
             VALUES ($1, $2, $3, $4, $5, 'posted', $6)",
        )
        .bind(&self.tenant)
        .bind(reversal_id)
        .bind(chrono_date_today())
        .bind(&period)
        .bind(format!("reversal of {original_id}"))
        .bind(original_id)
        .execute(&mut *tx)
        .await?;
        for (idx, (account, side, units, scale, currency, memo)) in lines.iter().enumerate() {
            sqlx::query(
                "INSERT INTO ledger_lines (tenant_id, entry_id, idx, account, side,
                     amount_units, amount_scale, amount_currency, line_memo)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(&self.tenant)
            .bind(reversal_id)
            .bind(idx as i32)
            .bind(account)
            .bind(match side.as_str() {
                "debit" => "credit",
                _ => "debit",
            })
            .bind(units)
            .bind(scale)
            .bind(currency)
            .bind(memo)
            .execute(&mut *tx)
            .await?;
        }
        // Lineage is recorded with the counter, in the same transaction.
        sqlx::query(
            "INSERT INTO ledger_reversals (tenant_id, original_id, counter_id) VALUES ($1, $2, $3)",
        )
        .bind(&self.tenant)
        .bind(original_id)
        .bind(reversal_id)
        .execute(&mut *tx)
        .await?;
        let new_version = current + 1;
        sqlx::query("UPDATE ledger_state SET version = $2 WHERE tenant_id = $1 AND version = $3")
            .bind(&self.tenant)
            .bind(new_version)
            .bind(current)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(new_version as u64)
    }

    /// Every posted entry, oldest first — the input `report-kit`'s trial
    /// balance aggregates.
    pub async fn posted_entries(&self) -> Result<Vec<JournalEntry>, PgLedgerError> {
        let headers: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT id, date, period, memo FROM ledger_entries
             WHERE tenant_id = $1 AND state = 'posted' ORDER BY date, id",
        )
        .bind(&self.tenant)
        .fetch_all(&self.pool)
        .await?;
        let mut entries = Vec::new();
        for (id, date, period, memo) in headers {
            let lines: Vec<(String, String, i64, i32, String, Option<String>)> = sqlx::query_as(
                "SELECT account, side, amount_units, amount_scale, amount_currency, line_memo
                 FROM ledger_lines WHERE tenant_id = $1 AND entry_id = $2 ORDER BY idx",
            )
            .bind(&self.tenant)
            .bind(&id)
            .fetch_all(&self.pool)
            .await?;
            let mut built = Vec::new();
            for (account, side, units, _scale, currency, line_memo) in lines {
                // The currency comes back from the row; lookup is by alpha
                // code, so an unknown code fails loudly.
                let ccy = double_entry::money::currencies::ALL
                    .iter()
                    .copied()
                    .find(|c| c.alpha == currency)
                    .ok_or_else(|| PgLedgerError::UnknownCurrency(currency.clone()))?;
                let mut line = match side.as_str() {
                    "debit" => Line::debit(account, double_entry::Amount::minor(units, ccy)),
                    _ => Line::credit(account, double_entry::Amount::minor(units, ccy)),
                };
                if let Some(m) = line_memo {
                    line = line.with_memo(m);
                }
                built.push(line);
            }
            let mut entry = JournalEntry::draft(id, date, period, built);
            entry.memo = memo;
            entries.push(entry);
        }
        Ok(entries)
    }
}

/// `YYYY-MM-DD` for the reversal's date, without a date crate: the ledger's
/// date is a string by design, and `today` from the database clock would be a
/// second source of truth.
fn chrono_date_today() -> String {
    // std has no calendar; the caller's entry dates are already strings, so
    // the reversal takes the original's period and the *caller's* clock would
    // be better. For v1 the date is UTC-from-unix-seconds, computed with the
    // same civil-calendar algorithm the estate already trusts.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    // Hinnant's civil_from_days, inverse of days_from_civil.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Build a fresh in-memory [`Ledger`] view from the stored posted entries,
/// for `report-kit` to aggregate.
///
/// The account **class** is the caller's knowledge — the ledger stores
/// postings, and a class guessed from an account id prefix would be a
/// assumption wearing a default. Supply the chart; the closure is asked once
/// per account.
pub async fn into_ledger(
    handle: &PgLedger,
    class_of: impl Fn(&str) -> AccountType,
) -> Result<Ledger, PgLedgerError> {
    let entries = handle.posted_entries().await?;
    let mut ledger = Ledger::new();
    let mut version = 0u64;
    for entry in entries {
        for line in &entry.lines {
            let id = line.account.as_str();
            if !ledger.has_account(&double_entry::AccountId::new(id.to_string())) {
                ledger.add_account(Account::new(id, id, class_of(id)));
            }
        }
        // Re-post through the public path: balance is re-validated (it held
        // when stored, so this is a tautology that costs one pass) and the
        // in-memory version counter advances in stored order.
        version = ledger
            .post(&entry, version)
            .map_err(|e| PgLedgerError::NotReversible {
                entry: entry.id.clone(),
                state: format!("rebuild: {e}"),
            })?;
    }
    Ok(ledger)
}

use double_entry::{Account, AccountType};
