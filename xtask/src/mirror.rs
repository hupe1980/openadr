//! The local specification mirror, held at the commit `spec.lock` pins.
//!
//! The pinned commit is fetched *by hash* and checked out detached. That works from an empty
//! directory, from a stale checkout and from a history with no common ancestor — the case
//! `git pull --ff-only` could not cross when the mirror was rewritten on 2026-09-30. A mirror with
//! local changes is refused rather than discarded, and every reader goes through [`verify`], so a
//! mirror at any other commit fails every check with the instruction that fixes it.

use std::{collections::BTreeSet, fs, path::Path, process::Command};

use anyhow::{Context, Result, bail};

use crate::pin::{PIN_PATH, SpecPin};

/// Fail unless `dir` is a checkout at exactly the pinned commit (spec-pin/SR-002).
pub fn verify(dir: &Path, pin: &SpecPin) -> Result<()> {
    let head = git_output(dir, &["rev-parse", "HEAD"]).ok();
    if head.as_deref() != Some(pin.commit.as_str()) {
        bail!(
            "the specification mirror at {} is {}, but {PIN_PATH} pins {}.\n\
             Run `cargo xtask spec-sync`.",
            dir.display(),
            head.map_or_else(|| "missing".to_string(), |h| format!("at {}", &h[..12])),
            pin.short()
        );
    }
    Ok(())
}

/// Put `dir` at the pinned commit; with `update`, at the remote's head instead, returning the new
/// pin when it moved. Refuses a mirror with local changes (spec-pin/SR-004).
pub fn sync(dir: &Path, pin: &SpecPin, update: bool) -> Result<Option<SpecPin>> {
    prepare(dir, &pin.url)?;
    if !update {
        checkout(dir, &pin.url, &pin.commit)?;
        return Ok(None);
    }
    git(dir, &["fetch", "--quiet", "--depth", "1", "origin", "HEAD"])?;
    let next = SpecPin::new(
        pin.url.clone(),
        git_output(dir, &["rev-parse", "FETCH_HEAD"])?,
    )?;
    if &next == pin {
        println!(
            "specification {} @ {}: already at head",
            pin.url,
            pin.short()
        );
        checkout(dir, &pin.url, &pin.commit)?;
        return Ok(None);
    }
    // The old commit is needed for the diff. It may be gone upstream; the diff then says so.
    let _ = git(
        dir,
        &["fetch", "--quiet", "--depth", "1", "origin", &pin.commit],
    );
    describe_move(dir, pin, &next);
    checkout(dir, &next.url, &next.commit)?;
    Ok(Some(next))
}

/// Create the repository if absent, refuse it if dirty, and point `origin` at `remote`.
fn prepare(dir: &Path, remote: &str) -> Result<()> {
    if dir.join(".git").exists() {
        let dirty = git_output(dir, &["status", "--porcelain"])?;
        if !dirty.is_empty() {
            bail!(
                "the specification mirror at {} has local changes; nothing was touched:\n{dirty}",
                dir.display()
            );
        }
        git(dir, &["remote", "set-url", "origin", "--", remote])
    } else {
        fs::create_dir_all(dir)?;
        git(dir, &["init", "--quiet"])?;
        git(dir, &["remote", "add", "origin", "--", remote])
    }
}

/// Fetch one commit by hash and check it out detached (spec-pin/SR-001).
fn checkout(dir: &Path, remote: &str, commit: &str) -> Result<()> {
    if git(dir, &["fetch", "--quiet", "--depth", "1", "origin", commit]).is_err() {
        bail!(
            "{remote} no longer has the pinned commit {commit} — its history was rewritten again.\n\
             Run `cargo xtask spec-sync --update` and review what it reports."
        );
    }
    git(dir, &["checkout", "--quiet", "--detach", commit])
}

/// Print what moving the pin changes: the commits, the versions `VERSIONS.yaml` lists, the files.
fn describe_move(dir: &Path, from: &SpecPin, to: &SpecPin) {
    println!(
        "specification {}\n  {} → {}",
        from.url,
        from.short(),
        to.short()
    );
    let versions = |commit: &str| -> BTreeSet<String> {
        git_output(dir, &["show", &format!("{commit}:VERSIONS.yaml")])
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().strip_prefix("- version:"))
            .map(|v| v.trim().trim_matches('"').to_string())
            .collect()
    };
    let (before, after) = (versions(&from.commit), versions(&to.commit));
    for added in after.difference(&before) {
        println!("  + version {added}");
    }
    for dropped in before.difference(&after) {
        println!("  - version {dropped}");
    }
    match git_output(dir, &["diff", "--stat", &from.commit, &to.commit]) {
        Ok(stat) => println!("{stat}"),
        Err(_) => println!("  (the old commit is gone upstream, so no file diff is available)"),
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn git_output(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    //! Against local repositories standing in for the mirror: `checkout` and `prepare` take the
    //! remote as a string, so the tests reach them with a path while the public entry points only
    //! ever see a validated https pin.

    use std::path::PathBuf;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-mirror-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A repository with one commit holding `file`, returning its hash.
    fn upstream(dir: &Path, file: &str) -> String {
        git(dir, &["init", "--quiet"]).unwrap();
        fs::write(dir.join(file), file).unwrap();
        git(dir, &["add", "."]).unwrap();
        git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--quiet",
                "-m",
                file,
            ],
        )
        .unwrap();
        git(dir, &["config", "uploadpack.allowAnySHA1InWant", "true"]).unwrap();
        git_output(dir, &["rev-parse", "HEAD"]).unwrap()
    }

    fn pin(commit: &str) -> SpecPin {
        SpecPin::new("https://example.invalid/mirror".into(), commit.into()).unwrap()
    }

    // spec-pin/SR-001: a mirror on a history with no common ancestor lands on the pin.
    #[test]
    fn a_rewritten_history_is_crossed_by_fetching_the_pinned_commit() {
        let (old, new, local) = (scratch("old"), scratch("new"), scratch("local1"));
        let old_head = upstream(&old, "before.txt");
        let new_head = upstream(&new, "after.txt");
        let (old_url, new_url) = (old.to_string_lossy(), new.to_string_lossy());
        prepare(&local, &old_url).unwrap();
        checkout(&local, &old_url, &old_head).unwrap();
        prepare(&local, &new_url).unwrap();
        checkout(&local, &new_url, &new_head).unwrap();
        verify(&local, &pin(&new_head)).unwrap();
        assert!(local.join("after.txt").exists() && !local.join("before.txt").exists());
    }

    // spec-pin/SR-001: a pin the remote no longer has says which command fixes it.
    #[test]
    fn a_commit_the_remote_lost_points_at_update() {
        let (remote, local) = (scratch("remote"), scratch("local2"));
        upstream(&remote, "a.txt");
        prepare(&local, &remote.to_string_lossy()).unwrap();
        let gone = "0123456789abcdef0123456789abcdef01234567";
        let err = checkout(&local, &remote.to_string_lossy(), gone).unwrap_err();
        assert!(err.to_string().contains("spec-sync --update"), "{err}");
    }

    // spec-pin/SR-002: a mirror at another commit, or none, fails with the instruction.
    #[test]
    fn a_mirror_not_at_the_pin_fails_every_reader() {
        let (remote, missing) = (scratch("remote3"), scratch("missing"));
        let head = upstream(&remote, "a.txt");
        verify(&remote, &pin(&head)).unwrap();
        let other = "0123456789abcdef0123456789abcdef01234567";
        let err = verify(&remote, &pin(other)).unwrap_err().to_string();
        assert!(
            err.contains("Run `cargo xtask spec-sync`") && err.contains(&head[..12]),
            "{err}"
        );
        let err = verify(&missing, &pin(&head)).unwrap_err().to_string();
        assert!(err.contains("is missing"), "{err}");
    }

    // spec-pin/SR-004: local changes are refused and left exactly as they were.
    #[test]
    fn local_changes_are_refused_and_kept() {
        let (remote, local) = (scratch("remote4"), scratch("local4"));
        let head = upstream(&remote, "a.txt");
        let url = remote.to_string_lossy();
        prepare(&local, &url).unwrap();
        checkout(&local, &url, &head).unwrap();
        fs::write(local.join("a.txt"), "edited").unwrap();
        let err = prepare(&local, &url).unwrap_err().to_string();
        assert!(
            err.contains("a.txt") && err.contains("nothing was touched"),
            "{err}"
        );
        assert_eq!(fs::read_to_string(local.join("a.txt")).unwrap(), "edited");
    }
}
