//! Update check for `serve` and `watch`: an anonymous GET to the GitHub releases API at
//! start and then every 12 hours. Nothing is sent but the request itself. The result is
//! cached for the Web UI (which shows a banner with how to update the container) and
//! logged once per new version. A container never replaces itself. Set UPDATE_CHECK=false
//! to turn the check off; offline or GitHub errors are ignored.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const LATEST_API: &str = "https://api.github.com/repos/redn3/blue-compressor/releases/latest";
const EVERY: Duration = Duration::from_secs(12 * 3600);

#[derive(Debug, Serialize, Clone, Default)]
pub struct UpdateStatus {
    /// False when UPDATE_CHECK turns the check off.
    pub enabled: bool,
    pub current: String,
    /// Newest release version (without the leading v), once a check succeeded.
    pub latest: Option<String>,
    pub available: bool,
    /// Release page of the newest version.
    pub url: Option<String>,
    /// What's new in it: the first section of the release notes, shortened.
    pub notes: Option<String>,
    /// Unix seconds of the last successful check.
    pub checked: Option<u64>,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// UPDATE_CHECK: on unless set to false, 0, no or off.
pub fn enabled_by_env() -> bool {
    match std::env::var("UPDATE_CHECK") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "0" | "no" | "off"),
        Err(_) => true,
    }
}

/// (major, minor, patch, is a release rather than a pre-release) of "v1.2.3" or "1.2.3-x".
fn parse(v: &str) -> Option<(u64, u64, u64, bool)> {
    let v = v.trim().trim_start_matches('v');
    let (core, pre) = match v.split_once('-') {
        Some((c, _)) => (c, true),
        None => (v, false),
    };
    let core = core.split('+').next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let (a, b, c) = (it.next()??, it.next().flatten().unwrap_or(0), it.next().flatten().unwrap_or(0));
    Some((a, b, c, !pre))
}

/// Whether `latest` is a newer version than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// The first "What's new" section of release notes, as plain text of at most `max` chars.
pub fn first_section(body: &str, max: usize) -> String {
    let mut out = Vec::new();
    for (i, line) in body.lines().enumerate() {
        let t = line.trim();
        if i > 0 && t.starts_with("**What's new") {
            break;
        }
        if t.is_empty() && !out.is_empty() {
            break;
        }
        if !t.is_empty() {
            out.push(t.replace("**", "").replace('\\', ""));
        }
    }
    let s = out.join("\n");
    if s.chars().count() <= max {
        s
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{}...", cut.trim_end())
    }
}

fn check() -> Option<Release> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).build();
    let r: Release = agent
        .get(LATEST_API)
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", concat!("bluecompress/", env!("CARGO_PKG_VERSION")))
        .call()
        .ok()?
        .into_json()
        .ok()?;
    (!r.draft && !r.prerelease).then_some(r)
}

/// Fills `status` from one release (see [`check`]).
fn apply(status: &mut UpdateStatus, r: &Release, now: u64) {
    let latest = r.tag_name.trim_start_matches('v').to_string();
    status.available = is_newer(&latest, &status.current);
    status.url = Some(r.html_url.clone());
    status.notes = r.body.as_deref().map(|b| first_section(b, 600));
    status.latest = Some(latest);
    status.checked = Some(now);
}

/// A status for this build with the check on or off as UPDATE_CHECK says.
pub fn initial_status() -> Arc<Mutex<UpdateStatus>> {
    Arc::new(Mutex::new(UpdateStatus {
        enabled: enabled_by_env(),
        current: bluecompress::VERSION.into(),
        ..Default::default()
    }))
}

/// Starts the background check (when enabled): now, then every 12 hours. With `log` a new
/// version is announced once on stderr.
pub fn spawn(status: Arc<Mutex<UpdateStatus>>, log: bool) {
    // Unit tests start servers too; they never reach out to GitHub.
    if !status.lock().unwrap().enabled || cfg!(test) {
        return;
    }
    std::thread::spawn(move || {
        let mut announced = String::new();
        loop {
            if let Some(r) = check() {
                let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
                let mut s = status.lock().unwrap();
                apply(&mut s, &r, now);
                if log && s.available {
                    let v = s.latest.clone().unwrap_or_default();
                    if v != announced {
                        eprintln!(
                            "bluecompress: Blue Compressor {v} is available (this is {}): {}",
                            s.current,
                            s.url.as_deref().unwrap_or("")
                        );
                        announced = v;
                    }
                }
            }
            std::thread::sleep(EVERY);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("v0.1.8", "0.1.7"));
        assert!(is_newer("0.2.0", "0.1.17"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("v0.1.7", "0.1.7"));
        assert!(!is_newer("0.1.6", "0.1.7"));
        assert!(is_newer("0.1.8", "0.1.8-test.1"), "a release is newer than its pre-release");
        assert!(!is_newer("0.1.8-test.1", "0.1.8"));
        assert!(!is_newer("garbage", "0.1.7"));
    }

    #[test]
    fn notes_are_the_first_section() {
        let body = "**What's new in 0.1.8**\n- **One** thing.\n- Two \\* things.\n\n**What's new in 0.1.7**\n- Old.";
        assert_eq!(first_section(body, 600), "What's new in 0.1.8\n- One thing.\n- Two * things.");
        assert_eq!(first_section("abcdefghij", 4), "abcd...");
    }

    #[test]
    fn applies_a_release() {
        let mut s = UpdateStatus { enabled: true, current: "0.1.7".into(), ..Default::default() };
        let r = Release {
            tag_name: "v0.1.8".into(),
            html_url: "https://github.com/redn3/blue-compressor/releases/tag/v0.1.8".into(),
            body: Some("**What's new in 0.1.8**\n- New.".into()),
            draft: false,
            prerelease: false,
        };
        apply(&mut s, &r, 5);
        assert!(s.available);
        assert_eq!(s.latest.as_deref(), Some("0.1.8"));
        assert_eq!(s.checked, Some(5));
        let r = Release { tag_name: "v0.1.7".into(), ..r };
        apply(&mut s, &r, 6);
        assert!(!s.available);
    }

    #[test]
    fn env_switch() {
        // UPDATE_CHECK is read at start; only explicit "off" words turn it off.
        for (v, on) in [("false", false), ("0", false), ("OFF", false), ("no", false), ("true", true), ("1", true)] {
            std::env::set_var("UPDATE_CHECK", v);
            assert_eq!(enabled_by_env(), on, "{v}");
        }
        std::env::remove_var("UPDATE_CHECK");
        assert!(enabled_by_env());
    }
}
