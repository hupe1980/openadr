+++
title = "The command line"
description = "One binary: run a VTN, and talk to one. Reading, writing and watching any OpenADR 3.1 VTN from a shell, with output that pipes into jq."
weight = 25
+++

```console
$ cargo install openadr --features vtn,client,internal-auth,sqlite
```

One binary, two halves. `openadr vtn` runs a Virtual Top Node; `openadr get`, `post`, `put`,
`delete` and `watch` talk to one — this one, or anybody's.

They are in the same binary for a reason beyond convenience. The client commands are a **second
consumer** of the crate's own client library, and a library with one consumer is a library whose
untaken paths nobody notices. The first thing this arrangement found was that `list_if_changed`
decoded straight from the socket and skipped the adapter chain, so a client configured for a peer
that bends the schema got adapted objects from `list()` and raw ones from `list_if_changed()`.

## Connecting

Flags, or the environment. The environment is what you want in a shell session:

```console
$ export OPENADR_URL=http://localhost:3000/openadr3/3.1.0
$ export OPENADR_TOKEN=$(curl -s -d grant_type=client_credentials \
      -d client_id=bl-1 -d client_secret=$SECRET \
      $OPENADR_URL/auth/token | jq -r .access_token)
```

| | |
|---|---|
| `--url`, `OPENADR_URL` | the base URL, including the base path |
| `--token`, `OPENADR_TOKEN` | a pre-shared bearer token |
| `--client-id` / `--client-secret`, `OPENADR_CLIENT_ID` / `OPENADR_CLIENT_SECRET` | OAuth2 client credentials, exchanged at the token endpoint the VTN advertises |

With none of them the request goes out unauthenticated, which is what a public tariff server expects.

## Reading

```console
$ openadr get programs
$ openadr get programs prg-00000001
$ openadr get events --program prg-00000001 --active
$ openadr get events --targets group1,group2
$ openadr get reports --event evt-00000004 --client-name cpo-a
$ openadr get vens --name charge-point-42
$ openadr get resources --ven ven-00000001 --all
$ openadr get notifiers
```

The filters are the specification's own query parameters, under names that read at a terminal.
`--name` resolves to `programName`, `venName` or `resourceName` depending on the collection, because
the specification uses three parameters for one idea.

`--skip` and `--limit` fetch one page — the schema caps `limit` at 50 — and `--all` follows
pagination to the end instead. Pages are fetched in sequence, because `skip`/`limit` has no cursor
and a parallel fetch could miss or duplicate a record if the collection changes underneath it.

Output is the VTN's JSON, **unaltered**. It pipes into `jq`, and comparing two implementations is a
`diff`:

```console
$ openadr get events --url $MINE   > mine.json
$ openadr get events --url $THEIRS > theirs.json
$ diff <(jq -S . mine.json) <(jq -S . theirs.json)
```

## Writing

```console
$ openadr post programs --data '{"programName":"grid-aware-charging"}'
$ openadr post events --file event.json
$ cat event.json | openadr post events --file -
$ openadr put events evt-00000004 --file amended.json
$ openadr delete events evt-00000004
```

Errors arrive as the `problem` body the VTN sent, rendered for a human, with the request id — so the
complaint and the log line are the same string:

```console
$ openadr get programs prg-nope
error: 404 Not Found: PROGRAM prg-nope not found (request 5b82306a-c47b-4f22-adb0-07a01ed38206)
```

## Watching

```console
$ openadr watch events --targets group1 --interval 10
watching events every 10s; ^C to stop
```

A conditional poll that prints **only when something changes**. It holds the `ETag` and a cycle that
finds nothing new costs a `304` with no body — the client half of the VTN's caching, doing something
you can see. A transient failure is reported and the watch continues, because the point of it is to
survive the VTN restarting underneath it.

## Measuring a VTN

```console
$ openadr conformance --url http://localhost:3000/openadr3/3.1.0 \
      --bl-token $BL --ven-token $VEN --ven-client-id ven-1
```

Forty-four black-box checks against any VTN, each citing the sentence of the specification it
tests. `--json` emits what an interoperability matrix is built from; `--strict` makes any failure
non-zero rather than only a required one. It **writes** — see
**[measuring a VTN](@/docs/conformance.md)** for what that means and why it has to.

`cargo test --all-features --test interop -- --ignored --nocapture` runs it against another
implementation, in containers it starts itself.

## Finding a local VTN

```console
$ openadr discover
http://site-gateway.local:3000/openadr3/3.1.0
  instance      site-gateway
  version       3.1.0
  requires auth yes
  programmes    local-tariff
  openapi       http://site-gateway.local:3000/openadr3/3.1.0/openapi.json
```

Browses the local network for VTNs advertising `_openadr3._tcp`, and prints every one that answered
rather than picking one — which local VTN to enrol with is a decision about the site.
`--timeout <SECONDS>` and `--json`. Nothing found is not an error. See
**[finding a local VTN](@/docs/discovery.md)**; `openadr vtn --mdns` is the other end.

## Hashing a client secret

```console
$ openadr vtn --client-hashed "bl-1:$(openadr hash-secret "$SECRET"):bl"
```

`--client id:secret:role` puts a plaintext secret in the process table. This does not, and
`openadr hash-secret -` reads from stdin so it stays out of shell history as well.

## Running a VTN

```console
$ openadr vtn --database ./openadr.sqlite \
      --client bl-1:$BL_SECRET:bl \
      --client ven-1:$VEN_SECRET:ven \
      --webhooks --webhook-key $HMAC_SECRET
2026-02-11T09:00:00Z  INFO openadr::vtn: VTN listening addr=0.0.0.0:3000 base_path=/openadr3/3.1.0
```

Add `--tls-cert`/`--tls-key` and it serves HTTPS itself rather than needing a reverse proxy;
`--tls-client-ca` additionally refuses any connection whose client certificate that CA did not
issue.

`openadr vtn --help` lists everything. The pieces are covered in depth on their own pages:
[the VTN](@/docs/vtn.md), [authentication](@/docs/authentication.md),
[storage](@/docs/storage.md), [notifications](@/docs/notifications.md),
[transport security](@/docs/security.md#transport-security) and
[local discovery](@/docs/discovery.md).

Three refusals are worth knowing about before you meet them. Binding a non-loopback address with
in-memory storage needs `--ephemeral`, so shipping a VTN that silently forgets its programmes has to
be deliberate. And `--listen 0.0.0.0:3000` is a *bind* address, not a URL: the VTN advertises
`localhost:3000` from `/auth/server` rather than sending every client to its own machine. Pass
`--public-url` for the real one. And a webhook callback on a loopback or private address is refused
by default — that is the server-side request forgery the specification's webhook chapter is about —
so local development wants `--webhook-allow-private`, which says so loudly at start-up.
