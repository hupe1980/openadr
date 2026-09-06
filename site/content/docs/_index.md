+++
title = "Documentation"
description = "Everything needed to run an OpenADR 3.1 VTN, write a VEN against one, or embed the wire model and domain core in something else."
sort_by = "weight"
template = "section.html"
+++

`openadr` is an implementation of [OpenADR 3.1](https://www.openadr.org/specification) in Rust: the
wire model, a deterministic domain core, a VTN server, a VEN runtime and a typed client, in one
crate sliced with Cargo features.

If the protocol itself is new to you, start with **[What OpenADR is](@/docs/what-is-openadr.md)**.
If you want a VTN running, start with **[Getting started](@/docs/getting-started.md)**. If you have
one and want to poke at it, **[the command line](@/docs/cli.md)**. If you are writing the other end,
**[the VEN runtime](@/docs/ven-runtime.md)**. If you have a VTN — anyone's — and want to know how
much of the specification it actually implements, **[measuring a VTN](@/docs/conformance.md)**. If
you are running one where it matters,
**[the security model](@/docs/security.md)**. And if you are integrating against a VTN built on this
crate and need to know where it interprets an ambiguity, read
**[Reading the specification](@/docs/spec-notes.md)**.

Item-by-item API documentation lives on [docs.rs](https://docs.rs/openadr); this site covers what an
API listing cannot: why a piece is shaped the way it is, and which of several plausible readings of
the specification it implements.
