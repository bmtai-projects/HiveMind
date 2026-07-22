//! `hivemind auth login|logout|status` — the device-flow client for
//! HiveMind's hosted backend (`HiveMind-server`). Talks HTTP directly
//! (not through `harness-provider`, which is scoped to the model provider's
//! chat-completions SSE dialect, not this JSON control-plane API).
//!
//! Once logged in, `resolve()` in `harness-config` picks up the stored
//! token automatically as the lowest-priority key source — nothing else
//! in the CLI needs to know hosted mode exists.

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
                harness_config::save_hosted_credentials(
                    &harness_config::default_credentials_path(),
                    &creds,
                )?;
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
    let Some(creds) =
        harness_config::load_hosted_credentials(&harness_config::default_credentials_path())
    else {
        println!("Not signed in. Run `hivemind auth login`.");
        return Ok(());
    };

    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/me", creds.api_base))
        .bearer_auth(&creds.access_token)
        .send()
        .await?;

    if !response.status().is_success() {
        println!(
            "Signed in, but the session looks invalid (server returned {}). Run `hivemind auth login` again.",
            response.status()
        );
        return Ok(());
    }

    let me: Me = response.json().await?;
    println!("Signed in as {}", me.email);
    println!("Balance: ${:.6}", me.balance_micros as f64 / 1_000_000.0);
    Ok(())
}
