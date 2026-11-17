#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! dsh-launcher — a tray resident supervisor for one `dsh web` at a time.

mod config;
mod dialog;
mod dsh;
mod icon;
mod supervisor;
mod win;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};

use config::{Config, Profile};
use supervisor::Running;

const APP_TITLE: &str = "HarnessManager";
const SINGLETON: &str = "harness-manager-singleton-v1";
const RUN_KEY: &str = "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "HarnessManager";
const LEGACY_RUN_VALUE: &str = "DSH Launcher";

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Stopped,
    Starting,
    Running,
    Busy,
}

#[derive(Clone, Copy)]
enum JobKind {
    CheckUpdate,
    Update,
}

struct Done {
    kind: JobKind,
    ok: bool,
    message: String,
    remote: Option<dsh::Remote>,
    new_version: Option<String>,
    old_version: Option<String>,
}

impl Done {
    fn simple(kind: JobKind, ok: bool, message: String) -> Done {
        Done {
            kind,
            ok,
            message,
            remote: None,
            new_version: None,
            old_version: None,
        }
    }
}

struct App {
    config: Config,
    state: State,
    running: Option<Running>,
    tray: TrayIcon,
    /// Last thing worth telling the user, shown as a disabled menu line.
    last: String,
    busy_rx: Option<Receiver<Done>>,
    update_available: Option<String>,
    remote: Option<dsh::Remote>,
    autostart: bool,
    pending_restart: Option<String>,
    /// Something already answers on the port and it is not our child — almost
    /// always a `dsh web` the user started by hand. Never kill it; just offer
    /// to open it.
    external: bool,
    last_probe: Instant,
    status_logged: String,
    exit: bool,
}

struct MenuState<'a> {
    state: State,
    status: &'a str,
    last: &'a str,
    profiles: &'a [Profile],
    current: Option<&'a str>,
    update_available: Option<&'a str>,
    autostart: bool,
    external: bool,
}

fn main() {
    // Renamed from dsh-launcher: move an existing config and log history over
    // before anything reads them.
    config::migrate_legacy_layout();

    if let Some(code) = run_cli() {
        std::process::exit(code);
    }

    if win::acquire_single_instance(SINGLETON).is_none() {
        // Already running: a second launch is a request to open the UI.
        let (config, _) = Config::load_or_create();
        win::shell_open_url(&format!("http://127.0.0.1:{}/", config.port));
        return;
    }

    let (config, notice) = Config::load_or_create();
    migrate_legacy_autostart();
    let mut app = match App::new(config, notice) {
        Ok(app) => app,
        Err(error) => {
            win::message_box(&error, APP_TITLE, win::MB_OK | win::MB_ICONERROR);
            return;
        }
    };
    app.run();
}

/// Small headless surface, mostly so the registry side can be tested and
/// scripted without clicking a tray menu.
fn run_cli() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first()?.as_str();

    match command {
        "--autostart" => match args.get(1).map(String::as_str) {
            None | Some("status") => {
                println!("{}", if autostart_enabled() { "enabled" } else { "disabled" });
                Some(0)
            }
            Some("on") => match set_autostart(true) {
                Ok(()) => {
                    println!("enabled");
                    Some(0)
                }
                Err(error) => {
                    eprintln!("{error}");
                    Some(2)
                }
            },
            Some("off") => match set_autostart(false) {
                Ok(()) => {
                    println!("disabled");
                    Some(0)
                }
                Err(error) => {
                    eprintln!("{error}");
                    Some(2)
                }
            },
            Some(other) => {
                eprintln!("unknown autostart action: {other}");
                Some(2)
            }
        },
        "profile" => {
            let (mut config, _) = Config::load_or_create();
            match args.get(1).map(String::as_str) {
                None | Some("list") => {
                    if config.profiles.is_empty() {
                        println!("(no profiles)");
                    }
                    for profile in &config.profiles {
                        let marker = if config.current.as_deref() == Some(profile.name.as_str()) {
                            "*"
                        } else {
                            " "
                        };
                        println!(
                            "{marker} {:<24} {}",
                            profile.name,
                            profile
                                .cwd
                                .clone()
                                .unwrap_or_else(|| "(cwd not set)".to_string())
                        );
                    }
                    Some(0)
                }
                Some("add") => {
                    let Some(name) = args.get(2) else {
                        eprintln!("usage: profile add <name> [--cwd <dir>]");
                        return Some(2);
                    };
                    if let Err(error) = validate_profile_name(name) {
                        eprintln!("{error}");
                        return Some(2);
                    }
                    if config.profiles.iter().any(|profile| profile.name == *name) {
                        eprintln!("profile {name} already exists");
                        return Some(2);
                    }
                    let cwd = flag_value(&args, "--cwd");
                    match create_profile(&mut config, name, cwd.as_deref()) {
                        Ok(()) => {
                            match cwd {
                                Some(path) => println!("added {name} -> {path}"),
                                None => println!("added {name} (no workspace)"),
                            }
                            Some(0)
                        }
                        Err(error) => {
                            eprintln!("{error}");
                            Some(2)
                        }
                    }
                }
                Some("remove") => {
                    let Some(name) = args.get(2) else {
                        eprintln!("usage: profile remove <name>");
                        return Some(2);
                    };
                    match delete_profile(&mut config, name) {
                        Ok(()) => {
                            println!("removed {name}");
                            Some(0)
                        }
                        Err(error) => {
                            eprintln!("{error}");
                            Some(2)
                        }
                    }
                }
                Some(other) => {
                    eprintln!("unknown profile action: {other}");
                    Some(2)
                }
            }
        }
        "--help" | "-h" => {
            println!("dsh-launcher [--autostart on|off|status]");
            println!("dsh-launcher profile [list | add <name> [--cwd <dir>] | remove <name>]");
            Some(0)
        }
        _ => None,
    }
}

impl App {
    fn new(config: Config, notice: Option<String>) -> Result<App, String> {
        let initial = MenuState {
            state: State::Stopped,
            status: "DeepSeek Harness · 未运行",
            last: "",
            profiles: &config.profiles,
            current: config.current.as_deref(),
            update_available: None,
            autostart: autostart_enabled(),
            external: false,
        };
        let menu = build_menu(&initial);

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(APP_TITLE)
            .with_icon(icon::disc(icon::GRAY))
            .with_menu_on_left_click(false)
            .build()
            .map_err(|error| format!("无法创建托盘图标：{error}"))?;

        let mut app = App {
            config,
            state: State::Stopped,
            running: None,
            tray,
            last: notice.unwrap_or_default(),
            busy_rx: None,
            update_available: None,
            remote: None,
            autostart: autostart_enabled(),
            pending_restart: None,
            external: false,
            last_probe: Instant::now(),
            status_logged: String::new(),
            exit: false,
        };
        app.refresh();
        log_event(&format!(
            "startup: autostart={} profiles={} current={}",
            app.autostart,
            app.config.profiles.len(),
            app.config.current.clone().unwrap_or_default()
        ));
        Ok(app)
    }

    fn run(&mut self) {
        if self.config.start_on_launch {
            // At login nobody is looking at the screen: report through the
            // menu's status line, never through a modal dialog.
            self.start(false);
        }

        loop {
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if let TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } = event
                {
                    self.open_page();
                }
            }

            while let Ok(event) = MenuEvent::receiver().try_recv() {
                self.on_menu(&event.id.0);
                if self.exit {
                    break;
                }
            }

            self.poll();
            win::pump_messages();

            if self.exit {
                break;
            }
            thread::sleep(Duration::from_millis(40));
        }

        if let Some(running) = self.running.take() {
            running.stop();
        }
    }

    fn on_menu(&mut self, id: &str) {
        log_event(&format!("menu: {id}"));
        match id {
            "open" => self.open_page(),
            "toggle" => {
                if self.running.is_some() {
                    self.stop();
                } else {
                    self.start(true);
                }
            }
            "reload" => self.reload(),
            "quit" => self.exit = true,
            "open-config" => {
                let path = config::config_path();
                let _ = self.config.save();
                win::shell_open(&path);
            }
            "open-logs" => {
                let dir = config::logs_dir();
                let _ = std::fs::create_dir_all(&dir);
                win::shell_open(&dir);
            }
            "open-home" => {
                let home = config::harness_home(&self.config);
                win::shell_open(&home);
            }
            "check-update" => self.start_check_update(),
            "update-dsh" => self.start_update(),
            "profile-new" => self.new_profile(),
            "profile-delete" => self.delete_current_profile(),
            "autostart" => self.toggle_autostart(),
            other if other.starts_with("profile:") => {
                let name = other.trim_start_matches("profile:").to_string();
                self.select_profile(&name);
            }
            _ => {}
        }
    }

    fn poll(&mut self) {
        let mut dirty = false;

        // Idle probe: notice a dsh the user started by hand (or stopped by
        // hand) without waiting for the next menu click.
        if self.running.is_none()
            && self.state == State::Stopped
            && self.last_probe.elapsed() > Duration::from_secs(2)
        {
            self.last_probe = Instant::now();
            let external = supervisor::port_open(self.config.port);
            if external != self.external {
                self.external = external;
                dirty = true;
            }
        }

        if let Some(running) = self.running.as_mut() {
            if running.poll_url() && self.state == State::Starting {
                self.state = State::Running;
                self.last = format!("已就绪 · {}", running.plain_url());
                dirty = true;
            }
            if self.state == State::Starting
                && running.started.elapsed() > Duration::from_secs(30)
                && supervisor::port_open(running.port)
            {
                self.state = State::Running;
                self.last = format!("已就绪 · {}", running.plain_url());
                dirty = true;
            }
        }

        let exited = self
            .running
            .as_mut()
            .and_then(|running| running.poll_exit());
        if let Some(message) = exited {
            self.running = None;
            self.state = State::Stopped;
            self.last = format!("dsh 已退出（{message}）");
            dirty = true;
        }

        if let Some(rx) = self.busy_rx.as_ref() {
            match rx.try_recv() {
                Ok(done) => {
                    self.busy_rx = None;
                    self.apply_done(done);
                    dirty = true;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.busy_rx = None;
                    self.state = State::Stopped;
                    self.last = "后台任务意外中断".to_string();
                    dirty = true;
                }
            }
        }

        if dirty {
            self.refresh();
        }
    }

    // ---- dsh process ------------------------------------------------

    fn start(&mut self, interactive: bool) {
        if self.busy_rx.is_some() {
            return;
        }
        if self.running.is_some() {
            return;
        }

        let Some(profile) = self.config.current_profile().cloned() else {
            self.last = "没有可用的配置方案".to_string();
            self.refresh();
            return;
        };

        let Some(dir) = dsh::package_dir(&self.config) else {
            self.last = "找不到已安装的 dsh（@deepseek-ai/dsh）".to_string();
            if interactive {
                win::message_box(
                    "找不到已安装的 dsh。\n\n请先在终端执行：\nnpm install -g @deepseek-ai/dsh@next",
                    APP_TITLE,
                    win::MB_OK | win::MB_ICONERROR,
                );
            }
            self.refresh();
            return;
        };

        // Node is checked here, on every start, rather than from a menu item:
        // a version mismatch is only ever interesting right before we spawn.
        if let Some(problem) = node_problem(&self.config, &dir) {
            self.last = problem.clone();
            if interactive {
                win::message_box(&problem, APP_TITLE, win::MB_OK | win::MB_ICONERROR);
            }
            self.refresh();
            return;
        }

        let port = self.config.port;
        if supervisor::port_open(port) {
            self.last = format!("端口 {port} 已被占用（可能是手动启动的 dsh）");
            self.refresh();
            return;
        }

        let workspace = match Config::workspace(&profile) {
            Some(path) => path,
            None => {
                // dsh does not need a workspace to boot: it treats its launch
                // directory as the default filesystem location, and the Web UI
                // asks the user to add and select a workspace anyway.
                let fallback = fallback_workspace();
                log_event(&format!(
                    "start {}: 未设置工作目录，使用 {}",
                    profile.name,
                    fallback.display()
                ));
                fallback
            }
        };
        if !workspace.exists() {
            self.last = format!("工作目录不存在：{}", workspace.display());
            self.refresh();
            return;
        }

        let spec = supervisor::Launch {
            profile: profile.name.clone(),
            port,
            cwd: workspace,
            node: dsh::node_executable(&self.config),
            bin_js: dir.join("lib").join("bin.js"),
            home: config::harness_home(&self.config),
            log: supervisor::log_path_for(&profile.name),
        };

        match supervisor::launch(spec) {
            Ok(running) => {
                self.last = format!("启动 {} · 端口 {port}…", profile.name);
                self.running = Some(running);
                self.state = State::Starting;
            }
            Err(error) => {
                self.last = format!("启动失败：{error}");
                win::message_box(&error, APP_TITLE, win::MB_OK | win::MB_ICONERROR);
            }
        }
        self.refresh();
    }

    fn stop(&mut self) {
        let Some(running) = self.running.take() else {
            return;
        };
        let profile = running.profile.clone();
        self.state = State::Stopped;
        self.last = format!("正在停止 {profile}…");
        self.refresh();

        running.stop();

        self.last = format!("已停止 {profile}");
        self.refresh();
    }

    fn open_page(&mut self) {
        let Some(running) = self.running.as_ref() else {
            if supervisor::port_open(self.config.port) {
                win::shell_open_url(&format!("http://127.0.0.1:{}/", self.config.port));
            } else {
                self.last = "dsh 未运行".to_string();
                self.refresh();
            }
            return;
        };
        let url = running.open_url();
        if !win::shell_open_url(&url) {
            self.last = "无法打开浏览器".to_string();
            self.refresh();
        }
    }

    fn select_profile(&mut self, name: &str) {
        if self.config.current.as_deref() == Some(name) {
            return;
        }
        if !self.config.profiles.iter().any(|profile| profile.name == name) {
            return;
        }
        self.config.current = Some(name.to_string());
        let _ = self.config.save();

        if self.running.is_some() {
            self.stop();
            self.start(true);
        } else {
            self.last = format!("已切换到 {name}");
            self.refresh();
        }
    }

    fn reload(&mut self) {
        let (config, notice) = Config::load_or_create();
        self.config = config;
        self.autostart = autostart_enabled();
        self.last = notice.unwrap_or_else(|| "配置已重新加载".to_string());
        self.refresh();
    }

    // ---- maintenance ------------------------------------------------

    fn installed_version(&self) -> Option<String> {
        dsh::package_dir(&self.config).and_then(|dir| dsh::installed_version(&dir))
    }

    fn start_check_update(&mut self) {
        if self.busy_rx.is_some() {
            return;
        }
        let dist_tag = self.config.channel.clone();
        let worker_tag = dist_tag.clone();
        let (tx, rx) = channel::<Done>();
        thread::spawn(move || {
            let done = match dsh::remote_version(&worker_tag) {
                Ok(remote) => Done {
                    kind: JobKind::CheckUpdate,
                    ok: true,
                    message: String::new(),
                    remote: Some(remote),
                    new_version: None,
                    old_version: None,
                },
                Err(error) => Done::simple(JobKind::CheckUpdate, false, error),
            };
            let _ = tx.send(done);
        });
        self.busy_rx = Some(rx);
        self.state = State::Busy;
        self.last = format!("正在检查更新（通道 {dist_tag}）…");
        self.refresh();
    }

    // ---- profiles ---------------------------------------------------

    fn new_profile(&mut self) {
        let Some(raw_name) = dialog::input_text(
            "新建配置方案",
            "配置方案名（字母/数字/-/_）。profile 目录会自动创建，这里不用选路径。",
            "",
        ) else {
            return;
        };
        let name = raw_name.trim().to_string();
        if name.is_empty() {
            return;
        }
        if let Err(error) = validate_profile_name(&name) {
            self.last = error.clone();
            win::message_box(&error, APP_TITLE, win::MB_OK | win::MB_ICONERROR);
            self.refresh();
            return;
        }
        if self.config.profiles.iter().any(|profile| profile.name == name) {
            self.last = format!("配置方案 {name} 已存在");
            self.refresh();
            return;
        }

        // The second dialog is about the workspace, not about where the profile
        // lives: that directory is created by dsh under DSH_HOME.
        // The profile directory is created by dsh under DSH_HOME. No workspace
        // is asked for: dsh boots fine anywhere and the Web UI owns workspace
        // selection, so per-profile cwd is only an advanced config field.
        let profile_dir = config::harness_home(&self.config)
            .join("profiles")
            .join(&name);

        match create_profile(&mut self.config, &name, None) {
            Ok(()) => {
                self.last = format!("已新建 {name} · profile {}", profile_dir.display());
                log_event(&format!(
                    "profile add: {name} profile={}",
                    profile_dir.display()
                ));
            }
            Err(error) => {
                self.last = format!("新建失败：{error}");
                win::message_box(
                    &format!("新建配置方案失败：\n\n{error}"),
                    APP_TITLE,
                    win::MB_OK | win::MB_ICONERROR,
                );
            }
        }
        self.refresh();
    }

    fn delete_current_profile(&mut self) {
        let Some(profile) = self.config.current_profile().cloned() else {
            return;
        };
        let home = config::harness_home(&self.config);
        let dir = home.join("profiles").join(&profile.name);

        let answer = win::message_box(
            &format!(
                "删除配置方案 {}？\n\n会同时删除这个目录（不可恢复）：\n{}\n\n\
                 该方案安装的插件与配置都在里面。\n\
                 内置方案（如 web）会在下次启动时自动重建一个干净的。",
                profile.name,
                dir.display()
            ),
            APP_TITLE,
            win::MB_YESNO | win::MB_ICONQUESTION,
        );
        if answer != win::IDYES {
            return;
        }

        if self.running.is_some() {
            self.stop();
        }

        match delete_profile(&mut self.config, &profile.name) {
            Ok(()) => {
                self.last = format!("已删除 {}", profile.name);
                log_event(&format!("profile remove: {} ({})", profile.name, dir.display()));
            }
            Err(error) => {
                self.last = format!("删除失败：{error}");
                win::message_box(
                    &format!("删除配置方案失败：\n\n{error}"),
                    APP_TITLE,
                    win::MB_OK | win::MB_ICONERROR,
                );
            }
        }
        self.refresh();
    }

    fn start_update(&mut self) {
        if self.busy_rx.is_some() {
            return;
        }
        let Some(target) = self.update_available.clone() else {
            self.last = "没有已知的新版本，请先检查更新".to_string();
            self.refresh();
            return;
        };
        let Some(dir) = dsh::package_dir(&self.config) else {
            self.last = "找不到已安装的 dsh".to_string();
            self.refresh();
            return;
        };
        let old = dsh::installed_version(&dir).unwrap_or_else(|| "?".to_string());

        // Preflight: never install a dsh the local Node cannot run. Recovering
        // from that means downgrading again, which is exactly what this gate
        // exists to avoid.
        if let Some(remote) = self.remote.as_ref() {
            if let Some(required) = remote.node_req.as_ref() {
                if let Some(node) = dsh::node_version(&self.config) {
                    if !satisfies(&node, required) {
                        self.last = format!("Node {node} 不满足 {required}，已阻止更新");
                        win::message_box(
                            &format!(
                                "目标版本 {target} 要求 Node {required}，而当前是 {node}。\n\n\
                                 已阻止更新：先升级 Node，再来更新 dsh。"
                            ),
                            APP_TITLE,
                            win::MB_OK | win::MB_ICONERROR,
                        );
                        self.refresh();
                        return;
                    }
                }
            }
        }

        let answer = win::message_box(
            &format!(
                "将 dsh 从 {old} 更新到 {target}\n通道：{}\n\n会先停止正在运行的 dsh，更新后自动重启。",
                self.config.channel
            ),
            APP_TITLE,
            win::MB_YESNO | win::MB_ICONQUESTION,
        );
        if answer != win::IDYES {
            return;
        }

        if let Some(running) = self.running.take() {
            self.pending_restart = Some(running.profile.clone());
            running.stop();
        }
        self.state = State::Busy;
        self.last = format!("正在更新到 {target}…");
        self.refresh();

        let (tx, rx) = channel::<Done>();
        let install_target = target.clone();
        let rollback = old.clone();
        thread::spawn(move || {
            let done = match dsh::install(&install_target) {
                Ok(_) => {
                    let actual = dsh::installed_version(&dir).unwrap_or_default();
                    if actual == install_target {
                        Done {
                            kind: JobKind::Update,
                            ok: true,
                            message: String::new(),
                            remote: None,
                            new_version: Some(actual),
                            old_version: Some(rollback),
                        }
                    } else {
                        let _ = dsh::install(&rollback);
                        Done {
                            kind: JobKind::Update,
                            ok: false,
                            message: format!("安装后版本为 {actual}，期望 {install_target}"),
                            remote: None,
                            new_version: None,
                            old_version: Some(rollback),
                        }
                    }
                }
                Err(error) => {
                    let _ = dsh::install(&rollback);
                    Done {
                        kind: JobKind::Update,
                        ok: false,
                        message: short(&error),
                        remote: None,
                        new_version: None,
                        old_version: Some(rollback),
                    }
                }
            };
            let _ = tx.send(done);
        });
        self.busy_rx = Some(rx);
    }

    fn apply_done(&mut self, done: Done) {
        self.state = State::Stopped;
        match done.kind {
            JobKind::CheckUpdate => {
                if !done.ok {
                    self.last = format!("检查更新失败：{}", short(&done.message));
                } else if let Some(remote) = done.remote {
                    let installed = self.installed_version().unwrap_or_else(|| "?".to_string());
                    if is_newer(&installed, &remote.version) {
                        self.last = format!(
                            "发现 {} → {}（通道 {}）",
                            installed, remote.version, self.config.channel
                        );
                        self.update_available = Some(remote.version.clone());
                    } else {
                        self.last = format!(
                            "已是最新：{installed}（通道 {}）",
                            self.config.channel
                        );
                        self.update_available = None;
                    }
                    self.remote = Some(remote);
                }
            }
            JobKind::Update => {
                if done.ok {
                    let version = done.new_version.unwrap_or_default();
                    self.update_available = None;
                    self.remote = None;
                    self.last = format!("已更新到 {version}");
                    if let Some(profile) = self.pending_restart.take() {
                        self.config.current = Some(profile);
                        self.start(true);
                        return;
                    }
                } else {
                    self.last = format!(
                        "更新失败，已回滚到 {}：{}",
                        done.old_version.unwrap_or_else(|| "?".to_string()),
                        short(&done.message)
                    );
                }
            }
        }
        self.refresh();
    }

    fn toggle_autostart(&mut self) {
        let enabled = !self.autostart;
        match set_autostart(enabled) {
            Ok(()) => {
                self.autostart = enabled;
                self.last = if enabled {
                    "已开启开机自启".to_string()
                } else {
                    "已关闭开机自启".to_string()
                };
            }
            Err(error) => self.last = format!("设置开机自启失败：{error}"),
        }
        log_event(&format!("autostart -> {} ({})", self.autostart, self.last));
        self.refresh();
    }

    // ---- chrome -----------------------------------------------------

    fn status_text(&self) -> String {
        let profile = self
            .config
            .current_profile()
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| "无配置方案".to_string());
        let port = self.config.port;
        match self.state {
            State::Stopped if self.external => {
                format!("{APP_TITLE} · 已有实例 · 端口 {port}")
            }
            State::Stopped => format!("{APP_TITLE} · 未运行"),
            State::Starting => format!("{APP_TITLE} · 启动中 · {profile}"),
            State::Running => match self.running.as_ref() {
                Some(running) => format!(
                    "{APP_TITLE} · 运行中 · {} · {}",
                    running.profile, running.port
                ),
                None => format!("{APP_TITLE} · 运行中"),
            },
            State::Busy => format!("{APP_TITLE} · 维护中…"),
        }
    }

    fn refresh(&mut self) {
        self.external = self.running.is_none()
            && self.state == State::Stopped
            && supervisor::port_open(self.config.port);
        let status = self.status_text();
        if status != self.status_logged {
            log_event(&format!("state: {status}"));
            self.status_logged = status.clone();
        }
        let menu_state = MenuState {
            state: self.state,
            status: &status,
            last: &self.last,
            profiles: &self.config.profiles,
            current: self.config.current.as_deref(),
            update_available: self.update_available.as_deref(),
            autostart: self.autostart,
            external: self.external,
        };
        let menu = build_menu(&menu_state);
        self.tray.set_menu(Some(Box::new(menu)));

        let color = match self.state {
            State::Stopped => icon::GRAY,
            State::Starting => icon::AMBER,
            State::Running => icon::GREEN,
            State::Busy => icon::BLUE,
        };
        let _ = self.tray.set_icon(Some(icon::disc(color)));

        let tooltip = if self.last.is_empty() {
            status
        } else {
            format!("{status}\n{}", self.last)
        };
        let _ = self.tray.set_tooltip(Some(tooltip));
    }
}

fn build_menu(state: &MenuState) -> Menu {
    let menu = Menu::new();

    let status = MenuItem::with_id("status", state.status, false, None);
    let _ = menu.append(&status);

    if !state.last.is_empty() {
        let last = MenuItem::with_id("last", state.last, false, None);
        let _ = menu.append(&last);
    }

    let _ = menu.append(&PredefinedMenuItem::separator());

    let open = MenuItem::with_id(
        "open",
        "打开网页",
        state.state == State::Running || state.external,
        None,
    );
    let _ = menu.append(&open);

    let busy = state.state == State::Busy;
    let parked = state.external && state.state == State::Stopped;
    let toggle = MenuItem::with_id(
        "toggle",
        if parked {
            "启动（端口被占用）"
        } else if state.state == State::Stopped {
            "启动"
        } else {
            "停止"
        },
        !busy && !parked,
        None,
    );
    let _ = menu.append(&toggle);

    let _ = menu.append(&PredefinedMenuItem::separator());

    let profiles = Submenu::new("配置方案", true);
    for profile in state.profiles {
        let checked = state.current == Some(profile.name.as_str());
        // Text carries the state: a check mark alone has proven too easy to miss.
        let label = if checked {
            format!("● {}", profile.name)
        } else {
            format!("○ {}", profile.name)
        };
        let item = MenuItem::with_id(format!("profile:{}", profile.name), label, !busy, None);
        let _ = profiles.append(&item);
    }

    let _ = profiles.append(&PredefinedMenuItem::separator());
    let create = MenuItem::with_id("profile-new", "新建…", !busy, None);
    let _ = profiles.append(&create);
    let remove = MenuItem::with_id(
        "profile-delete",
        "删除当前方案…",
        !busy && state.current.is_some(),
        None,
    );
    let _ = profiles.append(&remove);

    let _ = menu.append(&profiles);

    let _ = menu.append(&PredefinedMenuItem::separator());

    let maintenance = Submenu::new("维护", true);
    let check = MenuItem::with_id("check-update", "检查更新…", !busy, None);
    let _ = maintenance.append(&check);

    let update_label = match state.update_available {
        Some(version) => format!("更新到 {version}"),
        None => "更新 dsh…".to_string(),
    };
    let update = MenuItem::with_id(
        "update-dsh",
        update_label,
        state.update_available.is_some() && !busy,
        None,
    );
    let _ = maintenance.append(&update);

    let _ = maintenance.append(&PredefinedMenuItem::separator());

    let config_item = MenuItem::with_id("open-config", "打开配置文件", true, None);
    let _ = maintenance.append(&config_item);
    let logs = MenuItem::with_id("open-logs", "打开日志目录", true, None);
    let _ = maintenance.append(&logs);
    let home = MenuItem::with_id("open-home", "打开 DSH 目录", true, None);
    let _ = maintenance.append(&home);
    let _ = menu.append(&maintenance);

    let _ = menu.append(&PredefinedMenuItem::separator());

    let autostart = MenuItem::with_id(
        "autostart",
        if state.autostart {
            "开机自启：已开启"
        } else {
            "开机自启：未开启"
        },
        true,
        None,
    );
    let _ = menu.append(&autostart);

    let reload = MenuItem::with_id("reload", "重新加载配置", true, None);
    let _ = menu.append(&reload);

    let quit = MenuItem::with_id("quit", "退出", true, None);
    let _ = menu.append(&quit);

    menu
}

// ---- helpers --------------------------------------------------------

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|argument| argument == flag)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

pub(crate) fn log_event(text: &str) {
    use std::io::Write;

    let dir = config::logs_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("launcher.log"))
    {
        let _ = writeln!(
            file,
            "{} {text}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
    }
}

fn short(text: &str) -> String {    let mut text = text.replace(['\r', '\n'], " ");
    text = text.trim().to_string();
    if text.chars().count() > 180 {
        text = text.chars().take(180).collect::<String>() + "…";
    }
    text
}

fn is_newer(installed: &str, remote: &str) -> bool {
    match (
        semver::Version::parse(installed),
        semver::Version::parse(remote),
    ) {
        (Ok(installed), Ok(remote)) => remote > installed,
        _ => installed != remote,
    }
}

fn satisfies(node_version: &str, requirement: &str) -> bool {
    let Ok(version) = semver::Version::parse(node_version) else {
        return true;
    };
    match semver::VersionReq::parse(requirement) {
        Ok(requirement) => requirement.matches(&version),
        Err(_) => true,
    }
}

/// `None` when the installed dsh can run on the local Node.
fn node_problem(config: &Config, package_dir: &Path) -> Option<String> {
    let Some(version) = dsh::node_version(config) else {
        return Some(
            "PATH 里找不到 node。\n\n如果 Node 装在自定义目录，请在配置文件里指定：\nnode = \"F:/Lang/NodeJS/node.exe\""
                .to_string(),
        );
    };
    let Some(required) = dsh::engines_node(package_dir) else {
        return None;
    };
    if satisfies(&version, &required) {
        return None;
    }
    Some(format!(
        "当前 Node {version}，而 dsh 要求 {required}。\n\n\
         launcher 不会自动升级 Node：它会影响你其它全局 npm 工具，\
         并让 profile 里的原生模块（如 better-sqlite3）失效。\n\n\
         请手动升级 Node 后重启 launcher：\nhttps://nodejs.org/zh-cn/download"
    ))
}

fn validate_profile_name(name: &str) -> Result<(), String> {
    if name.eq_ignore_ascii_case("desktop") {
        return Err("desktop 这个名字被 Electron 版独占，换一个".to_string());
    }
    if name.chars().count() > 64 {
        return Err("方案名过长".to_string());
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err("方案名只能包含字母、数字、- 和 _".to_string());
    }
    Ok(())
}

fn fallback_workspace() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// Create the profile directory first (from the shipped `web` template), and
/// only then record it in the config — a half-created profile is worse than a
/// failed one. `cwd` is optional: without it dsh still boots, and the workspace
/// is chosen inside the Web UI.
fn create_profile(config: &mut Config, name: &str, cwd: Option<&str>) -> Result<(), String> {
    let package_dir =
        dsh::package_dir(config).ok_or_else(|| "找不到已安装的 dsh".to_string())?;
    let home = config::harness_home(config);
    let bin_js = package_dir.join("lib").join("bin.js");
    let node = dsh::node_executable(config);

    let init_dir = match cwd {
        Some(path) => PathBuf::from(path),
        None => fallback_workspace(),
    };
    dsh::init_profile(&node, &bin_js, name, &home, &init_dir)?;

    config.upsert_profile(name, cwd.map(str::to_string));
    config.current = Some(name.to_string());
    config.save()
}

fn delete_profile(config: &mut Config, name: &str) -> Result<(), String> {
    let home = config::harness_home(config);
    let dir = home.join("profiles").join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|error| format!("删除 {} 失败：{error}", dir.display()))?;
    }
    config.drop_profile(name);
    config.save()
}

fn autostart_enabled() -> bool {
    dsh::run_exe("reg.exe", &["query", RUN_KEY, "/v", RUN_VALUE])
        .map(|(ok, _, _)| ok)
        .unwrap_or(false)
}

/// Write the Run value and read it back. A toggle that silently does nothing is
/// worse than no toggle at all, so the state is verified rather than assumed.
fn set_autostart(enabled: bool) -> Result<(), String> {
    if enabled {
        let exe = std::env::current_exe()
            .map_err(|error| error.to_string())?
            .display()
            .to_string();
        let value = format!("\"{exe}\"");
        let (ok, _, stderr) = dsh::run_exe(
            "reg.exe",
            &["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &value, "/f"],
        )?;
        if !ok {
            return Err(short(&stderr));
        }
        if !autostart_enabled() {
            return Err("写入后回读不到该值".to_string());
        }
        Ok(())
    } else {
        let (_, _, stderr) = dsh::run_exe("reg.exe", &["delete", RUN_KEY, "/v", RUN_VALUE, "/f"])?;
        let _ = dsh::run_exe(
            "reg.exe",
            &["delete", RUN_KEY, "/v", LEGACY_RUN_VALUE, "/f"],
        );
        if autostart_enabled() {
            return Err(short(&stderr));
        }
        Ok(())
    }
}

/// The previous name's Run value points at an exe that no longer exists. Carry
/// the intent over, then drop the stale entry.
fn migrate_legacy_autostart() {
    let legacy = dsh::run_exe("reg.exe", &["query", RUN_KEY, "/v", LEGACY_RUN_VALUE])
        .map(|(ok, _, _)| ok)
        .unwrap_or(false);
    if !legacy {
        return;
    }
    if !autostart_enabled() {
        let _ = set_autostart(true);
    }
    let _ = dsh::run_exe(
        "reg.exe",
        &["delete", RUN_KEY, "/v", LEGACY_RUN_VALUE, "/f"],
    );
    log_event("migrated autostart from the legacy Run value");
}
