//! Where the numbers actually come from: each provider's own credential on disk, its own usage
//! endpoint, and a parser for its own response shape.
//!
//! # Why an out-of-band HTTP read rather than scraping the CLI
//!
//! Jerry spawns `claude` and `codex` as interactive TUIs in a PTY with no arguments
//! (`crate::work_surface::agents::AgentKind::binary_name`). There is no structured event stream to
//! read, so the in-band routes those CLIs use internally - Claude's stream events, Codex's
//! `token_count` event and `x-codex-*` response headers - are not available to us at all. Neither
//! CLI has a `usage` subcommand. What is available is that both store an OAuth credential in a
//! well-known file, and both talk to a plain `GET` usage endpoint with it. That is what this
//! module does, and it is the *same* endpoint each CLI reads for its own `/status` display, so the
//! numbers agree with what the agent itself would tell you.
//!
//! # What is verified, and what is not
//!
//! The Claude path was verified live against a real account (issue #294's Phase 0 comment records
//! the captured 200 payload, which is this module's own test fixture). The Codex path is
//! *source*-verified against `openai/codex` - the URL
//! (`codex-rs/backend-client/src/client/rate_limit_resets.rs`, `{base}/wham/usage`), the auth file
//! (`codex-rs/login/src/auth/storage.rs`'s `AuthDotJson` -> `tokens.access_token`), the
//! `ChatGPT-Account-Id` header (`codex-rs/backend-client/src/client.rs`'s `headers()`), and the
//! payload shape (`codex-rs/codex-backend-openapi-models/src/models/rate_limit_status_*.rs`) -
//! but **not** executed, because there was no `codex` install or ChatGPT credential on the machine
//! this was written on. It therefore reads `not connected` there rather than inventing anything,
//! and its parser is tested against a fixture built from those published model definitions.
//!
//! # Credentials are read, never written or refreshed
//!
//! An expired token is reported as a failed poll, not silently refreshed. The refresh token lives
//! in the CLI's own credential file and racing that CLI for it risks clobbering the user's login -
//! re-authentication belongs to the CLI that owns the file. Nothing here ever writes to disk.
//!
//! On macOS the Claude CLI may keep its credential in the Keychain instead of the file, and Codex
//! may use a keyring backend; Jerry does not read either. Those installs read as `not connected`,
//! which is the honest statement: we have nothing, and nothing is broken.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use super::state::{window_label, BudgetWindow, Provider, ProviderSnapshot};

/// How long a single usage request may take before it is a failure. Short on purpose - this is a
/// background readout, and the Claude CLI's own call to the same endpoint uses a 5s timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The result of one real attempt to read one provider.
///
/// Three outcomes, matching the three states the UI keeps distinct: no credential at all, a real
/// snapshot, and a real failure with a real reason.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderRead {
    /// No credential for this provider on this machine - nothing was sent anywhere.
    NotConnected,
    Ok(ProviderSnapshot),
    /// The reason, for the popover's tooltip and the log. Never rendered as a number.
    Failed(String),
}

/// One provider's stored credential.
///
/// The `Debug` below is hand-written rather than derived, and that is the point of it - see its
/// own docs.
#[derive(Clone, PartialEq)]
pub struct Credential {
    pub token: String,
    /// Codex's `tokens.account_id`, sent as `ChatGPT-Account-Id`. `None` for Claude, which needs
    /// no equivalent.
    pub account_id: Option<String>,
}

/// Redacting, on purpose. This struct holds a live OAuth bearer token belonging to the user, and a
/// `#[derive(Debug)]` would put it verbatim into *any* `{:?}` - a stray `log::debug!`, a panic
/// message, a future error report. Nothing in this app has a reason to want the token's characters
/// in a diagnostic, so nothing is given the chance: the presence of each field is reported, and its
/// value never is.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("token", &"<redacted>")
            .field("account_id", &self.account_id.is_some())
            .finish()
    }
}

/// The directory a provider's CLI keeps its credential in, honouring the same environment
/// overrides that CLI itself honours (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`) so a relocated config
/// directory is found rather than reported as "not connected".
pub fn credential_dir(provider: Provider) -> Option<PathBuf> {
    let env_key = match provider {
        Provider::Claude => "CLAUDE_CONFIG_DIR",
        Provider::Codex => "CODEX_HOME",
    };
    credential_dir_from(
        provider,
        std::env::var_os(env_key).map(PathBuf::from),
        home_dir(),
    )
}

/// The pure half of [`credential_dir`], so the override rule is tested without mutating the
/// process-global environment (`std::env::set_var` is unsound to race in a threaded test binary).
pub fn credential_dir_from(
    provider: Provider,
    env_override: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(dir) = env_override.filter(|value| !value.as_os_str().is_empty()) {
        return Some(dir);
    }
    let default_dir = match provider {
        Provider::Claude => ".claude",
        Provider::Codex => ".codex",
    };
    Some(home?.join(default_dir))
}

/// This user's home directory. `$HOME` on unix, `%USERPROFILE%` on Windows - the same pair
/// `crate::settings::store` already resolves its own config path from.
fn home_dir() -> Option<PathBuf> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The credential file for a provider - `~/.claude/.credentials.json`, `~/.codex/auth.json`.
pub fn credential_path(provider: Provider) -> Option<PathBuf> {
    let file = match provider {
        Provider::Claude => ".credentials.json",
        Provider::Codex => "auth.json",
    };
    Some(credential_dir(provider)?.join(file))
}

/// Reads a provider's credential off disk, or `None` when there is genuinely none to read.
///
/// A file that exists but carries no OAuth token (an API-key-only Codex `auth.json`, a
/// half-written file) is `None` too: there is no token to send, which is the same fact as having
/// no file, and both are `not connected` rather than a failure.
pub fn read_credential(provider: Provider) -> Option<Credential> {
    let path = credential_path(provider)?;
    let raw = std::fs::read_to_string(path).ok()?;
    parse_credential(provider, &raw)
}

/// The pure half of [`read_credential`] - so both file shapes are tested without a home
/// directory.
pub fn parse_credential(provider: Provider, raw: &str) -> Option<Credential> {
    let json: serde_json::Value = serde_json::from_str(raw).ok()?;
    match provider {
        Provider::Claude => {
            let token = json
                .get("claudeAiOauth")?
                .get("accessToken")?
                .as_str()
                .filter(|token| !token.is_empty())?;
            Some(Credential {
                token: token.to_string(),
                account_id: None,
            })
        }
        Provider::Codex => {
            let tokens = json.get("tokens")?;
            let token = tokens
                .get("access_token")?
                .as_str()
                .filter(|token| !token.is_empty())?;
            Some(Credential {
                token: token.to_string(),
                account_id: tokens
                    .get("account_id")
                    .and_then(|value| value.as_str())
                    .filter(|id| !id.is_empty())
                    .map(|id| id.to_string()),
            })
        }
    }
}

/// The usage endpoint for a provider.
fn usage_url(provider: Provider) -> &'static str {
    match provider {
        // Read from the `claude` CLI's own call site: `GET /api/oauth/usage` against
        // `api.anthropic.com`, with the stored OAuth access token.
        Provider::Claude => "https://api.anthropic.com/api/oauth/usage",
        // `codex-rs/backend-client/src/client.rs` appends `/backend-api` to a `chatgpt.com` base
        // URL, and `rate_limit_resets.rs` appends `/wham/usage` to that for the ChatGPT path
        // style.
        Provider::Codex => "https://chatgpt.com/backend-api/wham/usage",
    }
}

/// One real, blocking read of one provider. **Never call this on the UI thread** - the whole of
/// `crate::budget::flow` runs it on `cx.background_executor()`.
///
/// Returns [`ProviderRead::NotConnected`] without touching the network when there is no
/// credential, which is what makes "a provider you have not logged into costs nothing" true
/// rather than aspirational.
pub fn read_provider(provider: Provider) -> ProviderRead {
    let Some(credential) = read_credential(provider) else {
        return ProviderRead::NotConnected;
    };
    match http_get_usage(provider, &credential) {
        Ok(body) => match parse_usage(provider, &body) {
            Ok(snapshot) => ProviderRead::Ok(snapshot),
            Err(reason) => ProviderRead::Failed(reason),
        },
        Err(reason) => ProviderRead::Failed(reason),
    }
}

/// The real HTTP call, with each provider's own required headers.
fn http_get_usage(provider: Provider, credential: &Credential) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|err| format!("could not build an HTTP client: {err}"))?;

    let mut request = client
        .get(usage_url(provider))
        .header("Authorization", format!("Bearer {}", credential.token))
        .header("Content-Type", "application/json");
    request = match provider {
        // The OAuth beta header the CLI's own OAuth-authorised calls carry.
        Provider::Claude => request.header("anthropic-beta", "oauth-2025-04-20"),
        // `codex-rs/backend-client/src/client.rs`'s `headers()`: a `codex-cli` user agent, plus
        // the account id when one is stored.
        Provider::Codex => {
            let request = request.header("User-Agent", "codex-cli");
            match &credential.account_id {
                Some(account_id) => request.header("ChatGPT-Account-Id", account_id.as_str()),
                None => request,
            }
        }
    };

    let response = request
        .send()
        .map_err(|err| format!("the request failed: {err}"))?;
    let status = response.status();
    if !status.is_success() {
        // Every non-2xx is one failure state with a real reason attached, including the two that
        // matter most here: a 401 (the CLI's stored token expired - re-authentication belongs to
        // that CLI, not to Jerry) and a 429 (the usage endpoint's own limiter, which is real and
        // is exactly why `POLL_INTERVAL` is as slow as it is).
        return Err(format!("the provider answered {}", status.as_u16()));
    }
    response
        .text()
        .map_err(|err| format!("the response could not be read: {err}"))
}

/// Parses one provider's usage payload into windows of *headroom*.
pub fn parse_usage(provider: Provider, body: &str) -> Result<ProviderSnapshot, String> {
    match provider {
        Provider::Claude => parse_claude_usage(body),
        Provider::Codex => parse_codex_usage(body),
    }
}

/// Percent used -> percent left, clamped to the real 0-100 range an over-quota account can
/// otherwise leave (a `utilization` above 100 is possible and would otherwise render as a
/// negative meter).
fn headroom_from_utilization(utilization: f64) -> f32 {
    (100.0 - utilization).clamp(0.0, 100.0) as f32
}

/// Anthropic's `GET /api/oauth/usage`.
///
/// ```json
/// {"five_hour": {"utilization": 19.0, "resets_at": "2026-08-15T20:00:00+00:00"},
///  "seven_day": {"utilization": 60.0, "resets_at": "2026-08-18T06:00:00+00:00"},
///  "limits": [ ... ]}
/// ```
///
/// The two named windows are what this reads: they are the two the CLI's own status display uses,
/// they carry their own reset instants, and their names fix their durations at 5h and 7d. The
/// `limits` array beside them carries the same two figures plus per-model scoped rows
/// (`weekly_scoped`), which are a *breakdown of* the weekly window rather than a third limit - so
/// reading them too would double-count the same budget under two labels.
pub fn parse_claude_usage(body: &str) -> Result<ProviderSnapshot, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("the response was not JSON: {err}"))?;

    let mut windows = Vec::new();
    for (key, label) in [("five_hour", "5h"), ("seven_day", "7d")] {
        let Some(entry) = json.get(key) else {
            continue;
        };
        let Some(utilization) = entry.get("utilization").and_then(|v| v.as_f64()) else {
            continue;
        };
        windows.push(BudgetWindow {
            label: label.to_string(),
            headroom_percent: headroom_from_utilization(utilization),
            resets_at: entry
                .get("resets_at")
                .and_then(|value| value.as_str())
                .and_then(parse_rfc3339),
        });
    }

    if windows.is_empty() {
        return Err("the response carried no usage windows".to_string());
    }
    Ok(ProviderSnapshot { windows })
}

/// ChatGPT's `GET /backend-api/wham/usage`, as `openai/codex`'s own generated models define it:
///
/// ```json
/// {"plan_type": "pro",
///  "rate_limit": {"allowed": true, "limit_reached": false,
///                 "primary_window":   {"used_percent": 12, "limit_window_seconds": 18000,
///                                      "reset_after_seconds": 900, "reset_at": 1786930000},
///                 "secondary_window": {"used_percent": 34, "limit_window_seconds": 604800,
///                                      "reset_after_seconds": 200000, "reset_at": 1787130000}}}
/// ```
///
/// Both window labels are formatted from the server's own `limit_window_seconds` rather than
/// assumed to be 5h/7d: unlike Claude's, these are numbers the API sends, and a plan whose primary
/// window is not five hours must not be labelled as though it were.
pub fn parse_codex_usage(body: &str) -> Result<ProviderSnapshot, String> {
    let json: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("the response was not JSON: {err}"))?;

    let rate_limit = json
        .get("rate_limit")
        .filter(|value| !value.is_null())
        .ok_or_else(|| "the response carried no rate_limit block".to_string())?;

    let mut windows = Vec::new();
    for key in ["primary_window", "secondary_window"] {
        let Some(entry) = rate_limit.get(key).filter(|value| !value.is_null()) else {
            continue;
        };
        let Some(used) = entry.get("used_percent").and_then(|v| v.as_f64()) else {
            continue;
        };
        let seconds = entry
            .get("limit_window_seconds")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        windows.push(BudgetWindow {
            label: window_label(seconds),
            headroom_percent: headroom_from_utilization(used),
            resets_at: codex_reset_instant(entry),
        });
    }

    if windows.is_empty() {
        return Err("the response carried no usage windows".to_string());
    }
    Ok(ProviderSnapshot { windows })
}

/// Codex sends both an absolute `reset_at` and a relative `reset_after_seconds`. The absolute one
/// wins when it is genuinely an epoch timestamp; otherwise the relative one is added to the
/// current clock, so a payload that only carries the relative form still produces a real
/// countdown instead of none.
fn codex_reset_instant(entry: &serde_json::Value) -> Option<SystemTime> {
    // Anything below this is not a plausible epoch second (it would be 2001), so it is a
    // relative value in a field named as though it were absolute.
    const PLAUSIBLE_EPOCH_FLOOR: i64 = 1_000_000_000;

    if let Some(reset_at) = entry.get("reset_at").and_then(|v| v.as_i64()) {
        if reset_at >= PLAUSIBLE_EPOCH_FLOOR {
            return Some(SystemTime::UNIX_EPOCH + Duration::from_secs(reset_at as u64));
        }
    }
    let after = entry
        .get("reset_after_seconds")
        .and_then(|v| v.as_i64())
        .filter(|seconds| *seconds >= 0)?;
    Some(SystemTime::now() + Duration::from_secs(after as u64))
}

/// The narrow slice of RFC 3339 the Anthropic payload actually uses:
/// `2026-08-15T20:00:00+00:00`, `...Z`, and offsets like `-05:00`.
///
/// Hand-written rather than reached for as a dependency: this crate has no date/time crate today,
/// and pulling `chrono` (or `time`) plus its own transitive stack in to read one field of one
/// response would be a large amount of new build surface for a fixed-shape timestamp. Fractional
/// seconds are accepted and discarded - a reset instant is not a sub-second fact.
pub fn parse_rfc3339(raw: &str) -> Option<SystemTime> {
    let bytes = raw.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: i64 = raw.get(0..4)?.parse().ok()?;
    let month: i64 = raw.get(5..7)?.parse().ok()?;
    let day: i64 = raw.get(8..10)?.parse().ok()?;
    if !matches!(bytes[10], b'T' | b't' | b' ') {
        return None;
    }
    let hour: i64 = raw.get(11..13)?.parse().ok()?;
    let minute: i64 = raw.get(14..16)?.parse().ok()?;
    let second: i64 = raw.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }

    // Everything after the seconds is an optional fractional part followed by the zone.
    let rest = &raw[19..];
    let zone = match rest.find(['+', '-', 'Z', 'z']) {
        Some(index) => &rest[index..],
        // No zone at all: RFC 3339 requires one, and guessing UTC would silently shift a
        // countdown by hours.
        None => return None,
    };
    let offset_seconds = match zone.as_bytes()[0] {
        b'Z' | b'z' => 0,
        sign => {
            let offset_hour: i64 = zone.get(1..3)?.parse().ok()?;
            let offset_minute: i64 = zone.get(4..6)?.parse().ok()?;
            let magnitude = offset_hour * 3600 + offset_minute * 60;
            if sign == b'-' {
                -magnitude
            } else {
                magnitude
            }
        }
    };

    let epoch_seconds =
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
            - offset_seconds;
    if epoch_seconds < 0 {
        return None;
    }
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(epoch_seconds as u64))
}

/// Days since 1970-01-01 for a proleptic Gregorian date - Howard Hinnant's `days_from_civil`, the
/// same algorithm every date library uses. Exact integer arithmetic, no floating point, correct
/// across leap years and century rules.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Real coverage for the two parsers and the credential readers, against real payload shapes.
#[cfg(test)]
mod budget_fetch_tests {
    use super::*;

    /// The genuine 200 body captured from `GET https://api.anthropic.com/api/oauth/usage` on a
    /// real Claude Max account (issue #294's Phase 0 comment records the capture). Trimmed to the
    /// fields this parser reads plus the neighbouring ones it must ignore.
    const CLAUDE_LIVE_PAYLOAD: &str = r#"{
      "five_hour": {"utilization": 19.0, "resets_at": "2026-08-15T20:00:00+00:00"},
      "seven_day": {"utilization": 60.0, "resets_at": "2026-08-18T06:00:00+00:00"},
      "limits": [
        {"kind": "session", "group": "session", "percent": 19, "severity": "normal",
         "resets_at": "2026-08-15T20:00:00+00:00", "scope": null, "is_active": false},
        {"kind": "weekly_all", "group": "weekly", "percent": 60, "severity": "normal",
         "resets_at": "2026-08-18T06:00:00+00:00", "scope": null, "is_active": true},
        {"kind": "weekly_scoped", "group": "weekly", "percent": 27, "severity": "normal",
         "resets_at": "2026-08-18T06:00:00+00:00",
         "scope": {"model": {"display_name": "Fable"}}, "is_active": false}
      ],
      "extra_usage": {},
      "spend": {}
    }"#;

    /// Built from `openai/codex`'s own published models - `RateLimitStatusPayload`,
    /// `RateLimitStatusDetails` and `RateLimitWindowSnapshot` - not from a live capture. See this
    /// module's docs for why the Codex side is source-verified rather than executed.
    const CODEX_SPEC_PAYLOAD: &str = r#"{
      "plan_type": "pro",
      "rate_limit": {
        "allowed": true,
        "limit_reached": false,
        "primary_window": {"used_percent": 1, "limit_window_seconds": 18000,
                           "reset_after_seconds": 7200, "reset_at": 1786940000},
        "secondary_window": {"used_percent": 12, "limit_window_seconds": 604800,
                             "reset_after_seconds": 400000, "reset_at": 1787240000}
      }
    }"#;

    /// The one direction rule the whole feature rests on: the API reports *used*, every value in
    /// Jerry is *left*. `19% used` must become `81% left`, never `19%`.
    #[test]
    fn the_claude_payload_parses_into_two_windows_of_headroom_not_spend() {
        let snapshot = parse_claude_usage(CLAUDE_LIVE_PAYLOAD).expect("a real payload parses");
        assert_eq!(
            snapshot.windows.len(),
            2,
            "two independent windows - the finding that settled the two-bar meter shape"
        );
        assert_eq!(snapshot.windows[0].label, "5h");
        assert_eq!(
            snapshot.windows[0].headroom_percent, 81.0,
            "19% used is 81% left - the popover's footnote promises headroom, not spend"
        );
        assert_eq!(snapshot.windows[1].label, "7d");
        assert_eq!(snapshot.windows[1].headroom_percent, 40.0);
        assert_eq!(
            snapshot.tightest().map(|w| w.label.clone()),
            Some("7d".to_string()),
            "on this real account the *week* is the tight window while the session is healthy - \
             the exact case a single bar would have misreported"
        );
    }

    /// The two reset instants are independent, and both are real absolute times rather than the
    /// same value repeated.
    #[test]
    fn each_claude_window_carries_its_own_reset_instant() {
        let snapshot = parse_claude_usage(CLAUDE_LIVE_PAYLOAD).expect("parses");
        let five_hour = snapshot.windows[0].resets_at.expect("a 5h reset instant");
        let seven_day = snapshot.windows[1].resets_at.expect("a 7d reset instant");
        assert!(
            seven_day > five_hour,
            "the weekly window resets later than the session one - two windows, two clocks"
        );
        assert_eq!(
            five_hour
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("after the epoch")
                .as_secs(),
            // 2026-08-15T20:00:00Z
            1_786_824_000,
            "the timestamp must parse to the exact instant, not an approximation"
        );
    }

    #[test]
    fn a_claude_payload_with_no_windows_at_all_is_a_failure_not_an_empty_meter() {
        let err = parse_claude_usage(r#"{"limits": []}"#).expect_err("no windows is not a read");
        assert!(err.contains("no usage windows"), "got {err}");
        assert!(
            parse_claude_usage("not json at all").is_err(),
            "a non-JSON body is a failure with a reason, never a silent zero"
        );
    }

    #[test]
    fn the_codex_payload_parses_into_two_windows_labelled_from_the_server() {
        let snapshot = parse_codex_usage(CODEX_SPEC_PAYLOAD).expect("the documented shape parses");
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(
            snapshot.windows[0].label, "5h",
            "the label comes from `limit_window_seconds`, not from an assumption"
        );
        assert_eq!(snapshot.windows[0].headroom_percent, 99.0);
        assert_eq!(snapshot.windows[1].label, "7d");
        assert_eq!(snapshot.windows[1].headroom_percent, 88.0);
        assert_eq!(
            snapshot.summary_label(),
            "5h 99%  \u{b7}  7d 88%",
            "window label before the value, on this provider too"
        );
    }

    /// A plan whose primary window is not five hours must label itself honestly - the whole reason
    /// the label is formatted from the payload.
    #[test]
    fn a_codex_window_of_an_unusual_length_is_labelled_as_what_it_is() {
        let body = r#"{"rate_limit": {"allowed": true, "limit_reached": false,
            "primary_window": {"used_percent": 50, "limit_window_seconds": 3600,
                               "reset_after_seconds": 600, "reset_at": 0}}}"#;
        let snapshot = parse_codex_usage(body).expect("parses");
        assert_eq!(
            snapshot.windows.len(),
            1,
            "one window is a real payload too"
        );
        assert_eq!(snapshot.windows[0].label, "1h");
        assert!(
            snapshot.windows[0].resets_at.is_some(),
            "a `reset_at` of 0 is not an epoch instant, so the relative `reset_after_seconds` must \
             be used instead of dropping the countdown"
        );
    }

    #[test]
    fn a_codex_payload_with_a_null_rate_limit_is_a_failure_with_a_reason() {
        let err = parse_codex_usage(r#"{"plan_type": "free", "rate_limit": null}"#)
            .expect_err("null is not a snapshot");
        assert!(err.contains("no rate_limit"), "got {err}");
    }

    #[test]
    fn both_credential_shapes_are_read_from_their_real_file_layouts() {
        let claude = parse_credential(
            Provider::Claude,
            r#"{"claudeAiOauth": {"accessToken": "sk-ant-oat01-x", "subscriptionType": "max"}}"#,
        )
        .expect("a real .credentials.json shape");
        assert_eq!(claude.token, "sk-ant-oat01-x");
        assert_eq!(claude.account_id, None);

        let codex = parse_credential(
            Provider::Codex,
            r#"{"OPENAI_API_KEY": null,
                "tokens": {"access_token": "jwt", "refresh_token": "r", "account_id": "acc-1"}}"#,
        )
        .expect("a real auth.json shape");
        assert_eq!(codex.token, "jwt");
        assert_eq!(
            codex.account_id.as_deref(),
            Some("acc-1"),
            "the account id is a real header the endpoint needs, not decoration"
        );
    }

    /// An API-key-only Codex login, and a Claude file with no OAuth block, both have no token to
    /// send - which is the same fact as having no file at all, and must read as `not connected`
    /// rather than as a failure.
    #[test]
    fn a_credential_file_with_no_oauth_token_is_not_connected_rather_than_broken() {
        assert_eq!(
            parse_credential(Provider::Codex, r#"{"OPENAI_API_KEY": "sk-proj-x"}"#),
            None
        );
        assert_eq!(
            parse_credential(
                Provider::Claude,
                r#"{"claudeAiOauth": {"accessToken": ""}}"#
            ),
            None,
            "an empty token is not a token"
        );
        assert_eq!(parse_credential(Provider::Claude, "{}"), None);
        assert_eq!(parse_credential(Provider::Claude, "broken"), None);
    }

    /// A live bearer token must never be printable. The one place a credential could plausibly
    /// escape into a log or a panic message is `{:?}`, so that is the hole this closes - and this
    /// test is what stops a later `#[derive(Debug)]` from quietly reopening it.
    #[test]
    fn a_credential_never_prints_its_own_token() {
        let credential = Credential {
            token: "sk-ant-oat01-super-secret".to_string(),
            account_id: Some("acc-secret-1".to_string()),
        };
        let printed = format!("{credential:?}");
        assert!(
            !printed.contains("super-secret"),
            "the token must never reach a `{{:?}}`, got {printed}"
        );
        assert!(
            !printed.contains("acc-secret-1"),
            "nor the account id, got {printed}"
        );
        assert!(
            printed.contains("<redacted>") && printed.contains("account_id: true"),
            "but a diagnostic must still be able to say a credential was there and complete, got \
             {printed}"
        );
    }

    #[test]
    fn the_timestamp_parser_handles_every_form_the_payload_uses() {
        let utc = parse_rfc3339("2026-08-15T20:00:00+00:00").expect("offset form");
        let zulu = parse_rfc3339("2026-08-15T20:00:00Z").expect("Z form");
        assert_eq!(utc, zulu, "`Z` and `+00:00` are the same instant");

        let fractional = parse_rfc3339("2026-08-15T20:00:00.123456Z").expect("fractional form");
        assert_eq!(
            fractional, zulu,
            "sub-second precision is discarded, not fatal"
        );

        let offset = parse_rfc3339("2026-08-15T15:00:00-05:00").expect("negative offset");
        assert_eq!(offset, zulu, "a -05:00 wall clock at 15:00 is 20:00 UTC");

        // A leap day, and a century that is not a leap year - the two cases a hand-rolled
        // conversion gets wrong.
        assert_eq!(
            parse_rfc3339("2024-02-29T00:00:00Z")
                .expect("a real leap day")
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("after the epoch")
                .as_secs(),
            1_709_164_800
        );
        assert_eq!(
            parse_rfc3339("1970-01-01T00:00:00Z"),
            Some(SystemTime::UNIX_EPOCH)
        );

        assert_eq!(
            parse_rfc3339("2026-08-15T20:00:00"),
            None,
            "no zone is not a valid instant"
        );
        assert_eq!(parse_rfc3339("nonsense"), None);
        assert_eq!(
            parse_rfc3339("2026-13-15T20:00:00Z"),
            None,
            "month 13 is not a date"
        );
    }

    /// The credential path really is the file each CLI writes, and really does follow that CLI's
    /// own environment override.
    #[test]
    fn the_credential_directory_follows_each_clis_own_environment_override() {
        let home = Some(PathBuf::from("/home/someone"));
        assert_eq!(
            credential_dir_from(Provider::Claude, None, home.clone()),
            Some(PathBuf::from("/home/someone/.claude")),
            "the default is the directory the `claude` CLI really writes"
        );
        assert_eq!(
            credential_dir_from(Provider::Codex, None, home.clone()),
            Some(PathBuf::from("/home/someone/.codex"))
        );
        assert_eq!(
            credential_dir_from(
                Provider::Claude,
                Some(PathBuf::from("/elsewhere/claude")),
                home.clone()
            ),
            Some(PathBuf::from("/elsewhere/claude")),
            "`CLAUDE_CONFIG_DIR` is honoured, so a relocated config directory is found rather \
             than reported as `not connected`"
        );
        assert_eq!(
            credential_dir_from(Provider::Codex, Some(PathBuf::new()), home),
            Some(PathBuf::from("/home/someone/.codex")),
            "an empty override is not an override"
        );
        assert_eq!(
            credential_dir_from(Provider::Claude, None, None),
            None,
            "no home directory at all means there is nothing to read, not a guessed path"
        );
    }
}
