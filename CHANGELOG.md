# Changelog

All notable changes to this project are documented here. Format: [Keep a
Changelog](https://keepachangelog.com/); versions follow [semver](https://semver.org).

## [Unreleased]

## [0.2.1] - 2026-10-26

### Fixed

- **0.2.0's reversals were double-flipped and doubled the posting.** The
  chain work introduced `flipped` line tuples, but the insert loop still
  bound a `match side` on top; flipping the already-flipped values back to
  the original sides. A reversal stored a counter-entry identical to its
  original: the posting was doubled, not reversed, and the hash chain
  detected it on every verify (the stored lines were not the hashed lines).
  0.2.0 is yanked. The tests that caught it are the chain tamper tests and
  the ledger-balance assertion working together.

### Known issue in 0.2.0 (yanked)

- Published before the integration suite ran; `cargo publish` verifies the
  library build but not `tests/`. Process change recorded in the loop docs:
  a release does not go out until the suite that covers the change has run
  green, even when the change is "just plumbing".


## [0.2.0] - 2026-10-26

### Added

- **A tamper-evident hash chain over posted entries**, modeled on Odoo's
  `account.move` inalterability hash: SHA-256 over the previous entry's hash
  plus a canonical serialization of the entry and every line (account, side,
  units, scale, currency, memo). The canonical form is deterministic without
  Odoo's `float_repr` stabilization, because the amounts are integers.
- `verify_chain`; walks the chain in version order, recomputes every hash,
  and reports the version, entry and reason of the first break. Tamper tests
  write through raw SQL with database credentials, deliberately bypassing the
  crate: altered amounts, altered memos (the quiet alteration; amounts still
  add up), and deleted entries are all detected.
- Reversals join the chain: a reversal is as much a posting as the entry it
  corrects, and omitting it would let a tamperer delete a reversal without
  breaking the walk.


## [0.1.3] - 2026-10-24

### Changed

- Tracks `double-entry 0.1.1` (docs-only upstream release, identical API). Pin move keeps one version in a consumer graph that binds 0.1.1.


## [0.1.2] - 2026-10-24

### Changed

- Tracks  (docs-only upstream release, identical API). Pin
  move keeps one version in a consumer graph that binds 0.1.1.


## [0.1.1] - 2026-10-24

### Changed

- `into_ledger` takes the account **class chart** as a closure. The previous
  default (every account an asset) made a P&L over the rebuilt books see no
  revenue class and silently report a zero retained result; the class is
  accounting knowledge the ledger does not store, so it is now a required,
  visible call-site decision.


## [0.1.0] - 2026-10-24

Initial release.

### Added

- `ensure_schema`; idempotent schema: `ledger_state` (the CAS row per
  tenant), `ledger_entries`, `ledger_lines`, `ledger_reversals`.
- `PgLedger::post`; balance validated inside the transaction, period-close
  checked after the state-row lock, optimistic concurrency via conditional
  update. Refused entries consume no version.
- `PgLedger::reverse`; counter-entry plus lineage in one transaction; a
  second reversal is refused under the same lock.
- `PgLedger::balance`, `close_period`, `posted_entries`, and `into_ledger` for
  handing the stored books to `report-kit`.
- Per-tenant isolation throughout; identical entry ids across tenants are
  legal and independent, and the tests post that collision.
- Integration tests against a real PostgreSQL (container harness, or
  `TEST_DATABASE_URL`), including eight concurrent posters serialising to
  distinct versions with no lost writes.
