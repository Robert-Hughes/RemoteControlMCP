use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

const PROFILE_NAME: &str = "remote-control-mcp";
const KEY_FILE_NAME: &str = "remote-control-mcp.key";
const TUNNEL_CLIENT_PATH_FILE: &str = "tunnel-client-path.txt";
const START_AUTOMATICALLY_FILE: &str = "start-tunnel-automatically.txt";
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub enum TunnelLaunchEvent {
    Ready,
    Failed(String),
}

pub struct TunnelLaunch {
    event_rx: Receiver<TunnelLaunchEvent>,
    cancel_tx: Sender<()>,
    worker: Option<JoinHandle<()>>,
    log_path: PathBuf,
}

impl TunnelLaunch {
    pub fn try_recv(&self) -> Result<TunnelLaunchEvent, TryRecvError> {
        self.event_rx.try_recv()
    }

    pub fn log_path(&self) -> &Path {
        &self.log_path
    }
}

impl Drop for TunnelLaunch {
    fn drop(&mut self) {
        let _ = self.cancel_tx.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn start_tunnel(mcp_endpoint: &str) -> Result<TunnelLaunch, String> {
    let config_dir = config_directory()?;
    let key_path = config_dir.join("tunnel-client").join(KEY_FILE_NAME);
    validate_key_file(&key_path)?;

    let tunnel_client = resolve_tunnel_client(&config_dir)?;
    let runtime_directory = crate::runtime_environment::application_temp_directory();
    fs::create_dir_all(&runtime_directory).map_err(|error| {
        format!(
            "Could not create tunnel runtime directory {}: {error}",
            runtime_directory.display()
        )
    })?;

    let launch_id = launch_id();
    let health_url_path = runtime_directory.join(format!("tunnel-health-{launch_id}.url"));
    let log_path = runtime_directory.join(format!("tunnel-client-{launch_id}.log"));
    let _ = fs::remove_file(&health_url_path);

    let (event_tx, event_rx) = mpsc::channel();
    let (cancel_tx, cancel_rx) = mpsc::channel();
    let worker_log_path = log_path.clone();
    let worker_mcp_endpoint = mcp_endpoint.to_string();
    let worker = thread::Builder::new()
        .name("tunnel_launcher".to_string())
        .spawn(move || {
            run_tunnel_launcher(
                tunnel_client,
                worker_mcp_endpoint,
                key_path,
                health_url_path,
                worker_log_path,
                event_tx,
                cancel_rx,
            );
        })
        .map_err(|error| format!("Could not start the tunnel launcher worker: {error}"))?;

    Ok(TunnelLaunch {
        event_rx,
        cancel_tx,
        worker: Some(worker),
        log_path,
    })
}

pub fn load_start_automatically() -> Result<bool, String> {
    load_start_automatically_from(&config_directory()?)
}

pub fn save_start_automatically(enabled: bool) -> Result<(), String> {
    save_start_automatically_to(&config_directory()?, enabled)
}

fn load_start_automatically_from(config_dir: &Path) -> Result<bool, String> {
    let path = launcher_config_directory(config_dir).join(START_AUTOMATICALLY_FILE);
    let value = match fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "Could not read the automatic tunnel setting {}: {error}",
                path.display()
            ));
        }
    };

    match value.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!(
            "The automatic tunnel setting {} must contain either true or false.",
            path.display()
        )),
    }
}

fn save_start_automatically_to(config_dir: &Path, enabled: bool) -> Result<(), String> {
    let directory = launcher_config_directory(config_dir);
    fs::create_dir_all(&directory).map_err(|error| {
        format!(
            "Could not create the application configuration directory {}: {error}",
            directory.display()
        )
    })?;
    let path = directory.join(START_AUTOMATICALLY_FILE);
    fs::write(&path, if enabled { "true\n" } else { "false\n" }).map_err(|error| {
        format!(
            "Could not save the automatic tunnel setting {}: {error}",
            path.display()
        )
    })
}

fn launcher_config_directory(config_dir: &Path) -> PathBuf {
    config_dir.join("RemoteControlMCP")
}

fn config_directory() -> Result<PathBuf, String> {
    // Mirror the tunnel-client profile directory resolution so the key file
    // lands next to the profiles it is used with: XDG_CONFIG_HOME, then
    // ~/.config, then the OS user-config directory (e.g. %APPDATA%).
    if let Some(xdg_config_home) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg_config_home));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".config"));
    }
    dirs::config_dir().ok_or_else(|| {
        "The user configuration directory could not be determined, so the tunnel configuration cannot be located.".to_string()
    })
}

fn validate_key_file(key_path: &Path) -> Result<(), String> {
    let metadata = fs::metadata(key_path).map_err(|error| {
        format!(
            "The tunnel runtime key file is missing or unreadable: {} ({error}). Follow docs/DEVELOPER_SETUP.md to create it.",
            key_path.display()
        )
    })?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(format!(
            "The tunnel runtime key file must be a non-empty regular file: {}",
            key_path.display()
        ));
    }
    Ok(())
}

fn resolve_tunnel_client(config_dir: &Path) -> Result<PathBuf, String> {
    let configured_path_file = launcher_config_directory(config_dir).join(TUNNEL_CLIENT_PATH_FILE);
    if configured_path_file.exists() {
        let configured_path = fs::read_to_string(&configured_path_file).map_err(|error| {
            format!(
                "Could not read the tunnel-client path file {}: {error}",
                configured_path_file.display()
            )
        })?;
        let configured_path = configured_path.trim().trim_start_matches('\u{feff}');
        let configured_path = PathBuf::from(configured_path);
        if !configured_path.is_absolute() || !configured_path.is_file() {
            return Err(format!(
                "The tunnel-client path in {} is not an existing absolute file path.",
                configured_path_file.display()
            ));
        }
        return Ok(configured_path);
    }

    if let Ok(current_exe) = std::env::current_exe()
        && let Some(directory) = current_exe.parent()
    {
        let adjacent = directory.join(tunnel_client_executable_name());
        if adjacent.is_file() {
            return Ok(adjacent);
        }
    }

    Ok(PathBuf::from(tunnel_client_executable_name()))
}

fn tunnel_client_executable_name() -> &'static str {
    if cfg!(windows) {
        "tunnel-client.exe"
    } else {
        "tunnel-client"
    }
}

fn launch_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

fn prefixed_path_argument(prefix: &str, path: &Path) -> OsString {
    let mut argument = OsString::from(prefix);
    argument.push(path.as_os_str());
    argument
}

fn run_tunnel_launcher(
    tunnel_client: PathBuf,
    mcp_endpoint: String,
    key_path: PathBuf,
    health_url_path: PathBuf,
    log_path: PathBuf,
    event_tx: Sender<TunnelLaunchEvent>,
    cancel_rx: Receiver<()>,
) {
    let result = run_tunnel_launcher_inner(
        &tunnel_client,
        &mcp_endpoint,
        &key_path,
        &health_url_path,
        &log_path,
        &event_tx,
        &cancel_rx,
    );
    let _ = fs::remove_file(&health_url_path);
    if let Err(error) = result {
        let _ = event_tx.send(TunnelLaunchEvent::Failed(format!(
            "{error} Tunnel log: {}",
            log_path.display()
        )));
    }
}

fn run_tunnel_launcher_inner(
    tunnel_client: &Path,
    mcp_endpoint: &str,
    key_path: &Path,
    health_url_path: &Path,
    log_path: &Path,
    event_tx: &Sender<TunnelLaunchEvent>,
    cancel_rx: &Receiver<()>,
) -> Result<(), String> {
    let log = create_log_file(log_path)?;
    let stderr_log = log
        .try_clone()
        .map_err(|error| format!("Could not duplicate the tunnel log handle: {error}"))?;

    let mut command = Command::new(tunnel_client);
    command
        .arg("run")
        .arg("--profile")
        .arg(PROFILE_NAME)
        .arg("--mcp.server-url")
        .arg(mcp_endpoint)
        .arg(prefixed_path_argument(
            "--control-plane.api-key=file:",
            key_path,
        ))
        .arg("--mcp.connection-max-ttl=24h")
        .arg("--health.listen-addr=127.0.0.1:0")
        .arg(prefixed_path_argument(
            "--health.url-file=",
            health_url_path,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr_log));

    #[cfg(target_os = "windows")]
    command.creation_flags(CREATE_NO_WINDOW);

    let mut child = command.spawn().map_err(|error| {
        format!(
            "Could not launch {}: {error}.",
            tunnel_client.as_os_str().to_string_lossy()
        )
    })?;

    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if cancel_rx.try_recv().is_ok() {
            stop_child(&mut child);
            return Ok(());
        }

        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("Could not query tunnel-client status: {error}"))?
        {
            return Err(format!(
                "Tunnel client exited before becoming ready ({status})."
            ));
        }

        if let Some(base_url) = read_health_base_url(health_url_path)
            && probe_ready(&base_url)
        {
            let _ = event_tx.send(TunnelLaunchEvent::Ready);
            break;
        }

        if Instant::now() >= deadline {
            stop_child(&mut child);
            return Err(format!(
                "Tunnel client did not become ready within {} seconds.",
                READY_TIMEOUT.as_secs()
            ));
        }

        thread::sleep(POLL_INTERVAL);
    }

    loop {
        if cancel_rx.try_recv().is_ok() {
            stop_child(&mut child);
            return Ok(());
        }

        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("Could not query tunnel-client status: {error}"))?
        {
            return Err(format!("Tunnel client stopped ({status})."));
        }

        thread::sleep(POLL_INTERVAL);
    }
}

fn create_log_file(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| format!("Could not create tunnel log {}: {error}", path.display()))
}

fn stop_child(child: &mut Child) {
    if child.kill().is_ok() {
        let _ = child.wait();
    } else {
        let _ = child.try_wait();
    }
}

fn read_health_base_url(path: &Path) -> Option<String> {
    let value = fs::read_to_string(path).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn parse_loopback_http_address(base_url: &str) -> Result<SocketAddr, String> {
    let authority = base_url
        .trim()
        .strip_prefix("http://")
        .ok_or_else(|| "Tunnel health URL is not HTTP.".to_string())?
        .trim_end_matches('/');
    if authority.contains('/') {
        return Err("Tunnel health URL contains an unexpected path.".to_string());
    }
    let address: SocketAddr = authority
        .parse()
        .map_err(|error| format!("Tunnel health URL has an invalid address: {error}"))?;
    if !address.ip().is_loopback() {
        return Err("Tunnel health URL is not loopback-only.".to_string());
    }
    Ok(address)
}

fn probe_ready(base_url: &str) -> bool {
    let Ok(address) = parse_loopback_http_address(base_url) else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&address, PROBE_TIMEOUT) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));

    let request = format!("GET /readyz HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }

    let mut status_line = String::new();
    if BufReader::new(stream).read_line(&mut status_line).is_err() {
        return false;
    }
    response_status_is_ready(&status_line)
}

fn response_status_is_ready(status_line: &str) -> bool {
    let mut fields = status_line.split_whitespace();
    matches!(fields.next(), Some("HTTP/1.0" | "HTTP/1.1")) && fields.next() == Some("200")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn dropping_launch_signals_cancellation_and_joins_worker() {
        let (_event_tx, event_rx) = mpsc::channel();
        let (cancel_tx, cancel_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            cancel_rx.recv().expect("launch should signal cancellation");
            finished_tx.send(()).unwrap();
        });
        let launch = TunnelLaunch {
            event_rx,
            cancel_tx,
            worker: Some(worker),
            log_path: PathBuf::from("tunnel.log"),
        };

        drop(launch);

        finished_rx.try_recv().expect("worker should be joined");
    }

    #[test]
    fn health_url_parser_accepts_only_loopback_http_addresses() {
        assert_eq!(
            parse_loopback_http_address("http://127.0.0.1:43123/").unwrap(),
            "127.0.0.1:43123".parse().unwrap()
        );
        assert!(parse_loopback_http_address("https://127.0.0.1:43123").is_err());
        assert!(parse_loopback_http_address("http://192.0.2.10:43123").is_err());
        assert!(parse_loopback_http_address("http://127.0.0.1:43123/ui").is_err());
    }

    #[test]
    fn readiness_response_requires_an_http_200_status_line() {
        assert!(response_status_is_ready("HTTP/1.1 200 OK\r\n"));
        assert!(response_status_is_ready("HTTP/1.0 200 Ready\r\n"));
        assert!(!response_status_is_ready("HTTP/1.1 503 Unavailable\r\n"));
        assert!(!response_status_is_ready("not HTTP\r\n"));
    }

    #[test]
    fn readiness_probe_calls_the_loopback_ready_endpoint() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            assert_eq!(request_line, "GET /readyz HTTP/1.1\r\n");
            reader
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });

        assert!(probe_ready(&format!("http://{address}")));
        server.join().unwrap();
    }

    #[test]
    fn path_arguments_preserve_spaces_without_shell_quoting() {
        let argument = prefixed_path_argument(
            "--health.url-file=",
            Path::new(r"C:\Temp\Remote Control\health.url"),
        );
        assert_eq!(
            argument,
            OsStr::new(r"--health.url-file=C:\Temp\Remote Control\health.url")
        );
    }

    #[test]
    fn tunnel_client_executable_name_matches_platform_convention() {
        #[cfg(windows)]
        assert_eq!(tunnel_client_executable_name(), "tunnel-client.exe");
        #[cfg(not(windows))]
        assert_eq!(tunnel_client_executable_name(), "tunnel-client");
    }

    #[test]
    fn automatic_start_setting_defaults_to_false_and_round_trips() {
        let config_dir = std::env::temp_dir().join(format!(
            "rmcp-auto-start-setting-test-{}-{}",
            std::process::id(),
            launch_id()
        ));

        assert!(!load_start_automatically_from(&config_dir).unwrap());
        save_start_automatically_to(&config_dir, true).unwrap();
        assert!(load_start_automatically_from(&config_dir).unwrap());
        save_start_automatically_to(&config_dir, false).unwrap();
        assert!(!load_start_automatically_from(&config_dir).unwrap());

        fs::remove_dir_all(config_dir).unwrap();
    }

    #[test]
    fn tunnel_client_resolution_falls_back_to_platform_executable_name() {
        let config_dir =
            std::env::temp_dir().join(format!("rmcp-tunnel-resolve-test-{}", std::process::id()));
        assert_eq!(
            resolve_tunnel_client(&config_dir).unwrap(),
            PathBuf::from(tunnel_client_executable_name())
        );
    }
}
