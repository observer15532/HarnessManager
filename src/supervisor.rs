//! Spawning and supervising one `dsh web` process tree.
//!
//! The child gets a hidden console (`CREATE_NO_WINDOW`) so nothing flashes, and
//! is attached to a job object so stopping it takes the MCP servers and tool
//! subprocesses down with it.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::win::Job;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const TAIL_LINES: usize = 10;

pub struct Launch {
    pub profile: String,
    pub port: u16,
    pub cwd: PathBuf,
    pub node: String,
    pub bin_js: PathBuf,
    pub home: PathBuf,
    pub log: PathBuf,
}

pub struct Running {
    child: Child,
    job: Option<Job>,
    pub pid: u32,
    pub profile: String,
    pub port: u16,
    pub token_url: Option<String>,
    pub stderr_tail: Arc<Mutex<VecDeque<String>>>,
    pub started: Instant,
    pub url_rx: Receiver<String>,
}

impl Running {
    pub fn plain_url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    /// Prefer the tokenised URL so a fresh browser (or an expired cookie) still
    /// gets in; fall back to the plain origin, which is enough for the 30-day
    /// session cookie dsh already handed the browser.
    pub fn open_url(&self) -> String {
        self.token_url
            .clone()
            .unwrap_or_else(|| self.plain_url())
    }

    pub fn poll_url(&mut self) -> bool {
        let mut found = false;
        while let Ok(url) = self.url_rx.try_recv() {
            if self.token_url.is_none() {
                self.token_url = Some(url);
                found = true;
            }
        }
        found
    }

    /// `Some(message)` once the process is gone.
    pub fn poll_exit(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                let code = status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "?".to_string());
                Some(format!("退出码 {code}{}", self.tail_suffix()))
            }
            _ => None,
        }
    }

    fn tail_suffix(&self) -> String {
        let tail = match self.stderr_tail.lock() {
            Ok(tail) => tail.iter().cloned().collect::<Vec<_>>().join(" | "),
            Err(_) => String::new(),
        };
        if tail.is_empty() {
            String::new()
        } else {
            format!(" · {tail}")
        }
    }

    /// Terminate the whole job and wait for the root process to settle.
    ///
    /// This is a hard kill by design: dsh is a console process with no signal
    /// channel we can reach from a GUI parent, and the alternative (orphaned
    /// MCP servers holding the port) is worse than losing an in-flight turn.
    pub fn stop(mut self) {
        if let Some(job) = self.job.as_ref() {
            job.terminate();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(80));
                }
                _ => {
                    // Job assignment can fail when the parent is already inside
                    // another job (agents, terminals); fall back to taskkill.
                    if self.job.is_none() {
                        let _ = crate::dsh::run_shell(&format!(
                            "taskkill /PID {} /T /F",
                            self.pid
                        ));
                    }
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        // `job` drops here, closing the handle; the tree is already dead.
    }
}

pub fn launch(spec: Launch) -> Result<Running, String> {
    use std::os::windows::process::CommandExt;

    if let Some(parent) = spec.log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.log)
        .map_err(|error| format!("无法创建日志 {}：{error}", spec.log.display()))?;
    let log = Arc::new(Mutex::new(file));

    write_log(
        &log,
        &format!(
            "\n=== launch {} profile={} port={} cwd={} ===\n  {} {} --profile {} --port {} --no-open",
            now(),
            spec.profile,
            spec.port,
            spec.cwd.display(),
            spec.node,
            spec.bin_js.display(),
            spec.profile,
            spec.port
        ),
    );

    let mut command = Command::new(&spec.node);
    command
        .arg(&spec.bin_js)
        .arg("--profile")
        .arg(&spec.profile)
        .arg("--port")
        .arg(spec.port.to_string())
        .arg("--no-open")
        .current_dir(&spec.cwd)
        .env("DSH_HOME", &spec.home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);

    let mut child = command
        .spawn()
        .map_err(|error| format!("无法启动 {}：{error}", spec.node))?;
    let pid = child.id();

    let job = Job::new();
    let assigned = match job.as_ref() {
        Some(job) => job.assign(child.as_raw_handle() as usize),
        None => false,
    };
    if !assigned {
        write_log(&log, "warn: 无法把 dsh 加入 job object，停止时将退化为 taskkill /T\n");
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (url_tx, url_rx) = channel::<String>();
    let tail = Arc::new(Mutex::new(VecDeque::with_capacity(TAIL_LINES)));

    if let Some(stdout) = stdout {
        pump(stdout, log.clone(), None, Some(url_tx));
    }
    if let Some(stderr) = stderr {
        pump(stderr, log.clone(), Some(tail.clone()), None);
    }

    Ok(Running {
        child,
        job,
        pid,
        profile: spec.profile,
        port: spec.port,
        token_url: None,
        stderr_tail: tail,
        started: Instant::now(),
        url_rx,
    })
}

/// Drain one of the child's pipes on its own thread; not draining would block
/// the child as soon as the pipe buffer fills.
fn pump<R: Read + Send + 'static>(
    reader: R,
    log: Arc<Mutex<File>>,
    tail: Option<Arc<Mutex<VecDeque<String>>>>,
    urls: Option<Sender<String>>,
) {
    std::thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            // The URL carries a process credential; the log must not keep it.
            write_log(&log, &redact(&line));
            if let Some(tail) = tail.as_ref() {
                if let Ok(mut tail) = tail.lock() {
                    if tail.len() == TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line.clone());
                }
            }
            if let (Some(sender), Some(url)) = (urls.as_ref(), parse_url(&line)) {
                let _ = sender.send(url);
            }
        }
    });
}

/// `dsh web: http://127.0.0.1:3080/?token=...`
fn parse_url(line: &str) -> Option<String> {
    let rest = line.strip_prefix("dsh web: ")?;
    let url = rest.trim();
    if url.starts_with("http://") || url.starts_with("https://") {
        Some(url.to_string())
    } else {
        None
    }
}

fn redact(line: &str) -> String {
    match line.find("token=") {
        Some(index) => format!("{}<redacted>", &line[..index + 6]),
        None => line.to_string(),
    }
}

fn write_log(log: &Arc<Mutex<File>>, line: &str) {
    if let Ok(mut file) = log.lock() {
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
}

fn now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// True when something already listens on the port (usually a dsh the user
/// started by hand). The launcher must not kill it.
pub fn port_open(port: u16) -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_ok()
}

pub fn log_path_for(profile: &str) -> PathBuf {
    let dir = crate::config::logs_dir();
    let _ = std::fs::create_dir_all(&dir);
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let safe: String = profile
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect();
    dir.join(format!("dsh-{safe}-{stamp}.log"))
}
