# ledger-pg

PostgreSQL persistence for [`double-entry`](https://crates.io/crates/double-entry)
ledgers: per-tenant isolation, optimistic concurrency at the SQL level, period
close, and atomic reversal.

## The concurrency design, and the SSI note behind it

Posting serialises on **one row per tenant** in `ledger_state`, incremented by
a conditional update; a compare-and-swap. Zero rows updated means another
poster moved the version first. The CAS is safe under plain READ COMMITTED
because the `UPDATE` takes a row lock on the tenant's state row: concurrent
posters for one tenant physically serialise on it, so "read version, then
insert" is never two separate steps. PostgreSQL §13.2.3 warns that SERIALIZABLE
can raise serialization failures plain serial execution never would; the CAS
needs none of that machinery, and declining it is a documented decision.

## Balance validation is in the transaction

Debits must equal credits, per currency, *before* any row is written; checked
inside the same transaction as the insert, so an unbalanced entry cannot exist
even transiently. A refused entry consumes no version.

## Multi-tenancy

Every table keys on `tenant_id` first; every query carries it. Two tenants may
use the same entry id, the same account ids, even the same period; their
ledgers are independent by construction, and the tests post exactly that
collision. There is no cross-tenant query anywhere in the crate.

## Reversal, not mutation

Posted entries are never rewritten. A reversal inserts the counter-entry -
every leg mirrored with its side flipped, amount unchanged (negating the
amount too would be a double negation that reverses nothing); and records the
lineage in `ledger_reversals`, all in one transaction. This mirrors
`double-entry`'s own bookkeeping exactly: the original stays posted, the
counter cancels it, and a second reversal of the same entry is refused by the
lineage table under the same row lock.

## Tests

`TEST_DATABASE_URL` is used if set; otherwise the harness starts a throwaway
`postgres:16-alpine` container and tears it down. Without either, the tests
say they were skipped rather than pretending to have run.

## Licence

MIT OR Apache-2.0.
