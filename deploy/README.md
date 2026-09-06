# Deploying a VTN and its broker

Working configurations, and a compose file that runs the pair. They are here because they are the
one piece an operator cannot derive from the documentation: *what the broker's side looks like*.

```console
$ docker compose -f deploy/compose.yaml up --build
```

That gives a VTN on `localhost:3000` with SQLite storage, and EMQX on `localhost:1883` configured to
ask the VTN two questions.

| File | |
|---|---|
| `emqx.conf` | EMQX 5, HTTP authentication and authorization pointed at the VTN |
| `mosquitto.conf` | Mosquitto with `mosquitto-go-auth`, the same two callbacks |
| `Dockerfile` | the VTN as one binary in a distroless image |
| `compose.yaml` | both, wired together — a demonstration, not a production template |

## What the broker asks the VTN, and why it has to

Per-VEN topics are only privacy if the broker refuses one VEN a subscription to another's. The
specification requires the VTN to arrange that and declines to say how `[Notifiers §9.4]`, because
the VTN is the only party that knows which VEN a `clientID` belongs to.

**Who are you** — `POST /internal/mqtt/auth`. The OpenADR access token arrives as the MQTT
*password*, which is the convention the binding names `[Notifiers §12.2]`, and the *username* must
be the caller's own `clientID`. Checked through the same authenticator the REST API uses, so a
revoked credential stops working on both at the same instant.

**May you subscribe to this** — `POST /internal/mqtt/acl`. A username and a topic and no token,
because that is all a broker has at subscribe time — so the answer comes from the topic:
`…/vens/{venID}/…` is yours if that VEN's `clientID` is your username, and a collection-wide topic is
business logic's.

Expose these two to the broker and to nothing else. They are a private contract between the pair,
not part of the OpenADR surface, which is why they are mounted at the root rather than under the
base path.

## The VTN is a client of its own broker

A broker configured this way authenticates *everyone* through the VTN — including the VTN's own
publisher. Two lines follow from that, and leaving either out produces a fan-out that reconnects for
ever against `NotAuthorized` with nothing else reporting a problem:

```
--mqtt-username=business-logic --mqtt-password=$BL_TOKEN   # a credential the callback accepts
--mqtt-publisher-client=business-logic                     # the one identity /acl lets publish
```

`/internal/mqtt/acl` refuses every publish it has not been told about, because a client that could
publish could forge a dispatch instruction. `--mqtt-publisher-client` names the single exception,
and it may publish only under this VTN's own `--mqtt-topic-prefix` — so the same identity on a
shared broker still cannot reach another deployment's topics.

If your broker authenticates the VTN some other way — its own user database, or mutual TLS — leave
that flag off and the blanket refusal stands.

## What running it proves

Four things hold against EMQX 5.8 with the configuration in this directory:

- an event targeted at `group1` is published by the VTN and arrives on the entitled VEN's **private
  topic**, carrying only that VEN's own target;
- a VEN asking to subscribe to **another VEN's** private topic is refused by the broker;
- a VEN's **publish** to any topic reaches no subscriber, so a forged notification cannot be planted;
- a wrong password is refused at connect — by the VTN's own authenticator, through the broker.

```console
$ cargo test --all-features --test broker -- --ignored --nocapture
```

`tests/broker.rs` asserts those four and CI runs it, bringing this compose file up itself so what is
under test is the configuration as shipped. Nothing is read from a log, and the cross-VEN claim is a
**refusal** rather than silence — silence is also what a fan-out that published nothing looks like.
The run starts only once a credential the VTN knows is accepted *and* one it does not is refused,
because before that the broker's answers are not yet the VTN's.

`deploy/emqx.conf` sets `authorization.no_match = deny`. That is load-bearing: EMQX's default is
`allow`, and a VTN whose ACL endpoint is briefly unreachable would otherwise hand every VEN every
topic — object privacy on the push path, undone by a timeout.

## Not a production template

Pre-shared tokens, no TLS, storage in a volume. For a real deployment: `--client-hashed` or an
external authorization server, `--tls-cert`/`--tls-key` (the image is built with the `tls` feature,
so no reverse proxy is required), `mqtts://` with certificates the broker trusts, PostgreSQL, and
`/internal/*` reachable only from the broker.
