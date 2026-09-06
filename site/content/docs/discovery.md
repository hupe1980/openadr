+++
title = "Finding a local VTN"
description = "mDNS/DNS-SD discovery: a VTN on a customer site advertises _openadr3._tcp, and a VEN on the same network finds it without being told a URL."
weight = 75
+++

```console
$ cargo add openadr --features vtn,mdns     # to advertise
$ cargo add openadr --features ven,mdns     # to find one
```

A VEN inside a customer site — a heat pump, a charge point, a battery — **should** be able to find
the VTN on the same network without an installer typing a URL into it, and a VTN meant for that site
**should** advertise itself `[Def §Discovery and Configuration of Local VTNs]`.

This is the half of the single-binary deployment that [SQLite](@/docs/storage.md) starts: one
process, one file, and now a VEN that finds it.

```console
$ openadr vtn --database ./vtn.db --mdns --mdns-name site-gateway \
      --mdns-program local-tariff --client ven-1:secret:ven
# add --tls-cert/--tls-key and the record advertises https://, which is what the
# specification's own worked example writes

# on any other machine on the network
$ openadr discover
http://site-gateway.local:3000/openadr3/3.1.0
  instance      site-gateway
  version       3.1.0
  requires auth yes
  programmes    local-tariff
  openapi       http://site-gateway.local:3000/openadr3/3.1.0/openapi.json
```

`--json` gives the same thing machine-readably. Nothing found is **not** an error: a site with no
local VTN is the ordinary case for a VEN configured with a cloud URL.

## What is advertised

The service type is `_openadr3._tcp`, and the TXT record carries the specification's six keys with
its own spellings:

| Key | Value |
|---|---|
| `version` | the OpenADR release served, e.g. `3.1.0` |
| `base_path` | the prefix before the endpoints, e.g. `openadr3/3.1.0` |
| `local_url` | `http(s)://{hostname}.local:{port}/{base_path}` |
| `program_names` | comma-separated `programName`s a local VEN should follow |
| `requires_auth` | `True` or `False` |
| `openapi_url` | where the OpenAPI document is, when the VTN serves one — for `openadr vtn`, `{local_url}/openapi.json`, which it does |

`local_url` names the **`.local` hostname, not an address**, deliberately: a DHCP lease changes and
a name does not, and the specification says a VEN should prefer the name for exactly that reason.

`openapi_url` is *omitted* rather than empty when there is none. A key with an empty value claims
the document is at the empty URL, and a browser cannot tell that from a document at all. It is also
*derived* from `local_url` rather than written beside it, so a change of scheme, host, port or base
path moves both — and it points at a document the VTN actually serves. A key naming a `404` is worse
than a missing key: the VEN has been told the document exists.

```console
$ openadr discover --json | jq -r '.[0].openapiUrl'
https://site-gateway.local:3000/openadr3/3.1.0/openapi.json
```

## Reading somebody else's record is more forgiving than writing one

Writing is exact; reading is tolerant, because the record on the wire came from another
implementation:

- keys are matched case-insensitively;
- a missing `base_path` is derived from `local_url` rather than from this crate's default — which
  that peer may not serve;
- an absent `requires_auth` reads as **yes**. A VEN that assumes a VTN is open sends unauthenticated
  requests, collects `401`s, and has nothing to tell its operator; one that assumes a credential is
  needed asks for one;
- a record with no `local_url` names no VTN and is skipped, without abandoning the browse. One
  malformed advertisement must not hide every other VTN on the network.

## `discover` returns everything

Not the first answer, and not a "best" one. Which local VTN to enrol with is a decision about the
site, and a library that picked would be choosing for somebody who can see the room and cannot see
this code.

```rust
use openadr::discovery::discover;
use std::time::Duration;

for vtn in discover(Duration::from_secs(3))? {
    println!("{} — auth: {}", vtn.local_url(), vtn.requires_auth);
}
```

## The record is a value; the socket is a detail

`openadr::discovery::VtnService` is always compiled and has no network in it. It renders the TXT
record and parses one, and that is where every rule on this page lives. The `mdns` feature adds only
the responder and the browser.

That is why the TXT spellings are pinned by an ordinary unit test against the specification's own
worked example, rather than by a test that needs a multicast group. It also closes a gap a
round-trip test cannot: both ends of *this* crate read the same constants, so renaming `base_path`
to `basePath` would be symmetric and a responder-to-browser test would still pass. Two halves that
are consistently wrong still meet.

## Withdrawal

The advertisement is withdrawn when the `Advertisement` handle is dropped, and `openadr vtn` holds
it for as long as it serves. An mDNS record outlives the process that published it for as long as
its TTL, so a VTN that stops without unregistering leaves every VEN on the site dialling a port that
is no longer open.

## What it does not change

Discovery happens **before** any OpenADR communication and changes nothing about the protocol. A VEN
that was given a URL never needs it, and enrollment — the `clientID` and `clientSecret` — remains
out of band `[Def §VEN enrollment]`.
