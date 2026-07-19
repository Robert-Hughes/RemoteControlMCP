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
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
#[cfg(target_os = "windows")]
use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

const PROFILE_NAME: &str = "remote-control-mcp";
const KEY_FILE_NAME: &str = "remote-control-mcp.key";
const TUNNEL_CLIENT_PATH_FILE: &str = "tunnel-client-path.txt";
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

#[cfg(target_os = "windows")]
pub fn has_mcp_stdio_transport() -> bool {
    let input_type = std_handle_file_type(STD_INPUT_HANDLE);
    let output_type = std_handle_file_type(STD_OUTPUT_HANDLE);
    stdio_types_are_pipes(input_type, output_type)
}

#[cfg(not(target_os = "windows"))]
pub fn has_mcp_stdio_transport() -> bool {
    true
}

#[cfg(target_os = "windows")]
fn std_handle_file_type(handle_id: u32) -> Option<u32> {
    // SAFETY: GetStdHandle only reads the calling process's standard-handle
    // table. The returned borrowed handle is checked before GetFileType and is
    // not closed by this function.
    let handle = unsafe { GetStdHandle(handle_id) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return None;
    }

    // SAFETY: `handle` is a non-null, non-INVALID_HANDLE_VALUE process handle
    // obtained from GetStdHandle. GetFileType does not take ownership of it.
    Some(unsafe { GetFileType(handle) })
}

#[cfg(target_os = "windows")]
fn stdio_types_are_pipes(input_type: Option<u32>, output_type: Option<u32>) -> bool {
    input_type == Some(FILE_TYPE_PIPE) && output_type == Some(FILE_TYPE_PIPE)
}

pub fn start_tunnel() -> Result<TunnelLaunch, String> {
    let app_data = app_data_directory()?;
    let key_path = app_data.join("tunnel-client").join(KEY_FILE_NAME);
    validate_key_file(&key_path)?;

    let tunnel_client = resolve_tunnel_client(&app_data)?;
    let mcp_executable = std::env::current_exe().map_err(|error| {
        format!("Could not determine the currently running MCP executable: {error}")
    })?;
    let runtime_directory = std::env::temp_dir().join("RemoteControlMCP");
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
    let worker = thread::Builder::new()
        .name("tunnel_launcher".to_string())
        .spawn(move || {
            run_tunnel_launcher(
                tunnel_client,
                mcp_executable,
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

fn app_data_directory() -> Result<PathBuf, String> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| {
            "APPDATA is unavailable, so the tunnel configuration cannot be located.".to_string()
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

fn resolve_tunnel_client(app_data: &Path) -> Result<PathBuf, String> {
    let configured_path_file = app_data
        .join("RemoteControlMCP")
        .join(TUNNEL_CLIENT_PATH_FILE);
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
        let adjacent = directory.join("tunnel-client.exe");
        if adjacent.is_file() {
            return Ok(adjacent);
        }
    }

    Ok(PathBuf::from("tunnel-client.exe"))
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

fn mcp_command_argument(path: &Path) -> OsString {
    let escaped_path = path.to_string_lossy().replace('\\', "\\\\");
    OsString::from(format!("command={escaped_path},channel=main"))
}

fn run_tunnel_launcher(
    tunnel_client: PathBuf,
    mcp_executable: PathBuf,
    key_path: PathBuf,
    health_url_path: PathBuf,
    log_path: PathBuf,
    event_tx: Sender<TunnelLaunchEvent>,
    cancel_rx: Receiver<()>,
) {
    let result = run_tunnel_launcher_inner(
        &tunnel_client,
        &mcp_executable,
        &key_path,
        &health_url_path,
        &log_path,
        &cancel_rx,
    );
    let _ = fs::remove_file(&health_url_path);
    if let Err(error) = result {
        let _ = event_tx.send(TunnelLaunchEvent::Failed(format!(
            "{error} Tunnel log: {}",
            log_path.display()
        )));
    } else {
        let _ = event_tx.send(TunnelLaunchEvent::Ready);
    }
}

fn run_tunnel_launcher_inner(
    tunnel_client: &Path,
    mcp_executable: &Path,
    key_path: &Path,
    health_url_path: &Path,
    log_path: &Path,
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
        .arg("--mcp.command")
        .arg(mcp_command_argument(mcp_executable))
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
            return Err("Tunnel startup was cancelled.".to_string());
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
            // Dropping Child does not terminate the process. Ownership is
            // intentionally handed off after the complete runtime is ready.
            drop(child);
            return Ok(());
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

    #[cfg(target_os = "windows")]
    #[test]
    fn stdio_transport_requires_input_and_output_pipes() {
        use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_CHAR, FILE_TYPE_DISK};

        assert!(stdio_types_are_pipes(
            Some(FILE_TYPE_PIPE),
            Some(FILE_TYPE_PIPE)
        ));
        assert!(!stdio_types_are_pipes(
            Some(FILE_TYPE_CHAR),
            Some(FILE_TYPE_PIPE)
        ));
        assert!(!stdio_types_are_pipes(
            Some(FILE_TYPE_PIPE),
            Some(FILE_TYPE_DISK)
        ));
        assert!(!stdio_types_are_pipes(None, Some(FILE_TYPE_PIPE)));
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
    fn mcp_command_override_uses_the_running_executable_path() {
        let argument = mcp_command_argument(Path::new(
            r"D:\Programming\Remote Control MCP\remote-control-mcp.exe",
        ));
        assert_eq!(
            argument,
            OsStr::new(
                r"command=D:\\Programming\\Remote Control MCP\\remote-control-mcp.exe,channel=main"
            )
        );
    }
}
