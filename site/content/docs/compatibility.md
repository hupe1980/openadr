+++
title = "Compatibility"
description = "How to assess an openadr upgrade for a Rust application, a VEN appliance or a scripted VTN deployment."
weight = 111
+++

## Choosing an upgrade

During 0.x development, an upgrade can require changes to your code, commands or database. Read the
release changelog before updating. The crate version and OpenADR wire version are independent.
`openadr version` prints both, and Rust applications can read `openadr::SPEC_VERSION`.

From 1.0 onward, adopting a new OpenADR specification release requires a new major crate version.
A fix that brings behavior into line with the supported specification can ship in a patch. The
current wire model implements OpenADR 3.1.0 and accepts the clarified 3.1.1 resource bodies.
[Wire interpretation](@/docs/spec-notes.md)

## What 1.0 covers

| Surface | Upgrade policy |
| --- | --- |
| Rust library | Documented public paths, reexports, fields, variants, traits and their implementations are API, including paths outside the prelude. Breaking a supported use requires a major release. Private items, `#[doc(hidden)]` helpers, example source and contributor tools are internal. |
| Cargo features | Public feature names, capabilities and default membership are supported. Removing, renaming or weakening a feature, or changing the defaults, requires a major release. New opt-in capabilities can ship in a minor. |
| Rust compiler | The manifest declares the minimum supported Rust version, currently 1.96. A minor release may raise it with a changelog entry. Patch releases retain the declared floor. |
| Embedded builds | The model, schema and core support `no_std` with an allocator on `thumbv7em-none-eabihf` and `wasm32-unknown-unknown`. Dropping either target or requiring `std` for these layers requires a major release. |
| CLI automation | Documented command names, flags, environment variables, exit statuses and machine-readable output fields retain their meanings for the same enabled features. Removing or changing one incompatibly requires a major release. JSON readers should accept additional fields. Human-readable help, logs and table layouts may evolve. |

For example, `openadr conformance --json` returns status **0** for a passing measurement, **1** for
a measured failure, and **2** when the command cannot complete a measurement or produce its requested
output. With `--strict`, any failed check counts. For each role, the last explicit token or
client-credential option takes precedence over earlier options and environment defaults.
[Conformance command](@/docs/conformance.md)

## How changes are reviewed

Pull requests and release tags run the same check. It compares the all-features Rust library,
Cargo feature names, their public implications, default membership and the compiler floor with a
checked-in inventory. A diff names the changed interface. Contributors review intentional changes
alongside the changelog. Separate CI jobs compile feature combinations and embedded targets and
exercise the command line. Behavioral changes still need tests.
[Reviewed API inventory](https://github.com/hupe1980/openadr/blob/main/.github/compatibility/public-api.txt)

Durable deployments should also check the storage schema version before upgrading.
[Storage](@/docs/storage.md)
