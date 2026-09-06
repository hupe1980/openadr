# Security policy

## Reporting a vulnerability

Please report privately through
[GitHub security advisories](https://github.com/hupe1980/openadr/security/advisories/new) rather
than in a public issue.

Include what you did, what happened, and what you expected. A failing test or a `curl` sequence is
ideal; a description of the shape of the problem is fine too.

You should get an acknowledgement within a few days. This is a small project without a paid security
team, so please allow reasonable time for a fix before disclosing publicly.

## What is most worth reporting

OpenADR dispatches load. The failures that matter most are the ones nothing observes:

- **Object privacy.** Any path by which one VEN learns of an event, a target, a report, a
  subscription or a VEN object belonging to another. This includes the push transports: a
  notification delivered to a subscriber that was not entitled to it, or an MQTT topic reachable by
  the wrong client.
- **The notification outbox.** Any way a queued notification can be dropped, marked delivered
  without being sent, or delivered to the wrong recipient. Nothing in the protocol can tell a VEN
  afterwards that a dispatch instruction was never sent, so these fail silently at both ends.
- **Authentication and scopes.** Any way to act with authority you were not granted — in particular
  a VEN performing a business-logic write, or writing its own `targets`.
- **The webhook and broker surfaces.** The VTN makes outbound requests to subscriber-supplied URLs
  and answers authorization callbacks for a broker; both are described in the
  [security model](https://hupe1980.github.io/openadr/docs/security/).

Denial of service through unbounded work driven by request content is in scope. Rate limiting is
explicitly delegated to an API gateway and is not.

## Supported versions

Pre-1.0: fixes land on `main` and in the next release. There are no maintained release branches yet.

## Scope

This repository — the `openadr` crate and the `openadr` binary. The OpenADR specification itself is
published by the [OpenADR Alliance](https://www.openadr.org/specification); this project is
independent and not affiliated with them. Issues in the specification are worth raising upstream,
and worth telling us about so the deviation can be documented.
