//! `trace` — the specification's requirements against the citations in the source.
//!
//! The third of the three. [`check-model`](crate::model) guards the objects and
//! [`check-paths`](crate::paths) the endpoint surface, both against `openadr3.yaml`; this asks the
//! same question of the *prose*, where the requirements that are not a schema live.
//!
//! A **normative statement** is a line carrying an RFC 2119 keyword in capitals — `MUST`,
//! `MUST NOT`, `SHALL`, `SHALL NOT` — and belongs to the deepest heading above it. `SHOULD` and
//! `MAY` are excluded: they are recommendations and permissions, and folding them in would turn a
//! coverage number into an opinion.
//!
//! A **citation** is `[Def §Section Name]` or `[Notifiers §7.2]` in a doc comment or a conformance
//! check's `clause`. That convention was already in use in fifty places, so this reads what was
//! being written rather than asking for a second annotation to keep in step with the first.
//!
//! Two numbers come out of it, and the second is the one that matters.
//!
//! * **Cited.** Every section carrying a requirement is *named* by something in the tree. This is a
//!   gate: a requirement nothing anywhere names fails the build. What it removes is a requirement
//!   with no code near it and nobody noticing — the shape of D-092 and R-016.
//! * **Checked.** The section is named by a **test or a conformance check** rather than only by a
//!   doc comment. A citation is a claim by whoever wrote it, and the only evidence for a claim is
//!   something that runs. This is a ratchet rather than a gate: [`MIN_CHECKED`] may not fall.
//!
//! Where a citation counts as evidence is decided by where it is written:
//!
//! * a `clause` in `src/conformance/suite.rs` — a black-box check run against a live VTN;
//! * anything under `tests/` — an integration test;
//! * anything in `src/` after that file's first `#[cfg(test)]` — a unit test. A heuristic, and an
//!   exact one for this crate, where the test module is the last thing in every file;
//! * anything else — a mention.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// How a document's sections are cited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cite {
    /// `[Def §Object Privacy]` — the heading's text.
    Named,
    /// `[Notifiers §7.2]` — the heading's leading number.
    Numbered,
}

/// A specification document to trace.
struct Document {
    /// The citation prefix, e.g. `Def`.
    prefix: &'static str,
    /// Path under `specs/openadr3-specification`, with `{version}` for the release directory.
    path: &'static str,
    style: Cite,
}

const DOCUMENTS: &[Document] = &[
    Document {
        prefix: "Def",
        path: "{version}/Definition.md",
        style: Cite::Named,
    },
    Document {
        prefix: "Notifiers",
        path: "doc/OpenADR3 Object Operation Notifications via Additional Protocols.md",
        style: Cite::Numbered,
    },
];

/// Sections whose requirements are not this crate's to meet, each with the reason.
///
/// **Empty, and that is the finding.** Every normative statement in both documents is met by
/// something in `src/` and cited from it. The list exists because a future release will add
/// requirements, and an exemption is a decision on the record: it appears in the report, so the next
/// person reads the reason rather than rediscovering the gap.
///
/// An entry that is not needed is refused, in both directions — a section that carries no
/// requirement, and a section that is exempted *and* cited. An unused exemption is a claim nobody
/// checks, which is the shape D-119 found in the problem-type registry.
const NOT_OURS: &[(&str, &str, &str)] = &[];

/// The floor for requirements whose section a test or a conformance check names.
///
/// A ratchet, not a target: raise it when the number rises, and never lower it to make a build pass.
/// It currently sits at the total: every normative statement is in a section something that *runs*
/// names. That is a stronger claim than the "cited" number and still not the strongest available —
/// the granularity is the section, not the sentence — but it is the one that stops a requirement
/// from being covered by a doc comment and nothing else.
const MIN_CHECKED: usize = 34;

/// How strong the evidence behind a citation is.
///
/// Ordered: a check outranks a test, and a test outranks a mention. A section is reported at its
/// strongest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Evidence {
    /// A doc comment naming the section. A claim, and no more than that.
    Mention,
    /// A unit or integration test.
    Test,
    /// A conformance check, which runs black-box against a live VTN.
    Check,
}

impl Evidence {
    fn mark(self) -> &'static str {
        match self {
            Evidence::Mention => "cited",
            Evidence::Test => "tested",
            Evidence::Check => "checked",
        }
    }

    /// Whether this is evidence rather than a claim.
    fn runs(self) -> bool {
        self >= Evidence::Test
    }
}

/// One place a section is cited from.
#[derive(Debug, Clone)]
struct Citation {
    file: String,
    evidence: Evidence,
}

/// One requirement, and where it is written.
#[derive(Debug)]
struct Statement {
    line: usize,
    text: String,
}

/// A heading that carries at least one requirement.
#[derive(Debug, Default)]
struct Section {
    /// What a citation must say to name this section.
    key: String,
    /// The heading as written, for the report.
    title: String,
    statements: Vec<Statement>,
}

/// Emit the traceability report, failing if a requirement's section is uncited.
pub fn run(spec_dir: &Path, version: &str, repo_root: &Path) -> Result<()> {
    let citations = collect_citations(repo_root)?;

    let mut report = String::new();
    let mut uncited: Vec<String> = Vec::new();
    let mut unchecked: Vec<String> = Vec::new();
    let mut stale: Vec<String> = Vec::new();
    let mut total = 0usize;
    let mut covered = 0usize;
    let mut checked = 0usize;
    let mut traced: std::collections::BTreeSet<(&'static str, String)> = Default::default();

    for document in DOCUMENTS {
        let path = spec_dir.join(document.path.replace("{version}", version));
        let source = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "reading {}; run `cargo xtask spec-sync` first",
                path.display()
            )
        })?;
        let sections = normative_sections(&source, document.style);

        let _ = writeln!(report, "\n{} ({})", document.prefix, path.display());
        if sections.is_empty() {
            bail!(
                "no normative statements found in {}; the extractor and the document have diverged",
                path.display()
            );
        }

        for section in &sections {
            total += section.statements.len();
            traced.insert((document.prefix, section.key.clone()));
            let cited = citations
                .get(&(document.prefix, section.key.clone()))
                .map(Vec::as_slice)
                .unwrap_or_default();
            let strongest = cited.iter().map(|c| c.evidence).max();
            let sites: Vec<String> = cited
                .iter()
                .map(|c| format!("{} ({})", c.file, c.evidence.mark()))
                .collect();
            let exempt = NOT_OURS
                .iter()
                .find(|(p, k, _)| *p == document.prefix && *k == section.key);

            if strongest.is_some_and(Evidence::runs) {
                checked += section.statements.len();
            } else if !cited.is_empty() && exempt.is_none() {
                unchecked.push(format!(
                    "  - [{} §{}] — {} requirement(s), named only by {}",
                    document.prefix,
                    section.key,
                    section.statements.len(),
                    sites.join(", ")
                ));
            }

            let mark = match (cited.is_empty(), exempt) {
                (false, Some(_)) => {
                    // Exempted and cited: the exemption is stale, and a stale exemption is a reason
                    // nobody will re-examine when the section next changes.
                    stale.push(format!(
                        "  - [{} §{}] is exempted in NOT_OURS and cited by {}; drop the exemption",
                        document.prefix,
                        section.key,
                        sites.join(", ")
                    ));
                    covered += section.statements.len();
                    "STALE"
                }
                (false, None) => {
                    covered += section.statements.len();
                    strongest.map_or("cited", Evidence::mark)
                }
                (true, Some(_)) => "n/a",
                (true, None) => {
                    // The text as well as the line, because the next step is deciding *where* the
                    // requirement is met, and that is a question about what it says.
                    uncited.push(format!(
                        "  - [{} §{}] has {} requirement(s) and nothing in {} names it.\n      \
                         {}:{}  {}",
                        document.prefix,
                        section.key,
                        section.statements.len(),
                        repo_root.display(),
                        path.display(),
                        section.statements[0].line,
                        excerpt(&section.statements[0].text),
                    ));
                    "UNCITED"
                }
            };

            let _ = writeln!(
                report,
                "  {:>7}  {:>2} req  §{}",
                mark,
                section.statements.len(),
                section.title
            );
            if let Some((_, _, why)) = exempt {
                let _ = writeln!(report, "           ↳ not ours: {why}");
            }
            for by in &sites {
                let _ = writeln!(report, "           ↳ {by}");
            }
        }
    }

    print!("{report}");
    println!(
        "\n{covered}/{total} normative statements sit in a section this implementation cites."
    );
    println!(
        "{checked}/{total} sit in a section a test or a conformance check names \
         (floor {MIN_CHECKED})."
    );
    if !unchecked.is_empty() {
        println!(
            "\nNamed, but only by a doc comment — a claim rather than evidence (ROADMAP B-03):\n{}",
            unchecked.join("\n")
        );
    }

    // An exemption for a section that carries no requirement at all is the other kind of stale: it
    // describes a gap the document does not have.
    for (prefix, key, _) in NOT_OURS {
        if !traced.contains(&(*prefix, (*key).to_string())) {
            stale.push(format!(
                "  - [{prefix} §{key}] is exempted in NOT_OURS but carries no requirement"
            ));
        }
    }

    if !stale.is_empty() {
        bail!(
            "the traceability exemptions are out of date:\n{}",
            stale.join("\n")
        );
    }
    if checked < MIN_CHECKED {
        bail!(
            "{checked} requirement(s) are named by a test or a conformance check, and the floor is \
             {MIN_CHECKED}. Something that used to be covered by a test is not any more. Restore \
             the coverage rather than lowering `MIN_CHECKED` in xtask/src/trace.rs — the floor is a \
             ratchet, and lowering it is how a coverage number becomes a decoration."
        );
    }
    if !uncited.is_empty() {
        bail!(
            "{} section(s) carry a requirement nothing cites:\n{}\n\nCite the section from the \
             code that meets it — `[Def §Name]` or `[Notifiers §7.2]` in a doc comment or a \
             conformance check's `clause` — or add it to `NOT_OURS` in xtask/src/trace.rs with the \
             reason it is out of scope.",
            uncited.len(),
            uncited.join("\n")
        );
    }
    Ok(())
}

/// One line of the requirement, short enough to read in an error message.
fn excerpt(text: &str) -> String {
    const LIMIT: usize = 140;
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(LIMIT) {
        Some((at, _)) => format!("{}…", &flat[..at]),
        None => flat,
    }
}

/// Every heading that carries at least one requirement, in document order.
fn normative_sections(source: &str, style: Cite) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut current: Option<Section> = None;

    for (number, line) in source.lines().enumerate() {
        let number = number + 1;
        if let Some(title) = heading(line) {
            if let Some(section) = current.take()
                && !section.statements.is_empty()
            {
                sections.push(section);
            }
            current = Some(Section {
                key: key_for(title, style),
                title: title.to_string(),
                statements: Vec::new(),
            });
            continue;
        }
        if !is_normative(line) {
            continue;
        }
        if let Some(section) = current.as_mut() {
            section.statements.push(Statement {
                line: number,
                text: line.trim().to_string(),
            });
        }
    }
    if let Some(section) = current
        && !section.statements.is_empty()
    {
        sections.push(section);
    }
    sections
}

/// The heading text of a Markdown ATX heading, if this line is one.
fn heading(line: &str) -> Option<&str> {
    let rest = line.trim_end();
    let hashes = rest.len() - rest.trim_start_matches('#').len();
    (1..=6).contains(&hashes).then(|| rest[hashes..].trim())?;
    Some(rest[hashes..].trim())
}

/// What a citation has to say to name this heading.
fn key_for(title: &str, style: Cite) -> String {
    match style {
        Cite::Named => title.to_string(),
        // `## 7.2 MQTT Notifier` is cited as `[Notifiers §7.2]`.
        Cite::Numbered => title
            .split_whitespace()
            .next()
            .unwrap_or(title)
            .trim_end_matches('.')
            .to_string(),
    }
}

/// Whether a line states a requirement.
///
/// The keyword has to be a standalone capitalised word: `MUST` in prose is a requirement, `must` is
/// not, and neither is `MUSTARD`. Markdown emphasis around it (`**MUST**`) is ordinary punctuation
/// as far as word boundaries go.
fn is_normative(line: &str) -> bool {
    const KEYWORDS: [&str; 2] = ["MUST", "SHALL"];
    line.split(|c: char| !c.is_ascii_alphabetic())
        .any(|word| KEYWORDS.contains(&word))
}

/// The conformance suite, whose `clause` fields are checks rather than claims.
const SUITE: &str = "src/conformance/suite.rs";

/// Every `[Prefix §Section]` citation in the tree, which file makes it, and how strong it is.
///
/// `src/` and `tests/` both, because an integration test naming a section is exactly the evidence a
/// doc comment is not.
fn collect_citations(root: &Path) -> Result<BTreeMap<(&'static str, String), Vec<Citation>>> {
    let mut out: BTreeMap<(&'static str, String), Vec<Citation>> = BTreeMap::new();
    let mut files = rust_files(&root.join("src"))?;
    files.extend(rust_files(&root.join("tests"))?);
    files.sort();

    for file in files {
        let source = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let shown = file
            .strip_prefix(root)
            .unwrap_or(&file)
            .display()
            .to_string()
            .replace('\\', "/");
        // Everything after a file's first `#[cfg(test)]` is test code. Exact for this crate, where
        // the test module is the last item in every file that has one.
        let tests_begin = source.find("#[cfg(test)]").unwrap_or(usize::MAX);
        let is_suite = shown == SUITE;

        for document in DOCUMENTS {
            let opener = format!("[{} §", document.prefix);
            for (at, _) in source.match_indices(&opener) {
                let rest = &source[at + opener.len()..];
                let Some(end) = rest.find(']') else { continue };
                // `[Def §Object Privacy, CL 3.1.0 issue 321]` cites one section and adds a note.
                let key = rest[..end]
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if key.is_empty() {
                    continue;
                }
                let evidence = if is_suite && on_a_clause_line(&source, at) {
                    Evidence::Check
                } else if shown.starts_with("tests/") || at > tests_begin {
                    Evidence::Test
                } else {
                    Evidence::Mention
                };
                let entry = out.entry((document.prefix, key)).or_default();
                match entry.iter_mut().find(|c| c.file == shown) {
                    // One file may both describe a rule and test it; the stronger reading wins.
                    Some(existing) => existing.evidence = existing.evidence.max(evidence),
                    None => entry.push(Citation {
                        file: shown.clone(),
                        evidence,
                    }),
                }
            }
        }
    }
    Ok(out)
}

/// Whether the citation at `at` is the value of a `clause` field rather than prose around one.
///
/// A `Check`'s `clause` is what the suite *asserts against*; the same file's module documentation
/// cites sections it merely describes, and counting those would make the suite evidence for itself.
fn on_a_clause_line(source: &str, at: usize) -> bool {
    let line_start = source[..at].rfind('\n').map_or(0, |i| i + 1);
    source[line_start..at].contains("clause:")
}

/// Every `.rs` file under a directory.
fn rust_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_requirement_is_a_capitalised_keyword_and_nothing_else() {
        assert!(is_normative("A VTN **MUST** use TLS."));
        assert!(is_normative("A VTN SHALL return a 400."));
        assert!(is_normative("The VTN MUST NOT allow creation."));
        // Recommendations and permissions are a different severity and are not counted.
        assert!(!is_normative("A VEN SHOULD support MQTT 5."));
        assert!(!is_normative("A VEN MAY be pre-configured."));
        // Lower case is prose, and a longer word is a different word.
        assert!(!is_normative("this must be true"));
        assert!(!is_normative("MUSTARD is not a keyword"));
    }

    #[test]
    fn an_excerpt_is_one_readable_line() {
        assert_eq!(excerpt("  A VTN   SHALL\n  do it. "), "A VTN SHALL do it.");
        let long = "word ".repeat(80);
        let short = excerpt(&long);
        assert!(short.ends_with('…'));
        assert!(short.chars().count() <= 141);
    }

    #[test]
    fn a_section_key_is_what_a_citation_writes() {
        assert_eq!(key_for("Object Privacy", Cite::Named), "Object Privacy");
        assert_eq!(key_for("7.2 MQTT Notifier", Cite::Numbered), "7.2");
        assert_eq!(key_for("8. Obtaining Topics", Cite::Numbered), "8");
    }

    #[test]
    fn headings_are_recognised_and_prose_is_not() {
        assert_eq!(heading("## Object Privacy"), Some("Object Privacy"));
        assert_eq!(heading("#### ven objects"), Some("ven objects"));
        assert_eq!(heading("not a heading"), None);
        assert_eq!(heading(""), None);
    }

    /// A section with no requirement in it is not a gap, and must not be reported as one.
    #[test]
    fn only_sections_carrying_a_requirement_are_traced() {
        let doc = "\
# Prose only
Nothing normative here.

## Has a rule
A VTN SHALL do the thing.

## Also prose
A VTN MAY do the other thing.
";
        let sections = normative_sections(doc, Cite::Named);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].key, "Has a rule");
        assert_eq!(sections[0].statements.len(), 1);
        assert_eq!(sections[0].statements[0].line, 5);
    }
}
