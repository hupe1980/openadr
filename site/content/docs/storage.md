+++
title = "Storage"
description = "Choosing between in-memory, SQLite and PostgreSQL for an OpenADR VTN, the schema shape, and the conformance suite that keeps the three interchangeable."
weight = 60
+++

Three backends behind one trait. Which one you want follows from the deployment, not from taste.

| | In-memory | SQLite | PostgreSQL |
|---|---|---|---|
| **For** | tests, development, a feed rebuilt on start | a site controller, a single-tenant pilot, a home gateway | multi-tenant, utility-scale, more than one server instance |
| **Survives a restart** | no | yes | yes |
| **Concurrent writers** | one process | one writer, database-wide | many |
| **Notification queue** | in process | lease columns, poll interval | `FOR UPDATE SKIP LOCKED`, `LISTEN`/`NOTIFY` |
| **Costs** | nothing | nothing — one file | a database to run |
| **Feature** | `vtn` | `sqlite` | `postgres` |

```console
$ openadr vtn --database ./openadr.sqlite
$ openadr vtn --database postgres://openadr:secret@db.internal/openadr
```

Anything that is not a `postgres://` or `postgresql://` URL is a SQLite file path.

## Choosing

**SQLite is the deployment with no competition.** A site controller, a pilot, a home gateway: one
binary and one file, no cluster, no operator. The reference Rust implementation is PostgreSQL-only
and refuses to compile otherwise, so that deployment had nowhere to go.

**PostgreSQL is what a utility-scale VTN wants**, and the notification queue is where the difference
becomes concrete rather than a preference. SQLite takes a database-wide write lock. A single
`POST /events` to a thousand subscribers is one transaction with a thousand rows, followed by
roughly two thousand more write transactions as the dispatcher claims and completes them. Add VENs
posting reports on a schedule — the genuinely high-write path in any real deployment — and that lock
is the ceiling well before the HTTP layer is.

Two Postgres primitives change the design rather than just the placeholders:

- **`SELECT … FOR UPDATE SKIP LOCKED`** claims outbox entries, so N dispatchers take N disjoint
  batches. On SQLite the lease columns *are* the claiming mechanism and two dispatchers contend on
  one write lock.
- **`LISTEN`/`NOTIFY`** wakes a dispatcher when a write commits, so notification latency is a commit
  rather than a poll interval. That matters when the payload is a curtailment instruction.

**In-memory is complete and fully consistent** — it passes the same conformance suite — but it loses
everything on restart, and nothing in OpenADR tells a VEN that happened: the VEN simply reads an
empty collection and stands down. The binary therefore **refuses** to bind a non-loopback address
with in-memory storage unless you pass `--ephemeral`.

## Schema shape

Both SQL backends use the same shape: **one table per object**, holding the client-provided request
body as JSON plus the columns the API actually queries.

```sql
CREATE TABLE event (
    id           TEXT PRIMARY KEY,
    created_at   TEXT NOT NULL,
    modified_at  TEXT NOT NULL,
    program_id   TEXT NOT NULL REFERENCES program(id) ON DELETE CASCADE,
    active_from  TEXT,          -- the window the event is live over, resolved at write time
    active_to    TEXT,
    doc          TEXT NOT NULL  -- the request body, verbatim
);
```

Documents keep the wire model authoritative: a new field in the specification needs no column.
Columns keep the queries indexable. Cascades and uniqueness live in the schema, where the database
enforces them rather than the code remembering to.

Two details are load-bearing.

**`active_from`/`active_to` are resolved when the event is written.** `?active=true` asks whether an
event's intervals have all elapsed — a question about the *expansion*, not about a stored field.
Answering it at read time would mean expanding every event in the collection on every request, and
would put interval arithmetic inside the storage layer where SQL cannot follow.

**Timestamps are stored fixed-width**, nine fractional digits, always. `Timestamp`'s own `Display`
prints the fewest digits it needs, so a timestamp half a second past the minute is `…:00.5Z` and one
at 0.55 seconds is `…:00.55Z` — and compared as text the *longer* one sorts first, because `5` is
below `Z` in ASCII. Two objects created 50 ms apart came back in the wrong order. Every collection is
ordered by `(created_at, id)` precisely so that offset pagination is repeatable, so a comparison that
is wrong for sub-second differences breaks exactly that, silently.

## Referential integrity

Deleting a programme removes its events, their reports, and any subscription scoped to it. Deleting
a VEN removes its resources.

**A cascade announces what it removed.** The database deletes silently; a VEN subscribed to event
deletions would otherwise be left holding a dispatch instruction for an event that no longer exists,
with no way to find out. So a `DELETE` reads the objects the cascade is about to take — inside the
same transaction, and only when something is subscribed — and queues their notifications too.

**One table the cascade cannot reach.** Targets live in a single `object_target` table for every
kind of object that carries them, which is what makes the object-privacy predicate one join whatever
it is filtering. The cost of that shape is that it can hold no foreign key, so `ON DELETE CASCADE`
does not follow into it and each kind of row a cascade removes has to be cleared explicitly. Both SQL
backends assert the invariant directly — *no target row outlives its object* — because it is
unobservable through the storage API and so cannot be a conformance behaviour.

Uniqueness constraints: programme name per VTN, VEN name per VTN, resource name per VEN, and one
`ven` object per `clientID`.

That last one is not in the specification, and is worth knowing about. `VEN_VEN_REQUEST` carries no
identity, so the VTN must infer the VEN from the token; with two `ven` objects for one client,
`PUT /vens/{id}` and VEN-written resource creation become ambiguous. An aggregator representing many
sites uses one VEN with many resources, or one credential per VEN.

Conflicts are classified by SQLSTATE, not message text. Reading the message would make conflict
detection depend on the server's `lc_messages`, so a VTN would return `500` instead of `409` purely
because its database was installed in another locale.

## Migrations

There are none. The project is pre-release and the schema is authoritative rather than historical:
it is applied on connect, and a change to it is a change to the file format. When the crate reaches
1.0 that will change; until then, treat a schema change as a reason to recreate the database.

On PostgreSQL the script runs under a transaction-scoped advisory lock, so several instances may
start at once. `CREATE TABLE IF NOT EXISTS` reads as idempotent and is not: the check and the
creation are separate steps, so two sessions racing on one table both decide to create it and the
loser fails — with a message naming an internal catalogue index and no hint of a race. This is the
backend for a VTN that runs as more than one process, which makes a rolling deploy the normal case
rather than an edge one.

SQLite is the single-binary deployment: one process, one file. A VEN on the same network can find
it without being told a URL — see [Finding a local VTN](@/docs/discovery.md).

## One suite, three backends

A second backend is exactly where a rule quietly diverges: the in-memory one filters with an
iterator, SQLite with a `WHERE` clause, and nothing forces those to mean the same thing. So the
behaviour is stated once — 57 functions over `&dyn Storage` — and every backend runs all of them.

The list is written out explicitly rather than discovered, so adding a behaviour fails to compile
until every backend runs it. And `cargo xtask check-suite` fails if a method of the trait has no
behaviour at all, or if a behaviour is written and left out of the list — the first is a rule the
backends may quietly differ on, and the second is a test nothing runs.

Three of the 57 exist for methods the OpenADR API never calls. `subscribers()` is the notification
fan-out's own query: it returns each subscription together with the *kind* of client that created
it, which is the one thing `GET /subscriptions` has no field for and the fan-out cannot do without.
The other two are the [subscriber circuit breaker](@/docs/notifications.md) — the counter that
decides when an endpoint has stopped answering, and the reset behind
`POST /admin/outbox/retry`.

**And a suite bounds only the divergences it names.** A behaviour nobody writes down is one no
backend has to have, and the gap is silent: every backend passes everything it was asked. One
backend once let any authenticated VEN read every subscription in the VTN, `bearerToken` included,
simply because no behaviour said otherwise. Growing the suite is not maintenance; it is the
coverage — which is why the count above is checked by `cargo xtask check-suite` rather than aspired
to.

The PostgreSQL run starts its own container when no server is configured, so an ordinary
`cargo test` exercises all three backends. `OPENADR_TEST_POSTGRES` points it at one you already
have, which is what CI does. Each test creates a database of its own — emptying one means
`TRUNCATE … CASCADE`, and two tests sharing a database deadlock rather than merely interfere:

```console
$ docker run -d --rm --name pg -e POSTGRES_PASSWORD=openadr -e POSTGRES_USER=openadr \
      -e POSTGRES_DB=openadr -p 5432:5432 postgres:17-alpine
$ OPENADR_TEST_POSTGRES=postgres://openadr:openadr@localhost:5432/openadr \
      cargo test --all-features
```

Queries are runtime-checked rather than macro-checked, so a clean checkout builds with no database
and no offline cache. The safety a `query!` macro would have given is bought instead by that suite,
which exercises every query against a real database — including the failures a type check would not
catch, like a `WHERE` clause that filters the wrong way round.

## Writing your own backend

Implement `Storage`: typed CRUD per object, two privacy queries (`grant_for`, `all_grants`), and the
outbox operations. The trait is deliberately narrow — every object is a document with a handful of
indexed attributes, which is exactly what the API queries and what any backend can index.

Three rules the conformance suite will hold you to:

1. **Filter, order, paginate — in that order.** Every list method takes the caller's `Access` and
   must apply it as part of the predicate. Filtering after the page is cut silently drops records.
2. **Queue notifications in the write's own transaction.** Every mutating method takes a `Fanout`;
   call `fanout.deliveries(&object, operation)` — it is pure and synchronous, so it works from
   inside a transaction — and insert the rows before committing. Pass `Fanout::none()` if you are
   calling storage directly and do not want notifications.
3. **Order by `(created_at, id)`,** to the nanosecond.

Then run the suite against it. That is the contract.
