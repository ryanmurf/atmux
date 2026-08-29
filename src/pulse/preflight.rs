//! Secret-free profile credential preflight and narrow config-dir healing.
//!
//! Launcher wrappers are parsed as bounded text; they are never executed.
//! Healing only changes the effective in-memory/store profile when its current
//! Claude credential identity is unusable, exactly one unclaimed candidate is
//! usable, and the operator enabled healing.

use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

use super::{
    Instant, Profile, ProfileName, ProfileOrigin, Vendor,
    collect::read_regular_bounded,
    credentials::{
        CredentialInspection, CredentialPlatform, CredentialState, inspect_claude_credentials_for,
    },
};

const MAX_WRAPPER_BYTES: usize = 64 * 1024;
const CLAUDE_CONFIG_ASSIGNMENT: &str = "CLAUDE_CONFIG_DIR=";

/// One safe startup diagnostic. Credential values never enter this type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfilePreflight {
    pub profile: ProfileName,
    pub vendor: Vendor,
    pub configured_dir: Option<PathBuf>,
    pub effective_dir: Option<PathBuf>,
    pub credential_state: Option<CredentialState>,
    pub healed_from: Option<PathBuf>,
    pub healed: bool,
    pub healthy: bool,
}

/// Inspects configured profiles and narrowly updates effective Claude paths.
///
/// The caller persists changed profiles only after this pure profile mutation
/// returns; failures are represented in the result and never stop healthy
/// sibling profiles from collecting.
#[must_use]
pub fn preflight_profiles(
    profiles: &mut [Profile],
    now: Instant,
    heal: bool,
    home: &Path,
    wrapper_dirs: &[PathBuf],
) -> Vec<ProfilePreflight> {
    preflight_profiles_for(
        profiles,
        now,
        heal,
        home,
        wrapper_dirs,
        CredentialPlatform::current(),
    )
}

fn preflight_profiles_for(
    profiles: &mut [Profile],
    now: Instant,
    heal: bool,
    home: &Path,
    wrapper_dirs: &[PathBuf],
    platform: CredentialPlatform,
) -> Vec<ProfilePreflight> {
    let mut claimed = profiles
        .iter()
        .filter_map(|profile| profile.config_dir.as_deref())
        .filter_map(normalize_absolute)
        .collect::<BTreeSet<_>>();
    profiles
        .iter_mut()
        .map(|profile| {
            preflight_profile(
                profile,
                now,
                heal,
                home,
                wrapper_dirs,
                platform,
                &mut claimed,
            )
        })
        .collect()
}

fn preflight_profile(
    profile: &mut Profile,
    now: Instant,
    heal: bool,
    home: &Path,
    wrapper_dirs: &[PathBuf],
    platform: CredentialPlatform,
    claimed: &mut BTreeSet<PathBuf>,
) -> ProfilePreflight {
    let configured_dir = profile.config_dir.clone();
    if profile.origin != ProfileOrigin::Local || profile.vendor != Vendor::AnthropicOauth {
        return ProfilePreflight {
            profile: profile.name.clone(),
            vendor: profile.vendor,
            configured_dir: configured_dir.clone(),
            effective_dir: configured_dir,
            credential_state: None,
            healed_from: None,
            healed: false,
            healthy: true,
        };
    }

    let current = configured_dir
        .as_deref()
        .map_or_else(missing_directory_inspection, |directory| {
            inspect_claude_credentials_for(directory, now.epoch_millis(), platform)
        });
    if current.is_usable() {
        return diagnostic(profile, configured_dir, &current, None, false);
    }

    for candidate in candidate_config_dirs(&profile.name, home, wrapper_dirs) {
        if profile
            .config_dir
            .as_deref()
            .and_then(normalize_absolute)
            .as_ref()
            == Some(&candidate)
            || claimed.contains(&candidate)
        {
            continue;
        }
        let candidate_health =
            inspect_claude_credentials_for(&candidate, now.epoch_millis(), platform);
        if !candidate_health.is_usable() {
            continue;
        }
        if !heal {
            break;
        }
        let healed_from = profile.config_dir.replace(candidate.clone());
        if let Some(previous) = healed_from.as_deref().and_then(normalize_absolute) {
            claimed.remove(&previous);
        }
        claimed.insert(candidate);
        return diagnostic(
            profile,
            configured_dir,
            &candidate_health,
            healed_from,
            true,
        );
    }
    diagnostic(profile, configured_dir, &current, None, false)
}

fn diagnostic(
    profile: &Profile,
    configured_dir: Option<PathBuf>,
    credentials: &CredentialInspection,
    healed_from: Option<PathBuf>,
    healed: bool,
) -> ProfilePreflight {
    ProfilePreflight {
        profile: profile.name.clone(),
        vendor: profile.vendor,
        configured_dir,
        effective_dir: profile.config_dir.clone(),
        credential_state: Some(credentials.state),
        healed_from,
        healed,
        healthy: credentials.is_usable(),
    }
}

fn missing_directory_inspection() -> CredentialInspection {
    CredentialInspection {
        state: CredentialState::MissingDirectory,
        expires_at_millis: None,
        scopes: Vec::new(),
    }
}

fn candidate_config_dirs(
    profile: &ProfileName,
    home: &Path,
    wrapper_dirs: &[PathBuf],
) -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(2);
    if let Some(from_wrapper) = config_dir_from_wrapper(profile, home, wrapper_dirs) {
        candidates.push(from_wrapper);
    }
    let convention = home.join(format!(".{}", profile.as_str()));
    if let Some(convention) = normalize_absolute(&convention)
        && !candidates.contains(&convention)
    {
        candidates.push(convention);
    }
    candidates
}

/// Reads the last literal `CLAUDE_CONFIG_DIR` assignment from fixed wrapper
/// search directories. No command substitution or shell expansion is allowed.
#[must_use]
pub fn config_dir_from_wrapper(
    profile: &ProfileName,
    home: &Path,
    wrapper_dirs: &[PathBuf],
) -> Option<PathBuf> {
    for directory in wrapper_dirs {
        let wrapper = directory.join(profile.as_str());
        let Ok(text) = read_regular_bounded(&wrapper, MAX_WRAPPER_BYTES) else {
            continue;
        };
        let mut found = None;
        for line in text.lines() {
            let line = line.trim();
            let assignment = line
                .strip_prefix("export ")
                .unwrap_or(line)
                .strip_prefix(CLAUDE_CONFIG_ASSIGNMENT);
            if let Some(raw) = assignment
                && let Some(path) = expand_literal_home(raw, home)
            {
                found = Some(path);
            }
        }
        if found.is_some() {
            return found;
        }
    }
    None
}

fn expand_literal_home(raw: &str, home: &Path) -> Option<PathBuf> {
    let raw = raw.trim();
    let raw = if raw.len() >= 2
        && ((raw.starts_with('"') && raw.ends_with('"'))
            || (raw.starts_with('\'') && raw.ends_with('\'')))
    {
        &raw[1..raw.len() - 1]
    } else {
        raw
    };
    if raw.is_empty()
        || raw.chars().any(char::is_control)
        || raw.contains("$(")
        || raw.contains('`')
    {
        return None;
    }
    let home_text = home.to_str()?;
    let expanded = if raw == "~" || raw == "$HOME" || raw == "${HOME}" {
        home_text.to_owned()
    } else if let Some(suffix) = raw.strip_prefix("~/") {
        home.join(suffix).to_string_lossy().into_owned()
    } else {
        raw.replace("${HOME}", home_text)
            .replace("$HOME", home_text)
    };
    if expanded.contains('$') {
        return None;
    }
    normalize_absolute(Path::new(&expanded))
}

fn normalize_absolute(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return None;
    }
    Some(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::pulse::{AccountId, RefreshPolicy};

    fn temp_root() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("atmux-pulse-preflight-{nonce}"));
        fs::create_dir_all(&root).expect("temp root");
        root
    }

    fn profile(name: &str, directory: PathBuf) -> Profile {
        Profile {
            account_id: AccountId::new(1).expect("account"),
            name: ProfileName::new(name).expect("profile"),
            vendor: Vendor::AnthropicOauth,
            config_dir: Some(directory),
            poll_interval_minutes: 15,
            monthly_budget_usd: None,
            api_key_env: None,
            api_key_file: None,
            refresh: RefreshPolicy::InMemory,
            hidden: false,
            origin: ProfileOrigin::Local,
        }
    }

    fn write_credentials(directory: &Path, expires_at: i64) {
        fs::create_dir_all(directory).expect("credential directory");
        fs::write(
            directory.join(".credentials.json"),
            format!(
                r#"{{"claudeAiOauth":{{"accessToken":"token","refreshToken":"refresh","expiresAt":{expires_at},"scopes":["user:profile"]}}}}"#
            ),
        )
        .expect("credentials");
    }

    #[test]
    fn wrapper_parser_is_literal_bounded_and_last_assignment_wins() {
        let root = temp_root();
        let bin = root.join("bin");
        fs::create_dir_all(&bin).expect("bin");
        fs::write(
            bin.join("claude-max"),
            "export CLAUDE_CONFIG_DIR=$HOME/.old\nCLAUDE_CONFIG_DIR=\"${HOME}/.claude-max\"\n",
        )
        .expect("wrapper");
        let name = ProfileName::new("claude-max").expect("name");
        assert_eq!(
            config_dir_from_wrapper(&name, &root, &[bin]),
            Some(root.join(".claude-max"))
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn shell_substitution_and_relative_paths_are_rejected() {
        let home = Path::new("/home/tester");
        assert_eq!(expand_literal_home("$(steal)", home), None);
        assert_eq!(expand_literal_home("`steal`", home), None);
        assert_eq!(expand_literal_home("relative/path", home), None);
        assert_eq!(expand_literal_home("$OTHER/path", home), None);
    }

    #[test]
    fn unhealthy_profile_heals_only_to_unclaimed_usable_identity() {
        let root = temp_root();
        let now = Instant::from_epoch_millis(1_786_214_400_000).expect("now");
        let healthy = root.join(".claude-max");
        write_credentials(&healthy, now.epoch_millis() + 3_600_000);
        let mut profiles = vec![profile("claude-max", root.join("wrong"))];
        let results = preflight_profiles_for(
            &mut profiles,
            now,
            true,
            &root,
            &[],
            CredentialPlatform::Linux,
        );
        assert_eq!(profiles[0].config_dir.as_deref(), Some(healthy.as_path()));
        assert_eq!(results[0].healed_from, Some(root.join("wrong")));
        assert!(results[0].healed);
        assert!(results[0].healthy);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn expired_current_identity_is_not_repointed() {
        let root = temp_root();
        let now = Instant::from_epoch_millis(1_786_214_400_000).expect("now");
        let current = root.join("current");
        let candidate = root.join(".claude-max");
        write_credentials(&current, now.epoch_millis() - 1);
        write_credentials(&candidate, now.epoch_millis() + 3_600_000);
        let mut profiles = vec![profile("claude-max", current.clone())];
        let results = preflight_profiles_for(
            &mut profiles,
            now,
            true,
            &root,
            &[],
            CredentialPlatform::Linux,
        );
        assert_eq!(profiles[0].config_dir.as_deref(), Some(current.as_path()));
        assert_eq!(results[0].credential_state, Some(CredentialState::Expired));
        assert!(!results[0].healed);
        assert!(results[0].healthy);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn claimed_candidate_never_merges_two_profiles() {
        let root = temp_root();
        let now = Instant::from_epoch_millis(1_786_214_400_000).expect("now");
        let candidate = root.join(".claude-max");
        write_credentials(&candidate, now.epoch_millis() + 3_600_000);
        let mut profiles = vec![
            profile("claude-max", root.join("wrong")),
            profile("owner", candidate.clone()),
        ];
        let results = preflight_profiles_for(
            &mut profiles,
            now,
            true,
            &root,
            &[],
            CredentialPlatform::Linux,
        );
        assert_eq!(profiles[0].config_dir, Some(root.join("wrong")));
        assert!(!results[0].healthy);
        fs::remove_dir_all(root).expect("cleanup");
    }
}
