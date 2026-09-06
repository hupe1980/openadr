# Changelog

Notable changes to `openadr`. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) — with the 0.x caveat
that a minor release may break.

The reasoning behind a change lives in `concepts/DECISIONS.md`, cited here as `D-nnn`. What is not
built yet lives in `concepts/ROADMAP.md`. Neither belongs on this page.

## [0.3.1] — unreleased

A release-pipeline fix. 0.3.0 shipped without its `aarch64-unknown-linux-musl` binary — the one the
single-binary site-controller deployment is for — because that job failed.

### Removed

- The `x86_64-apple-darwin` binary. GitHub has retired the Intel macOS runners, so building it would
  be the release matrix's only cross-compile, and macOS is Apple Silicon now. `cargo install openadr`
  still builds it from source.

### Fixed

- The `aarch64-unknown-linux-musl` binary builds. `upload-rust-binary-action` reaches for `cross`
  whenever the target triple differs from the host's, without asking whether only the *libc* differs;
  cross's last release assumes an x86_64 Linux host, so on the arm64 runner it tried to install an
  amd64 toolchain and rustup refused. Every target in the matrix is the runner's own architecture and
  now builds with plain cargo against the musl C compiler `musl-dev` provides.

## [0.3.0] — 2026-09-06

Pre-1.0: this release breaks. There is no migration path and none is needed — the schema is applied
on connect, and a change to it is a change to the file format.

### Breaking

- A `target` may no longer contain a comma. `?targets=a,b` is accepted as two targets — a leniency
  both forms of which occur in the field — so a target carrying one was a single value on the way in
  and two filter terms on the way out, in the one place where that difference decides who sees whose
  dispatch schedule. Narrower than the schema by one character, and refused at the edge with an error
  that names the field (D-132).
- `DispatchConfig::lease` is gone; `DispatchConfig::attempt_timeout` replaces it and the lease is
  derived. The two were bounds on one quantity, and with the shipped defaults they were exactly
  equal — so any scheduling jitter put a second dispatcher onto entries the first was still
  delivering (D-134).

### Added

- **Reports carry a `payloadDescriptor`.** `[UG §7.6]`: a payload is deliberately just a type and
  values, and the descriptor is what supplies the unit and reading type needed to interpret them.
  The VEN runtime filed reports without one, so every series it sent was bare numbers whose unit a
  consumer had to assume — while this project's own conformance suite fails a *peer* for dropping
  them. Everything the descriptor needs came from the `reportDescriptor` that asked for the report
  (D-140).
- **Aggregate reports.** A `reportDescriptor` with `aggregate: true` asks for one resource entry
  named `AGGREGATED_REPORT`, and `[UG §7.7]` defines aggregation as a sum. The VEN runtime now
  performs it: a meter reports per resource as it always did, and the runtime sums the series
  interval by interval and payload by payload on exact decimals, then names the result. A meter that
  must aggregate differently returns the reserved name itself and is left alone; a payload addition
  is not defined on is a refusal naming the type. `core::aggregate` and `core::AggregateError` are
  public, so a VEN not built on this runtime can use the same rule (D-139).
- `webhook::echo_challenge` and `webhook::ECHO_PARAM` — the receiver's half of the proof-of-control
  challenge, on the same terms as the signature verifier: `no_std`, no HTTP stack, no `vtn`. It is
  the first thing a subscriber has to answer and the only one that decides whether a subscription is
  created at all, and the crate shipped only the asking half of it (D-135).
- `model::MAX_PAGE_LIMIT` — the schema's cap on `limit`, in the wire model, with the three callers
  that each used to write it out for themselves (D-131).
- A fault table for the conformance suite: thirty-nine deliberately broken VTNs, each naming exactly
  the checks that must catch it, replacing seven hand-written weakenings. **47 of the 48 checks are
  now proven able to fail**, against 7 before anybody counted; the one that is not is named with its
  reason. Building it found four checks that could not fail — `targets-accept-both-forms` compared
  two empty lists with each other, and two ownership checks asserted that nothing foreign appeared in
  a collection holding nothing foreign — all three now seed the state they are about (D-138).
- Six tests for requirements that were covered only by a doc comment: the listener offers TLS 1.2 and
  1.3 and is asserted against a client pinned to each; the MQTT protocol level is read off the
  CONNECT packet rather than taken from a dependency's default; the mTLS notifier binding's key
  spellings are pinned against the binding document's worked example; a `POST` is asserted to stamp
  `id` and both timestamps *and* to ignore the ones a client sends; and a VEN enrols over a real
  socket from a URL, a `clientID` and a `clientSecret` and nothing else — the client's half of the
  client-credentials grant, which had never run outside the router (D-136).
- `cargo xtask trace` grades each citation by where it is written — a conformance check, a test, or a
  doc comment — and reports how many requirements are named by something that *runs*. 34 of 34, with
  a ratchet CI enforces (D-136).

### Fixed

- Three conformance checks could not fail. `targets-accept-both-forms` compared two unfiltered reads
  to each other and agreed with itself against a VTN that ignored `?targets=` entirely;
  `ven-reads-only-its-own-resources` and `ven-reads-only-its-own-subscriptions` asserted that no
  foreign object appeared without ever creating one. A run against a VTN that reads every credential
  as business logic now trips all three (D-138).
- The VEN runtime no longer stops at a peer's page boundary. `sync` decided it held every event by
  comparing one page's length against 50 — the schema's *maximum* for `limit`, which has no default —
  while sending no `limit` at all. Against a VTN that pages at twenty it saw twenty events, concluded
  that was the collection, and followed a truncated schedule indefinitely with a `200` at both ends.
  The limit is now named on the request and the comparison is against what was asked for (D-131).
- The MQTT publisher overlaps its broker round trips. Its lock covered the whole publish-to-`PUBACK`
  round trip rather than the queue-to-write step correlation actually needs, so exactly one broker
  notification was in flight at a time — which made the time to clear a batch the sum of its publish
  timeouts and reinstated, invisibly, the failure `DispatchConfig::concurrency` exists to prevent
  (D-133).
- A delivery attempt is bounded by the dispatcher rather than only by the transport, so a transport
  with a longer timeout — or none — cannot hold an outbox entry past its lease (D-134).
- A JWKS that is failing is fetched once per `min_refresh_interval` rather than once per request. The
  window was measured from the last *successful* fetch, so while the authorization server was down
  there was no successful fetch to be too soon after and the limit never applied — every incoming
  token opened another request to a server already in trouble. A token that cannot be checked because
  the key set could not be refreshed now answers `Unavailable` rather than `Invalid`, so an operator
  is not sent hunting for a credential problem (D-137).
- A refusal of a malformed body names its position once rather than twice, and names the value it
  refused rather than reciting it: `serde_json` already ends its own message with "at line L column
  C", and a 200-character `programName` used to come back in full.
- The release workflow creates the GitHub Release before uploading binaries into it. The job that
  created it depended on the jobs that upload to it, so every target built and then failed with
  `release not found`, and no release ever carried a binary.
- Documentation no longer claims the tree has no C in it. `ring` and, under the `sqlite` feature,
  SQLite are vendored C that cargo builds; the property that actually holds — and the one that makes
  cross-compilation ordinary — is that no C *library* has to be installed.

## [0.2.0] — 2026-09-06

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

## [0.1.0] — 2026-09-06

Initial implementation: the wire model, the payload schema, the domain core, the VTN server with
three storage backends, the typed client, the VEN runtime, webhook and MQTT transports, mDNS
discovery, TLS, and the black-box conformance kit.

[0.3.1]: https://github.com/hupe1980/openadr/compare/v0.3.0...main
[0.3.0]: https://github.com/hupe1980/openadr/releases/tag/v0.3.0
[0.2.0]: https://github.com/hupe1980/openadr/releases/tag/v0.2.0
[0.1.0]: https://github.com/hupe1980/openadr/releases/tag/v0.1.0
