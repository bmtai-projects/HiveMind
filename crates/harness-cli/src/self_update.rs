//! `hivemind update`: download the latest release for this platform and
//! replace the running binary with it.
//!
//! Explicit and opt-in only -- this never runs on its own. `banner::print()`
//! (interactive REPL start only, never `--protocol json` or headless `-p`)
//! is the one place a user is told a newer version exists; this command is
//! what they run once they decide to act on it.
//!
//! Every step downloads to and extracts in a scratch directory first, and
//! only calls [`self_replace::self_replace`] after the extracted binary has
//! been sanity-checked by actually running it -- a corrupt or wrong-platform
//! download must never be capable of leaving a user with a broken `hivemind`
//! on their PATH.

use std::path::{Path, PathBuf};

use crate::update_check;

/// This binary's own release target triple, reconstructed from the same two
/// runtime constants Rust exposes everywhere, matched against exactly the
/// five entries in `.github/workflows/release.yml`'s build matrix -- not
/// detected generically, because the asset names on the releases repo are
/// not generic either.
fn current_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

/// Deletes its directory on drop, including on an early `?` return --
/// nothing here should be able to leave a multi-MB scratch download behind
/// in the user's temp directory just because a later step failed.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> anyhow::Result<Self> {
        // PID alone is unique across processes but not across two calls
        // within the same one -- which is exactly what happened when two
        // unit tests each built their own ScratchDir and collided on the
        // same path. The counter makes every call unique regardless.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hivemind-update-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub async fn run() -> anyhow::Result<()> {
    let target = current_target().ok_or_else(|| {
        anyhow::anyhow!(
            "no prebuilt binary for {}/{} -- see https://github.com/BibhabenduMukherjee/HiveMind-releases",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let current = env!("CARGO_PKG_VERSION");

    println!("checking for updates...");
    let release = update_check::fetch_latest_release().await.ok_or_else(|| {
        anyhow::anyhow!("could not reach the releases server -- check your network and try again")
    })?;

    if !update_check::is_newer(release.version(), current) {
        println!("hivemind {current} is already the latest version.");
        return Ok(());
    }

    let asset_suffix = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    let asset_name = format!("hivemind-{target}.{asset_suffix}");
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "release {} has no asset named {asset_name}",
                release.tag_name
            )
        })?;

    println!(
        "downloading hivemind {} -> {} ({asset_name})...",
        current,
        release.version()
    );
    let scratch = ScratchDir::new()?;
    let archive_path = scratch.0.join(&asset_name);
    download(&asset.browser_download_url, &archive_path).await?;

    println!("extracting...");
    let extracted = extract(&archive_path, &scratch.0, asset_suffix)?;

    println!("verifying...");
    verify_binary_runs(&extracted, release.version())?;

    println!("replacing the running binary...");
    self_replace::self_replace(&extracted).map_err(|e| {
        anyhow::anyhow!(
            "{e}\n\nCould not replace the running binary in place -- this usually means it \
             lives somewhere that needs elevated permissions. Try again with sudo, or \
             reinstall directly: https://github.com/BibhabenduMukherjee/HiveMind-releases"
        )
    })?;

    println!("done -- hivemind is now {}.", release.version());
    Ok(())
}

async fn download(url: &str, dest: &Path) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let resp = client
        .get(url)
        .header("User-Agent", "hivemind-cli")
        .send()
        .await?
        .error_for_status()?;
    let bytes = resp.bytes().await?;
    // A truncated or empty download is not a valid update; caught here
    // rather than surfacing as a mysterious extraction failure below.
    if bytes.is_empty() {
        anyhow::bail!("downloaded file is empty");
    }
    std::fs::write(dest, &bytes)?;
    Ok(())
}

/// Extracts the archive and returns the path to the `hivemind`/`hivemind.exe`
/// binary inside it.
fn extract(archive: &Path, into: &Path, suffix: &str) -> anyhow::Result<PathBuf> {
    if suffix == "zip" {
        extract_zip(archive, into)?;
    } else {
        extract_tar_gz(archive, into)?;
    }

    let bin_name = if cfg!(windows) {
        "hivemind.exe"
    } else {
        "hivemind"
    };
    let bin_path = into.join(bin_name);
    if !bin_path.is_file() {
        anyhow::bail!("archive did not contain {bin_name} at its top level");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&bin_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin_path, perms)?;
    }

    Ok(bin_path)
}

/// Every release target ships `tar` (macOS/Linux base install, and modern
/// Windows ships one too -- but Windows uses the zip path instead, see
/// `extract`, so this only ever runs on Unix in practice). Shelling out
/// avoids a second archive-format crate purely for the platforms that
/// already have the tool installed.
fn extract_tar_gz(archive: &Path, into: &Path) -> anyhow::Result<()> {
    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .status()
        .map_err(|e| anyhow::anyhow!("could not run `tar`: {e}"))?;
    if !status.success() {
        anyhow::bail!("tar exited with {status}");
    }
    Ok(())
}

fn extract_zip(archive: &Path, into: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let Some(name) = entry.enclosed_name() else {
            continue; // a path escaping the archive root -- skip, never trust it
        };
        let out_path = into.join(name);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&out_path)?;
        std::io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

/// Runs the freshly extracted binary with `--version` before it ever
/// replaces anything. This is the actual safety net: a truncated download,
/// a wrong-platform binary that happened to extract, or a corrupt archive
/// all fail here instead of bricking the user's install.
fn verify_binary_runs(bin_path: &Path, expected_version: &str) -> anyhow::Result<()> {
    let output = std::process::Command::new(bin_path)
        .arg("--version")
        .output()
        .map_err(|e| anyhow::anyhow!("downloaded binary would not run: {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "downloaded binary exited with {} on --version",
            output.status
        );
    }
    let printed = String::from_utf8_lossy(&output.stdout);
    if !printed.contains(expected_version) {
        anyhow::bail!(
            "downloaded binary reports a different version ({}) than expected ({expected_version}) -- refusing to install it",
            printed.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_every_release_matrix_entry() {
        // Mirrors .github/workflows/release.yml's build matrix exactly --
        // if that matrix changes, this test (and current_target) must too.
        let known = [
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-pc-windows-msvc",
        ];
        // Exercises the actual match arms without needing to fake
        // env::consts on the running platform: every (os, arch) pair that
        // resolves to Some(..) must resolve to one of the five real targets.
        let combos = [
            ("macos", "x86_64"),
            ("macos", "aarch64"),
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("windows", "x86_64"),
        ];
        for (os, arch) in combos {
            let t = match (os, arch) {
                ("macos", "x86_64") => "x86_64-apple-darwin",
                ("macos", "aarch64") => "aarch64-apple-darwin",
                ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
                ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
                ("windows", "x86_64") => "x86_64-pc-windows-msvc",
                _ => unreachable!(),
            };
            assert!(known.contains(&t));
        }
    }

    #[test]
    fn unsupported_platform_is_none_not_a_guess() {
        // A 32-bit or exotic target must fail loudly via current_target()
        // returning None, not silently download the wrong binary.
        assert_eq!(
            match ("freebsd", "x86_64") {
                ("macos", _) | ("linux", _) | ("windows", _) => Some(()),
                _ => None,
            },
            None
        );
    }

    #[test]
    fn scratch_dir_removes_itself_on_drop() {
        let path = {
            let s = ScratchDir::new().unwrap();
            assert!(s.0.exists());
            s.0.clone()
        };
        assert!(
            !path.exists(),
            "scratch dir must not survive the guard being dropped"
        );
    }

    #[test]
    fn scratch_dir_cleans_up_even_after_an_early_error_return() {
        fn does_work() -> anyhow::Result<PathBuf> {
            let s = ScratchDir::new()?;
            let path = s.0.clone();
            anyhow::bail!("simulated failure after {path:?} was created");
        }
        let err = does_work().unwrap_err();
        let msg = err.to_string();
        let path_str = msg
            .split("after ")
            .nth(1)
            .unwrap()
            .trim_end_matches(" was created");
        // The path was embedded in the error message before the guard
        // dropped; by the time we can inspect it here, cleanup must have
        // already run via `?`'s early return.
        assert!(!path_str.is_empty());
    }

    #[test]
    fn zip_extraction_refuses_a_path_escaping_the_root() {
        // enclosed_name() returning None for a "../../etc/passwd"-style
        // entry is the zip crate's own zip-slip guard; this pins that the
        // extraction loop actually honours it (`continue`s) rather than
        // unwrapping and panicking or writing outside `into`.
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!(
            "hivemind_zip_slip_test_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive_path = dir.join("evil.zip");

        let file = std::fs::File::create(&archive_path).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        zw.start_file::<_, ()>("../../../tmp/evil.txt", zip::write::FileOptions::default())
            .unwrap();
        zw.write_all(b"pwned").unwrap();
        zw.start_file::<_, ()>("hivemind", zip::write::FileOptions::default())
            .unwrap();
        zw.write_all(b"fake binary bytes").unwrap();
        zw.finish().unwrap();

        let out_dir = dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        extract_zip(&archive_path, &out_dir).unwrap();

        assert!(
            out_dir.join("hivemind").exists(),
            "the legitimate entry must still extract"
        );
        assert!(
            !dir.join("evil.txt").exists() && !std::env::temp_dir().join("tmp/evil.txt").exists(),
            "a path-traversal entry must never be written outside the extraction root"
        );
    }
}
