//! Launcher configuration: a small TOML file the user is expected to edit.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    /// Working directory dsh is launched in. DSH uses the invoking directory as
    /// the workspace root, so this must be explicit: an Explorer-launched
    /// process has no terminal cwd to inherit.
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_channel")]
    pub channel: String,
    /// Harness home. `None` means `%USERPROFILE%\.dsh` (or `$DSH_HOME`).
    #[serde(default)]
    pub dsh_home: Option<String>,
    /// Node executable. `None` means whatever `node` resolves to on PATH.
    #[serde(default)]
    pub node: Option<String>,
    /// `@deepseek-ai/dsh` package directory. `None` means auto-detect.
    #[serde(default)]
    pub dsh_dir: Option<String>,
    /// Start the last profile as soon as the launcher starts.
    #[serde(default = "default_true")]
    pub start_on_launch: bool,
    /// Last selected profile.
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

fn default_port() -> u16 {
    3080
}

fn default_channel() -> String {
    "next".to_string()
}

fn default_true() -> bool {
    true
}

const APP_DIR_NAME: &str = "HarnessManager";
const LEGACY_APP_DIR_NAME: &str = "dsh-launcher";

/// Skeleton used when no config file exists yet, so the first save keeps the
/// explanatory comments.
const HEADER: &str = "\
# HarnessManager configuration.
#
# cwd 是 dsh 启动时的工作目录（可选的高级项），不写则用 %USERPROFILE%。
# 不需要在这里配置 workspace：工作区在 Web UI 里添加和选择。
";

/// Replace the comment block left by the previous name, so a migrated file does
/// not keep explaining a rule that no longer exists.
fn normalize_header(text: &str) -> Option<String> {
    let mut lines = text.lines().peekable();
    if !lines.peek()?.starts_with("# dsh-launcher configuration") {
        return None;
    }
    while let Some(line) = lines.peek() {
        if line.trim_start().starts_with('#') {
            lines.next();
        } else {
            break;
        }
    }
    let body: Vec<&str> = lines.collect();
    let mut out = String::from(HEADER);
    out.push('\n');
    out.push_str(&body.join("\n"));
    out.push('\n');
    Some(out)
}

/// Skeleton used when no config file exists yet.
const TEMPLATE: &str = "\
# HarnessManager configuration.
#
# cwd 是 dsh 启动时的工作目录（可选的高级项），不写则用 %USERPROFILE%。
# 不需要在这里配置 workspace：工作区在 Web UI 里添加和选择。

port = 3080
channel = \"next\"
start_on_launch = true
";


impl Default for Config {
    fn default() -> Self {
        Config {
            port: default_port(),
            channel: default_channel(),
            dsh_home: None,
            node: None,
            dsh_dir: None,
            start_on_launch: true,
            current: None,
            profiles: Vec::new(),
        }
    }
}

pub fn app_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join(APP_DIR_NAME)
}

/// One-time move from the launcher's previous name, so an existing config and
/// log history survive the rename.
pub fn migrate_legacy_layout() {
    let Some(base) = std::env::var_os("APPDATA").map(PathBuf::from) else {
        return;
    };
    let legacy = base.join(LEGACY_APP_DIR_NAME);
    let current = base.join(APP_DIR_NAME);
    if current.exists() || !legacy.exists() {
        return;
    }
    if std::fs::rename(&legacy, &current).is_err() {
        // Rename fails when a log file is still open; copy instead.
        if copy_tree(&legacy, &current).is_err() {
            return;
        }
        let _ = std::fs::remove_dir_all(&legacy);
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

pub fn config_path() -> PathBuf {
    app_dir().join("config.toml")
}

pub fn logs_dir() -> PathBuf {
    app_dir().join("logs")
}

pub fn harness_home(config: &Config) -> PathBuf {
    if let Some(home) = config.dsh_home.as_ref() {
        return PathBuf::from(home);
    }
    if let Some(home) = std::env::var_os("DSH_HOME") {
        return PathBuf::from(home);
    }
    let profile = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    profile.join(".dsh")
}

impl Config {
    /// Load the config, creating a default one (from discovered profiles) on
    /// first run. A malformed file is kept as `config.toml.bak` and replaced.
    pub fn load_or_create() -> (Config, Option<String>) {
        let path = config_path();
        let mut notice = None;

        let mut config = match fs::read_to_string(&path) {
            Ok(text) => {
                let text = match normalize_header(&text) {
                    Some(fixed) => {
                        let _ = fs::write(&path, &fixed);
                        fixed
                    }
                    None => text,
                };
                match toml::from_str::<Config>(&text) {
                    Ok(config) => config,
                    Err(error) => {
                        let backup = path.with_extension("toml.bak");
                        let _ = fs::copy(&path, &backup);
                        notice = Some(format!(
                            "配置文件无法解析（{error}），已备份为 {} 并重建。",
                            backup.display()
                        ));
                        Config::default()
                    }
                }
            }
            Err(_) => Config::default(),
        };

        let before = config.profiles.len();
        merge_discovered_profiles(&mut config);
        if config.current.is_none() {
            // Prefer the stock `web` profile; anything else is someone's
            // experiment and a surprising default at login.
            config.current = config
                .profiles
                .iter()
                .find(|profile| profile.name == "web")
                .or_else(|| config.profiles.first())
                .map(|profile| profile.name.clone());
        }

        if before != config.profiles.len() || !path.exists() {
            if let Err(error) = config.save() {
                notice = Some(format!("无法写入配置文件：{error}"));
            }
        }

        (config, notice)
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }

        // Edit the existing document in place so hand-written comments and key
        // order survive a save (the launcher saves on every profile change).
        let existing = fs::read_to_string(&path).unwrap_or_else(|_| TEMPLATE.to_string());
        let mut document = existing
            .parse::<toml_edit::DocumentMut>()
            .unwrap_or_else(|_| TEMPLATE.parse().expect("built-in template is valid TOML"));

        let table = document.as_table_mut();
        table["port"] = toml_edit::value(i64::from(self.port));
        table["channel"] = toml_edit::value(self.channel.as_str());
        table["start_on_launch"] = toml_edit::value(self.start_on_launch);

        for (key, value) in [
            ("current", self.current.as_deref()),
            ("dsh_home", self.dsh_home.as_deref()),
            ("node", self.node.as_deref()),
            ("dsh_dir", self.dsh_dir.as_deref()),
        ] {
            match value {
                Some(value) => table[key] = toml_edit::value(value),
                None => {
                    table.remove(key);
                }
            }
        }

        let mut profiles = toml_edit::ArrayOfTables::new();
        for profile in &self.profiles {
            let mut entry = toml_edit::Table::new();
            entry["name"] = toml_edit::value(profile.name.as_str());
            if let Some(cwd) = profile.cwd.as_deref() {
                entry["cwd"] = toml_edit::value(cwd);
            }
            profiles.push(entry);
        }
        table["profiles"] = toml_edit::Item::ArrayOfTables(profiles);

        fs::write(&path, document.to_string()).map_err(|error| error.to_string())
    }

    /// Add a profile, or update the workspace of an existing one.
    /// Returns `true` when a new entry was created.
    pub fn upsert_profile(&mut self, name: &str, cwd: Option<String>) -> bool {
        if let Some(existing) = self
            .profiles
            .iter_mut()
            .find(|profile| profile.name == name)
        {
            if cwd.is_some() {
                existing.cwd = cwd;
            }
            return false;
        }
        self.profiles.push(Profile {
            name: name.to_string(),
            cwd,
        });
        true
    }

    /// Drop a profile entry and keep `current` pointing at something valid.
    pub fn drop_profile(&mut self, name: &str) -> bool {
        let before = self.profiles.len();
        self.profiles.retain(|profile| profile.name != name);
        let removed = self.profiles.len() != before;
        if removed && self.current.as_deref() == Some(name) {
            self.current = self.profiles.first().map(|profile| profile.name.clone());
        }
        removed
    }

    pub fn current_profile(&self) -> Option<&Profile> {
        let current = self.current.as_deref()?;
        self.profiles.iter().find(|profile| profile.name == current)
    }

    /// The workspace directory dsh boots in. `None` means the profile has not
    /// been given one yet, and starting it would drop dsh into an arbitrary
    /// directory, so the launcher refuses instead.
    pub fn workspace(profile: &Profile) -> Option<PathBuf> {
        profile.cwd.as_ref().map(PathBuf::from)
    }
}

/// Add profiles that exist on disk but are missing from the config file.
pub fn merge_discovered_profiles(config: &mut Config) {
    let home = harness_home(config);
    for name in discover(&home) {
        if !config.profiles.iter().any(|profile| profile.name == name) {
            config.profiles.push(Profile { name, cwd: None });
        }
    }
    if config.profiles.is_empty() {
        config.profiles.push(Profile {
            name: "web".to_string(),
            cwd: None,
        });
    }
}

pub fn discover(home: &Path) -> Vec<String> {
    let mut names = BTreeSet::new();
    if let Ok(entries) = fs::read_dir(home.join("profiles")) {
        for entry in entries.flatten() {
            if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            // `desktop` is reserved by the Electron application; `node_modules`
            // and dotted names are pnpm/workspace artefacts, not profiles.
            if name == "desktop" || name == "node_modules" || name.starts_with('.') {
                continue;
            }
            // A real profile carries a manifest or a composed patch file.
            let dir = entry.path();
            if dir.join("package.json").exists() || dir.join("cordis.yml").exists() {
                names.insert(name);
            }
        }
    }
    names.into_iter().collect()
}
