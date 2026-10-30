//! Telling the user when a newer version is out: a once-a-day look at the project's latest
//! release on GitHub. Nothing is downloaded or installed; the user gets a link to the release.
//! Switched off with "Tell me when a new version is out" in the window.

use std::time::Duration;

pub const RELEASES_URL: &str = "https://github.com/sweatshopking/switch-swap/releases/latest";
const API_URL: &str = "https://api.github.com/repos/sweatshopking/switch-swap/releases/latest";

/// "1.2.3" or "v1.2.3" as numbers; anything else (a pre-release tag, say) is None.
fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().strip_prefix('v').unwrap_or(v.trim());
    let mut it = v.split('.');
    let t = (
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    );
    it.next().is_none().then_some(t)
}

/// Whether `latest` is a newer version than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

/// The latest release's version ("0.3.6"); Ok(None) when GitHub answered but has no ordinary
/// release to offer (none yet, or a pre-release); Err when it couldn't be asked (offline, busy).
pub fn latest_version() -> Result<Option<String>, ()> {
    // Debug builds can pretend a release is out, for screenshots and testing.
    #[cfg(debug_assertions)]
    if let Ok(version) = std::env::var("SWITCH_SWAP_TEST_LATEST") {
        return Ok(parse(&version).map(|(a, b, c)| format!("{a}.{b}.{c}")));
    }
    let url = API_URL;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .get(url)
        // GitHub asks for one; the version tells nothing about the user
        .header("User-Agent", "switch-swap")
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|_| ())?;
    match resp.status().as_u16() {
        200 => {}
        // no release yet
        404 => return Ok(None),
        _ => return Err(()),
    }
    let v: serde_json::Value = resp.body_mut().read_json().map_err(|_| ())?;
    // drafts and pre-releases aren't offered
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        return Ok(None);
    }
    Ok(v["tag_name"]
        .as_str()
        .and_then(parse)
        .map(|(a, b, c)| format!("{a}.{b}.{c}")))
}

/// The newer version, if there is one (None also when GitHub couldn't be asked).
pub fn newer_version() -> Option<String> {
    latest_version()
        .ok()
        .flatten()
        .filter(|l| is_newer(l, env!("CARGO_PKG_VERSION")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("v0.3.6", "0.3.5"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("v0.3.5", "0.3.5"));
        assert!(!is_newer("0.3.4", "0.3.5"));
        assert!(!is_newer("v0.4.0-beta", "0.3.5"));
        assert!(!is_newer("nonsense", "0.3.5"));
        assert!(!is_newer("0.4", "0.3.5"));
    }
}

/// The window's own choices, kept per user in `~/.config/switch-swap/gui.conf`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preferences {
    pub check_for_updates: bool,
    /// A version the user said not to be told about again.
    pub skipped_version: Option<String>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            check_for_updates: true,
            skipped_version: None,
        }
    }
}

fn preferences_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".config"))
        })?;
    Some(base.join("switch-swap/gui.conf"))
}

impl Preferences {
    pub fn load() -> Self {
        preferences_path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|contents| Self::parse(&contents))
            .unwrap_or_default()
    }

    fn parse(contents: &str) -> Self {
        let mut preferences = Self::default();
        for line in contents.lines() {
            match line.split_once('=') {
                Some(("check_for_updates", value)) => {
                    preferences.check_for_updates = value.trim() != "false";
                }
                Some(("skipped_version", value)) if parse(value).is_some() => {
                    preferences.skipped_version = Some(value.trim().to_owned());
                }
                _ => {}
            }
        }
        preferences
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = preferences_path()
            .ok_or_else(|| std::io::Error::other("no home directory to save settings in"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut contents = format!("check_for_updates={}\n", self.check_for_updates);
        if let Some(version) = &self.skipped_version {
            contents.push_str(&format!("skipped_version={version}\n"));
        }
        let temporary = path.with_extension("conf.tmp");
        std::fs::write(&temporary, contents)?;
        std::fs::rename(temporary, path)
    }

    /// Whether to tell the user about `version`.
    pub fn should_offer(&self, version: &str) -> bool {
        self.check_for_updates && self.skipped_version.as_deref() != Some(version)
    }
}

#[cfg(test)]
mod preference_tests {
    use super::*;

    #[test]
    fn preferences_round_trip_and_default_to_checking() {
        assert_eq!(Preferences::parse(""), Preferences::default());
        let preferences = Preferences::parse("check_for_updates=false\nskipped_version=0.3.0\n");
        assert!(!preferences.check_for_updates);
        assert_eq!(preferences.skipped_version.as_deref(), Some("0.3.0"));
        assert!(!preferences.should_offer("0.3.0"));
        let checking = Preferences {
            check_for_updates: true,
            skipped_version: Some("0.3.0".to_owned()),
        };
        assert!(!checking.should_offer("0.3.0"));
        assert!(checking.should_offer("0.3.1"));
        assert_eq!(
            Preferences::parse("skipped_version=garbage\n").skipped_version,
            None
        );
    }
}
