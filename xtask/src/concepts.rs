//! `cargo xtask check-concepts` — the conventions in `concepts/README.md`, enforced.
//!
//! Those conventions are claims about lists, and rule 42 of [`concepts/RULES.md`] says a claim about
//! a list is a check waiting to be written. Four of them were each true when somebody looked and had
//! each stopped being true by the time somebody looked again:
//!
//! * **"`D` and `R` numbers are stable identifiers cited from code comments and from the other
//!   documents here."** A citation to an entry that does not exist reads exactly like one that does.
//!   D-118 is the entry for the last time this happened.
//! * **"Every open work item lives in `ROADMAP.md`. Nothing else here records unfinished work."**
//!   Two 📐 markers sat in `SECURITY.md` recording work no backlog carried, and the storage schema's
//!   "there are no migrations to run" sat in a doc comment.
//! * **Links between these documents resolve.** A document that sends a reader to a file that is not
//!   there is D-110's defect in Markdown: a record is a claim, and this one fails by sending
//!   somebody somewhere that is not.
//! * **Every `D` entry is reachable.** `DECISIONS.md` is ordered by the audit that produced each
//!   entry, which is the right order to *write* and the wrong one to *read*: a reader arriving from
//!   a code comment that says `(D-095)` has 140 entries and no index.
//!
//! The last of those is generated rather than checked, because an index somebody maintains by hand
//! is the second statement of a list and every second statement of a list in this repository has
//! eventually disagreed with the first.
//!
//! **This does not run in CI, and that is not an oversight.** `.gitignore` carries `concepts/`, so a
//! checkout has no documents to check and the step would report success against an empty directory
//! for ever — which is D-120 exactly. It is a local command. R-020 records what else follows from
//! those documents being untracked, and it is more than this.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use anyhow::{Result, bail};

/// Where the index is written, and where it is checked against.
const DECISIONS: &str = "concepts/DECISIONS.md";
const ROADMAP: &str = "concepts/ROADMAP.md";
const RISKS: &str = "concepts/RISKS.md";

/// The marker for designed-but-unwritten work, and the one document allowed to carry it.
const DESIGNED: char = '📐';

/// A finding. Collected rather than returned, because reporting the first of four problems means
/// four runs to see four problems.
struct Finding {
    where_: String,
    what: String,
}

pub fn run(root: &Path, write_index: bool) -> Result<()> {
    let mut findings = Vec::new();

    let decisions = read(root, DECISIONS)?;
    let roadmap = read(root, ROADMAP)?;
    let risks = read(root, RISKS)?;

    let d_ids = headings(&decisions, "D-");
    let r_ids = headings(&risks, "R-");
    let b_ids = headings(&roadmap, "B-");

    if d_ids.is_empty() || r_ids.is_empty() || b_ids.is_empty() {
        bail!(
            "no headings found — DECISIONS has {}, RISKS has {}, ROADMAP has {}. The heading \
             pattern this check reads has changed, which means it would pass vacuously.",
            d_ids.len(),
            r_ids.len(),
            b_ids.len()
        );
    }

    // A check that reads no files is a check that cannot fail (D-072).
    let mut sources = Vec::new();
    collect(&root.join("concepts"), "md", &mut sources)?;
    collect(&root.join("src"), "rs", &mut sources)?;
    collect(&root.join("xtask/src"), "rs", &mut sources)?;
    if sources.len() < 10 {
        bail!(
            "only {} source files found; refusing to report success",
            sources.len()
        );
    }

    for path in &sources {
        let text = fs::read_to_string(path)?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();

        // `D` and `R` citations resolve, everywhere.
        for (prefix, known, home) in [("D-", &d_ids, DECISIONS), ("R-", &r_ids, RISKS)] {
            for id in cited(&text, prefix) {
                if !known.contains_key(&id) {
                    findings.push(Finding {
                        where_: rel.clone(),
                        what: format!("cites {id}, which has no entry in {home}"),
                    });
                }
            }
        }

        // `B` citations are different, and the difference is the backlog's own convention: a
        // finished item is *deleted* rather than struck through, so `DECISIONS.md` legitimately
        // says "the gap the backlog named as B-05, closed" about an entry that is gone. That is
        // history, which is what `DECISIONS.md` is for.
        //
        // What is not legitimate is a **link** to it. Prose naming a deleted id tells a reader it
        // was deleted; a link sends them to an anchor that is not there, which is D-110 — a record
        // is a claim, and this one fails by sending somebody somewhere that is not.
        if rel.starts_with("concepts/") && rel != ROADMAP {
            for (start, para) in paragraphs(&text) {
                let historical = para.contains("closed") || para.contains("deleted");
                for id in cited(&para, "B-") {
                    if b_ids.contains_key(&id) {
                        continue;
                    }
                    let linked = para.contains(&format!("[{id}]"));
                    if linked {
                        findings.push(Finding {
                            where_: format!("{rel}:{start}"),
                            what: format!(
                                "links to {id}, which {ROADMAP} no longer carries. A finished item \
                                 is deleted, so name it in prose rather than linking to it."
                            ),
                        });
                    } else if !historical {
                        findings.push(Finding {
                            where_: format!("{rel}:{start}"),
                            what: format!(
                                "cites {id}, which has no entry in {ROADMAP} and is not described \
                                 as closed."
                            ),
                        });
                    }
                }
            }
        }

        // Links between concepts documents resolve.
        if rel.starts_with("concepts/") {
            for target in links(&text) {
                let (file, _anchor) = target.split_once('#').unwrap_or((target.as_str(), ""));
                if file.is_empty() {
                    continue; // a same-document anchor
                }
                let resolved = root.join("concepts").join(file);
                if !resolved.exists() {
                    findings.push(Finding {
                        where_: rel.clone(),
                        what: format!("links to `{file}`, which does not exist"),
                    });
                }
            }
        }

        // Open work lives on the roadmap and nowhere else.
        //
        // The unit is the **paragraph**, not the line. The convention is not "never write 📐" — a
        // document that cannot name its own gaps is a document that hides them — it is "a gap is
        // marked only by citing the backlog entry that owns it". A paragraph is the smallest span
        // in which that citation is legible to a reader, so it is the span the check reads.
        //
        // `README.md` defines the marker and `RULES.md` quotes the convention; neither records work.
        if rel.starts_with("concepts/")
            && rel != ROADMAP
            && rel != "concepts/README.md"
            && rel != "concepts/RULES.md"
        {
            for (start, para) in paragraphs(&text) {
                if !para.contains(DESIGNED) {
                    continue;
                }
                // A paragraph *about* a marker — "the last 📐, closed" — is prose, not work.
                if para.contains("closed") {
                    continue;
                }
                // `ROADMAP.md`, not `concepts/ROADMAP.md`: these documents link to each other by
                // basename, so the constant is the wrong string to look for in prose.
                if cited(&para, "B-").is_empty() && !para.contains("ROADMAP.md") {
                    findings.push(Finding {
                        where_: format!("{rel}:{start}"),
                        what: format!(
                            "carries {DESIGNED} and cites no backlog entry. Open work belongs on \
                             {ROADMAP}; a document marks a gap by naming the entry that owns it."
                        ),
                    });
                }
            }
        }
    }

    let index = render_index(&d_ids);
    let index_path = root.join("concepts/DECISIONS_INDEX.md");
    if write_index {
        fs::write(&index_path, &index)?;
        println!(
            "wrote concepts/DECISIONS_INDEX.md ({} entries)",
            d_ids.len()
        );
    } else {
        match fs::read_to_string(&index_path) {
            Ok(current) if current == index => {}
            Ok(_) => findings.push(Finding {
                where_: "concepts/DECISIONS_INDEX.md".into(),
                what: "is out of date. Run `cargo xtask concepts-index`.".into(),
            }),
            Err(_) => findings.push(Finding {
                where_: "concepts/DECISIONS_INDEX.md".into(),
                what: "is missing. Run `cargo xtask concepts-index`.".into(),
            }),
        }
    }

    if findings.is_empty() {
        println!(
            "concepts: {} decisions, {} risks, {} backlog entries; every citation resolves, every \
             link resolves, and no open work is recorded outside {ROADMAP}.",
            d_ids.len(),
            r_ids.len(),
            b_ids.len()
        );
        return Ok(());
    }

    let mut report = String::from("the concepts documents contradict their own conventions:\n\n");
    for f in &findings {
        report.push_str(&format!("  {} — {}\n", f.where_, f.what));
    }
    bail!("{report}\n{} finding(s)", findings.len());
}

fn read(root: &Path, rel: &str) -> Result<String> {
    Ok(fs::read_to_string(root.join(rel))?)
}

/// Blank-line-separated blocks, each with the 1-based line it starts on.
fn paragraphs(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut start = 1;
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                out.push((start, std::mem::take(&mut current)));
            }
            start = n + 2;
        } else {
            current.push_str(line);
            current.push('\n');
        }
    }
    if !current.is_empty() {
        out.push((start, current));
    }
    // A heading is not a paragraph. `## Upgrading 📐` followed by a blank line and then the prose
    // that names the backlog entry is one statement, and reading it as two reports the heading for
    // failing to say what the sentence under it says.
    let mut merged: Vec<(usize, String)> = Vec::new();
    for (start, para) in out {
        let is_heading = para.lines().count() == 1 && para.starts_with('#');
        match merged.last_mut() {
            Some((_, prev)) if prev.lines().count() == 1 && prev.starts_with('#') => {
                prev.push_str(&para);
            }
            _ => merged.push((start, para)),
        }
        let _ = is_heading;
    }
    merged
}

/// `### D-042 · Timestamps are stored fixed-width` → `D-042` and its title.
///
/// Both `##` and `###` are read: RISKS states a risk at `##` and DECISIONS at `###`, and a check
/// that knew only one of those would pass vacuously against the other.
fn headings(text: &str, prefix: &str) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line
            .strip_prefix("## ")
            .or_else(|| line.strip_prefix("### "))
        else {
            continue;
        };
        // `~~R-011 · …~~ — closed` is still an entry a citation may resolve to.
        let rest = rest.trim_start_matches('~');
        // Three heading shapes, and a parser that knew only the first would pass vacuously against
        // the other two — which is what the guard in `run` caught on this function's first run:
        //
        //   `### D-042 · Timestamps are stored fixed-width`   DECISIONS
        //   `## R-008, R-010, R-012, R-015 · Delivery …`      RISKS, several ids at once
        //   `### B-02`                                        ROADMAP, the title is the line below
        let (ids, title) = rest.split_once(" · ").unwrap_or((rest, ""));
        for id in ids.split(',').map(str::trim) {
            let id = id.trim_end_matches('~');
            if id.starts_with(prefix) && id.len() > prefix.len() {
                found.insert(
                    id.to_string(),
                    title.trim_end_matches('~').trim().to_string(),
                );
            }
        }
    }
    found
}

/// Every `D-123`-shaped token in a text, however it is punctuated around.
///
/// The width is read from the headings rather than assumed: `D` and `R` entries are three digits
/// and `B` entries are two, and a check that assumed three found no backlog citation anywhere —
/// which is rule 44 in this function, a bound assumed from local habit rather than read from the
/// thing it describes.
fn cited(text: &str, prefix: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(at) = text[i..].find(prefix) {
        let start = i + at;
        // `read_bl` must not match `B-`, and `MIN_CHECKED` must not match `D-`: a citation is
        // preceded by a boundary, never by an identifier character.
        let preceded_ok = start == 0 || {
            let c = bytes[start - 1] as char;
            !c.is_alphanumeric() && c != '_'
        };
        let digits: String = text[start + prefix.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if preceded_ok && (digits.len() == 2 || digits.len() == 3) {
            found.insert(format!("{prefix}{digits}"));
        }
        i = start + prefix.len();
    }
    found
}

/// `[text](TARGET)` for relative targets only — an `https://` link is somebody else's to keep alive.
fn links(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("](") {
        let after = &rest[open + 2..];
        if let Some(close) = after.find(')') {
            let target = &after[..close];
            // `[Outcome](crate::conformance::Outcome)` is a rustdoc intra-doc link quoted in prose,
            // not a path on disk. Anything carrying `::` is Rust's namespace, not the filesystem's.
            let on_disk =
                !target.contains("://") && !target.starts_with("mailto:") && !target.contains("::");
            if on_disk {
                out.push(target.to_string());
            }
            rest = &after[close..];
        } else {
            break;
        }
    }
    out
}

fn render_index(ids: &BTreeMap<String, String>) -> String {
    let mut s = String::from(
        "# Decisions — index\n\n\
         Generated by `cargo xtask concepts-index`; `cargo xtask check-concepts` fails when it is \
         out of date.\n\n\
         [DECISIONS.md](DECISIONS.md) is ordered by the audit that produced each entry, which is \
         the right order to write and the wrong one to read. This is the order a reader arriving \
         from a code comment needs.\n\n",
    );
    s.push_str("| | Decision |\n|---|---|\n");
    for (id, title) in ids {
        let anchor = anchor_for(id, title);
        s.push_str(&format!("| [{id}](DECISIONS.md#{anchor}) | {title} |\n"));
    }
    s
}

/// GitHub's heading anchor: lowercased, punctuation dropped, spaces to hyphens.
fn anchor_for(id: &str, title: &str) -> String {
    let heading = format!("{id} · {title}");
    heading
        .chars()
        .filter_map(|c| {
            if c.is_alphanumeric() {
                Some(c.to_ascii_lowercase())
            } else if c == ' ' || c == '-' {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

fn collect(dir: &Path, ext: &str, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, ext, out)?;
        } else if path.extension().is_some_and(|e| e == ext) {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_heading_yields_its_id_and_title() {
        let ids = headings("### D-042 · Timestamps are stored fixed-width\n", "D-");
        assert_eq!(
            ids.get("D-042").map(String::as_str),
            Some("Timestamps are stored fixed-width")
        );
    }

    #[test]
    fn a_closed_entry_is_still_an_entry() {
        // A citation to R-011 resolves whether or not somebody struck the heading through.
        let ids = headings(
            "## ~~R-011 · The MQTT path is unit-tested~~ — closed\n",
            "R-",
        );
        assert!(ids.contains_key("R-011"));
    }

    #[test]
    fn a_heading_naming_several_yields_all_of_them() {
        let ids = headings(
            "## R-008, R-010, R-012, R-015 · Delivery on the request path\n",
            "R-",
        );
        assert_eq!(ids.len(), 4);
        assert!(ids.contains_key("R-015"));
    }

    #[test]
    fn a_citation_is_found_however_it_is_punctuated() {
        let found = cited("the fix (D-056) and D-057, plus [D-058](x)", "D-");
        assert_eq!(found.len(), 3);
    }

    #[test]
    fn an_identifier_that_merely_contains_the_prefix_is_not_a_citation() {
        // `read_bl` and `MIN_CHECKED` are the real strings this used to match.
        assert!(cited("read_bl-123 and MIND-042", "B-").is_empty());
        assert!(cited("MIND-042", "D-").is_empty());
    }

    #[test]
    fn a_number_of_the_wrong_width_is_not_a_citation() {
        // Two and three digits are ids (`B-02`, `D-140`); one and four are prose.
        assert!(cited("see D-4 sometime", "D-").is_empty());
        assert!(cited("see D-1400 sometime", "D-").is_empty());
    }

    #[test]
    fn only_relative_links_are_collected() {
        let out = links("[a](OVERVIEW.md) [b](https://example.com) [c](RISKS.md#r-001)");
        assert_eq!(out, vec!["OVERVIEW.md", "RISKS.md#r-001"]);
    }

    #[test]
    fn the_anchor_matches_the_form_github_generates() {
        assert_eq!(
            anchor_for("D-042", "Timestamps are stored fixed-width"),
            "d-042--timestamps-are-stored-fixed-width"
        );
    }
}
