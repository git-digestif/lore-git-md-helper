//! Access tokens for Azure AI Foundry and Azure OpenAI from the Azure CLI,
//! for deployments that do not accept API keys.

use std::io::ErrorKind;
use std::process::{Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::Mutex;

/// The audience of the tokens that Azure AI Foundry and Azure OpenAI accept.
const RESOURCE: &str = "https://cognitiveservices.azure.com";

/// A token is replaced when less than this is left, so that a request that
/// is retried for a few minutes does not outlive it.
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// How long a token is kept when the Azure CLI does not say when it expires.
const UNKNOWN_EXPIRY_LIFETIME: Duration = Duration::from_secs(5 * 60);

const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// Runs a command that prints an Azure CLI access token as JSON, and keeps
/// the token until shortly before it expires.
pub struct CliToken {
    /// The first of these that can be started is the command to run.
    programs: Vec<String>,
    args: Vec<String>,
    cached: Mutex<Option<Cached>>,
}

struct Cached {
    token: String,
    valid_until: Instant,
}

#[derive(Deserialize)]
struct Reply {
    #[serde(rename = "accessToken")]
    access_token: String,
    /// Seconds since the epoch.  Without it, a token is kept for a short
    /// time only.
    expires_on: Option<u64>,
}

impl CliToken {
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self::with_programs([program], args)
    }

    /// Like `new`, for several programs: the first one that can be started
    /// is run, and the others are for when it cannot.
    pub fn with_programs(
        programs: impl IntoIterator<Item = impl Into<String>>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            programs: programs.into_iter().map(Into::into).collect(),
            args: args.into_iter().map(Into::into).collect(),
            cached: Mutex::new(None),
        }
    }

    /// Ask the Windows Azure CLI, through `powershell.exe`.  On Windows,
    /// `az` is a batch script that starts Python, which only a shell
    /// resolves, and from WSL `powershell.exe` is the program to start.
    pub fn via_powershell() -> Self {
        Self::new(
            "powershell.exe",
            [
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("az account get-access-token --resource {RESOURCE} --output json"),
            ],
        )
    }

    /// A token that is valid for at least the refresh margin, asking the
    /// command for a new one when needed.
    pub async fn get(&self) -> Result<String> {
        let mut slot = self.cached.lock().await;
        if let Some(cached) = slot.as_ref()
            && cached.valid_until > Instant::now()
        {
            return Ok(cached.token.clone());
        }
        let fresh = self.fetch().await?;
        let token = fresh.token.clone();
        *slot = Some(fresh);
        Ok(token)
    }

    /// Drop the token on hand, so that the next `get` asks the command for
    /// a new one.  For a token that the server refused although it should
    /// still be valid, for example after the machine slept.
    pub async fn forget(&self) {
        *self.cached.lock().await = None;
    }

    async fn fetch(&self) -> Result<Cached> {
        let (program, output) = self.run().await?;
        // Only standard error goes into an error message: standard output
        // holds the token.
        anyhow::ensure!(
            output.status.success(),
            "{program} {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        );
        let reply: Reply = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("{program} did not print an access token as JSON"))?;

        let lifetime = match reply.expires_on {
            Some(expires_on) => (UNIX_EPOCH + Duration::from_secs(expires_on))
                .duration_since(SystemTime::now())
                .unwrap_or_default()
                .saturating_sub(REFRESH_MARGIN),
            None => UNKNOWN_EXPIRY_LIFETIME,
        };
        Ok(Cached {
            token: reply.access_token,
            valid_until: Instant::now() + lifetime,
        })
    }

    /// Run the first program that can be started, and return it with what
    /// it printed.  A program that starts but fails is not skipped.
    async fn run(&self) -> Result<(&str, Output)> {
        let mut unstartable = std::io::Error::from(ErrorKind::NotFound);
        for program in &self.programs {
            let started = tokio::time::timeout(
                COMMAND_TIMEOUT,
                Command::new(program)
                    .args(&self.args)
                    .stdin(Stdio::null())
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .with_context(|| format!("{program} did not answer within {COMMAND_TIMEOUT:?}"))?;
            match started {
                Ok(output) => return Ok((program, output)),
                // A directory in the PATH that cannot be searched, such as
                // that of the Windows Store aliases in WSL, makes a program
                // that does not exist "permission denied" on Linux.
                Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::PermissionDenied) => {
                    unstartable = e;
                }
                Err(e) => return Err(e).with_context(|| format!("could not run {program}")),
            }
        }
        Err(unstartable).with_context(|| format!("could not run {}", self.programs.join(" or ")))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::Path;

    /// A command that notes each run in `counter` and prints `json`.
    fn fake_az(counter: &Path, json: &str) -> CliToken {
        CliToken::new(
            "sh",
            [
                "-c",
                r#"echo run >> "$1"; printf '%s' "$2""#,
                "sh",
                counter.to_str().unwrap(),
                json,
            ],
        )
    }

    fn runs(counter: &Path) -> usize {
        std::fs::read_to_string(counter).map_or(0, |runs| runs.lines().count())
    }

    fn reply(token: &str, valid_for_secs: u64) -> String {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        format!(
            r#"{{"accessToken":"{token}","expires_on":{}}}"#,
            now.as_secs() + valid_for_secs
        )
    }

    #[tokio::test]
    async fn test_a_token_is_reused_until_it_is_about_to_expire() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("runs");
        let tokens = fake_az(&counter, &reply("TOKEN-1", 3600));

        assert_eq!(tokens.get().await.unwrap(), "TOKEN-1");
        assert_eq!(tokens.get().await.unwrap(), "TOKEN-1");
        assert_eq!(runs(&counter), 1);
    }

    #[tokio::test]
    async fn test_a_token_with_little_time_left_is_fetched_again() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("runs");
        let tokens = fake_az(&counter, &reply("TOKEN-1", 60));

        tokens.get().await.unwrap();
        tokens.get().await.unwrap();
        assert_eq!(runs(&counter), 2);
    }

    #[tokio::test]
    async fn test_a_token_without_expiry_is_kept_for_a_few_minutes() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("runs");
        let tokens = fake_az(&counter, r#"{"accessToken":"TOKEN-1"}"#);

        tokens.get().await.unwrap();
        tokens.get().await.unwrap();
        assert_eq!(runs(&counter), 1);
    }

    #[tokio::test]
    async fn test_a_failing_command_reports_its_standard_error() {
        let tokens = CliToken::new("sh", ["-c", "echo 'Please run az login' >&2; exit 3"]);

        let err = format!("{:#}", tokens.get().await.unwrap_err());
        assert!(err.contains("Please run az login"), "{err}");
        assert!(err.contains("exit status: 3"), "{err}");
    }

    #[tokio::test]
    async fn test_a_program_that_cannot_be_started_is_skipped() {
        let tokens = CliToken::with_programs(
            ["no-such-program", "sh"],
            ["-c", r#"printf '{"accessToken":"TOKEN-1"}'"#],
        );

        assert_eq!(tokens.get().await.unwrap(), "TOKEN-1");
    }

    #[tokio::test]
    async fn test_a_program_that_fails_is_not_skipped() {
        let tokens = CliToken::with_programs(
            ["false", "sh"],
            ["-c", r#"printf '{"accessToken":"TOKEN-1"}'"#],
        );

        let err = format!("{:#}", tokens.get().await.unwrap_err());
        assert!(err.contains("exit status: 1"), "{err}");
    }

    #[tokio::test]
    async fn test_the_error_names_all_programs_that_cannot_be_started() {
        let tokens = CliToken::with_programs(["no-such-program", "no-such-either"], ["-c", ""]);

        let err = format!("{:#}", tokens.get().await.unwrap_err());
        assert!(
            err.contains("could not run no-such-program or no-such-either"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn test_unparsable_output_is_not_shown_in_the_error() {
        let tokens = CliToken::new("sh", ["-c", "echo not JSON, but SECRET-VALUE"]);

        let err = format!("{:#}", tokens.get().await.unwrap_err());
        assert!(err.contains("did not print an access token"), "{err}");
        assert!(!err.contains("SECRET-VALUE"), "{err}");
    }
}
