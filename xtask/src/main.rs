//! Repository automation.
//!
//! `cargo xtask <command>`:
//!
//! * `spec-sync`  — fetch the public OpenADR 3 specification mirror into `specs/`
//! * `codegen`    — regenerate `src/schema/table.rs` from the enumeration schemas
//! * `check-drift`— fail if the checked-in table no longer matches the specification
//! * `check-model`— fail if the wire model no longer matches the OpenAPI document
//! * `check-paths`— fail if the VTN's routes or their scopes no longer match it
//!
//! The payload table is the one place where a change upstream would otherwise pass unnoticed: a new
//! payload type, or a tightened bound, would simply never be enforced. `check-drift` runs in CI.

use std::{
    collections::BTreeSet,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde_yaml_ng::Value;

mod model;
mod paths;
mod trace;

const SPEC_REPO: &str = "https://github.com/grid-coordination/openadr3-specification";
const TABLE_PATH: &str = "src/schema/table.rs";
const OPENAPI_PATH: &str = "src/vtn/openapi.json";
/// The published registry every `problem.type` URI resolves to.
const PROBLEMS_PATH: &str = "site/content/docs/problems.md";
/// The `Storage` trait, whose every method the shared suite must exercise.
const STORAGE_PATH: &str = "src/vtn/store/mod.rs";
/// The suite that exercises it.
const SUITE_PATH: &str = "src/vtn/store/suite.rs";
const LIB_PATH: &str = "src/lib.rs";

fn main() -> Result<()> {
    let command = std::env::args().nth(1).unwrap_or_else(|| "help".into());
    match command.as_str() {
        "spec-sync" => spec_sync(),
        "codegen" => {
            let generated = generate_table()?;
            fs::write(root().join(TABLE_PATH), &generated)?;
            println!("wrote {TABLE_PATH}");
            let document = generate_openapi()?;
            fs::write(root().join(OPENAPI_PATH), &document)?;
            println!("wrote {OPENAPI_PATH}");
            Ok(())
        }
        "check-drift" => check_drift(),
        "check-model" => check_model(),
        "check-paths" => check_paths(),
        "check-problems" => check_problems(),
        "check-suite" => check_suite(),
        "trace" => trace::run(&spec_dir(), &spec_version()?, &root()),
        other => {
            let usage = format!(
                "cargo xtask <command>\n\n\
                 spec-sync       fetch the specification mirror into specs/\n\
                 codegen         regenerate {TABLE_PATH} from the enumeration schemas\n\n\
                 check-drift     fail if {TABLE_PATH} is out of date\n\
                 check-model     fail if the wire model no longer matches openadr3.yaml\n\
                 check-paths     fail if the VTN's routes or scopes no longer match openadr3.yaml\n\
                 check-problems  fail if a problem.type URI resolves to nothing\n\
                 check-suite     fail if a Storage method or a written behaviour goes unexercised\n\
                 trace           fail if a MUST or SHALL sits in a section nothing cites"
            );
            // A misspelled check that exits 0 is a check nobody notices is gone — which is how
            // `check-links`, a command that never existed, spent a while being reported as passing.
            if other == "help" || other == "--help" || other == "-h" {
                println!("{usage}");
                Ok(())
            } else {
                bail!("unknown command `{other}`\n\n{usage}")
            }
        }
    }
}

/// The specification version the crate says it implements.
///
/// Read from `src/lib.rs` rather than repeated here. It was repeated here once, the two drifted,
/// and the generated payload table was quietly built from a different release of the specification
/// than the crate claimed to implement.
fn spec_version() -> Result<String> {
    let lib = fs::read_to_string(root().join(LIB_PATH))
        .with_context(|| format!("could not read {LIB_PATH}"))?;
    let marker = "pub const SPEC_VERSION: &str = \"";
    let start = lib
        .find(marker)
        .map(|i| i + marker.len())
        .with_context(|| format!("{LIB_PATH} does not define SPEC_VERSION"))?;
    let end = lib[start..]
        .find('"')
        .map(|i| start + i)
        .context("SPEC_VERSION is not a string literal")?;
    Ok(lib[start..end].to_string())
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the workspace root")
        .to_path_buf()
}

fn spec_dir() -> PathBuf {
    root().join("specs/openadr3-specification")
}

fn spec_sync() -> Result<()> {
    let dir = spec_dir();
    if dir.exists() {
        println!("updating {}", dir.display());
        run(Command::new("git")
            .arg("-C")
            .arg(&dir)
            .arg("pull")
            .arg("--ff-only"))
    } else {
        fs::create_dir_all(dir.parent().unwrap())?;
        println!("cloning {SPEC_REPO}");
        run(Command::new("git")
            .arg("clone")
            .arg("--depth")
            .arg("1")
            .arg(SPEC_REPO)
            .arg(&dir))
    }
}

fn run(command: &mut Command) -> Result<()> {
    let status = command.status().context("failed to run git")?;
    if !status.success() {
        bail!("command failed with {status}");
    }
    Ok(())
}

fn check_drift() -> Result<()> {
    let generated = generate_table()?;
    let existing = fs::read_to_string(root().join(TABLE_PATH))
        .with_context(|| format!("{TABLE_PATH} is missing; run `cargo xtask codegen`"))?;
    if generated != existing {
        bail!(
            "{TABLE_PATH} no longer matches the specification in specs/.\n\
             The enumerations changed upstream. Run `cargo xtask codegen`, read the diff, and \
             adjust any code that relied on the old constraints."
        );
    }

    let document = generate_openapi()?;
    let existing = fs::read_to_string(root().join(OPENAPI_PATH))
        .with_context(|| format!("{OPENAPI_PATH} is missing; run `cargo xtask codegen`"))?;
    if document != existing {
        bail!(
            "{OPENAPI_PATH} no longer matches the specification in specs/.\n\
             `GET /openapi.json` serves this document, so a client generated from it would \
             describe a different API than the one this VTN implements. Run `cargo xtask codegen`."
        );
    }

    println!(
        "{TABLE_PATH} and {OPENAPI_PATH} are up to date with OpenADR {}",
        spec_version()?
    );
    Ok(())
}

/// The OpenAPI document the VTN serves at `GET /openapi.json`, as compact JSON.
///
/// A copy of `openadr3.yaml`, transcoded to JSON and checked in — because `specs/` is fetched
/// rather than vendored, and a document the VTN serves has to exist in a build that never ran
/// `spec-sync`. The Alliance publishes it under Apache 2.0 (see `specs/…/LICENSE` and `NOTICE`), so
/// redistributing it is a matter of attribution rather than permission, and the attribution is
/// written into `info` below.
///
/// Two things are deliberately *not* done here:
///
/// * **Nothing is invented.** The paths, schemas and security requirements are the Alliance's,
///   unedited. What this VTN adds or omits is applied at *serve* time by `vtn::openapi`, against the
///   configuration — so the document a client fetches describes the VTN it fetched it from, and this
///   file stays a faithful copy that `check-drift` can compare.
/// * **Nothing is reformatted.** Compact and key-sorted, so a diff after `spec-sync` is the
///   specification's change and not `serde_yaml_ng`'s idea of ordering.
fn generate_openapi() -> Result<String> {
    let spec = read_spec()?;
    let mut json = to_json(&spec)?;

    let version = spec_version()?;
    let object = json
        .as_object_mut()
        .context("openadr3.yaml is not a mapping")?;
    // The mock server the Alliance points at is not this VTN. `vtn::openapi` replaces this with a
    // relative URL naming the deployment's own base path; leaving SwaggerHub's here would send a
    // generated client somewhere else entirely.
    object.remove("servers");
    if let Some(info) = object.get_mut("info").and_then(|i| i.as_object_mut()) {
        info.insert(
            "x-openadr-source".into(),
            serde_json::json!(format!(
                "OpenADR {version} openadr3.yaml, © OpenADR Alliance, Apache-2.0. Transcoded to \
                 JSON by `cargo xtask codegen`; servers, and the paths this deployment does not \
                 serve, are set by the VTN at request time."
            )),
        );
    }

    let mut out = serde_json::to_string(&json)?;
    out.push('\n');
    Ok(out)
}

/// Convert a YAML value to JSON, sorting object keys so the output is stable.
fn to_json(value: &Value) -> Result<serde_json::Value> {
    Ok(match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Number(n) => serde_json::from_str(&n.to_string())?,
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Sequence(items) => {
            serde_json::Value::Array(items.iter().map(to_json).collect::<Result<Vec<_>>>()?)
        }
        Value::Mapping(map) => {
            // `serde_json::Map` preserves insertion order under `preserve_order`, and orders by key
            // otherwise. Collecting through a `BTreeMap` makes the result the same either way.
            let mut sorted = std::collections::BTreeMap::new();
            for (k, v) in map {
                let key = match k {
                    Value::String(s) => s.clone(),
                    other => serde_yaml_ng::to_string(other)?.trim().to_string(),
                };
                sorted.insert(key, to_json(v)?);
            }
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        Value::Tagged(tagged) => to_json(&tagged.value)?,
    })
}

/// Compare the wire model against the OpenAPI document.
///
/// The companion to `check-drift`: that one guards the payload *enumerations*, this one guards the
/// *objects*. Between them nothing in `openadr3.yaml` can change upstream without CI saying so.
fn check_model() -> Result<()> {
    model::check(&read_spec()?)
}

/// Compare the VTN's endpoint surface against the document.
///
/// The half `check_model` cannot reach: it guards the objects, this guards the paths, the methods
/// and — the part that has actually gone wrong — the scopes. It drives the real router rather than
/// a table describing it, because a table is a second copy of the routing rules.
fn check_paths() -> Result<()> {
    paths::check(&read_spec()?)
}

/// Every `problem.type` this crate can mint resolves to documentation of that type.
///
/// RFC 9457 §3.1.1 asks for that, and the docs claim it. It held only as long as somebody
/// remembered to add a page whenever a slug was added — so this compares the two lists instead.
/// The crate side is [`openadr::model::problem::PROBLEM_TYPES`]; the published side is the
/// `aliases` array in the registry page, which is what Zola turns into the resolving URLs.
fn check_problems() -> Result<()> {
    let page = fs::read_to_string(root().join(PROBLEMS_PATH))
        .with_context(|| format!("reading {PROBLEMS_PATH}"))?;
    let front = page
        .split("+++")
        .nth(1)
        .context("the registry page has no TOML front matter")?;
    let aliases = front
        .split_once("aliases = [")
        .context("the registry page declares no aliases, so no problem type URI resolves")?
        .1
        .split_once(']')
        .context("the aliases array is not closed")?
        .0;

    let published: BTreeSet<&str> = aliases
        .split(',')
        .filter_map(|line| line.trim().trim_matches('"').rsplit('/').next())
        .filter(|slug| !slug.is_empty())
        .collect();
    let minted: BTreeSet<&str> = openadr::model::problem::PROBLEM_TYPES
        .iter()
        .copied()
        .collect();

    let unpublished: Vec<&&str> = minted.difference(&published).collect();
    let orphaned: Vec<&&str> = published.difference(&minted).collect();
    if !unpublished.is_empty() || !orphaned.is_empty() {
        let mut message = String::from("the problem type registry no longer matches the code:\n");
        for slug in unpublished {
            message.push_str(&format!(
                "  - {slug} is minted but has no alias in {PROBLEMS_PATH}, so its type URI 404s\n"
            ));
        }
        for slug in orphaned {
            message.push_str(&format!(
                "  - {slug} is published but nothing mints it any more\n"
            ));
        }
        bail!(message);
    }

    println!("all {} problem type URIs resolve", minted.len());
    Ok(())
}

/// Every `Storage` method has a behaviour, and every behaviour written is a behaviour run.
///
/// The suite is the only thing keeping three backends interchangeable (D-032), so "a behaviour it
/// does not name is a behaviour they may differ on" (D-054) is the governing rule. Two directions,
/// because it can rot either way: a trait method nothing in `suite.rs` calls is one the backends may
/// disagree about, and a `pub async fn` the `run_suite!` list does not name is a behaviour nothing
/// runs (D-129).
///
/// Textual rather than semantic, which is enough because both files are written in one shape: the
/// trait is a flat list of `async fn`, and the suite calls each method as `.name(`.
fn check_suite() -> Result<()> {
    let trait_source = fs::read_to_string(root().join(STORAGE_PATH))
        .with_context(|| format!("reading {STORAGE_PATH}"))?;
    let suite = fs::read_to_string(root().join(SUITE_PATH))
        .with_context(|| format!("reading {SUITE_PATH}"))?;

    // The trait body, so a helper defined beside it is not mistaken for a method.
    let body = trait_source
        .split_once("pub trait Storage")
        .context("src/vtn/store/mod.rs declares no `pub trait Storage`")?
        .1;
    let methods: BTreeSet<&str> = body
        .lines()
        .filter_map(|line| line.trim().strip_prefix("async fn "))
        .filter_map(|rest| rest.split(['(', '<']).next())
        .filter(|name| !name.is_empty())
        .collect();
    if methods.is_empty() {
        bail!("no methods found on `Storage`; the parser and the trait have diverged");
    }

    let unexercised: Vec<&&str> = methods
        .iter()
        .filter(|name| !suite.contains(&format!(".{name}(")))
        .collect();

    // Every behaviour the suite defines, against the list the backends actually run.
    //
    // A *behaviour* takes the backend and returns nothing: it asserts. A `pub async fn` that
    // returns something is a shared *fixture* — `seed_a_cascade_of_targeted_children` builds the
    // state the SQL backends' own orphan check inspects — and belongs to its caller rather than to
    // the list. Told apart by the signature rather than by a naming convention, because a
    // convention is one more thing to remember and this is the file whose whole point is not
    // relying on that.
    let defined: BTreeSet<&str> = suite
        .lines()
        .filter_map(|line| line.strip_prefix("pub async fn "))
        .filter(|rest| !rest.contains("->"))
        .filter_map(|rest| rest.split('(').next())
        .collect();
    let listed: BTreeSet<&str> = suite
        .match_indices("check!(")
        .filter_map(|(at, _)| suite[at + "check!(".len()..].split(')').next())
        .collect();
    let unrun: Vec<&&str> = defined.difference(&listed).collect();

    if !unexercised.is_empty() || !unrun.is_empty() {
        let mut message = String::from("the storage conformance suite has gaps:\n");
        for name in unexercised {
            message.push_str(&format!(
                "  - Storage::{name} is never called by {SUITE_PATH}, so the backends may differ \
                 about it\n"
            ));
        }
        for name in unrun {
            message.push_str(&format!(
                "  - {name} is written but not in the `run_suite!` list, so no backend runs it\n"
            ));
        }
        bail!(message);
    }

    println!(
        "all {} Storage methods are exercised by {} behaviours, and every behaviour runs",
        methods.len(),
        listed.len()
    );
    Ok(())
}

/// The OpenAPI document for the version the crate declares.
fn read_spec() -> Result<Value> {
    let version = spec_version()?;
    let path = spec_dir().join(&version).join("openadr3.yaml");
    if !path.exists() {
        bail!(
            "{} not found; run `cargo xtask spec-sync` first",
            path.display()
        );
    }
    serde_yaml_ng::from_str(&fs::read_to_string(&path)?)
        .with_context(|| format!("could not parse {}", path.display()))
}

/// One payload type, as read from a schema file.
#[derive(Debug)]
struct Spec {
    name: String,
    group: &'static str,
    kinds: Vec<&'static str>,
    min_items: u64,
    max_items: Option<u64>,
    minimum: Option<String>,
    maximum: Option<String>,
    min_length: Option<u64>,
    max_length: Option<u64>,
    allowed: Vec<String>,
}

/// A `definitions`-style enumeration file, and the [`PayloadGroup`] its entries belong to.
///
/// All four have the same shape — one definition per enumerated name, describing the `values`
/// array of a `valuesMap` — so all four are read by the same function. Two of them were fetched by
/// `spec-sync` and compiled by nothing for as long as this file existed, which meant a programme
/// attribute or a VEN attribute was an unvalidated free string.
const VALUE_SCHEMAS: &[(&str, &str)] = &[
    ("event-interval-payloads.schema.yaml", "Event"),
    ("report-payloads.schema.yaml", "Report"),
    ("program-attributes.schema.yaml", "ProgramAttribute"),
    ("ven-resource-attributes.schema.yaml", "VenAttribute"),
];

/// The two enumeration files that are a bare `enum` rather than a set of definitions.
///
/// They constrain a *descriptor field* rather than a `valuesMap`, so they become string tables
/// rather than [`PayloadSpec`]s: `units.schema.yaml` is `eventPayloadDescriptor.units` and
/// `reading-types.schema.yaml` is `reportPayloadDescriptor.readingType`.
const STRING_ENUMS: &[(&str, &str, &str)] = &[
    (
        "units.schema.yaml",
        "UNITS",
        "Every unit of measure `eventPayloadDescriptor.units` enumerates.",
    ),
    (
        "reading-types.schema.yaml",
        "READING_TYPES",
        "Every reading type `reportPayloadDescriptor.readingType` enumerates.",
    ),
];

fn generate_table() -> Result<String> {
    let version = spec_version()?;
    let base = spec_dir().join(&version).join("enumerations");
    if !base.exists() {
        bail!(
            "{} not found; run `cargo xtask spec-sync` first",
            base.display()
        );
    }

    let mut specs: Vec<Spec> = Vec::new();
    for (file, group) in VALUE_SCHEMAS {
        specs.extend(read_schema(&base.join(file), group)?);
    }
    // Deterministic order: group, then name. A stable file makes `check-drift` meaningful.
    specs.sort_by(|a, b| a.group.cmp(b.group).then_with(|| a.name.cmp(&b.name)));

    let mut out = String::new();
    writeln!(
        out,
        "//! Payload specifications generated from the OpenADR enumeration schemas.\n\
         //!\n\
         //! **Generated file — do not edit.** Regenerate with `cargo xtask codegen` after updating\n\
         //! `specs/openadr3-specification/<version>/enumerations/*.schema.yaml`.\n\
         //!\n\
         //! All six enumeration files are read: the four that describe a `valuesMap` become\n\
         //! [`PAYLOAD_SPECS`], and the two that constrain a descriptor field become string tables.\n\
         \n\
         use super::{{PayloadGroup, PayloadSpec, ValueKinds}};\n\
         use rust_decimal_macros::dec;\n\
         \n\
         /// Every payload type the specification enumerates, in name order within each group.\n\
         pub(super) static PAYLOAD_SPECS: &[PayloadSpec] = &["
    )?;
    for spec in &specs {
        writeln!(out, "    PayloadSpec {{")?;
        writeln!(out, "        name: {:?},", spec.name)?;
        writeln!(out, "        group: PayloadGroup::{},", spec.group)?;
        writeln!(out, "        kinds: {},", kinds_expr(&spec.kinds))?;
        writeln!(out, "        min_items: {},", spec.min_items)?;
        writeln!(out, "        max_items: {},", opt(&spec.max_items))?;
        writeln!(out, "        minimum: {},", dec_opt(&spec.minimum))?;
        writeln!(out, "        maximum: {},", dec_opt(&spec.maximum))?;
        writeln!(out, "        min_length: {},", opt(&spec.min_length))?;
        writeln!(out, "        max_length: {},", opt(&spec.max_length))?;
        writeln!(out, "        allowed: {},", allowed_expr(&spec.allowed))?;
        writeln!(out, "    }},")?;
    }
    writeln!(out, "];")?;

    for (file, name, doc) in STRING_ENUMS {
        let values = read_string_enum(&base.join(file))?;
        writeln!(out)?;
        writeln!(out, "/// {doc}")?;
        writeln!(out, "pub(super) static {name}: &[&str] = &[")?;
        for value in &values {
            writeln!(out, "    {value:?},")?;
        }
        writeln!(out, "];")?;
    }

    // Run the result through rustfmt. Without this, `cargo fmt` would reformat the generated file
    // and `check-drift` would then report a difference that has nothing to do with the specification.
    Ok(rustfmt(&out).unwrap_or(out))
}

/// Format Rust source, returning `None` if rustfmt is unavailable.
fn rustfmt(source: &str) -> Option<String> {
    use std::io::Write as _;
    use std::process::Stdio;

    let mut child = Command::new("rustfmt")
        .arg("--edition")
        .arg("2024")
        .arg("--emit")
        .arg("stdout")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(source.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
}

fn kinds_expr(kinds: &[&str]) -> String {
    if kinds.is_empty() || kinds.len() == 5 {
        return "ValueKinds::ANY".into();
    }
    let mut expr = format!("ValueKinds::{}", kinds[0].to_uppercase());
    for k in &kinds[1..] {
        expr = format!("{expr}.or(ValueKinds::{})", k.to_uppercase());
    }
    expr
}

fn opt(v: &Option<u64>) -> String {
    v.map_or_else(|| "None".into(), |n| format!("Some({n})"))
}

fn dec_opt(v: &Option<String>) -> String {
    v.as_ref()
        .map_or_else(|| "None".into(), |n| format!("Some(dec!({n}))"))
}

fn allowed_expr(values: &[String]) -> String {
    if values.is_empty() {
        return "&[]".into();
    }
    let items: Vec<String> = values.iter().map(|v| format!("{v:?}")).collect();
    format!("&[{}]", items.join(", "))
}

fn read_schema(path: &Path, group: &'static str) -> Result<Vec<Spec>> {
    let doc = read_yaml(path)?;
    let definitions = doc
        .get("definitions")
        .and_then(Value::as_mapping)
        .with_context(|| format!("{} has no `definitions`", path.display()))?;

    let mut out = Vec::new();
    for (name, body) in definitions {
        let Some(name) = name.as_str() else { continue };
        // Two shapes. `event-interval-payloads`, `report-payloads` and `ven-resource-attributes`
        // describe the `values` *array*, with the element schema under `items`.
        // `program-attributes` describes the element directly — `type: string`, `type: boolean` —
        // because every programme attribute carries exactly one value. Reading the second as if it
        // were the first loses every constraint it states, which is what happened for as long as
        // the file was not read at all.
        let array = body.get("items").is_some()
            || body.get("type").and_then(Value::as_str) == Some("array");
        let items = if array { body.get("items") } else { Some(body) };

        let mut kinds: Vec<&'static str> = Vec::new();
        let mut allowed: Vec<String> = Vec::new();
        let mut minimum = None;
        let mut maximum = None;
        let mut min_length = None;
        let mut max_length = None;

        if let Some(items) = items {
            collect_kinds(items, &mut kinds);
            if let Some(list) = items.get("enum").and_then(Value::as_sequence) {
                allowed = list
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
            }
            minimum = number_of(items.get("minimum"));
            maximum = number_of(items.get("maximum"));
            // String bounds may sit on the element itself or inside one `oneOf` branch —
            // `CONTROL_SETPOINT` states `minLength`/`maxLength` only on its string alternative.
            (min_length, max_length) = collect_lengths(items);
        }
        kinds.sort_unstable();
        kinds.dedup();

        let (min_items, max_items) = if array {
            (
                body.get("minItems").and_then(Value::as_u64).unwrap_or(1),
                body.get("maxItems").and_then(Value::as_u64),
            )
        } else {
            (1, Some(1))
        };

        out.push(Spec {
            name: name.to_string(),
            group,
            kinds,
            min_items,
            max_items,
            minimum,
            maximum,
            min_length,
            max_length,
            allowed,
        });
    }
    Ok(out)
}

/// Read a bare `enum:` file — the two that constrain a descriptor field rather than a `valuesMap`.
fn read_string_enum(path: &Path) -> Result<Vec<String>> {
    let doc = read_yaml(path)?;
    let values = doc
        .get("enum")
        .and_then(Value::as_sequence)
        .with_context(|| format!("{} has no top-level `enum`", path.display()))?;
    let out: Vec<String> = values
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    if out.is_empty() {
        bail!("{} enumerates nothing", path.display());
    }
    Ok(out)
}

fn read_yaml(path: &Path) -> Result<Value> {
    let text =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    serde_yaml_ng::from_str(&text).with_context(|| format!("could not parse {}", path.display()))
}

/// The tightest `minLength`/`maxLength` an element schema states, following `oneOf`/`anyOf`.
fn collect_lengths(items: &Value) -> (Option<u64>, Option<u64>) {
    let mut min = items.get("minLength").and_then(Value::as_u64);
    let mut max = items.get("maxLength").and_then(Value::as_u64);
    for key in ["oneOf", "anyOf"] {
        if let Some(list) = items.get(key).and_then(Value::as_sequence) {
            for variant in list {
                let (vmin, vmax) = collect_lengths(variant);
                min = min.or(vmin);
                max = max.or(vmax);
            }
        }
    }
    (min, max)
}

/// Walk an `items` schema, collecting the JSON types it permits.
///
/// Handles the four shapes the enumeration files actually use: a plain `type`, a `oneOf` list, a
/// `$ref` to `point`, and a nested array of points (which `CURVE` uses).
fn collect_kinds(items: &Value, out: &mut Vec<&'static str>) {
    if let Some(reference) = items.get("$ref").and_then(Value::as_str)
        && reference.contains("point")
    {
        out.push("Point");
    }
    if let Some(t) = items.get("type").and_then(Value::as_str) {
        match t {
            "integer" => out.push("Integer"),
            "number" => out.push("Number"),
            "string" => out.push("String"),
            "boolean" => out.push("Boolean"),
            "array" => {
                if let Some(inner) = items.get("items") {
                    collect_kinds(inner, out);
                }
            }
            _ => {}
        }
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(list) = items.get(key).and_then(Value::as_sequence) {
            for variant in list {
                collect_kinds(variant, out);
            }
        }
    }
}

fn number_of(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_expressions_compose() {
        assert_eq!(kinds_expr(&["Number"]), "ValueKinds::NUMBER");
        assert_eq!(
            kinds_expr(&["Integer", "Number"]),
            "ValueKinds::INTEGER.or(ValueKinds::NUMBER)"
        );
        assert_eq!(
            kinds_expr(&["Boolean", "Integer", "Number", "Point", "String"]),
            "ValueKinds::ANY"
        );
    }

    /// The check runs against the real files, which is the only version of it worth having.
    ///
    /// A parser that silently found nothing would report success on an empty set — the shape D-120
    /// found in this same file — so this asserts the check *passes* and, separately, that it had
    /// something to look at.
    #[test]
    fn the_storage_suite_check_reads_the_real_files() {
        check_suite().expect("the storage suite should exercise the whole trait");

        let trait_source = fs::read_to_string(root().join(STORAGE_PATH)).unwrap();
        let body = trait_source.split_once("pub trait Storage").unwrap().1;
        let methods = body
            .lines()
            .filter(|line| line.trim().starts_with("async fn "))
            .count();
        assert!(
            methods > 30,
            "the trait parser found only {methods} methods; it and the trait have diverged"
        );
    }

    #[test]
    fn curves_are_recognised_through_the_nested_array() {
        let schema: Value = serde_yaml_ng::from_str(
            "type: array\nitems:\n  type: array\n  items:\n    $ref: '#/components/schemas/point'\n",
        )
        .unwrap();
        let mut kinds = Vec::new();
        collect_kinds(&schema, &mut kinds);
        assert_eq!(kinds, vec!["Point"]);
    }
}
