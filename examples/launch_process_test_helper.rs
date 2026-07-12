use std::io::Write;

struct Outputs {
    stdout: std::io::StdoutLock<'static>,
    stderr: std::io::StderrLock<'static>,
}

impl Outputs {
    fn new() -> Self {
        Self {
            stdout: std::io::stdout().lock(),
            stderr: std::io::stderr().lock(),
        }
    }

    fn stdout(&mut self, bytes: &[u8]) {
        self.stdout.write_all(bytes).unwrap();
        self.stdout.flush().unwrap();
    }

    fn stderr(&mut self, bytes: &[u8]) {
        self.stderr.write_all(bytes).unwrap();
        self.stderr.flush().unwrap();
    }
}

fn main() {
    let action = std::env::var("RMCP_TEST_HELPER_ACTION").unwrap_or_default();
    let mut outputs = Outputs::new();

    match action.as_str() {
        "stdout_stderr" => {
            outputs.stdout(
                std::env::var("RMCP_TEST_HELPER_STDOUT")
                    .unwrap_or_default()
                    .as_bytes(),
            );
            outputs.stderr(
                std::env::var("RMCP_TEST_HELPER_STDERR")
                    .unwrap_or_default()
                    .as_bytes(),
            );
        }
        "exit_code" => {
            let code = std::env::var("RMCP_TEST_HELPER_CODE")
                .unwrap_or_default()
                .parse()
                .unwrap_or(0);
            std::process::exit(code);
        }
        "pwd" => {
            outputs.stdout(
                std::env::current_dir()
                    .unwrap()
                    .to_string_lossy()
                    .as_bytes(),
            );
        }
        "env" => {
            let name = std::env::var("RMCP_TEST_HELPER_ENV_NAME").unwrap_or_default();
            outputs.stdout(std::env::var(name).unwrap_or_default().as_bytes());
        }
        "stdin_eof" => {
            use std::io::Read;
            let mut buffer = String::new();
            let value = match std::io::stdin().read_to_string(&mut buffer) {
                Ok(_) if buffer.is_empty() => b"STDIN_EOF".as_slice(),
                Ok(_) => b"STDIN_DATA".as_slice(),
                Err(_) => b"STDIN_ERROR".as_slice(),
            };
            outputs.stdout(value);
        }
        "sleep" => {
            if let Ok(marker) = std::env::var("RMCP_TEST_HELPER_STARTED_MARKER") {
                std::fs::write(marker, "started").unwrap();
            }
            if let Ok(value) = std::env::var("RMCP_TEST_HELPER_PARTIAL_STDOUT") {
                outputs.stdout(format!("{value}\n").as_bytes());
            }
            if let Ok(value) = std::env::var("RMCP_TEST_HELPER_PARTIAL_STDERR") {
                outputs.stderr(format!("{value}\n").as_bytes());
            }
            let milliseconds = std::env::var("RMCP_TEST_HELPER_SLEEP_MS")
                .unwrap_or_default()
                .parse()
                .unwrap_or(0);
            std::thread::sleep(std::time::Duration::from_millis(milliseconds));
            if let Ok(marker) = std::env::var("RMCP_TEST_HELPER_MARKER") {
                std::fs::write(marker, "done").unwrap();
            }
        }
        "large_output" => {
            let count = std::env::var("RMCP_TEST_HELPER_COUNT")
                .unwrap_or_default()
                .parse()
                .unwrap_or(2000);
            let stdout_character =
                std::env::var("RMCP_TEST_HELPER_STDOUT_CHAR").unwrap_or_else(|_| "A".to_string());
            let stderr_character =
                std::env::var("RMCP_TEST_HELPER_STDERR_CHAR").unwrap_or_else(|_| "B".to_string());
            let stdout_tail = std::env::var("RMCP_TEST_HELPER_STDOUT_TAIL").unwrap_or_default();
            let stderr_tail = std::env::var("RMCP_TEST_HELPER_STDERR_TAIL").unwrap_or_default();
            outputs.stdout(format!("{}{stdout_tail}", stdout_character.repeat(count)).as_bytes());
            outputs.stderr(format!("{}{stderr_tail}", stderr_character.repeat(count)).as_bytes());
        }
        "invalid_utf8" => outputs.stdout(&[0xff, 0xff, 0xff, 0xff]),
        "echo_args" => {
            let arguments = std::env::args().skip(1).collect::<Vec<_>>().join("|");
            outputs.stdout(arguments.as_bytes());
        }
        _ => {}
    }
}
