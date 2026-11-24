+++
title = "The command line"
description = "One binary: run a VTN, and talk to one. Reading, writing and watching any OpenADR 3.1 VTN from a shell, with output that pipes into jq."
weight = 25
+++

```console
$ brew install hupe1980/tap/openadr
```

One binary, two halves. `openadr vtn` and `openadr dev` run a Virtual Top Node; `openadr get`,
`post`, `put`, `delete`, `watch` and `trigger` talk to one — this one, or anybody's. The client
commands are a second consumer of the crate's own client library, which is how its untaken paths get
noticed.

## A development VTN

```console
$ openadr dev
$ source .openadr-dev.env
```

`openadr vtn` with presets: loopback, two generated clients (`bl-1`, `ven-1`), a per-run token key,
the inspector at `/_inspect`, and seeded data — a 24-hour price curve rolled forward every quarter
hour, a capacity event two minutes out, and one for `feeder-7` the VEN cannot see. The credentials
are printed and written to `.openadr-dev.env` (mode `0600`). Use `--database` to make it durable and `--env-file` to move the file. Server configuration flags
pass through to the VTN. The dev command supplies the clients and static tokens itself. A non-loopback `--listen` needs
`--ephemeral`, because the credentials are on screen.

```console
$ openadr trigger event      # a 5 kW capacity limit, one minute out, for 30 minutes
$ openadr trigger cancel     # delete the most recent triggered event
$ openadr trigger report     # the VEN files a usage report against the price curve
```

## Connecting

Flags, or the environment. With client credentials set, the commands do the OAuth2 grant themselves:

```console
$ export OPENADR_URL=http://localhost:3000/openadr3/3.1.0
$ export OPENADR_CLIENT_ID=bl-1 OPENADR_CLIENT_SECRET=$SECRET
```

| | |
|---|---|
| `--url`, `OPENADR_URL` | the base URL, including the base path |
| `--token`, `OPENADR_TOKEN` | a pre-shared bearer token |
| `--client-id` / `--client-secret`, `OPENADR_CLIENT_ID` / `OPENADR_CLIENT_SECRET` | OAuth2 client credentials, exchanged at the token endpoint the VTN advertises |
| `OPENADR_VEN_CLIENT_ID` / `OPENADR_VEN_CLIENT_SECRET` | the VEN, for `conformance` and `trigger report` |

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
      --bl-token $BL --ven-token $VEN --ven-client-id ven-1 --report report.html
```

52 black-box checks against any VTN, each citing the sentence of the specification it tests. With
the environment above it needs no flags. `--report` writes a page to share, `--badge` a shields.io
badge, `--json` a matrix row; `--strict` makes any failure non-zero. Exit `2` means nothing required
could be attempted. It **writes** — see **[measuring a VTN](@/docs/conformance.md)**.

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

## Explaining an error

```console
$ openadr get events nope-0000000
error: 404 Not Found: EVENT nope-0000000 not found (request b8e3…)
  → openadr explain not-found

$ openadr explain not-found
$ openadr explain ven-cannot-change-a-foreign-ven
```

Every error that carries one of this VTN's [problem types](@/docs/problems.md) ends with the command
that explains it. `explain` also takes a conformance check id, a full `type` URI, or nothing (the
list). The text is compiled in, so it works offline.

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
