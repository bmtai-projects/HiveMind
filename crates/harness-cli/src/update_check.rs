//! A real, best-effort check against the public releases repo's latest
//! GitHub release -- never blocks startup meaningfully and never fails
//! loudly. `hivemind` has no self-update mechanism (unlike `install.sh`,
//! which is re-invoked by hand), so this only ever prints a hint to
//! reinstall, never attempts to replace itself.

use std::time::Duration;

use serde::Deserialize;

const RELEASES_REPO: &str = "BibhabenduMukherjee/HiveMind-releases";
const CHECK_TIMEOUT: Duration = Duration::from_millis(800);

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
}

/// Returns `Some(newer_version)` only if a strictly newer tag exists on the
/// public releases repo, resolved within `CHECK_TIMEOUT`. Any failure --
/// offline, DNS, rate limit, a slow network, an unparsable response --
/// resolves to `None` silently; this is a nice-to-have, never worth
/// surfacing an error (or a startup delay) for.
pub async fn newer_version_available() -> Option<String> {
    let check = async {
        let client = reqwest::Client::builder().build().ok()?;
        let resp = client
            .get(format!(
                "https://api.github.com/repos/{RELEASES_REPO}/releases/latest"
            ))
            .header("User-Agent", "hivemind-cli")
            .send()
            .await
            .ok()?;
        let release: LatestRelease = resp.json().await.ok()?;
        let latest = release.tag_name.trim_start_matches('v');
        let current = env!("CARGO_PKG_VERSION");
        if is_newer(latest, current) {
            Some(latest.to_string())
        } else {
            None
        }
    };
    tokio::time::timeout(CHECK_TIMEOUT, check)
        .await
        .ok()
        .flatten()
}

/// Plain numeric `major.minor.patch` comparison -- good enough for this
/// project's tags, which are always plain semver with no pre-release
/// suffix (confirmed: every tag from v0.1.0 through v0.5.0 matches this
/// shape). `Vec<u64>`'s lexicographic `PartialOrd` handles multi-digit
/// components correctly (e.g. "0.10.0" > "0.5.0"), unlike a string compare.
fn is_newer(latest: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split('.').filter_map(|p| p.parse().ok()).collect()
    }
    parts(latest) > parts(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_patch_and_minor_versions_are_detected() {
        assert!(is_newer("0.5.1", "0.5.0"));
        assert!(is_newer("0.6.0", "0.5.0"));
        assert!(!is_newer("0.5.0", "0.5.0"));
        assert!(!is_newer("0.4.9", "0.5.0"));
    }

    #[test]
    fn double_digit_components_compare_numerically_not_lexically() {
        assert!(is_newer("0.10.0", "0.5.0"));
        assert!(!is_newer("0.5.0", "0.10.0"));
    }
}
