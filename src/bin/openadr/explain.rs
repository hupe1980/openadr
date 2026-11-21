//! `openadr explain` — what a problem type or a conformance check means, at the terminal.
//!
//! The text is compiled in: the problem table is the one the published registry page is held to by
//! `cargo xtask check-problems`, and the checks are the suite's own list. Nothing is fetched, so it
//! works on the machine where the error happened.

use openadr::model::problem::{PROBLEM_TYPE_BASE, PROBLEMS, problem_type};

/// Print the usage.
pub fn print_help() {
    println!(
        "openadr explain — what a problem type or a conformance check means\n\n\
         USAGE:\n    \
         openadr explain <problem-type | check-id>\n    \
         openadr explain                  list everything that can be explained\n\n\
         EXAMPLES:\n    \
         openadr explain not-found\n    \
         openadr explain ven-cannot-change-a-foreign-ven"
    );
}

/// Run the command. Exit status 2 for a name nothing answers to.
pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let Some(name) = args.first().map(String::as_str) else {
        list();
        return Ok(());
    };
    if name == "--help" {
        print_help();
        return Ok(());
    }
    // A full `type` URI pasted from a problem body is accepted as well as its slug.
    let name = name.strip_prefix(PROBLEM_TYPE_BASE).unwrap_or(name);
    if let Some(problem) = problem_type(name) {
        let status = problem
            .status
            .map_or_else(|| "varies".to_string(), |s| s.to_string());
        println!(
            "{}  ·  HTTP {status}\n\n{}\n",
            problem.slug,
            plain(problem.meaning)
        );
        println!("{PROBLEM_TYPE_BASE}{}", problem.slug);
        return Ok(());
    }
    if let Some(text) = check(name) {
        println!("{text}");
        return Ok(());
    }
    let hint = nearest(name).map_or_else(String::new, |n| format!(" — did you mean `{n}`?"));
    eprintln!("nothing called `{name}`{hint}\nRun `openadr explain` for the list.");
    std::process::exit(2);
}

fn list() {
    println!("Problem types:");
    for p in PROBLEMS {
        let status = p
            .status
            .map_or_else(|| "   ".to_string(), |s| s.to_string());
        println!("  {status}  {}", p.slug);
    }
    #[cfg(feature = "conformance")]
    {
        println!("\nConformance checks:");
        for c in openadr::conformance::checks() {
            println!("  {:<11}  {}", c.severity.to_string(), c.id);
        }
    }
}

#[cfg(feature = "conformance")]
fn check(id: &str) -> Option<String> {
    use openadr::conformance::{Needs, checks};
    let c = checks().iter().find(|c| c.id == id)?;
    let needs = match c.needs {
        Needs::Nothing => "no credentials",
        Needs::BusinessLogic => "a business-logic credential",
        Needs::BothRoles => "a business-logic credential, and a VEN credential with its clientID",
    };
    Some(format!(
        "{}  ·  {}\n\n{}.\n\nTests  {}\nNeeds  {needs}\n\nhttps://hupe1980.github.io/openadr/docs/conformance/",
        c.id, c.severity, c.title, c.clause
    ))
}

#[cfg(not(feature = "conformance"))]
fn check(_: &str) -> Option<String> {
    None
}

/// Every name `explain` answers to.
fn names() -> Vec<&'static str> {
    #[allow(unused_mut)]
    let mut names: Vec<&'static str> = PROBLEMS.iter().map(|p| p.slug).collect();
    #[cfg(feature = "conformance")]
    names.extend(openadr::conformance::checks().iter().map(|c| c.id));
    names
}

/// The closest known name, if any is close enough to be a typo rather than a different word.
fn nearest(name: &str) -> Option<&'static str> {
    names()
        .into_iter()
        .map(|n| (distance(name, n), n))
        .filter(|(d, n)| *d <= 3.max(n.len() / 4))
        .min()
        .map(|(_, n)| n)
}

/// Levenshtein distance over bytes; names here are ASCII.
fn distance(a: &str, b: &str) -> usize {
    let b = b.as_bytes();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.bytes().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let next = (prev + usize::from(ca != cb))
                .min(row[j] + 1)
                .min(row[j + 1] + 1);
            prev = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// The registry's Markdown, readable at a terminal: inline code marks dropped.
fn plain(markdown: &str) -> String {
    markdown.replace('`', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typo_suggests_the_name_it_was_meant_to_be() {
        assert_eq!(nearest("not-fund"), Some("not-found"));
        assert_eq!(nearest("unauthorised"), Some("unauthorized"));
        assert_eq!(nearest("something-else-entirely"), None);
    }

    #[test]
    fn every_problem_type_is_explainable() {
        for p in PROBLEMS {
            assert!(!p.meaning.is_empty(), "{} has no meaning", p.slug);
        }
    }

    #[cfg(feature = "conformance")]
    #[test]
    fn every_check_is_explainable() {
        for c in openadr::conformance::checks() {
            let text = check(c.id).unwrap();
            assert!(text.contains(c.clause), "{}", c.id);
        }
    }
}
