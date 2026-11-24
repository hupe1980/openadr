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
$ cargo test --workspace --all-features
$ cargo clippy --workspace --all-features --all-targets -- -D warnings
$ cargo fmt --all
$ cargo xtask check-drift          # fails if the Alliance's payload enumerations changed
$ cargo xtask check-model          # fails if the wire model and openadr3.yaml disagree
$ cargo xtask check-paths          # fails if a declared endpoint is unrouted or wrongly scoped
$ cargo xtask check-problems       # fails if a problem.type URI would resolve to nothing
$ cargo xtask check-suite          # fails if a Storage method or a written behaviour goes unrun
$ cargo xtask trace                # fails if a MUST or SHALL sits in a section nothing cites
```

The PostgreSQL storage tests start their own container when no server is configured, so an ordinary
`cargo test` exercises all three backends. `OPENADR_TEST_POSTGRES` points them at one you already
have, which is what CI does. Each test creates a database of its own, so they may run in parallel
like everything else:

```console
$ docker run -d --rm --name pg -e POSTGRES_PASSWORD=openadr -e POSTGRES_USER=openadr \
      -e POSTGRES_DB=openadr -p 5432:5432 postgres:17-alpine
$ OPENADR_TEST_POSTGRES=postgres://openadr:openadr@localhost:5432/openadr \
      cargo test --all-features
```

```console
$ cargo test --all-features --test broker -- --ignored --nocapture
```

Brings up `deploy/compose.yaml` — the shipped image, the shipped `emqx.conf`, a real EMQX — and
asserts four things about what actually arrives on the broker, including that one VEN cannot read
another's topic. Worth running for any change to MQTT or object privacy. Set `OPENADR_VTN_URL` and
`OPENADR_BROKER` to point it at a stack you already have.

`cargo xtask spec-sync` puts the public specification mirror at the commit `spec.lock` pins, in a
git-ignored reference directory (set `OPENADR_REFERENCE` to choose one); it is not vendored here.
Some tests and the payload-table generator need it. `cargo xtask spec-sync --update` moves the pin
to the mirror's head and prints what changed — commit that one-line diff on its own, after reading
the output. Only the OpenAPI document is Apache-2.0; never commit the Definition or User Guide.
`cargo xtask inputs` reports which other inputs — peer releases, profile validators, the Fluvius
PDF — have moved since `inputs.lock` recorded them; CI runs it weekly and files an issue.

## Reviewing a public interface change

Run the same read-only check as release CI. The first command installs the pinned development tools
and checks the inventory. Subsequent checks can use the second command.

```sh
python3 .github/compatibility/check.py --install-tools
python3 .github/compatibility/check.py
```

A diff lists the affected signatures, features or compiler floor. For an intentional change, run
`python3 .github/compatibility/check.py --update`, inspect the diff in
`.github/compatibility/public-api.txt`, and describe the consumer impact in the changelog.
CI uses Linux. The renderer omits blanket implementations and retains derived and auto-trait
implementations. The inventory covers the all-features library. CLI behavior and other feature
combinations have separate tests. [Compatibility policy](https://hupe1980.github.io/openadr/docs/compatibility/)

## Cutting a release

Update the changelog, workspace version and lockfile. Commit the release, then push the matching
`v<version>` tag. The release workflow checks the tagged commit, publishes the crate through
crates.io Trusted Publishing, and distributes CLI binaries, checksums, a CycloneDX SBOM, a signed
container image and a Homebrew formula. The Homebrew job uses a GitHub App scoped to the tap.

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

## Licence

By contributing you agree that your work is licensed under Apache-2.0 OR MIT, matching the project.
