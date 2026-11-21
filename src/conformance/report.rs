//! The report as something to share: one HTML file, and a badge.
//!
//! Both are rendered from the same [`Report`] the terminal and `--json` output come from, so no two
//! of them can disagree about a result.
//!
//! The HTML is written for the person a report is *sent to* — an implementer reading what their
//! VTN did, a DSO engineer taking it to a meeting — and it quotes the VTN under test, which may be
//! hostile: a failure message carries the body the VTN answered with. So every string is escaped,
//! and a Content-Security-Policy forbids scripts outright, so that an escaping mistake is still not
//! an executed one (spec 004, SR-001).

use std::fmt::Write as _;

use super::{Finding, Outcome, Report, Severity};

/// What the report says about the run itself, beyond its findings.
#[derive(Debug, Clone)]
pub struct RunMeta {
    /// When the run finished, as the reader should see it (RFC 3339).
    pub generated: String,
    /// The version of the suite that ran.
    pub suite_version: String,
    /// The OpenADR version the suite measures against.
    pub spec_version: String,
}

/// The one-word answer to "did it pass".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Every required check that ran passed, and at least one ran.
    Conformant,
    /// A required check failed.
    NotConformant,
    /// No required check could be attempted: nothing was measured, which is not a pass.
    NotMeasured,
}

impl Report {
    fn verdict(&self) -> Verdict {
        let (passed, failed, _) = self.tally(Severity::Required);
        if failed > 0 {
            Verdict::NotConformant
        } else if passed == 0 {
            Verdict::NotMeasured
        } else {
            Verdict::Conformant
        }
    }

    /// A [shields.io endpoint](https://shields.io/badges/endpoint-badge) document:
    /// `OpenADR 3.1 · 51/52 required`, green, red, or grey when nothing was measured.
    pub fn badge(&self, meta: &RunMeta) -> serde_json::Value {
        let (passed, failed, _) = self.tally(Severity::Required);
        let (message, color) = match self.verdict() {
            Verdict::NotMeasured => ("not measured".to_string(), "lightgrey"),
            Verdict::Conformant => (
                format!("{passed}/{} required", passed + failed),
                "brightgreen",
            ),
            Verdict::NotConformant => (format!("{passed}/{} required", passed + failed), "red"),
        };
        serde_json::json!({
            "schemaVersion": 1,
            "label": format!("OpenADR {}", short_version(&meta.spec_version)),
            "message": message,
            "color": color,
        })
    }

    /// The whole report as one self-contained HTML document.
    pub fn to_html(&self, meta: &RunMeta) -> String {
        let (headline, tone) = match self.verdict() {
            Verdict::Conformant => ("No required check failed", "pass"),
            Verdict::NotConformant => ("Required checks failed", "fail"),
            Verdict::NotMeasured => ("Nothing was measured", "skip"),
        };

        let mut tallies = String::new();
        for severity in [
            Severity::Required,
            Severity::Recommended,
            Severity::Extension,
        ] {
            let (passed, failed, skipped) = self.tally(severity);
            if passed + failed + skipped == 0 {
                continue;
            }
            let _ = write!(
                tallies,
                r#"<div class="tally"><h3>{severity}</h3><p><b class="pass">{passed}</b> passed · <b class="fail">{failed}</b> failed · <b class="skip">{skipped}</b> skipped</p></div>"#
            );
        }

        // Failures first, in run order; then everything else.
        let mut ordered: Vec<&Finding> = self
            .findings
            .iter()
            .filter(|f| f.outcome.is_failed())
            .collect();
        ordered.extend(self.findings.iter().filter(|f| !f.outcome.is_failed()));
        let mut rows = String::new();
        for finding in ordered {
            rows.push_str(&row(finding));
        }

        format!(
            r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data:">
<meta name="referrer" content="no-referrer">
<title>OpenADR conformance — {target}</title>
<style>{STYLE}</style>
</head>
<body>
<header>
<p class="kicker">OpenADR {spec} conformance</p>
<h1 class="{tone}">{headline}</h1>
<p class="target">{target}</p>
<p class="meta">{generated} · openadr {suite} · {count} checks</p>
</header>
<section class="tallies">{tallies}</section>
<section class="checks">{rows}</section>
<footer>
<p>Each check tests one sentence of the specification, cited beside it. <b>Required</b> is a MUST or SHALL; <b>recommended</b> a SHOULD; <b>extension</b> is not in the specification and is information only. A skipped check was not attempted and is not a pass.</p>
<p>Run <code>openadr explain &lt;check-id&gt;</code> for any check. Generated by <a href="https://hupe1980.github.io/openadr/docs/conformance/">openadr conformance</a>, an open, independent suite; not an OpenADR Alliance certification.</p>
</footer>
</body>
</html>
"#,
            target = escape(&self.target),
            spec = escape(&meta.spec_version),
            generated = escape(&meta.generated),
            suite = escape(&meta.suite_version),
            count = self.findings.len(),
        )
    }
}

fn row(finding: &Finding) -> String {
    let check = &finding.check;
    let (class, label, detail) = match &finding.outcome {
        Outcome::Passed => ("pass", "pass", None),
        Outcome::Failed(m) => ("fail", "fail", Some(m.as_str())),
        Outcome::Skipped(m) => ("skip", "skip", Some(m.as_str())),
    };
    let open = if finding.outcome.is_failed() {
        " open"
    } else {
        ""
    };
    let body = match detail {
        Some(message) => format!(r#"<p class="detail">{}</p>"#, escape(message)),
        None => String::new(),
    };
    format!(
        r#"<details class="{class}"{open}><summary><span class="pill {class}">{label}</span><span class="title">{title}</span><span class="sev">{severity}</span></summary><div class="body">{body}<p class="cite">Tests <span>{clause}</span> · <code>openadr explain {id}</code></p></div></details>"#,
        title = escape(check.title),
        severity = check.severity,
        clause = escape(check.clause),
        id = escape(check.id),
    )
}

/// `3.1.0` → `3.1`, the form a badge has room for.
fn short_version(version: &str) -> &str {
    match version.match_indices('.').nth(1) {
        Some((i, _)) => &version[..i],
        None => version,
    }
}

/// HTML-escape text for an element body or a double-quoted attribute.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

const STYLE: &str = r#"
:root{--bg:#fbfbfa;--fg:#1d1d1f;--muted:#6b6b70;--line:#e6e6e3;--card:#fff;--pass:#1a7f37;--fail:#c4262e;--skip:#8a8a8f;--pass-bg:#e7f5ea;--fail-bg:#fdecec;--skip-bg:#f0f0f0}
@media (prefers-color-scheme:dark){:root{--bg:#141416;--fg:#ececee;--muted:#9a9aa1;--line:#2a2a2e;--card:#1c1c1f;--pass:#4ac26b;--fail:#ff6b6b;--skip:#8a8a8f;--pass-bg:#16301d;--fail-bg:#3a1a1c;--skip-bg:#26262a}}
*{box-sizing:border-box}
body{margin:0 auto;max-width:60rem;padding:3rem 1.25rem;background:var(--bg);color:var(--fg);font:15px/1.55 system-ui,-apple-system,"Segoe UI",sans-serif}
header{margin-bottom:2rem}
.kicker{margin:0;color:var(--muted);text-transform:uppercase;letter-spacing:.08em;font-size:.75rem;font-weight:600}
h1{margin:.25rem 0;font-size:2.1rem;line-height:1.15}
h1.pass{color:var(--pass)}h1.fail{color:var(--fail)}h1.skip{color:var(--skip)}
.target{margin:.25rem 0;font:500 1rem ui-monospace,SFMono-Regular,Menlo,monospace;word-break:break-all}
.meta{margin:0;color:var(--muted);font-size:.85rem}
.tallies{display:grid;grid-template-columns:repeat(auto-fit,minmax(14rem,1fr));gap:.75rem;margin-bottom:2rem}
.tally{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:.9rem 1rem}
.tally h3{margin:0 0 .25rem;font-size:.8rem;text-transform:uppercase;letter-spacing:.06em;color:var(--muted)}
.tally p{margin:0}
b.pass{color:var(--pass)}b.fail{color:var(--fail)}b.skip{color:var(--skip)}
details{background:var(--card);border:1px solid var(--line);border-radius:10px;margin:.4rem 0}
details.fail{border-color:var(--fail)}
summary{display:flex;gap:.75rem;align-items:baseline;padding:.6rem .9rem;cursor:pointer;list-style:none}
summary::-webkit-details-marker{display:none}
.pill{flex:none;width:3.2rem;text-align:center;border-radius:999px;font-size:.72rem;font-weight:700;text-transform:uppercase;padding:.1rem 0}
.pill.pass{background:var(--pass-bg);color:var(--pass)}.pill.fail{background:var(--fail-bg);color:var(--fail)}.pill.skip{background:var(--skip-bg);color:var(--skip)}
.title{flex:1}
.sev{flex:none;color:var(--muted);font-size:.8rem}
.body{padding:0 .9rem .8rem 4.85rem}
.detail{margin:.2rem 0 .5rem;white-space:pre-wrap;overflow-wrap:anywhere}
.cite{margin:0;color:var(--muted);font-size:.85rem}
.cite span{color:var(--fg)}
code{font:.85em ui-monospace,SFMono-Regular,Menlo,monospace;background:var(--skip-bg);padding:.05rem .3rem;border-radius:4px}
footer{margin-top:2.5rem;color:var(--muted);font-size:.85rem;border-top:1px solid var(--line);padding-top:1rem}
a{color:inherit}
@media print{body{padding:0;max-width:none}details{break-inside:avoid}details:not([open]) .body{display:none}}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::{Check, Needs};

    fn check(id: &'static str, severity: Severity) -> Check {
        Check {
            id,
            clause: "[Def §Object Privacy]",
            title: "a title",
            severity,
            needs: Needs::Nothing,
        }
    }

    fn meta() -> RunMeta {
        RunMeta {
            generated: "2026-11-21T12:00:00Z".into(),
            suite_version: "0.5.0".into(),
            spec_version: "3.1.0".into(),
        }
    }

    fn report(findings: Vec<(Check, Outcome)>) -> Report {
        Report {
            target: "https://vtn.example/openadr3/3.1.0".into(),
            findings: findings
                .into_iter()
                .map(|(check, outcome)| Finding { check, outcome })
                .collect(),
        }
    }

    // conformance-report/SR-001: what a hostile VTN said is shown, never run.
    #[test]
    fn markup_from_the_vtn_under_test_is_inert() {
        let payload = r#"<script>alert(1)</script><img src=x onerror=alert(1)>"#;
        let mut r = report(vec![(
            check("a", Severity::Required),
            Outcome::Failed(format!("POST answered 500: {payload}")),
        )]);
        r.target = "https://x/\"><script>alert(2)</script>".to_string();
        let html = r.to_html(&meta());
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("<img"), "{html}");
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("Content-Security-Policy\" content=\"default-src 'none'"));
        // Nothing to fetch: the only URLs are links a reader may follow.
        assert!(
            !html.contains("src=\"http") && !html.contains("<link"),
            "{html}"
        );
    }

    #[test]
    fn failures_come_first_and_open() {
        let html = report(vec![
            (check("passes", Severity::Required), Outcome::Passed),
            (
                check("fails", Severity::Required),
                Outcome::Failed("no".into()),
            ),
        ])
        .to_html(&meta());
        let fail = html.find("openadr explain fails").unwrap();
        let pass = html.find("openadr explain passes").unwrap();
        assert!(fail < pass);
        assert!(html.contains(r#"<details class="fail" open>"#));
        assert!(html.contains("Required checks failed"));
    }

    #[test]
    fn a_run_that_attempted_no_required_check_is_not_measured() {
        let r = report(vec![
            (
                check("a", Severity::Required),
                Outcome::Skipped("no VEN".into()),
            ),
            (check("b", Severity::Extension), Outcome::Passed),
        ]);
        assert!(r.to_html(&meta()).contains("Nothing was measured"));
        assert_eq!(r.badge(&meta())["message"], "not measured");
        assert_eq!(r.badge(&meta())["color"], "lightgrey");
    }

    #[test]
    fn the_badge_counts_required_checks_that_ran() {
        let r = report(vec![
            (check("a", Severity::Required), Outcome::Passed),
            (check("b", Severity::Required), Outcome::Failed("x".into())),
            (check("c", Severity::Required), Outcome::Skipped("x".into())),
            (
                check("d", Severity::Recommended),
                Outcome::Failed("x".into()),
            ),
        ]);
        let badge = r.badge(&meta());
        assert_eq!(badge["label"], "OpenADR 3.1");
        assert_eq!(badge["message"], "1/2 required");
        assert_eq!(badge["color"], "red");
        assert_eq!(badge["schemaVersion"], 1);
    }
}
