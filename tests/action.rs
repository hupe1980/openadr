//! The GitHub Action runs in other people's workflows, with their secrets.
//!
//! An input interpolated into a `run:` block with `${{ inputs.x }}` is pasted into the script
//! before the shell sees it, so an input containing `"; curl … | sh; "` becomes code. The action
//! passes every input through `env:` instead, and this holds it
//! (conformance-report/SR-003).

#[test]
fn no_expression_is_interpolated_into_a_run_block() {
    let action = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/action.yml"))
        .expect("action.yml at the repository root");
    let mut in_run: Option<usize> = None;
    let mut blocks = 0;
    let mut offending = Vec::new();
    for (n, line) in action.lines().enumerate() {
        let indent = line.len() - line.trim_start().len();
        if let Some(run_indent) = in_run {
            if !line.trim().is_empty() && indent <= run_indent {
                in_run = None;
            } else if line.contains("${{") {
                offending.push(format!("action.yml:{}: {}", n + 1, line.trim()));
            }
        }
        if line.trim_start().starts_with("run: |") {
            in_run = Some(indent);
            blocks += 1;
        } else if line.trim_start().starts_with("run: ") && line.contains("${{") {
            offending.push(format!("action.yml:{}: {}", n + 1, line.trim()));
        }
    }
    assert!(
        blocks >= 2,
        "found {blocks} run blocks; the scan is not reading the file"
    );
    assert!(
        offending.is_empty(),
        "expressions inside run blocks:\n{}",
        offending.join("\n")
    );
}

/// The `run:` block of the step with this `name:`, dedented.
fn step_script(action: &str, step: &str) -> String {
    let start = action
        .find(&format!("- name: {step}"))
        .expect("the step exists");
    let rest = &action[start..];
    let run = rest.find("run: |").expect("the step has a run block") + "run: |".len();
    let body = &rest[run..];
    let lines: Vec<&str> = body.lines().skip(1).collect();
    let indent = lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .unwrap();
    lines
        .iter()
        .take_while(|l| l.trim().is_empty() || l.len() - l.trim_start().len() >= indent)
        .map(|l| l.get(indent..).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A downloaded archive that does not match the release's checksum is never extracted, let alone
/// run (conformance-report/SR-002). The install script is run for real, with a stand-in `gh` that
/// serves a tampered archive.
#[cfg(unix)]
#[test]
fn a_tampered_archive_is_refused_before_it_is_extracted() {
    use std::os::unix::fs::PermissionsExt as _;

    let action =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/action.yml")).unwrap();
    let script = step_script(&action, "Install openadr");
    let dir = std::env::temp_dir().join(format!("openadr-action-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();

    // `gh release download … --dir D --pattern A --pattern A.sha256`: write an archive and a
    // checksum file that does not describe it.
    let gh = r#"#!/usr/bin/env bash
set -e
dir=; patterns=()
while [ $# -gt 0 ]; do
  case "$1" in --dir) dir="$2"; shift 2;; --pattern) patterns+=("$2"); shift 2;; *) shift;; esac
done
archive="${patterns[0]}"
mkdir -p "$dir/payload" && echo 'echo pwned' > "$dir/payload/openadr" && chmod +x "$dir/payload/openadr"
tar -czf "$dir/$archive" -C "$dir/payload" openadr && rm -rf "$dir/payload"
printf '%064d  %s\n' 0 "$archive" > "$dir/$archive.sha256"
"#;
    let gh_path = bin.join("gh");
    std::fs::write(&gh_path, gh).unwrap();
    std::fs::set_permissions(&gh_path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (os, arch) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", _) => ("macOS", "ARM64"),
        (_, "aarch64") => ("Linux", "ARM64"),
        _ => ("Linux", "X64"),
    };
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let output = std::process::Command::new("bash")
        .arg("-c")
        .arg(&script)
        .env("PATH", path)
        .env("VERSION", "v0.0.0")
        .env("RUNNER_OS_NAME", os)
        .env("RUNNER_ARCH_NAME", arch)
        .env("RUNNER_TEMP", &dir)
        .env("GITHUB_PATH", dir.join("github_path"))
        .output()
        .unwrap();
    let extracted = dir.join("openadr").join("openadr");
    assert!(
        !output.status.success(),
        "the script accepted a tampered archive"
    );
    assert!(!extracted.exists(), "the tampered archive was extracted");
    assert!(
        !dir.join("github_path").exists(),
        "the binary was put on PATH"
    );
}

/// The interop job runs somebody else's container image on its runner, so it holds nothing worth
/// taking: read-only contents, and no secrets (interop-schedule/SR-001).
#[test]
fn the_interop_job_is_read_only_and_holds_no_secrets() {
    let workflow = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/.github/workflows/interop.yml"
    ))
    .expect("the interop workflow");
    assert!(
        workflow.contains("permissions:\n  contents: read\n"),
        "top-level permissions must be contents: read"
    );
    assert!(
        !workflow.contains("secrets."),
        "the interop job must reference no secrets"
    );
    assert!(
        !workflow.contains("write"),
        "the interop job must request no write permission"
    );
}
