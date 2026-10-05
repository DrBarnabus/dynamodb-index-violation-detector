//! AWS profile discovery for the launch picker.
//!
//! Reads the shared config and credentials files (honouring `AWS_CONFIG_FILE`
//! and `AWS_SHARED_CREDENTIALS_FILE`) and lists every profile they declare,
//! with the region the config file pins for it. Only section headers and
//! `region` keys are read; credential resolution stays with the SDK.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// One profile as offered by the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub region: Option<String>,
}

/// A shared AWS file exists but could not be read.
#[derive(Debug)]
pub struct ProfileError {
    path: PathBuf,
    source: io::Error,
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot read AWS profiles from `{}`: {}; fix its permissions or pass --profile",
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for ProfileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Discover profiles from the user's shared AWS files, sorted by name with
/// `default` first. Missing files contribute nothing.
pub fn discover() -> Result<Vec<Profile>, ProfileError> {
    let config = read_or_empty(&shared_file("AWS_CONFIG_FILE", "config"))?;
    let credentials = read_or_empty(&shared_file("AWS_SHARED_CREDENTIALS_FILE", "credentials"))?;
    Ok(parse(&config, &credentials))
}

/// Merge the profiles declared in a config file and a credentials file.
///
/// Config sections are `[default]` or `[profile NAME]`; credentials sections are
/// a bare `[NAME]`. Other config sections (`[sso-session …]`, `[services …]`)
/// are not profiles and are skipped.
pub fn parse(config: &str, credentials: &str) -> Vec<Profile> {
    let mut profiles: BTreeMap<String, Option<String>> = BTreeMap::new();

    for (name, region) in sections(config, config_profile_name) {
        let slot = profiles.entry(name).or_default();
        if region.is_some() {
            *slot = region;
        }
    }

    for (name, _) in sections(credentials, |header| Some(header.to_string())) {
        profiles.entry(name).or_default();
    }

    let mut list: Vec<Profile> = profiles
        .into_iter()
        .map(|(name, region)| Profile { name, region })
        .collect();
    list.sort_by_key(|profile| profile.name != "default");
    list
}

fn config_profile_name(header: &str) -> Option<String> {
    if header == "default" {
        return Some(header.to_string());
    }

    header
        .strip_prefix("profile")
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .map(|rest| rest.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// Yield `(profile name, region)` for each section whose header `name_of`
/// accepts.
fn sections(text: &str, name_of: impl Fn(&str) -> Option<String>) -> Vec<(String, Option<String>)> {
    let mut found: Vec<(String, Option<String>)> = Vec::new();
    let mut in_profile = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            match name_of(header.trim()) {
                Some(name) => {
                    found.push((name, None));
                    in_profile = true;
                }
                None => in_profile = false,
            }
            continue;
        }

        if !in_profile {
            continue;
        }

        if let Some((key, value)) = line.split_once('=')
            && key.trim() == "region"
            && let Some((_, region)) = found.last_mut()
        {
            let value = value.trim();
            *region = (!value.is_empty()).then(|| value.to_string());
        }
    }

    found
}

/// A missing file reads as empty: it declares no profiles.
fn read_or_empty(path: &Path) -> Result<String, ProfileError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(source) => Err(ProfileError {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn shared_file(env_var: &str, file_name: &str) -> PathBuf {
    if let Some(path) = env::var_os(env_var) {
        return PathBuf::from(path);
    }

    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .unwrap_or_default();
    PathBuf::from(home).join(".aws").join(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str, region: Option<&str>) -> Profile {
        Profile {
            name: name.to_string(),
            region: region.map(str::to_string),
        }
    }

    #[test]
    fn config_sections_yield_profiles_with_regions() {
        let config = "\
[default]
region = eu-west-1

[profile prod]
sso_session = corp
region=us-east-1

[profile dev]
output = json
";
        assert_eq!(
            parse(config, ""),
            vec![
                profile("default", Some("eu-west-1")),
                profile("dev", None),
                profile("prod", Some("us-east-1")),
            ]
        );
    }

    #[test]
    fn non_profile_config_sections_are_skipped() {
        let config = "\
[sso-session corp]
region = eu-west-2

[services local]
dynamodb =
  endpoint_url = http://localhost:8000

[profilex]
region = ap-south-1

[profile  spaced ]
";
        assert_eq!(parse(config, ""), vec![profile("spaced", None)]);
    }

    #[test]
    fn credentials_only_profiles_are_listed_without_region() {
        let credentials = "\
[default]
aws_access_key_id = x
[ci]
aws_access_key_id = y
";
        assert_eq!(
            parse("", credentials),
            vec![profile("default", None), profile("ci", None)]
        );
    }

    #[test]
    fn config_region_survives_a_duplicate_credentials_section() {
        let config = "[profile ci]\nregion = eu-west-1\n";
        let credentials = "[ci]\naws_access_key_id = y\n";
        assert_eq!(
            parse(config, credentials),
            vec![profile("ci", Some("eu-west-1"))]
        );
    }

    #[test]
    fn comments_and_blank_regions_are_ignored() {
        let config = "\
# region = nope
[profile a]
; region = nope
region =
";
        assert_eq!(parse(config, ""), vec![profile("a", None)]);
    }

    #[test]
    fn missing_files_contribute_nothing() {
        assert!(parse("", "").is_empty());
        let missing = Path::new("/definitely/not/here/config");
        assert!(read_or_empty(missing).unwrap().is_empty());
    }
}
