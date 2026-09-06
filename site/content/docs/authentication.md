+++
title = "Authentication"
description = "OAuth2 scopes, the VTN's own token endpoint, JWT validation against a JWKS, pre-shared tokens and anonymous mode — and how to choose between them."
weight = 40
+++

Deployments disagree about identity, and the disagreement is not academic. Belgian Fluvius'
NetFlex profile forbids OAuth2 outright and requires mutual TLS. Californian price servers are
anonymous. Dutch grid-aware charging uses Keycloak. A VTN that assumes one of those cannot serve the
others.

So authentication is a trait. `Authenticator` resolves a credential to a `Principal` — a client id
and a set of scopes — and nothing else in the VTN knows how that happened.

## Choosing a backend

| | For | Feature |
|---|---|---|
| **[Its own token endpoint](#its-own-token-endpoint)** | A site controller or single-tenant pilot: one binary, no authorization server beside it | `internal-auth` |
| **[An external one](#an-external-authorization-server)** | The common production case: Keycloak, Auth0, Entra, an in-house server | `external-auth` |
| **[Pre-shared tokens](#pre-shared-tokens)** | Development, and a gateway that has already authenticated the caller | `vtn` |
| **[Anonymous](#anonymous)** | A public tariff server | `vtn` |
| **Something else** | mTLS, an internal SSO, a database of API keys | implement the trait |

## Scopes

The specification never labels a token "business logic" or "VEN". The distinction falls out of the
scopes, and this crate treats any of `read_all`, `write_programs` or `write_events` as business
logic.

`read_bl` is deliberately **not** in that list, despite the name. It is a permission on five
endpoints — the collection-wide MQTT topic listings — and a token carrying only it is an ordinary
identified client everywhere else, with object privacy applying in full.

| Scope | Grants |
|---|---|
| `read_all` | Unrestricted read; also the MQTT programme topic endpoints |
| `read_bl` | The collection-wide MQTT topic endpoints, and only those |
| `read_targets` | Reading targeted programmes and events, by naming matching targets |
| `read_ven_objects` | Reading one's own `ven`, `resource`, `report`, `subscription` objects |
| `write_programs`, `write_events` | Business logic |
| `write_reports` | Creating and updating one's own reports |
| `write_subscriptions` | Managing one's own subscriptions |
| `write_vens` | Writing `ven` and `resource` objects |

Unknown scopes in a token are ignored rather than rejected — an authorization server shared with
other applications hands out scopes that mean nothing here.

## Its own token endpoint

`POST /auth/token` is optional in the specification. A VTN that delegates answers `501` and points
at `GET /auth/server`. One built with `internal-auth` runs the client-credentials grant itself,
which is what makes the single-binary deployment possible.

```console
$ openadr vtn --database ./openadr.sqlite \
      --client bl-1:$BL_SECRET:bl \
      --client ven-1:$VEN_SECRET:ven \
      --token-key $SIGNING_KEY \
      --token-ttl 3600
```

```rust
use openadr::vtn::auth::{InternalAuth, Scope, Scopes};

let auth = InternalAuth::builder("https://vtn.example.com/openadr3/3.1.0/auth/token")
    .client("bl-1", &bl_secret, Scopes::new(Scope::BUSINESS_LOGIC))?
    .client_hashed("ven-1", &stored_argon2_hash, Scopes::new(Scope::VEN))
    .signing_key(signing_key)
    .ttl(std::time::Duration::from_secs(3600))
    .build();
```

It is a real grant, not a token table with an HTTP endpoint in front of it:

- **Secrets are stored Argon2id-hashed**, as PHC strings. `InternalAuth::hash_secret` produces one
  so an operator never has to keep the plaintext; `client_hashed` takes it back.
- **An unknown `client_id` costs the same as a known one.** Verification runs against a fixed decoy
  hash when no client matches, so response time does not enumerate clients. That also means an
  *unauthenticated* request pays a full Argon2id verification, so the work runs on a blocking thread
  behind a bound — `max_concurrent_verifications`, defaulting to the machine's parallelism capped at
  eight — and a caller beyond it queues and eventually gets a `503`. See
  [the security model](@/docs/security.md).
- **A client may ask for fewer scopes than it holds, never more.** Requesting only scopes it does
  not hold is `invalid_scope`, not a token with an empty grant.
- **Tokens are signed JWTs**, so validating one is arithmetic rather than a lookup — which is what
  lets several VTN instances share `--token-key` and accept each other's tokens with no shared
  session store.

Without `--token-key` each process generates a random one, so a restart invalidates outstanding
tokens and clients simply fetch another. The binary warns.

Errors are RFC 6749 bodies with a `400` — except a `503`, which says the VTN could not attempt the
check rather than that the credential was wrong:

```json
{ "error": "invalid_client", "error_description": "unknown client, or the secret does not match" }
```

Including `invalid_client`: §5.2 requires `401` only when the client authenticated through the
`Authorization` header, and here the credentials are in the body — which is also what the OpenAPI
document's `badRequestOAuth` on `400` describes.

### Keep the secret out of the process table

```console
$ HASH=$(openadr hash-secret "$SECRET")          # or: openadr hash-secret -   (reads stdin)
$ openadr vtn --client-hashed "bl-1:$HASH:bl"
```

`--client id:secret:role` puts the plaintext in the process table, where every other process on the
machine can read it, and in shell history. `--client-hashed` takes the Argon2id PHC string instead —
safe in a configuration file or a database, because it identifies the secret without being usable as
one.

Both flags are repeatable and can be mixed. Prefer the hashed one anywhere but development.

## An external authorization server

```console
$ openadr vtn --database postgres://… \
      --jwks-url https://sso.example.com/realms/openadr/protocol/openid-connect/certs \
      --token-url https://sso.example.com/realms/openadr/protocol/openid-connect/token \
      --issuer   https://sso.example.com/realms/openadr \
      --audience openadr-vtn
```

```rust
use openadr::vtn::auth::{JwtAuthenticator, JwtConfig};

let auth = JwtAuthenticator::new(
    JwtConfig::new(jwks_url, token_url)
        .with_issuer("https://sso.example.com/realms/openadr")
        .with_audiences(["openadr-vtn"])
        .with_client_id_claims(["azp", "client_id", "sub"]),
)?;
```

Five details are load-bearing, and each is a documented way of getting JWT validation wrong.

**The algorithm comes from the key, never from the token.** A token claiming `alg: HS256` against a
JWKS full of RSA public keys is the classic confusion attack: the "signature" becomes an HMAC over a
key the attacker can read. The accepted algorithm is derived from the JWK's `alg`, or from its key
material when it has none. A symmetric key published in a JWKS is refused outright — a shared secret
that is not secret cannot authenticate anybody.

**A key published for encryption is not a key for verification.** A key set may carry both, and
`use` and `key_ops` are how its publisher says which is which (RFC 7517 §4.2, §4.3). A key stating
`use: "enc"`, or a `key_ops` without `verify`, is skipped — verifying a signature against it would
be using a key outside the purpose its owner declared. A key stating neither is accepted, because
most authorization servers publish a signing-only set and say nothing.

**`nbf` is checked.** The JWT library does not validate "not before" unless asked, so a token whose
validity begins tomorrow was accepted today. Clock-skew leeway still applies, so a small difference
between the two servers is not a refusal.

**Rotation is a cache miss, not a timer.** An unknown `kid` refetches the key set, rate-limited so
that a stream of tokens naming random key ids cannot turn the VTN into a load generator aimed at
your authorization server. The cache also expires, which is what bounds how long a *withdrawn* key
keeps working — a revocation window rather than a cache tuning knob.

**`clientID` is a configurable claim.** Keycloak puts it in `azp`, many servers in `client_id`, some
only in `sub`, and the specification says only that the VTN discovers it "by means not specified
here". The default order is `azp`, `client_id`, `sub`. A token with no recognisable client identity
yields a principal with none, which owns nothing.

Scopes are read from `scope` (a space-separated string) or `scp` (an array); both shapes occur.

> **Configure an audience.** Without one, any token that server issued for any application is
> accepted here. The binary warns; the warning is the whole message.

`--audience` makes the claim **required**, not merely matched: a token carrying no `aud` is refused.
Configure the other half too. Keycloak puts no `aud` on a `client_credentials` token until somebody
adds an audience mapper (*Clients → your client → Client scopes → dedicated → Add mapper →
Audience*), so `--audience` without one refuses every token.

Held by tests that mint real tokens with a real key against a real JWKS — a forged signature, the
wrong issuer, the wrong audience, *no* issuer, *no* audience, a missing `exp`, an `nbf` in the
future, an encryption key, a symmetric key, and twenty-five unknown key ids producing one fetch — and
by one against a real Keycloak, end to end: realm, service-account client, token, write.

## Pre-shared tokens

```console
$ openadr vtn --bl-token $BL --ven-token $VEN
```

Not an OAuth2 grant — a fixed table, compared in constant time. Two situations where it is the right
answer:

- **Development.** No authorization server to stand up.
- **A gateway that already authenticated the caller.** Fluvius' NetFlex profile authenticates with
  mutual TLS and does not use OAuth2 at all; a TLS-terminating proxy that maps a client certificate
  to a token is how to bridge that. The VTN does not terminate TLS, so it cannot check a certificate
  itself.

## Anonymous

```console
$ openadr vtn --anonymous --database ./tariffs.sqlite
```

The specification explicitly permits a VTN that publishes only public information to serve
unauthenticated readers, and `GET /auth/server` must still exist.

**It is read-only by construction, not by configuration.** An anonymous principal holds no scopes,
and every write requires one. There is no flag to misconfigure and no code path where an anonymous
write could be allowed by accident.

Anonymous readers also see no targeted object — an empty grant intersected with anything is empty —
so a public VTN can carry private events beside public tariffs without leaking them.

## Writing your own

```rust
use async_trait::async_trait;
use openadr::vtn::auth::{AuthError, Authenticator, Principal, Scope, Scopes};

#[derive(Debug)]
struct MyAuth { /* … */ }

#[async_trait]
impl Authenticator for MyAuth {
    async fn authenticate(&self, bearer: Option<&str>) -> Result<Principal, AuthError> {
        let token = bearer.ok_or(AuthError::Missing)?;
        let record = self.lookup(token).await.ok_or(AuthError::Invalid)?;
        Ok(Principal {
            client_id: Some(record.client_id),
            scopes: Scopes::new(Scope::VEN),
        })
    }

    fn token_url(&self) -> String {
        "https://sso.example.com/token".into()
    }
}
```

Two optional methods matter if you want a working `/auth/token`: `issues_tokens` and `issue_token`.
The default pair answers `501`, which is what a delegating VTN should say.

Everything downstream — object privacy, ownership, scope checks — works from the `Principal` alone.
There is nothing else to implement.

## What the VTN does with the identity

The `clientID` is the key of the entire privacy model. It is **stamped from the token** onto every
VEN-created object and matched on read; a request body can never set it. That is why
`VEN_VEN_REQUEST` has no `clientID` field at all: the privilege is unreachable rather than merely
unauthorised.

See [Object privacy](@/docs/object-privacy.md) for what follows from that.
