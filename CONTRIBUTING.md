# Contributing

Thanks for looking. This is a pre-1.0 implementation of a grid-control protocol, so the most
valuable contributions are the ones that find where it is wrong.

## The single most useful thing

**Run it against your VTN, or your VEN, and report what disagrees.**

```console
$ cargo run --features vtn,client,conformance -- conformance \
      --url https://your-vtn.example.com/openadr3/3.1.0 \
      --bl-token "$BL" --ven-token "$VEN" --ven-client-id your-ven-client
```

The suite runs black-box over HTTP against any OpenADR 3.1 VTN and cites the sentence of the
specification behind every check. Some failures will be your VTN's; some will be this suite's, and
those are the most useful of all — the first time it was pointed at another implementation it found
three divergences there and two defects here.

`cargo test --all-features --test interop -- --ignored --nocapture` does the same against another
implementation, in containers it starts itself.

## Building and testing

```console
$ cargo test --all-features
$ cargo clippy --all-features --all-targets -- -D warnings
$ cargo fmt --all
$ cargo xtask check-drift          # fails if the Alliance's payload enumerations changed
$ cargo xtask check-model          # fails if the wire model and openadr3.yaml disagree
$ cargo xtask check-paths          # fails if a declared endpoint is unrouted or wrongly scoped
```

The PostgreSQL storage tests skip unless a server is configured, and a skipped backend is an
untested backend:

```console
$ docker run -d --rm --name pg -e POSTGRES_PASSWORD=openadr -e POSTGRES_USER=openadr \
      -e POSTGRES_DB=openadr -p 5432:5432 postgres:17-alpine
$ OPENADR_TEST_POSTGRES=postgres://openadr:openadr@localhost:5432/openadr \
      cargo test --all-features -- --test-threads=1
```

```console
$ cargo test --all-features --test broker -- --ignored --nocapture
```

Brings up `deploy/compose.yaml` — the shipped image, the shipped `emqx.conf`, a real EMQX — and
asserts four things about what actually arrives on the broker, including that one VEN cannot read
another's topic. Worth running for any change to MQTT or object privacy. Set `OPENADR_VTN_URL` and
`OPENADR_BROKER` to point it at a stack you already have.

`cargo xtask spec-sync` fetches the public specification mirror into `specs/`, which is not vendored
here. Some tests and the payload-table generator need it.

## Cutting a release

A tag, and nothing else:

```console
$ git tag v0.2.0 && git push origin v0.2.0
```

`.github/workflows/release.yml` runs `ci.yml` itself at the tagged commit — every job of it, not a
copy of some of them — then checks the tag against `Cargo.toml`, checks the `.crate` holds only the
library, publishes to crates.io, uploads static musl and macOS binaries with checksums, pushes a
cosign-signed multi-architecture image to `ghcr.io`, and attaches a CycloneDX SBOM to the GitHub
release.

One secret: `CARGO_REGISTRY_TOKEN`, a crates.io API token scoped to `publish-update` and to this
crate. Everything else uses `GITHUB_TOKEN` or the run's OIDC identity.

## What a change should come with

- **A test that fails without it.** For a bug fix this is the whole argument; for a feature it is
  what stops the feature quietly disappearing later.
- **A note on *why*, not what.** The code says what it does. Comments here carry the reason, and
  especially the alternative that was rejected.
- **A storage behaviour, if it touches storage.** The three backends are interchangeable only to the
  extent `src/vtn/store/suite.rs` says so — a rule the suite does not name is a rule they may
  differ on, and have. The list is written out by hand so adding one fails to compile until every
  backend runs it.
- **A conformance check, if it is a specification rule.** With the clause it tests. A check that
  cannot cite a sentence is an opinion, and belongs at `Severity::Extension`.

## Things that will come up in review

- **Filter, then order, then paginate** — in the query, never after. Filtering a page after it is
  cut silently drops records.
- **Sentinels are types.** `P9999Y` and `0001-01-01` never travel as ordinary values.
- **Money is `Decimal`,** never `f64`.
- **No `unsafe`.** The crate is `#![forbid(unsafe_code)]`.
- **`model`, `schema` and `core` must build with `--no-default-features`** for
  `thumbv7em-none-eabihf` and `wasm32-unknown-unknown`. CI checks it.
- **A feature flag no `cfg` reads is a claim the manifest makes and nothing keeps.** Same for a
  function with no caller.

## Where help is most wanted

1. Running the conformance suite against other implementations, and publishing the matrix.
2. Profile validators for the deployed Dutch and Belgian profiles.
3. Load testing the notification fan-out, so its cost stops being an argument and becomes a number.

## Licence

By contributing you agree that your work is licensed under Apache-2.0 OR MIT, matching the project.
