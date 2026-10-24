# Changelog

All notable changes to this project are documented here. Format: [Keep a
Changelog](https://keepachangelog.com/) — versions follow [semver](https://semver.org).

## [Unreleased]

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
  revenue class and silently report a zero retained result — the class is
  accounting knowledge the ledger does not store, so it is now a required,
  visible call-site decision.


## [0.1.0] - 2026-10-24

Initial release.

### Added

- `ensure_schema` — idempotent schema: `ledger_state` (the CAS row per
  tenant), `ledger_entries`, `ledger_lines`, `ledger_reversals`.
- `PgLedger::post` — balance validated inside the transaction, period-close
  checked after the state-row lock, optimistic concurrency via conditional
  update. Refused entries consume no version.
- `PgLedger::reverse` — counter-entry plus lineage in one transaction; a
  second reversal is refused under the same lock.
- `PgLedger::balance`, `close_period`, `posted_entries`, and `into_ledger` for
  handing the stored books to `report-kit`.
- Per-tenant isolation throughout; identical entry ids across tenants are
  legal and independent, and the tests post that collision.
- Integration tests against a real PostgreSQL (container harness, or
  `TEST_DATABASE_URL`), including eight concurrent posters serialising to
  distinct versions with no lost writes.
