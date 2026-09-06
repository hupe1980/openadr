+++
title = "Problem types"
description = "Every problem.type this implementation mints, what it means, and what a client should do about it."
weight = 125
aliases = [
  "/problems/bad-request",
  "/problems/invalid-payload",
  "/problems/dangling-reference",
  "/problems/unsupported-media-type",
  "/problems/payload-too-large",
  "/problems/unauthorized",
  "/problems/missing-scope",
  "/problems/forbidden",
  "/problems/not-found",
  "/problems/no-such-route",
  "/problems/method-not-allowed",
  "/problems/conflict",
  "/problems/not-implemented",
  "/problems/internal-server-error",
  "/problems/storage-unavailable",
  "/problems/unavailable",
  "/problems/timeout",
  "/problems/error",
  "/problems/unexpected",
]
+++

Every 4xx and 5xx carries an RFC 9457 [problem](@/docs/vtn.md#errors) body whose `type` is an
absolute URI under `https://hupe1980.github.io/openadr/problems/`. RFC 9457 asks that dereferencing
such a URI produce documentation of the type; each one below is that documentation.

The `type` is the stable identifier — match on it, not on `title` or `detail`, which are prose and
may be reworded. `status` repeats the HTTP status so a stored problem is still complete.

## What a client should do

| `type` | Status | Meaning |
|---|---|---|
| `bad-request` | 400 | The request is malformed in a way the more specific types below do not cover. Not retryable unchanged. |
| `invalid-payload` | 400 | The body parsed as JSON but failed validation — a field out of range, a missing required property, a pattern that does not match. `detail` names the field. |
| `dangling-reference` | 400 | The body refers to an object that does not exist, typically a `programID` on an event. The URL was fine; the payload was not. |
| `unsupported-media-type` | 415 | The body was labelled something other than `application/json` or an RFC 6839 `+json` type. |
| `payload-too-large` | 413 | The body exceeds the VTN's configured limit. |
| `unauthorized` | 401 | No credential, or one that did not verify. The response carries `WWW-Authenticate`. Get a new token and retry once. |
| `missing-scope` | 403 | The token verified but does not carry the scope this endpoint requires. Retrying will not help; the client's registration needs changing. |
| `forbidden` | 403 | The credential is valid and scoped but may not act on this object — a VEN writing another VEN's report, for instance. |
| `not-found` | 404 | No such object. Also returned for an object that exists but this client may not see, so that a `403` never confirms existence. |
| `no-such-route` | 404 | No endpoint at that path. Usually a base-path or version mistake rather than a missing object. |
| `method-not-allowed` | 405 | The path exists; the method is not defined for it. |
| `conflict` | 409 | The write lost a race, or would break uniqueness. Re-read and retry. |
| `not-implemented` | 501 | A specified feature this deployment does not provide — the MQTT topic endpoints on a VTN with no broker, for example. |
| `internal-server-error` | 500 | A defect. `instance` is the request id; quote it. |
| `storage-unavailable` | 500 | The database could not be reached. Transient; retry with backoff. |
| `unavailable` | 503 | The VTN is up but declining work, typically while shutting down. Retry with backoff. |
| `timeout` | 504 | The VTN gave up on the request. Retry with backoff. |
| `error` | *varies* | A status produced by a middleware with no more specific mapping. |
| `unexpected` | *varies* | Minted by **this crate's client**, not by a VTN: the peer returned an error with no problem body, so one was synthesised from the status. It says nothing about the peer beyond its status code. |

`POST /auth/token` is the exception. Its errors are RFC 6749 §5.2 `authError` bodies
(`{"error": "invalid_client", …}`) rather than problems, because that is the shape OAuth clients
parse.
