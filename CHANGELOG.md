# Changelog

Notable changes to `openadr`. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) — with the 0.x caveat
that a minor release may break.

The reasoning behind a change lives in `concepts/DECISIONS.md`, cited here as `D-nnn`. What is not
built yet lives in `concepts/ROADMAP.md`. Neither belongs on this page.

## [0.2.0] — unreleased

Pre-1.0: this release breaks. There is no migration path and none is needed — the schema is applied
on connect, and a change to it is a change to the file format.

### Security

- A credential carrying only the `read_bl` scope no longer resolves to the business-logic role.
  `read_bl` gates five endpoints — the collection-wide MQTT topic listings — and read as an identity
  it granted unrestricted reads of every VEN's reports and every targeted event, because business
  logic is the role object privacy does not apply to. It now passes the read gate as an ordinary
  identified client and confers nothing else (D-121).
- The `/notifiers/mqtt/topics/…` handlers check the caller's scope before they check whether a broker
  is configured, so an unauthorised caller no longer learns the deployment's shape (D-121).

### Breaking

- `problem.type` URIs move from `https://openadr.dev/problems/` to
  `https://hupe1980.github.io/openadr/problems/`, a domain this project publishes, and each one now
  resolves to documentation of that type. A client matching on the literal URI must be updated
  (D-119).
- `vtn::api::fanout` splits into `fanout` for `program` and `event` and `fanout_owned` for `ven`,
  `resource`, `report` and `subscription`. Only an embedder that calls the API module directly is
  affected; the router is unchanged (D-124).
- `Scopes::is_business_logic` no longer returns `true` for `read_bl` alone (D-121).
- `POST`/`PUT /events` reject a body whose `duration`, `intervalPeriod.duration` or `randomizeStart`
  is negative. The schema's pattern permits a leading `-`; nothing OpenADR uses a duration for has a
  backwards reading, and such an event was accepted and then inert for ever (D-126).

### Added

- `Duration::is_negative`, so a client can report what a peer sent without the model refusing it
  (D-126).
- `cargo xtask trace` — every `MUST` and `SHALL` in the Definitions and the notifier binding document
  is matched against the clause citations in the source. 34 of 34, nothing exempted, and CI fails
  when a release adds a requirement nobody places (D-130).
- `cargo xtask check-suite` — fails if a `Storage` method has no behaviour in the shared conformance
  suite, or if a behaviour is written and left out of the run list (D-129).
- `cargo xtask check-problems` — fails if a `problem.type` URI would resolve to nothing (D-119).
- Five storage conformance behaviours, taking the suite from 52 to 57: the owner lookup including its
  absent case, update and delete across every collection, backend liveness, and that waiting for work
  returns inside its budget. Four of the six collections had no update or delete behaviour, so the
  SQL backends' write paths for them were exercised only in memory (D-129).

### Fixed

- `Vtn::serve_tls` starts the retention sweeper, which only `Vtn::serve` did. A VTN configured to age
  reports out and served over TLS aged nothing out — on the deployment `--tls-cert` exists for
  (D-128).
- `VenRuntime::time_to_next_wakeup` measures to the nanosecond rather than to the whole second. A
  transition 999 ms away rounded to a second late; one 1 ms away rounded to zero, and the loop then
  synced in a tight circle until the second turned over (D-122).
- Deleting a programme clears the target rows of the subscriptions its cascade removes. `object_target`
  is polymorphic and carries no foreign key, so nothing cascades into it; both SQL backends leaked
  rows for every programme-scoped subscription ever deleted (D-123).
- The PostgreSQL schema is applied under an advisory lock. `CREATE TABLE IF NOT EXISTS` is not safe to
  run concurrently, so two instances starting at once — a rolling deploy — could fail with an error
  naming an internal catalogue index (D-125).
- The JWKS key cache serves a key it already holds when the cache is stale and the refresh is
  rate-limited. Setting `refresh_after` below `min_refresh_interval` put every request in the gap
  between the two and refused every token (D-127).
- `POST /reports` no longer sweeps every VEN's grant when a broker is configured. It reaches exactly
  one VEN topic, and the snapshot is now one indexed lookup: 9.58 ms → 0.56 ms p50 at a thousand VENs
  on SQLite, with the dependence on fleet size gone rather than reduced (D-124).
- Prometheus label values escape a newline as well as a quote and a backslash.

### Changed

- `cargo test` runs in parallel again. Each PostgreSQL test creates a database of its own, so
  `--test-threads=1` is no longer needed to keep them from deadlocking on `TRUNCATE … CASCADE`
  (D-125).
- `tests/load.rs` builds a clean backend per row and serialises its four cases. Rows had been
  inheriting each other's data, and the cases had been racing for one machine and one SQLite file.
  Report ingest is now measured with and without a broker, which is the configuration that hid
  D-124.
- `cargo xtask` exits non-zero on an unrecognised command instead of printing usage and succeeding.

## [0.1.0] — unreleased

Never published. Initial implementation: the wire model, the payload schema, the domain core, the VTN
server with three storage backends, the typed client, the VEN runtime, webhook and MQTT transports,
mDNS discovery, TLS, and the black-box conformance kit.

[0.2.0]: https://github.com/hupe1980/openadr/releases/tag/v0.2.0
[0.1.0]: https://github.com/hupe1980/openadr/releases/tag/v0.1.0
