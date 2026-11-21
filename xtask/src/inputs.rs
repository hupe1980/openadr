//! `cargo xtask inputs [--update] [--markdown]` — which of the inputs nobody here owns have moved.
//!
//! The specification mirror, the Alliance's enumeration schemas, the surveyed implementations'
//! releases, the profile validators and the Fluvius profile PDF are all read by people here and
//! changed by people elsewhere. Each is recorded in `inputs.lock` as it was last read; this
//! reports the difference. It never fails a build: a release somewhere else is not a defect here,
//! and deciding what a move means is somebody's job, not a check's.

use std::{fs, path::Path, process::Command};

use anyhow::{Context, Result, bail};

pub const INPUTS_PATH: &str = "inputs.lock";

/// How an input's current state is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The commit at a repository's `HEAD`.
    Head,
    /// The highest version-shaped tag, without a leading `v`.
    Tag,
    /// The SHA-256 of a downloaded file.
    Sha256,
}

#[derive(Debug, Clone)]
struct Input {
    name: String,
    kind: Kind,
    url: String,
    recorded: String,
}

fn parse(text: &str) -> Result<Vec<Input>> {
    let mut inputs = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [name, kind, url, recorded] = fields[..] else {
            bail!("{INPUTS_PATH}:{}: expected `name kind url recorded`", n + 1);
        };
        let kind = match kind {
            "head" => Kind::Head,
            "tag" => Kind::Tag,
            "sha256" => Kind::Sha256,
            other => bail!("{INPUTS_PATH}:{}: unknown kind `{other}`", n + 1),
        };
        // Every URL reaches `git` or `curl` as an argument, so nothing that could read as an
        // option or a non-https transport is accepted.
        let rest = url.strip_prefix("https://").unwrap_or("");
        if rest.is_empty()
            || !rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._~/:%".contains(c))
        {
            bail!(
                "{INPUTS_PATH}:{}: `{url}` is not a plain https:// URL",
                n + 1
            );
        }
        inputs.push(Input {
            name: name.into(),
            kind,
            url: url.into(),
            recorded: recorded.into(),
        });
    }
    Ok(inputs)
}

fn output(program: &str, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {program}"))?;
    if !out.status.success() {
        bail!(
            "`{program} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// The version-shaped part of a tag, compared numerically: `v0.2.10` > `v0.2.9`.
fn version_key(tag: &str) -> Option<Vec<u64>> {
    let tag = tag.strip_prefix('v').unwrap_or(tag);
    let parts: Option<Vec<u64>> = tag.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| !p.is_empty())
}

fn latest_tag(listing: &str) -> Option<String> {
    listing
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1)?.strip_prefix("refs/tags/"))
        .filter(|t| !t.ends_with("^{}"))
        .filter_map(|t| version_key(t).map(|k| (k, t.trim_start_matches('v').to_string())))
        .max()
        .map(|(_, t)| t)
}

fn current(input: &Input) -> Result<String> {
    match input.kind {
        Kind::Head => {
            let out = String::from_utf8(output("git", &["ls-remote", &input.url, "HEAD"])?)?;
            out.split_whitespace()
                .next()
                .map(str::to_string)
                .context("no HEAD")
        }
        Kind::Tag => {
            let out = String::from_utf8(output("git", &["ls-remote", "--tags", &input.url])?)?;
            latest_tag(&out).context("no version-shaped tag")
        }
        Kind::Sha256 => {
            let bytes = output(
                "curl",
                &[
                    "--fail",
                    "--silent",
                    "--location",
                    "--max-filesize",
                    "52428800",
                    "--",
                    &input.url,
                ],
            )?;
            use sha2::Digest as _;
            Ok(sha2::Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect())
        }
    }
}

/// Run the report. Returns how many inputs moved, for the workflow that files it.
pub fn run(root: &Path, pin: &crate::pin::SpecPin, update: bool, markdown: bool) -> Result<usize> {
    let path = root.join(INPUTS_PATH);
    let text = fs::read_to_string(&path).with_context(|| format!("reading {INPUTS_PATH}"))?;
    let inputs = parse(&text)?;

    let mut rows = Vec::new();
    let mut moved = 0;
    let mirror_head = output("git", &["ls-remote", &pin.url, "HEAD"])
        .ok()
        .and_then(|o| String::from_utf8(o).ok())
        .and_then(|o| o.split_whitespace().next().map(str::to_string));
    match &mirror_head {
        Some(head) if head == &pin.commit => rows.push((
            "spec-mirror".to_string(),
            short(&pin.commit),
            short(head),
            String::new(),
        )),
        Some(head) => {
            moved += 1;
            rows.push((
                "spec-mirror".to_string(),
                short(&pin.commit),
                short(head),
                "moved — `cargo xtask spec-sync --update`".to_string(),
            ));
        }
        None => rows.push((
            "spec-mirror".to_string(),
            short(&pin.commit),
            "?".into(),
            "unreachable".to_string(),
        )),
    }

    let mut updated = text.clone();
    for input in &inputs {
        match current(input) {
            Ok(now) if now == input.recorded => rows.push((
                input.name.clone(),
                short(&input.recorded),
                short(&now),
                String::new(),
            )),
            Ok(now) => {
                moved += 1;
                rows.push((
                    input.name.clone(),
                    short(&input.recorded),
                    short(&now),
                    "moved".to_string(),
                ));
                updated = updated.replacen(&input.recorded, &now, 1);
            }
            Err(e) => rows.push((
                input.name.clone(),
                short(&input.recorded),
                "?".into(),
                format!("unreachable: {e}"),
            )),
        }
    }

    if markdown {
        println!("| Input | Recorded | Now | |\n|---|---|---|---|");
        for (name, recorded, now, note) in &rows {
            println!("{}", markdown_row(name, recorded, now, note));
        }
    } else {
        for (name, recorded, now, note) in &rows {
            println!("{name:<16} {recorded:<14} {now:<14} {note}");
        }
    }
    println!("\n{moved} of {} input(s) moved", rows.len());
    if update && updated != text {
        fs::write(&path, updated)?;
        println!("wrote {INPUTS_PATH}: commit it with a note on what the change means");
    }
    Ok(moved)
}

/// One row of the issue body. Fetched values sit in code spans, so a tag named `@someone` or
/// `**urgent**` is shown, not acted on (inputs-report/SR-002).
fn markdown_row(name: &str, recorded: &str, now: &str, note: &str) -> String {
    let span = |v: &str| format!("`{}`", v.replace('`', "'"));
    format!("| {name} | {} | {} | {note} |", span(recorded), span(now))
}

fn short(s: &str) -> String {
    if s.len() == 40 || s.len() == 64 {
        s[..12].to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_highest_version_wins_whatever_its_prefix() {
        let listing = "a\trefs/tags/v2.0.4\nb\trefs/tags/2.4.2\nc\trefs/tags/2.4.2^{}\nd\trefs/tags/nightly\ne\trefs/tags/v0.2.10\n";
        assert_eq!(latest_tag(listing).as_deref(), Some("2.4.2"));
        assert!(version_key("v0.2.10") > version_key("0.2.9"));
    }

    // inputs-report/SR-002
    #[test]
    fn fetched_values_render_inert() {
        let row = markdown_row("peer", "1.0", "@everyone`**x**", "moved");
        assert_eq!(row, "| peer | `1.0` | `@everyone'**x**` | moved |");
    }

    // inputs-report/SR-001
    #[test]
    fn a_url_that_could_be_an_option_is_refused() {
        for url in [
            "--upload-pack=x",
            "http://example.com",
            "file:///etc/passwd",
            "https://",
        ] {
            assert!(parse(&format!("x tag {url} 1.0\n")).is_err(), "{url}");
        }
        assert!(parse("x tag https://github.com/a/b 1.0\n").is_ok());
        assert!(parse("x tag https://github.com/a/b\n").is_err());
    }
}
