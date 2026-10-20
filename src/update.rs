//! Checks GitHub for a newer release when the app starts. It only tells the
//! user: the new version is downloaded from its release page.

use std::time::Duration;

use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

const LATEST: &str = "https://api.github.com/repos/25Dark25/silk/releases/latest";
const RELEASE_PAGE: &str = "https://github.com/25Dark25/silk/releases/tag/";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    /// Such as "0.2.0".
    pub version: String,
    pub url: String,
}

/// The latest release if it is newer than this build; `None` when it is not,
/// or when the check fails (offline, rate-limited…), which is not worth
/// bothering the user about.
pub fn check() -> Option<Update> {
    newer_than(env!("CARGO_PKG_VERSION"), &fetch(LATEST).ok()?)
}

fn fetch(url: &str) -> Result<String, ureq::Error> {
    // Windows' own TLS stack and certificate store.
    let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build();
    let agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(Duration::from_secs(10)))
        .user_agent(concat!("Silk/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();
    agent
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .call()?
        .body_mut()
        .with_config()
        .limit(1 << 20)
        .read_to_string()
}

/// The release described by `json` (GitHub's "latest release" response) if
/// its tag is a plain `vMAJOR.MINOR.PATCH` newer than `current`.
fn newer_than(current: &str, json: &str) -> Option<Update> {
    let tag = json_string(json, "tag_name")?;
    let version = tag.strip_prefix('v')?;
    (parse_version(version)? > parse_version(current)?)
        // The tag is digits and dots only, so it is safe to put in the URL.
        .then(|| Update { version: version.to_owned(), url: format!("{RELEASE_PAGE}{tag}") })
}

/// A top-level string field of a JSON object, assuming the value has no
/// escapes (enough for a tag name, which is validated afterwards).
fn json_string<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let start = json.find(&format!("\"{key}\""))? + key.len() + 2;
    let value = json[start..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    Some(&value[..value.find('"')?])
}

fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.').map(|p| p.parse::<u64>().ok());
    let parsed = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESPONSE: &str = r#"{
  "url": "https://api.github.com/repos/25Dark25/silk/releases/1",
  "tag_name": "v0.2.0",
  "name": "Silk 0.2.0",
  "body": "Mentions \"tag_name\": \"v9.9.9\" in the notes"
}"#;

    #[test]
    fn finds_a_newer_release() {
        let update = newer_than("0.1.0", RESPONSE).unwrap();
        assert_eq!(update.version, "0.2.0");
        assert_eq!(update.url, "https://github.com/25Dark25/silk/releases/tag/v0.2.0");
    }

    #[test]
    fn ignores_the_same_or_an_older_release() {
        assert_eq!(newer_than("0.2.0", RESPONSE), None);
        assert_eq!(newer_than("0.10.0", RESPONSE), None, "versions compare numerically");
    }

    /// Needs the network: `cargo test --release live_github -- --ignored`.
    #[test]
    #[ignore]
    fn live_github() {
        // A repository with releases proves the TLS setup and the parsing.
        let json = fetch("https://api.github.com/repos/rust-lang/rust/releases/latest").unwrap();
        assert!(json_string(&json, "tag_name").is_some_and(|t| parse_version(t).is_some()), "{json:.300}");
        // Silk's own check must not fail loudly, whatever GitHub answers.
        println!("Silk's check: {:?}", check());
    }

    #[test]
    fn ignores_unexpected_tags() {
        for tag in ["0.2.0", "v0.2", "v0.2.0-beta", "v0.2.0/../x", "v0.2.0?x=1", ""] {
            let json = format!(r#"{{"tag_name": "{tag}"}}"#);
            assert_eq!(newer_than("0.1.0", &json), None, "{tag}");
        }
        assert_eq!(newer_than("0.1.0", r#"{"message": "API rate limit exceeded"}"#), None);
        assert_eq!(newer_than("0.1.0", "not json"), None);
    }
}
