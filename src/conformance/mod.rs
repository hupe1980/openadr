//! A black-box conformance suite for **any** OpenADR 3.1 VTN.
//!
//! Every other test in this crate proves that this implementation agrees with itself. Two systems
//! can each be perfectly self-consistent and refuse to talk to each other, so self-agreement is not
//! interoperability — it is the thing people mistake for it. This module is the measurement.
//!
//! It speaks nothing but HTTP. Point it at a base URL and hand it credentials and it will run
//! against the Alliance's reference VTN, against another Rust one, or against something written
//! this afternoon in Go, exactly as it runs against this one. Nothing here imports
//! [`crate::vtn`].
//!
//! ## What a result means
//!
//! Three outcomes, and the third is the one that makes the other two worth reading:
//!
//! * **Passed** — the VTN did what the cited clause requires.
//! * **Failed** — it did something else. The message says what was expected and what arrived.
//! * **Skipped** — the check could not be *run*, and says why. A VTN with no broker skips the MQTT
//!   topic checks; a suite without VEN credentials skips every privacy check.
//!
//! A skip is not a pass, and the report never adds them together. A suite that quietly counts
//! everything it could not attempt as success is how "166 of 168" becomes a number nobody can act
//! on.
//!
//! ## Every check cites a clause
//!
//! [`Check::clause`] names the sentence being tested — `[Def §Object Privacy]`, `[API events]`,
//! `[UG §7.3]`. That is not decoration. A failing check has to be arguable, and an argument about
//! conformance is an argument about a specific sentence; a check that cannot name one is a check
//! asserting this implementation's opinion, and those are marked [`Severity::Extension`] instead.
//!
//! ## It writes
//!
//! Conformance cannot be observed from reads alone — half the interesting behaviour is what a VTN
//! does with a `POST`. The suite creates programmes, events, VENs and reports under names prefixed
//! with [`RUN_PREFIX`] and deletes them afterwards, including on failure. **Do not run it against a
//! VTN whose data matters**, and see [`Runner::cleanup`] for what "afterwards" means when a check
//! panics or the network drops.

use std::fmt;

use crate::client::{BusinessLogic, Client, ClientError, VirtualEndNode};

mod suite;

pub use suite::checks;

/// Prefix for every object the suite creates, so a run is identifiable and cleanable.
pub const RUN_PREFIX: &str = "oadr-conformance-";

/// How much a failure means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// The specification says **SHALL** or **MUST**. A failure is non-conformance.
    Required,
    /// The specification says **SHOULD**, or leaves the choice open while naming a preference.
    Recommended,
    /// Not in the specification at all: something this implementation adds, checked so that a
    /// report can say whether a peer happens to support it. A failure is *information*.
    Extension,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Severity::Required => "required",
            Severity::Recommended => "recommended",
            Severity::Extension => "extension",
        })
    }
}

/// What a check needs before it can run at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Needs {
    /// Business-logic credentials only.
    BusinessLogic,
    /// Business logic *and* a VEN, because the check is about the boundary between them.
    BothRoles,
    /// Nothing: the endpoint is unauthenticated.
    Nothing,
}

/// One thing the suite checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Check {
    /// Stable identifier, for a report a human diffs between runs.
    pub id: &'static str,
    /// The sentence being tested, in this repository's citation convention.
    pub clause: &'static str,
    /// What the check asserts, in one line.
    pub title: &'static str,
    /// What a failure means.
    pub severity: Severity,
    /// What the check needs in order to run.
    pub needs: Needs,
}

/// What happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The VTN did what the clause requires.
    Passed,
    /// It did something else.
    Failed(String),
    /// The check could not be attempted, and this is why.
    Skipped(String),
}

impl Outcome {
    /// Whether this is a pass.
    pub fn is_passed(&self) -> bool {
        matches!(self, Outcome::Passed)
    }
    /// Whether this is a failure.
    pub fn is_failed(&self) -> bool {
        matches!(self, Outcome::Failed(_))
    }
    /// Whether this was not attempted.
    pub fn is_skipped(&self) -> bool {
        matches!(self, Outcome::Skipped(_))
    }
}

/// One check and what it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The check.
    pub check: Check,
    /// What happened.
    pub outcome: Outcome,
}

/// Everything one run found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The VTN this ran against.
    pub target: String,
    /// One entry per check, in the order they ran.
    pub findings: Vec<Finding>,
}

impl Report {
    /// How many passed, failed and were skipped, at a given severity.
    pub fn tally(&self, severity: Severity) -> (usize, usize, usize) {
        let mut counts = (0, 0, 0);
        for finding in self
            .findings
            .iter()
            .filter(|f| f.check.severity == severity)
        {
            match finding.outcome {
                Outcome::Passed => counts.0 += 1,
                Outcome::Failed(_) => counts.1 += 1,
                Outcome::Skipped(_) => counts.2 += 1,
            }
        }
        counts
    }

    /// Whether every **required** check that ran passed.
    ///
    /// Skips are excluded rather than counted either way, which is the whole point of having a
    /// third outcome: a run that could not attempt a check has not shown it works.
    pub fn is_conformant(&self) -> bool {
        !self
            .findings
            .iter()
            .any(|f| f.check.severity == Severity::Required && f.outcome.is_failed())
    }

    /// The checks that failed.
    pub fn failures(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.outcome.is_failed())
    }

    /// The report as JSON, for a published interoperability matrix.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "target": self.target,
            "conformant": self.is_conformant(),
            "findings": self.findings.iter().map(|f| serde_json::json!({
                "id": f.check.id,
                "clause": f.check.clause,
                "title": f.check.title,
                "severity": f.check.severity.to_string(),
                "outcome": match &f.outcome {
                    Outcome::Passed => "passed",
                    Outcome::Failed(_) => "failed",
                    Outcome::Skipped(_) => "skipped",
                },
                "detail": match &f.outcome {
                    Outcome::Passed => None,
                    Outcome::Failed(m) | Outcome::Skipped(m) => Some(m.clone()),
                },
            })).collect::<Vec<_>>(),
        })
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "openadr conformance — {}", self.target)?;
        writeln!(f)?;
        for finding in &self.findings {
            let mark = match finding.outcome {
                Outcome::Passed => "pass",
                Outcome::Failed(_) => "FAIL",
                Outcome::Skipped(_) => "skip",
            };
            writeln!(
                f,
                "{mark}  {:<38} {}  {}",
                finding.check.id, finding.check.clause, finding.check.title
            )?;
            match &finding.outcome {
                Outcome::Failed(message) => writeln!(f, "      → {message}")?,
                Outcome::Skipped(message) => writeln!(f, "      ~ {message}")?,
                Outcome::Passed => {}
            }
        }
        writeln!(f)?;
        for severity in [
            Severity::Required,
            Severity::Recommended,
            Severity::Extension,
        ] {
            let (passed, failed, skipped) = self.tally(severity);
            if passed + failed + skipped == 0 {
                continue;
            }
            writeln!(
                f,
                "{severity:<12} {passed} passed, {failed} failed, {skipped} skipped"
            )?;
        }
        writeln!(f)?;
        if self.is_conformant() {
            writeln!(f, "No required check failed.")
        } else {
            writeln!(
                f,
                "NOT CONFORMANT: {} required check(s) failed.",
                self.tally(Severity::Required).1
            )
        }
    }
}

/// How to reach the VTN under test.
#[derive(Debug, Clone)]
pub struct Target {
    /// Base URL, including the base path.
    pub base_url: String,
    /// A business-logic bearer token, or credentials to obtain one.
    pub business_logic: Option<Credential>,
    /// A VEN bearer token, or credentials to obtain one.
    ///
    /// Without it every object-privacy check skips — and object privacy is the half of OpenADR 3.1
    /// most worth measuring, so a run without VEN credentials is a much weaker run.
    pub ven: Option<Credential>,
    /// The `clientID` the VEN credential authenticates as.
    ///
    /// The suite cannot discover it: a VTN maps a token to a `clientID` "by means not specified
    /// here" `[Def §VEN created object privacy]`. Without it the checks that need business logic to
    /// grant this VEN a target skip.
    pub ven_client_id: Option<String>,
}

/// A credential, in whichever of the two shapes a deployment has.
#[derive(Clone)]
pub enum Credential {
    /// A pre-shared bearer token.
    Token(String),
    /// OAuth2 client credentials, exchanged at the token endpoint the VTN advertises.
    ClientCredentials {
        /// The client identifier.
        id: String,
        /// The client secret.
        secret: String,
    },
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Credential::Token(_) => f.write_str("Token(<redacted>)"),
            Credential::ClientCredentials { id, .. } => f
                .debug_struct("ClientCredentials")
                .field("id", id)
                .field("secret", &"<redacted>")
                .finish(),
        }
    }
}

impl Target {
    /// A target with no credentials — enough for the unauthenticated checks and nothing else.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            business_logic: None,
            ven: None,
            ven_client_id: None,
        }
    }

    /// Set the business-logic credential.
    pub fn with_business_logic(mut self, credential: Credential) -> Self {
        self.business_logic = Some(credential);
        self
    }

    /// Set the VEN credential and the `clientID` it authenticates as.
    pub fn with_ven(mut self, credential: Credential, client_id: impl Into<String>) -> Self {
        self.ven = Some(credential);
        self.ven_client_id = Some(client_id.into());
        self
    }
}

/// Runs the suite against one VTN.
pub struct Runner {
    target: Target,
    bl: Option<Client<BusinessLogic>>,
    ven: Option<Client<VirtualEndNode>>,
    /// Everything created during the run, newest first, so cleanup unwinds in dependency order.
    litter: std::sync::Mutex<Vec<(&'static str, String)>>,
}

impl fmt::Debug for Runner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runner")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl Runner {
    /// Build a runner. Fails only if the base URL cannot be parsed.
    pub fn new(target: Target) -> Result<Self, ClientError> {
        let bl = target
            .business_logic
            .as_ref()
            .map(|c| build::<BusinessLogic>(&target.base_url, c))
            .transpose()?;
        let ven = target
            .ven
            .as_ref()
            .map(|c| build::<VirtualEndNode>(&target.base_url, c))
            .transpose()?;
        Ok(Self {
            target,
            bl,
            ven,
            litter: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// A client with no credentials, for the endpoints that must work without them.
    fn anonymous(&self) -> Result<Client<BusinessLogic>, ClientError> {
        Client::<BusinessLogic>::builder(&self.target.base_url)?.build()
    }

    /// Run everything, then clean up.
    ///
    /// Cleanup runs whether or not checks failed, because a suite that leaves a VTN full of
    /// `oadr-conformance-*` programmes after one bad run is a suite nobody runs twice.
    pub async fn run(&self) -> Report {
        let mut report = Report {
            target: self.target.base_url.clone(),
            findings: Vec::new(),
        };
        for check in checks() {
            let outcome = match self.precondition(check) {
                Some(reason) => Outcome::Skipped(reason),
                None => suite::run_one(self, check).await,
            };
            report.findings.push(Finding {
                check: *check,
                outcome,
            });
        }
        self.cleanup().await;
        report
    }

    /// Why a check cannot run, if it cannot.
    fn precondition(&self, check: &Check) -> Option<String> {
        match check.needs {
            Needs::Nothing => None,
            Needs::BusinessLogic if self.bl.is_none() => {
                Some("no business-logic credential was supplied".into())
            }
            Needs::BothRoles if self.bl.is_none() => {
                Some("no business-logic credential was supplied".into())
            }
            Needs::BothRoles if self.ven.is_none() => Some("no VEN credential was supplied".into()),
            Needs::BothRoles if self.target.ven_client_id.is_none() => {
                Some("the VEN's clientID was not supplied".into())
            }
            _ => None,
        }
    }

    /// Delete everything the run created, newest first.
    ///
    /// Best effort, and deliberately quiet: a VTN that refuses a delete has already failed a check
    /// that says so, and a second complaint here would only bury it. Objects that go by cascade are
    /// gone before their own delete is attempted, and a `404` there is success.
    pub async fn cleanup(&self) {
        let litter = {
            let mut guard = self.litter.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *guard)
        };
        let Some(bl) = &self.bl else { return };
        for (collection, id) in litter.into_iter().rev() {
            let _ = bl
                .exchange(
                    "DELETE",
                    &format!("{collection}/{id}"),
                    &crate::client::Query::new(),
                    None,
                    &[],
                )
                .await;
        }
    }

    /// Remember an object for cleanup.
    fn track(&self, collection: &'static str, id: impl Into<String>) {
        self.litter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((collection, id.into()));
    }

    fn business_logic(&self) -> &Client<BusinessLogic> {
        self.bl
            .as_ref()
            .expect("the precondition check guarantees a business-logic client")
    }

    fn virtual_end_node(&self) -> &Client<VirtualEndNode> {
        self.ven
            .as_ref()
            .expect("the precondition check guarantees a VEN client")
    }
}

fn build<R: crate::client::Role>(
    base_url: &str,
    credential: &Credential,
) -> Result<Client<R>, ClientError> {
    let builder = Client::<R>::builder(base_url)?;
    match credential {
        Credential::Token(token) => builder.bearer_token(token),
        Credential::ClientCredentials { id, secret } => {
            builder.credentials(crate::client::Credentials::new(id, secret))
        }
    }
    .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(id: &'static str, severity: Severity, outcome: Outcome) -> Finding {
        Finding {
            check: Check {
                id,
                clause: "[API]",
                title: "t",
                severity,
                needs: Needs::Nothing,
            },
            outcome,
        }
    }

    #[test]
    fn a_skip_is_not_a_pass() {
        // The whole reason there are three outcomes. A suite that counts what it could not attempt
        // as success produces a number nobody can act on.
        let report = Report {
            target: "https://vtn.test".into(),
            findings: vec![
                finding("a", Severity::Required, Outcome::Passed),
                finding(
                    "b",
                    Severity::Required,
                    Outcome::Skipped("no broker".into()),
                ),
            ],
        };
        assert_eq!(report.tally(Severity::Required), (1, 0, 1));
        assert!(
            report.is_conformant(),
            "a skip is not a failure either — it is an absence of evidence"
        );
        assert!(report.to_string().contains("skip"));
    }

    #[test]
    fn a_required_failure_is_the_only_thing_that_makes_a_run_non_conformant() {
        let mut report = Report {
            target: "https://vtn.test".into(),
            findings: vec![finding(
                "ext",
                Severity::Extension,
                Outcome::Failed("no ETag".into()),
            )],
        };
        assert!(
            report.is_conformant(),
            "an extension this VTN happens not to implement is information, not non-conformance"
        );

        report.findings.push(finding(
            "req",
            Severity::Required,
            Outcome::Failed("no".into()),
        ));
        assert!(!report.is_conformant());
        assert_eq!(report.failures().count(), 2);
        assert!(report.to_string().contains("NOT CONFORMANT"));
    }

    #[test]
    fn every_check_is_uniquely_identified_and_cites_a_clause() {
        // The identifier is what a human diffs between two runs, and the clause is what makes a
        // failure arguable. A check that cannot name a sentence is asserting an opinion.
        let mut ids: Vec<&str> = checks().iter().map(|c| c.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "two checks share an identifier");

        for check in checks() {
            assert!(!check.clause.is_empty(), "{} cites nothing", check.id);
            assert!(
                check.severity != Severity::Extension || check.clause.contains("extension"),
                "{} is marked Extension but cites {}",
                check.id,
                check.clause
            );
        }
    }

    #[test]
    fn the_report_renders_as_json_for_a_matrix() {
        let report = Report {
            target: "https://vtn.test".into(),
            findings: vec![finding(
                "a",
                Severity::Required,
                Outcome::Failed("expected 201".into()),
            )],
        };
        let json = report.to_json();
        assert_eq!(json["conformant"], false);
        assert_eq!(json["findings"][0]["outcome"], "failed");
        assert_eq!(json["findings"][0]["detail"], "expected 201");
    }

    #[test]
    fn a_credential_never_prints_its_secret() {
        let rendered = format!(
            "{:?}",
            Credential::ClientCredentials {
                id: "bl-1".into(),
                secret: "hunter2".into(),
            }
        );
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("bl-1"));
        assert!(!format!("{:?}", Credential::Token("t0ken".into())).contains("t0ken"));
    }
}
