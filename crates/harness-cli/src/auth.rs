//! `hivemind auth login|logout|status` — the device-flow client for
//! HiveMind's hosted backend (`HiveMind-server`). Talks HTTP directly
//! (not through `harness-provider`, which is scoped to the model provider's
//! chat-completions SSE dialect, not this JSON control-plane API).
//!
//! Once logged in, `resolve()` in `harness-config` picks up the stored
//! token automatically as the lowest-priority key source — nothing else
//! in the CLI needs to know hosted mode exists.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use harness_config::HostedCredentials;

/// The production HiveMind-server deployment. Overridable via
/// `--api-base`, mainly for pointing at a local/staging backend.
const DEFAULT_HOSTED_API_BASE: &str = "https://hivemind-server-549623498287.asia-south1.run.app/v1";

#[derive(Deserialize)]
struct DeviceStart {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum PollResult {
    Pending,
    Expired,
    Approved {
        access_token: String,
        api_base: String,
    },
}

#[derive(Deserialize)]
struct Me {
    email: String,
    balance_micros: i64,
}

pub async fn login(api_base_override: Option<String>) -> anyhow::Result<()> {
    let api_base = api_base_override.unwrap_or_else(|| DEFAULT_HOSTED_API_BASE.to_string());
    login_with(&api_base, &harness_config::default_credentials_path(), true).await
}

/// [`login`] against an explicit backend and credentials file. Split out so
/// tests can drive the whole device flow without touching the user's real
/// credentials or launching a browser.
async fn login_with(
    api_base: &str,
    credentials_path: &Path,
    open_browser: bool,
) -> anyhow::Result<()> {
    let client = reqwest::Client::new();

    let start: DeviceStart = client
        .post(format!("{api_base}/auth/device/start"))
        .json(&serde_json::json!({}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    println!("First, open this URL to sign in:\n");
    println!("    {}\n", start.verification_uri_complete);
    println!(
        "(or go to {} and enter the code {})",
        start.verification_uri, start.user_code
    );

    // Best-effort: open it automatically, the way `gh auth login` does.
    // Headless/SSH/container environments have no browser to open (no
    // $DISPLAY, no xdg-open, etc.) -- that's expected there, not a login
    // failure, so a failure here only degrades to the manual URL already
    // printed above rather than aborting.
    if open_browser {
        match open::that(&start.verification_uri_complete) {
            Ok(()) => println!("Opening in your browser..."),
            Err(e) => {
                println!("(couldn't open a browser automatically: {e} — open the URL above manually)")
            }
        }
    }

    println!("\nWaiting for approval...");

    let deadline = std::time::Instant::now() + Duration::from_secs(start.expires_in);
    loop {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("login code expired — run `hivemind auth login` again");
        }
        tokio::time::sleep(Duration::from_secs(start.interval)).await;

        let poll: PollResult = client
            .post(format!("{api_base}/auth/device/poll"))
            .json(&serde_json::json!({ "device_code": start.device_code }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        match poll {
            PollResult::Pending => continue,
            PollResult::Expired => {
                anyhow::bail!("login code expired — run `hivemind auth login` again")
            }
            PollResult::Approved {
                access_token,
                api_base,
            } => {
                let creds = HostedCredentials {
                    api_base,
                    access_token,
                };
                harness_config::save_hosted_credentials(credentials_path, &creds)?;
                println!("\nSigned in. `hivemind activate` will use your hosted balance now.");
                return Ok(());
            }
        }
    }
}

pub async fn logout() -> anyhow::Result<()> {
    let removed =
        harness_config::delete_hosted_credentials(&harness_config::default_credentials_path())?;
    println!(
        "{}",
        if removed {
            "Signed out."
        } else {
            "Not signed in."
        }
    );
    Ok(())
}

pub async fn status() -> anyhow::Result<()> {
    let path = harness_config::default_credentials_path();
    println!("{}", status_message(&path).await?);
    Ok(())
}

/// What `hivemind auth status` prints for the credentials at `path`.
async fn status_message(path: &Path) -> anyhow::Result<String> {
    let Some(creds) = harness_config::load_hosted_credentials(path) else {
        return Ok("Not signed in. Run `hivemind auth login`.".to_string());
    };

    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/me", creds.api_base))
        .bearer_auth(&creds.access_token)
        .send()
        .await?;

    if !response.status().is_success() {
        return Ok(format!(
            "Signed in, but the session looks invalid (server returned {}). Run `hivemind auth login` again.",
            response.status()
        ));
    }

    let me: Me = response.json().await?;
    Ok(format!(
        "Signed in as {}\nBalance: ${:.6}",
        me.email,
        me.balance_micros as f64 / 1_000_000.0
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    type Seen = Arc<Mutex<Vec<String>>>;

    /// Serves canned JSON responses from `route(path, nth_hit_on_that_path)`
    /// -> `(status, body)`, and records each raw request (head + body) so a
    /// test can assert on what the client actually sent.
    async fn serve<F>(route: F) -> (String, Seen)
    where
        F: Fn(&str, usize) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Seen = Arc::default();
        let seen_task = seen.clone();
        tokio::spawn(async move {
            let mut hits: HashMap<String, usize> = HashMap::new();
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut raw = Vec::new();
                let mut scratch = [0u8; 4096];
                let header_end = loop {
                    let n = sock.read(&mut scratch).await.unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    raw.extend_from_slice(&scratch[..n]);
                    if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(i + 4);
                    }
                };
                let Some(header_end) = header_end else {
                    continue;
                };
                let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        if k.eq_ignore_ascii_case("content-length") {
                            v.trim().parse().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                while raw.len() < header_end + len {
                    let n = sock.read(&mut scratch).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&scratch[..n]);
                }
                let body = String::from_utf8_lossy(&raw[header_end..]).to_string();
                seen_task.lock().unwrap().push(format!("{head}{body}"));

                let path = head.split(' ').nth(1).unwrap_or_default().to_string();
                let hit = hits.entry(path.clone()).or_insert(0);
                let (status, body) = route(&path, *hit);
                *hit += 1;
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}/v1"), seen)
    }

    /// A credentials path in a fresh scratch dir, removed on drop.
    struct ScratchCreds(std::path::PathBuf);

    impl ScratchCreds {
        fn new(tag: &str) -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            Self(
                std::env::temp_dir()
                    .join(format!("hivemind-auth-{tag}-{}-{n}", std::process::id()))
                    .join("credentials.toml"),
            )
        }

        fn save(&self, api_base: &str, token: &str) {
            harness_config::save_hosted_credentials(
                &self.0,
                &HostedCredentials {
                    api_base: api_base.to_string(),
                    access_token: token.to_string(),
                },
            )
            .unwrap();
        }
    }

    impl Drop for ScratchCreds {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    fn device_start(expires_in: u64) -> String {
        format!(
            r#"{{"device_code":"dev-123","user_code":"ABCD-EFGH","verification_uri":"https://x/device","verification_uri_complete":"https://x/device?c=ABCD","expires_in":{expires_in},"interval":0}}"#
        )
    }

    const APPROVED: &str = r#"{"status":"approved","access_token":"tok-xyz","api_base":"https://api.example/v1"}"#;

    #[tokio::test]
    async fn login_polls_until_approved_then_saves_the_returned_credentials() {
        let (base, seen) = serve(|path, nth| match (path, nth) {
            ("/v1/auth/device/start", _) => (200, device_start(60)),
            ("/v1/auth/device/poll", 0 | 1) => (200, r#"{"status":"pending"}"#.into()),
            ("/v1/auth/device/poll", _) => (200, APPROVED.into()),
            _ => (404, "{}".into()),
        })
        .await;
        let creds_path = ScratchCreds::new("approved");

        login_with(&base, &creds_path.0, false).await.unwrap();

        let creds = harness_config::load_hosted_credentials(&creds_path.0).expect("saved");
        assert_eq!(creds.access_token, "tok-xyz");
        // The server-issued base wins over the one used to log in.
        assert_eq!(creds.api_base, "https://api.example/v1");

        let seen = seen.lock().unwrap();
        let polls: Vec<_> = seen.iter().filter(|r| r.contains("/auth/device/poll")).collect();
        assert_eq!(polls.len(), 3, "two pending polls, then the approving one");
        assert!(polls.iter().all(|r| r.contains(r#""device_code":"dev-123""#)));
    }

    #[tokio::test]
    async fn login_fails_and_saves_nothing_when_the_server_says_expired() {
        let (base, _) = serve(|path, _| match path {
            "/v1/auth/device/start" => (200, device_start(60)),
            _ => (200, r#"{"status":"expired"}"#.into()),
        })
        .await;
        let creds_path = ScratchCreds::new("expired");

        let err = login_with(&base, &creds_path.0, false).await.unwrap_err();
        assert!(err.to_string().contains("expired"), "{err}");
        assert!(!creds_path.0.exists());
    }

    #[tokio::test]
    async fn login_gives_up_once_the_code_lifetime_has_passed() {
        // expires_in = 0: the deadline has already passed at the first check,
        // so no poll is made even though the server would say "pending"
        // forever.
        let (base, seen) = serve(|path, _| match path {
            "/v1/auth/device/start" => (200, device_start(0)),
            _ => (200, r#"{"status":"pending"}"#.into()),
        })
        .await;
        let creds_path = ScratchCreds::new("deadline");

        let err = login_with(&base, &creds_path.0, false).await.unwrap_err();
        assert!(err.to_string().contains("expired"), "{err}");
        assert!(!seen.lock().unwrap().iter().any(|r| r.contains("/poll")));
        assert!(!creds_path.0.exists());
    }

    #[tokio::test]
    async fn login_surfaces_a_failed_device_start() {
        let (base, _) = serve(|_, _| (500, "{}".into())).await;
        let creds_path = ScratchCreds::new("start-500");

        let err = login_with(&base, &creds_path.0, false).await.unwrap_err();
        assert!(err.to_string().contains("500"), "{err}");
        assert!(!creds_path.0.exists());
    }

    #[tokio::test]
    async fn status_without_credentials_says_not_signed_in() {
        let creds_path = ScratchCreds::new("none");
        let msg = status_message(&creds_path.0).await.unwrap();
        assert!(msg.starts_with("Not signed in"), "{msg}");
    }

    #[tokio::test]
    async fn status_reports_email_and_balance_using_the_stored_token() {
        let (base, seen) = serve(|path, _| match path {
            "/v1/me" => (200, r#"{"email":"a@b.dev","balance_micros":1234567}"#.into()),
            _ => (404, "{}".into()),
        })
        .await;
        let creds_path = ScratchCreds::new("me");
        creds_path.save(&base, "tok-abc");

        let msg = status_message(&creds_path.0).await.unwrap();
        assert_eq!(msg, "Signed in as a@b.dev\nBalance: $1.234567");
        let request = seen.lock().unwrap()[0].to_ascii_lowercase();
        assert!(request.contains("authorization: bearer tok-abc"), "{request}");
    }

    #[tokio::test]
    async fn status_flags_a_rejected_session_instead_of_erroring() {
        let (base, _) = serve(|_, _| (401, "{}".into())).await;
        let creds_path = ScratchCreds::new("401");
        creds_path.save(&base, "stale");

        let msg = status_message(&creds_path.0).await.unwrap();
        assert!(msg.contains("session looks invalid"), "{msg}");
        assert!(msg.contains("401"), "{msg}");
    }
}
