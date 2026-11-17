//! What the launcher needs to know about the installed dsh and Node.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::Config;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct Remote {
    pub version: String,
    pub node_req: Option<String>,
}

/// Run a command line through `cmd /C` with no console window and capture it.
///
/// npm ships as `npm.cmd`; CreateProcess cannot execute a `.cmd` directly, so
/// every package-manager call has to go through the shell.
pub fn run_shell(command_line: &str) -> Result<(bool, String, String), String> {
    use std::os::windows::process::CommandExt;

    let output = Command::new("cmd")
        .arg("/C")
        .arg(command_line)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| error.to_string())?;

    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

/// Run an executable directly with a separate argv array — no shell involved,
/// so paths and quoting are passed through verbatim.
pub fn run_exe(program: &str, args: &[&str]) -> Result<(bool, String, String), String> {
    use std::os::windows::process::CommandExt;

    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| error.to_string())?;

    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

/// Locate the globally installed `@deepseek-ai/dsh` package directory.
pub fn package_dir(config: &Config) -> Option<PathBuf> {
    if let Some(dir) = config.dsh_dir.as_ref() {
        let path = PathBuf::from(dir);
        if path.join("lib").join("bin.js").exists() {
            return Some(path);
        }
    }

    let mut candidates = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        candidates.push(
            PathBuf::from(appdata)
                .join("npm")
                .join("node_modules")
                .join("@deepseek-ai")
                .join("dsh"),
        );
    }
    if let Some(prefix) = std::env::var_os("NPM_CONFIG_PREFIX") {
        candidates.push(PathBuf::from(prefix).join("node_modules").join("@deepseek-ai").join("dsh"));
    }

    candidates
        .into_iter()
        .find(|path| path.join("lib").join("bin.js").exists())
}

/// Read the installed version straight from the package manifest — no npm call.
pub fn installed_version(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&text).ok()?;
    manifest
        .get("version")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// The Node range the *installed* dsh declares. Read locally so the check works
/// offline and always describes what is actually on disk.
pub fn engines_node(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&text).ok()?;
    manifest
        .get("engines")?
        .get("node")?
        .as_str()
        .map(str::to_string)
}

/// Create a profile from the shipped `web` template without booting a server.
///
/// `--from-default-profile` is a reset flag, so this runs exactly once per
/// profile, before it is added to the configuration; it must never end up in a
/// saved launch command.
pub fn init_profile(
    node: &str,
    bin_js: &Path,
    name: &str,
    home: &Path,
    cwd: &Path,
) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    let output = Command::new(node)
        .arg(bin_js)
        .args([
            "--profile",
            name,
            "--from-default-profile",
            "web",
            "--dump-config",
        ])
        .current_dir(cwd)
        .env("DSH_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("无法运行 dsh：{error}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let text: String = stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let text = text.trim();
    if text.is_empty() {
        Err("dsh 初始化 profile 失败".to_string())
    } else {
        Err(text.chars().take(300).collect())
    }
}

pub fn node_executable(config: &Config) -> String {
    config
        .node
        .clone()
        .unwrap_or_else(|| "node".to_string())
}

pub fn node_version(config: &Config) -> Option<String> {
    use std::os::windows::process::CommandExt;

    let output = Command::new(node_executable(config))
        .arg("-v")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Some(text.trim_start_matches('v').to_string())
}

/// Ask the registry which version the given dist-tag currently points at.
///
/// `latest` can be *older* than an installed pre-release, which is why the
/// launcher always shows the from → to pair before touching anything.
pub fn remote_version(channel: &str) -> Result<Remote, String> {
    let (ok, stdout, stderr) = run_shell(&format!(
        "npm view @deepseek-ai/dsh@{channel} version engines --json"
    ))?;
    if !ok {
        return Err(if stderr.is_empty() { stdout } else { stderr });
    }

    let value: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|error| format!("无法解析 npm 输出：{error}"))?;
    let version = value
        .get("version")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "npm 输出中没有 version 字段".to_string())?
        .to_string();
    let node_req = value
        .get("engines")
        .and_then(|engines| engines.get("node"))
        .and_then(|node| node.as_str())
        .map(str::to_string);

    Ok(Remote { version, node_req })
}

/// Install one exact version or dist-tag. Returns npm's combined output.
pub fn install(target: &str) -> Result<String, String> {
    let (ok, stdout, stderr) = run_shell(&format!(
        "npm install -g @deepseek-ai/dsh@{target} --no-fund --no-audit"
    ))?;
    let combined = format!("{stdout}\n{stderr}");
    if ok {
        Ok(combined)
    } else {
        Err(combined)
    }
}
