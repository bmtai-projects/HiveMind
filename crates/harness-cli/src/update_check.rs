//! A real, best-effort check against the public releases repo's latest
//! GitHub release -- never blocks startup meaningfully and never fails
//! loudly. Backs both `banner::print()`'s passive startup notice and the
//! `hivemind update` command's download -- see `crate::self_update`.

use std::time::Duration;

use serde::Deserialize;

const RELEASES_REPO: &str = "BibhabenduMukherjee/HiveMind-releases";
const CHECK_TIMEOUT: Duration = Duration::from_millis(800);
/// `hivemind update` is an explicit command a user typed and is actively
/// waiting on, not a background startup courtesy -- worth a real timeout
/// rather than the banner check's 800ms, but still bounded.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
}

#[derive(Deserialize)]
pub struct LatestRelease {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

impl LatestRelease {
    /// Version string with any leading `v` stripped, matching this crate's
    /// own `CARGO_PKG_VERSION` shape.
    pub fn version(&self) -> &str {
        self.tag_name.trim_start_matches('v')
    }
}

async fn fetch(timeout: Duration) -> Option<LatestRelease> {
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
        resp.json::<LatestRelease>().await.ok()
    };
    tokio::time::timeout(timeout, check).await.ok().flatten()
}

/// Returns `Some(newer_version)` only if a strictly newer tag exists on the
/// public releases repo, resolved within `CHECK_TIMEOUT`. Any failure --
/// offline, DNS, rate limit, a slow network, an unparsable response --
/// resolves to `None` silently; this is a nice-to-have, never worth
/// surfacing an error (or a startup delay) for.
pub async fn newer_version_available() -> Option<String> {
    let release = fetch(CHECK_TIMEOUT).await?;
    let current = env!("CARGO_PKG_VERSION");
    is_newer(release.version(), current).then(|| release.version().to_string())
}

/// Real fetch for `hivemind update`, with the assets a download actually
/// needs -- the passive check above only ever looks at the tag.
pub async fn fetch_latest_release() -> Option<LatestRelease> {
    fetch(FETCH_TIMEOUT).await
}

/// Plain numeric `major.minor.patch` comparison -- good enough for this
/// project's tags, which are always plain semver with no pre-release
/// suffix (confirmed: every tag from v0.1.0 through v1.3.0 matches this
/// shape). `Vec<u64>`'s lexicographic `PartialOrd` handles multi-digit
/// components correctly (e.g. "0.10.0" > "0.5.0"), unlike a string compare.
pub fn is_newer(latest: &str, current: &str) -> bool {
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

    #[test]
    fn version_strips_a_leading_v() {
        let r = LatestRelease {
            tag_name: "v1.3.0".to_string(),
            assets: vec![],
        };
        assert_eq!(r.version(), "1.3.0");
    }
}
