//! Normalized usage windows shared by the Claude relay and Codex's official app-server client.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::codex_app_server::{Client, RpcError, bounded};
use crate::codex_install::{codex_command, default_codex_home, parse_version_triple, probe_codex_version_in};

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
pub struct Window {
    pub used_percent: f64,
    pub resets_at: Option<i64>,
}

impl Window {
    pub(crate) fn from_json(value: &Value, percent_key: &str, reset_key: &str) -> Option<Self> {
        let percent = value.get(percent_key)?.as_f64()?;
        Some(Self {
            used_percent: percent.clamp(0.0, 100.0),
            resets_at: value.get(reset_key).and_then(Value::as_i64).filter(|&at| at > 0),
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct Windows {
    pub five_hour: Option<Window>,
    pub weekly: Option<Window>,
}

impl Windows {
    pub fn is_empty(self) -> bool {
        self.five_hour.is_none() && self.weekly.is_none()
    }
}

pub fn unix_seconds() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// The settings base used by the GUI, daemon and CLI.
pub fn config_file(name: &str) -> Option<PathBuf> {
    let base = std::env::var_os("SICH_CONFIG_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::config_dir)?;
    Some(base.join("sich").join(name))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexUnavailable {
    MissingOrOld,
    Unsupported,
    NoLimits,
    ReadFailed,
}

pub fn read_codex() -> Result<Windows, CodexUnavailable> {
    let home = default_codex_home().ok_or(CodexUnavailable::MissingOrOld)?;
    if !supports_codex_limits(probe_codex_version_in(&home).as_deref()) {
        return Err(CodexUnavailable::MissingOrOld);
    }
    let mut command = codex_command("app-server");
    command.args([
        "-c",
        "sandbox_mode=\"read-only\"",
        "-c",
        "approval_policy=\"never\"",
        "-c",
        "features.plugins=false",
    ]);
    let windows = bounded(query_codex(command, &home)).map_err(|error| {
        if error.downcast_ref::<RpcError>().is_some_and(|error| error.code == -32601) {
            CodexUnavailable::Unsupported
        } else {
            CodexUnavailable::ReadFailed
        }
    })?;
    if windows.is_empty() { Err(CodexUnavailable::NoLimits) } else { Ok(windows) }
}

fn supports_codex_limits(version: Option<&str>) -> bool {
    version.and_then(parse_version_triple).is_some_and(|version| version >= (0, 48, 0))
}

pub(crate) async fn query_codex(command: Command, home: &Path) -> anyhow::Result<Windows> {
    let mut client = Client::connect(command, home).await?;
    let result = client.call("account/rateLimits/read", Value::Null).await?;
    Ok(codex_windows(&result))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Period {
    FiveHour,
    Weekly,
}

fn classify(duration: Option<i64>) -> Option<Period> {
    match duration {
        Some(299..=301) => Some(Period::FiveHour),
        Some(10079..=10081) => Some(Period::Weekly),
        _ => None,
    }
}

fn codex_windows(result: &Value) -> Windows {
    let bucket = result["rateLimitsByLimitId"]["codex"].as_object();
    let snapshot = bucket.map_or(&result["rateLimits"], |_| &result["rateLimitsByLimitId"]["codex"]);
    let mut windows = Windows::default();
    let candidates = [("primary", Period::FiveHour), ("secondary", Period::Weekly)].map(|(key, fallback)| {
        let value = &snapshot[key];
        (
            Window::from_json(value, "usedPercent", "resetsAt"),
            classify(value["windowDurationMins"].as_i64()),
            fallback,
        )
    });
    // A known duration wins when it collides with a position-based fallback.
    for explicit in [true, false] {
        for (window, period, fallback) in candidates {
            if period.is_some() != explicit {
                continue;
            }
            let slot = match period.unwrap_or(fallback) {
                Period::FiveHour => &mut windows.five_hour,
                Period::Weekly => &mut windows.weekly,
            };
            *slot = slot.or(window);
        }
    }
    windows
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn codex_prefers_named_bucket_and_uses_duration_before_position() {
        let result = json!({
            "rateLimits": { "primary": { "usedPercent": 19, "windowDurationMins": 300 } },
            "rateLimitsByLimitId": {
                "codex": {
                    "limitId": "codex",
                    "planType": "plus",
                    "credits": null,
                    "primary": { "usedPercent": 102, "windowDurationMins": 10081, "resetsAt": 12345 },
                    "secondary": { "usedPercent": -2, "windowDurationMins": 299, "resetsAt": null }
                },
                "another-bucket": { "primary": { "usedPercent": 50 } }
            },
            "rateLimitResetCredits": null
        });
        assert_eq!(
            codex_windows(&result),
            Windows {
                five_hour: Some(Window { used_percent: 0.0, resets_at: None }),
                weekly: Some(Window { used_percent: 100.0, resets_at: Some(12345) }),
            }
        );
    }

    #[test]
    fn duration_boundaries_and_position_fallback() {
        assert_eq!(classify(Some(301)), Some(Period::FiveHour));
        assert_eq!(classify(Some(10079)), Some(Period::Weekly));
        assert_eq!(classify(Some(302)), None);
        assert_eq!(classify(Some(10082)), None);
        let result = json!({"rateLimits": {
            "primary": {"usedPercent": 19.5, "windowDurationMins": 240},
            "secondary": {"usedPercent": 86, "resetsAt": 990}
        }});
        let windows = codex_windows(&result);
        assert_eq!(windows.five_hour.unwrap().used_percent, 19.5);
        assert_eq!(windows.weekly.unwrap().resets_at, Some(990));

        let result = json!({"rateLimits": {
            "primary": {"usedPercent": 19.5},
            "secondary": {"usedPercent": 86, "windowDurationMins": 300}
        }});
        let windows = codex_windows(&result);
        assert_eq!(windows.five_hour.unwrap().used_percent, 86.0);
        assert!(windows.weekly.is_none());
    }

    #[test]
    fn partial_and_missing_codex_windows() {
        let result = json!({
            "rateLimits": {"primary": null, "secondary": {"usedPercent": 5}},
            "rateLimitsByLimitId": null
        });
        assert_eq!(
            codex_windows(&result),
            Windows { five_hour: None, weekly: Some(Window { used_percent: 5.0, resets_at: None }) }
        );
        assert!(codex_windows(&json!({"rateLimits": {"primary": {"resetsAt": 1000}}})).is_empty());
        assert!(codex_windows(&json!({})).is_empty());
    }

    #[test]
    fn codex_usage_version_floor() {
        assert!(supports_codex_limits(Some("0.48.0")));
        assert!(supports_codex_limits(Some("0.170.0")));
        assert!(!supports_codex_limits(Some("0.47.0")));
        assert!(!supports_codex_limits(None));
    }
}
