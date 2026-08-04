use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::db::{now_ns, stable_hash};

#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub catalog_db: PathBuf,
    pub ui_dist: PathBuf,
    pub source_paths: SourcePaths,
    pub source_settings: SourceSettings,
    pub identity: Identity,
}

#[derive(Debug, Clone)]
pub struct SourcePaths {
    pub codex: PathBuf,
    pub pi: PathBuf,
    pub claude: PathBuf,
    pub kanade: PathBuf,
    pub amp: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct SourceSettings {
    disabled: HashSet<String>,
    unknown: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Identity {
    pub device_id: String,
    pub device_label: String,
    pub profile_id: String,
    pub profile_label: String,
    pub macos_uid: Option<String>,
    pub macos_username: String,
}

impl Paths {
    pub fn resolve(data_dir: Option<PathBuf>) -> Result<Self> {
        let data_dir = data_dir.unwrap_or_else(default_data_dir);
        let data_dir = expand_tilde(data_dir)?;
        let config_dir = config_dir_for_data_dir(&data_dir)?;
        let source_paths = SourcePaths::resolve(&config_dir)?;
        let source_settings = SourceSettings::resolve(&config_dir)?;
        let identity = Identity::resolve(&config_dir)?;
        let catalog_db = data_dir.join("catalog.sqlite");
        let ui_dist = env::current_dir()
            .context("resolve current directory")?
            .join("ui")
            .join("dist");

        Ok(Self {
            data_dir,
            config_dir,
            catalog_db,
            ui_dist,
            source_paths,
            source_settings,
            identity,
        })
    }
}

impl Identity {
    fn resolve(data_dir: &Path) -> Result<Self> {
        let mut config = ConfigFile::load(data_dir)?;
        let mut changed = false;
        let mut identity = config.identity.unwrap_or_default();

        let macos_username = current_username();
        let macos_uid = env::var("SHIRABE_MACOS_UID").ok().or_else(current_uid);
        let device_label = env::var("SHIRABE_DEVICE_LABEL")
            .ok()
            .or_else(current_device_label)
            .unwrap_or_else(|| "This device".to_string());
        let profile_label = env::var("SHIRABE_PROFILE_LABEL")
            .ok()
            .unwrap_or_else(|| macos_username.clone());

        let device_id =
            if let Some(value) = env::var("SHIRABE_DEVICE_ID").ok().filter(|v| !v.is_empty()) {
                value
            } else if let Some(value) = identity.device_id.take().filter(|v| !v.is_empty()) {
                value
            } else if let Some(seed) = current_machine_identifier_seed() {
                changed = true;
                generated_id("device", &seed)
            } else {
                changed = true;
                generated_id("device", &format!("{}:{}", data_dir.display(), now_ns()))
            };

        let profile_id = if let Some(value) = env::var("SHIRABE_PROFILE_ID")
            .ok()
            .filter(|v| !v.is_empty())
        {
            value
        } else if let Some(value) = identity.profile_id.take().filter(|v| !v.is_empty()) {
            value
        } else {
            changed = true;
            generated_id(
                "profile",
                &format!(
                    "{}:{}:{}",
                    device_id,
                    macos_uid.as_deref().unwrap_or(&macos_username),
                    now_ns()
                ),
            )
        };

        let device_label = identity
            .device_label
            .take()
            .filter(|v| !v.is_empty())
            .unwrap_or(device_label);
        let profile_label = identity
            .profile_label
            .take()
            .filter(|v| !v.is_empty())
            .unwrap_or(profile_label);

        if changed {
            config.identity = Some(IdentityConfig {
                device_id: Some(device_id.clone()),
                device_label: Some(device_label.clone()),
                profile_id: Some(profile_id.clone()),
                profile_label: Some(profile_label.clone()),
            });
            config.save(data_dir)?;
        }

        Ok(Self {
            device_id,
            device_label,
            profile_id,
            profile_label,
            macos_uid,
            macos_username,
        })
    }
}

impl SourcePaths {
    fn resolve(data_dir: &Path) -> Result<Self> {
        let config = ConfigFile::load(data_dir)?;
        let sources = config.sources.unwrap_or_default();

        Ok(Self {
            codex: configured_path(
                sources.codex,
                &["SHIRABE_CODEX_SESSIONS_DIR", "CODEX_SESSIONS_DIR"],
                &[
                    ("SHIRABE_CODEX_DIR", &["sessions"]),
                    ("CODEX_DIR", &["sessions"]),
                ],
                &[".codex", "sessions"],
            )?,
            pi: configured_path(
                sources.pi,
                &["SHIRABE_PI_SESSIONS_DIR", "PI_SESSIONS_DIR"],
                &[
                    ("SHIRABE_PI_DIR", &["agent", "sessions"]),
                    ("PI_DIR", &["agent", "sessions"]),
                ],
                &[".pi", "agent", "sessions"],
            )?,
            claude: configured_path(
                sources.claude,
                &["SHIRABE_CLAUDE_PROJECTS_DIR", "CLAUDE_PROJECTS_DIR"],
                &[
                    ("SHIRABE_CLAUDE_DIR", &["projects"]),
                    ("CLAUDE_DIR", &["projects"]),
                ],
                &[".claude", "projects"],
            )?,
            kanade: configured_path(
                sources.kanade,
                &["SHIRABE_KANADE_TRACES_DIR", "KANADE_TRACES_DIR"],
                &[
                    ("SHIRABE_KANADE_DIR", &["traces"]),
                    ("KANADE_DIR", &["traces"]),
                ],
                &[".kanade", "traces"],
            )?,
            amp: resolve_amp_paths_from_values(
                sources.amp,
                env::var_os("SHIRABE_AMP_THREADS_DIR").map(PathBuf::from),
                env::var("AMP_DATA_DIR").ok(),
                home_dir(),
            )?,
        })
    }
}

impl SourceSettings {
    fn resolve(data_dir: &Path) -> Result<Self> {
        let config = ConfigFile::load(data_dir)?;
        Ok(Self::from_values(
            config.disabled_sources,
            env::var("SHIRABE_DISABLED_SOURCES").ok(),
        ))
    }

    pub(crate) fn from_values(configured: Vec<String>, environment: Option<String>) -> Self {
        let values = environment
            .map(|value| value.split(',').map(str::to_owned).collect())
            .unwrap_or(configured);
        let known = ["codex", "pi", "claude", "kanade", "amp"];
        let mut seen = HashSet::new();
        let mut disabled = HashSet::new();
        let mut unknown = Vec::new();

        for value in values {
            let source = value.trim().to_lowercase();
            if source.is_empty() || !seen.insert(source.clone()) {
                continue;
            }
            if known.contains(&source.as_str()) {
                disabled.insert(source);
            } else {
                unknown.push(source);
            }
        }

        Self { disabled, unknown }
    }

    pub fn is_enabled(&self, source: &str) -> bool {
        !self.disabled.contains(&source.trim().to_lowercase())
    }

    pub fn unknown_sources(&self) -> &[String] {
        &self.unknown
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ConfigFile {
    #[serde(default)]
    sources: Option<SourcePathConfig>,
    #[serde(default)]
    disabled_sources: Vec<String>,
    #[serde(default)]
    identity: Option<IdentityConfig>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct SourcePathConfig {
    codex: Option<PathBuf>,
    pi: Option<PathBuf>,
    claude: Option<PathBuf>,
    kanade: Option<PathBuf>,
    amp: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct IdentityConfig {
    device_id: Option<String>,
    device_label: Option<String>,
    profile_id: Option<String>,
    profile_label: Option<String>,
}

impl ConfigFile {
    fn load(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("config.json");
        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
    }

    fn save(&self, data_dir: &Path) -> Result<()> {
        fs::create_dir_all(data_dir).with_context(|| format!("create {}", data_dir.display()))?;
        let path = data_dir.join("config.json");
        let raw = serde_json::to_string_pretty(self)?;
        fs::write(&path, format!("{raw}\n")).with_context(|| format!("write {}", path.display()))
    }
}

fn default_data_dir() -> PathBuf {
    if let Some(data_dir) = env_path("SHIRABE_DIR") {
        return data_dir;
    }

    #[cfg(windows)]
    {
        if let Some(app_data) = env_path("LOCALAPPDATA").or_else(|| env_path("APPDATA")) {
            return app_data.join("Shirabe");
        }
    }

    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".shirabe")
}

fn config_dir_for_data_dir(data_dir: &Path) -> Result<PathBuf> {
    config_dir_for_data_dir_with_home(
        data_dir,
        env::var_os("SHIRABE_CONFIG_DIR").map(PathBuf::from),
        home_dir(),
    )
}

fn config_dir_for_data_dir_with_home(
    data_dir: &Path,
    configured_override: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(config_dir) = configured_override {
        return expand_tilde_with_home(config_dir, home);
    }

    if is_shared_workspace_dir(data_dir) {
        let home = home.context("home directory is not set")?;
        return Ok(home.join(".shirabe"));
    }

    Ok(data_dir.to_path_buf())
}

pub fn is_shared_workspace_dir(data_dir: &Path) -> bool {
    data_dir.join("workspace.json").exists() || data_dir.starts_with(default_shared_workspace_dir())
}

#[cfg(not(windows))]
pub fn default_shared_workspace_dir() -> PathBuf {
    PathBuf::from("/Users/Shared/Shirabe")
}

#[cfg(windows)]
pub fn default_shared_workspace_dir() -> PathBuf {
    env_path("PROGRAMDATA")
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("Shirabe")
}

fn expand_tilde(path: PathBuf) -> Result<PathBuf> {
    expand_tilde_with_home(path, home_dir())
}

fn expand_tilde_with_home(path: PathBuf, home: Option<PathBuf>) -> Result<PathBuf> {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return home.context("home directory is not set");
    }

    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix(r"~\")) {
        let home = home.context("home directory is not set")?;
        return Ok(home.join(rest));
    }

    Ok(path)
}

fn configured_path(
    config_path: Option<PathBuf>,
    exact_envs: &[&str],
    root_envs: &[(&str, &[&str])],
    home_suffix: &[&str],
) -> Result<PathBuf> {
    if let Some(path) = config_path {
        return expand_tilde(path);
    }

    for env_name in exact_envs {
        if let Some(path) = env::var_os(env_name) {
            return expand_tilde(PathBuf::from(path));
        }
    }

    for (env_name, suffix) in root_envs {
        if let Some(root) = env::var_os(env_name) {
            return expand_tilde(join_segments(PathBuf::from(root), suffix));
        }
    }

    if let Some(home) = home_dir() {
        return Ok(join_segments(home, home_suffix));
    }

    Ok(join_segments(PathBuf::new(), home_suffix))
}

pub(crate) fn resolve_amp_paths_from_values(
    configured: Option<PathBuf>,
    threads_environment: Option<PathBuf>,
    data_environment: Option<String>,
    home: Option<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let configured = configured.filter(|path| !path.to_string_lossy().trim().is_empty());
    let threads_environment =
        threads_environment.filter(|path| !path.to_string_lossy().trim().is_empty());
    let mut candidates = if let Some(path) = configured {
        vec![path]
    } else if let Some(path) = threads_environment {
        vec![path]
    } else if let Some(value) = data_environment {
        value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| PathBuf::from(value).join("threads"))
            .collect()
    } else {
        Vec::new()
    };

    if candidates.is_empty() {
        candidates.push(
            home.clone()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local/share/amp/threads"),
        );
    }

    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for candidate in candidates {
        let path = expand_tilde_with_home(candidate, home.clone())?;
        if seen.insert(path.clone()) {
            paths.push(path);
        }
    }
    Ok(paths)
}

pub fn home_dir() -> Option<PathBuf> {
    env_path("HOME")
        .or_else(|| env_path("USERPROFILE"))
        .or_else(|| {
            let drive = env::var_os("HOMEDRIVE")?;
            let path = env::var_os("HOMEPATH")?;
            Some(PathBuf::from(format!(
                "{}{}",
                drive.to_string_lossy(),
                path.to_string_lossy()
            )))
        })
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn join_segments(mut path: PathBuf, segments: &[&str]) -> PathBuf {
    for segment in segments {
        path.push(segment);
    }
    path
}

fn generated_id(prefix: &str, seed: &str) -> String {
    format!("{prefix}_{}", &stable_hash(seed)[..16])
}

fn current_uid() -> Option<String> {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn current_username() -> String {
    env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .or_else(|_| env::var("USERNAME"))
        .unwrap_or_else(|_| "local".to_string())
}

fn current_device_label() -> Option<String> {
    env::var("COMPUTERNAME")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| env::var("HOSTNAME").ok().filter(|value| !value.is_empty()))
        .or_else(|| command_output("scutil", &["--get", "ComputerName"]))
        .or_else(|| command_output("hostname", &[]))
}

fn current_machine_identifier_seed() -> Option<String> {
    current_platform_uuid()
        .map(|uuid| format!("macos-platform-uuid:{uuid}"))
        .or_else(|| {
            env::var("COMPUTERNAME")
                .ok()
                .filter(|value| !value.is_empty())
                .map(|value| format!("windows-computer-name:{value}"))
        })
        .or_else(|| command_output("hostname", &[]).map(|value| format!("hostname:{value}")))
        .or_else(|| {
            Command::new("scutil")
                .args(["--get", "LocalHostName"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|value| format!("macos-local-hostname:{}", value.trim()))
                .filter(|value| !value.ends_with(':'))
        })
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn current_platform_uuid() -> Option<String> {
    Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|output| parse_platform_uuid(&output))
}

fn parse_platform_uuid(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (_, value) = line.split_once("\"IOPlatformUUID\"")?;
        let (_, value) = value.split_once('=')?;
        let value = value.trim().trim_matches('"');
        (!value.is_empty()).then(|| value.to_string())
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        SourceSettings, config_dir_for_data_dir_with_home, default_shared_workspace_dir,
        expand_tilde_with_home, is_shared_workspace_dir, parse_platform_uuid,
        resolve_amp_paths_from_values,
    };

    #[test]
    fn configured_sources_are_normalized_and_deduplicated() {
        let settings = SourceSettings::from_values(
            vec![" Codex ".into(), "CODEX".into(), " pi ".into()],
            None,
        );

        assert!(!settings.is_enabled("codex"));
        assert!(!settings.is_enabled("PI"));
        assert!(settings.is_enabled("claude"));
        assert!(settings.unknown_sources().is_empty());
    }

    #[test]
    fn environment_replaces_configured_sources() {
        let settings =
            SourceSettings::from_values(vec!["codex".into()], Some(" Claude, AMP, claude ".into()));

        assert!(settings.is_enabled("codex"));
        assert!(!settings.is_enabled("claude"));
        assert!(!settings.is_enabled("amp"));
    }

    #[test]
    fn explicit_empty_environment_enables_all_sources() {
        let settings = SourceSettings::from_values(vec!["codex".into()], Some(String::new()));

        for source in ["codex", "pi", "claude", "kanade", "amp"] {
            assert!(settings.is_enabled(source));
        }
    }

    #[test]
    fn reports_normalized_unknown_sources_without_disabling_them() {
        let settings = SourceSettings::from_values(
            vec![" Future ".into(), "future".into(), "OTHER".into()],
            None,
        );

        assert_eq!(settings.unknown_sources(), &["future", "other"]);
        assert!(settings.is_enabled("future"));
    }

    #[test]
    fn resolves_two_amp_data_roots_and_deduplicates_them() {
        let paths = resolve_amp_paths_from_values(
            None,
            None,
            Some(" /one, /two, /one, , ".into()),
            Some(PathBuf::from("/home/test")),
        )
        .unwrap();

        assert_eq!(
            paths,
            vec![PathBuf::from("/one/threads"), PathBuf::from("/two/threads")]
        );
    }

    #[test]
    fn expands_supported_tilde_separators_with_injected_home() {
        let home = Some(PathBuf::from("/home/test"));

        assert_eq!(
            expand_tilde_with_home(PathBuf::from("~/amp"), home.clone()).unwrap(),
            PathBuf::from("/home/test/amp")
        );
        assert_eq!(
            expand_tilde_with_home(PathBuf::from(r"~\amp"), home).unwrap(),
            PathBuf::from("/home/test/amp")
        );
    }

    #[test]
    fn preserves_literal_filenames_beginning_with_tilde() {
        let path = PathBuf::from("~amp");

        assert_eq!(
            expand_tilde_with_home(path.clone(), Some(PathBuf::from("/home/test"))).unwrap(),
            path
        );
    }

    #[test]
    fn amp_resolver_expands_configured_and_environment_tildes_with_injected_home() {
        let configured = resolve_amp_paths_from_values(
            Some(PathBuf::from("~/amp")),
            None,
            None,
            Some(PathBuf::from("/home/test")),
        )
        .unwrap();
        let environment = resolve_amp_paths_from_values(
            None,
            Some(PathBuf::from(r"~\amp")),
            None,
            Some(PathBuf::from("/home/test")),
        )
        .unwrap();

        assert_eq!(configured, vec![PathBuf::from("/home/test/amp")]);
        assert_eq!(environment, vec![PathBuf::from("/home/test/amp")]);
    }

    #[test]
    fn configured_amp_source_has_highest_precedence() {
        let paths = resolve_amp_paths_from_values(
            Some(PathBuf::from("/configured/threads")),
            Some(PathBuf::from("/environment/threads")),
            Some("/amp-data".into()),
            Some(PathBuf::from("/home/test")),
        )
        .unwrap();

        assert_eq!(paths, vec![PathBuf::from("/configured/threads")]);
    }

    #[test]
    fn shirabe_amp_threads_dir_precedes_amp_data_dir_and_default() {
        let paths = resolve_amp_paths_from_values(
            None,
            Some(PathBuf::from("/environment/threads")),
            Some("/amp-data".into()),
            Some(PathBuf::from("/home/test")),
        )
        .unwrap();

        assert_eq!(paths, vec![PathBuf::from("/environment/threads")]);
    }

    #[test]
    fn amp_source_defaults_to_home_local_share() {
        let paths =
            resolve_amp_paths_from_values(None, None, None, Some(PathBuf::from("/home/test")))
                .unwrap();

        assert_eq!(
            paths,
            vec![PathBuf::from("/home/test/.local/share/amp/threads")]
        );
    }

    #[test]
    fn empty_amp_environment_values_fall_through_to_default() {
        for threads in ["", "  \t"] {
            let paths = resolve_amp_paths_from_values(
                None,
                Some(PathBuf::from(threads)),
                Some(" , \t,  ".into()),
                Some(PathBuf::from("/home/test")),
            )
            .unwrap();

            assert_eq!(
                paths,
                vec![PathBuf::from("/home/test/.local/share/amp/threads")]
            );
        }
    }

    #[test]
    fn parses_ioreg_platform_uuid() {
        let output = r#"    "IOPlatformUUID" = "ABCDEF12-3456-7890-ABCD-EF1234567890""#;
        assert_eq!(
            parse_platform_uuid(output),
            Some("ABCDEF12-3456-7890-ABCD-EF1234567890".to_string())
        );
    }

    #[test]
    fn recognizes_default_shared_workspace() {
        assert!(is_shared_workspace_dir(&default_shared_workspace_dir()));
    }

    #[test]
    fn shared_workspace_uses_home_config_directory() {
        let workspace = default_shared_workspace_dir().join("team");
        let config_dir =
            config_dir_for_data_dir_with_home(&workspace, None, Some(PathBuf::from("/home/test")))
                .unwrap();

        assert_eq!(config_dir, PathBuf::from("/home/test/.shirabe"));
        assert_ne!(config_dir, workspace);
    }
}
