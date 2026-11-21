//! `spec.lock` — which commit of the specification mirror this repository builds against.
//!
//! Two keys, parsed by hand rather than with a TOML dependency, because the parser is the security
//! boundary: both values end up as arguments to `git`, and a commit that begins with a dash is an
//! option rather than a revision. So a [`SpecPin`] cannot be constructed from anything but an
//! `https://` URL and exactly forty lowercase hex digits.

use anyhow::{Result, bail};

/// The file, relative to the repository root.
pub const PIN_PATH: &str = "spec.lock";

/// A validated pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecPin {
    pub url: String,
    pub commit: String,
}

impl SpecPin {
    /// Parse `spec.lock`. Comments (`#`) and blank lines are ignored; anything else must be
    /// `url = "…"` or `commit = "…"`, each exactly once.
    pub fn parse(text: &str) -> Result<Self> {
        let (mut url, mut commit) = (None, None);
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                bail!("{PIN_PATH}:{}: expected `key = \"value\"`", n + 1);
            };
            let value = value.trim();
            let Some(value) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
                bail!(
                    "{PIN_PATH}:{}: the value must be a double-quoted string",
                    n + 1
                );
            };
            let slot = match key.trim() {
                "url" => &mut url,
                "commit" => &mut commit,
                other => bail!("{PIN_PATH}:{}: unknown key `{other}`", n + 1),
            };
            if slot.replace(value.to_string()).is_some() {
                bail!("{PIN_PATH}:{}: `{}` appears twice", n + 1, key.trim());
            }
        }
        let url = url.ok_or_else(|| anyhow::anyhow!("{PIN_PATH}: no `url`"))?;
        let commit = commit.ok_or_else(|| anyhow::anyhow!("{PIN_PATH}: no `commit`"))?;
        Self::new(url, commit)
    }

    /// Validate a URL and commit.
    pub fn new(url: String, commit: String) -> Result<Self> {
        let host = url.strip_prefix("https://").unwrap_or("");
        if host.is_empty()
            || !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._~/:@".contains(c))
        {
            bail!("{PIN_PATH}: `url` must be an https:// URL of plain characters, not `{url}`");
        }
        if commit.len() != 40
            || !commit
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            bail!("{PIN_PATH}: `commit` must be 40 lowercase hex digits, not `{commit}`");
        }
        Ok(Self { url, commit })
    }

    /// The first twelve digits, for messages.
    pub fn short(&self) -> &str {
        &self.commit[..12]
    }

    /// Render the file, keeping the header comment of the one it replaces.
    pub fn render(&self, previous: &str) -> String {
        let mut out: String = previous
            .lines()
            .take_while(|l| l.trim().is_empty() || l.trim_start().starts_with('#'))
            .map(|l| format!("{l}\n"))
            .collect();
        out.push_str(&format!(
            "url = \"{}\"\ncommit = \"{}\"\n",
            self.url, self.commit
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://github.com/grid-coordination/openadr3-specification";
    const SHA: &str = "17b91725e0f07203574ddc946e28def31cb11e44";

    #[test]
    fn a_well_formed_pin_parses_and_renders_back() {
        let text = format!("# header\nurl = \"{URL}\"\ncommit = \"{SHA}\"\n");
        let pin = SpecPin::parse(&text).unwrap();
        assert_eq!(pin.commit, SHA);
        assert_eq!(pin.render(&text), text);
    }

    // spec-pin/SR-003: a value git would read as an option never reaches git.
    #[test]
    fn values_git_would_read_as_options_are_refused() {
        for commit in [
            "--upload-pack=touch /tmp/x",
            "-c",
            &SHA.to_uppercase(),
            &SHA[..39],
        ] {
            assert!(SpecPin::new(URL.into(), commit.into()).is_err(), "{commit}");
        }
        for url in [
            "http://github.com/x",
            "-oProxyCommand=touch /tmp/x",
            "ext::sh -c touch% /tmp/x",
            "file:///etc",
            "https://",
            "https://host/a b",
        ] {
            assert!(SpecPin::new(url.into(), SHA.into()).is_err(), "{url}");
        }
    }

    #[test]
    fn a_duplicate_missing_or_unknown_key_names_its_line() {
        let dup = format!("url = \"{URL}\"\nurl = \"{URL}\"\ncommit = \"{SHA}\"\n");
        assert!(
            SpecPin::parse(&dup)
                .unwrap_err()
                .to_string()
                .contains(":2:")
        );
        assert!(SpecPin::parse(&format!("url = \"{URL}\"\n")).is_err());
        assert!(
            SpecPin::parse("branch = \"main\"\n")
                .unwrap_err()
                .to_string()
                .contains(":1:")
        );
        assert!(SpecPin::parse(&format!("commit = {SHA}\n")).is_err());
    }
}
