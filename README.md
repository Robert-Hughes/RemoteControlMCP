# Remote Control MCP

A lightweight, Windows GUI application that also acts as a Model Context Protocol (MCP) server over stdin/stdout.

## Architecture

This application uses a multi-threaded architecture to separate the user interface from the MCP communication protocol:
* **Main Thread:** Runs an `egui`/`eframe` native Windows GUI that displays application state and a scrolling activity log of events.
* **Background Thread:** Spawns a dedicated Tokio runtime and runs the `rmcp` MCP server over `stdin`/`stdout`.
* **Communication:** The background worker sends structured events to the UI thread using a standard library channel (`std::sync::mpsc::channel`).

```text
MCP client
    │
    │ stdin/stdout
    ▼
Rust MCP worker thread
    │
    │ std::sync::mpsc
    ▼
egui main thread
```

## Critical Stdout Rule

Standard output (`stdout`) is strictly reserved for MCP protocol messages.
* **Never** print diagnostic, debug, or application output to `stdout` (e.g. using `println!`). Doing so will corrupt the protocol stream and cause the MCP client to disconnect.
* Diagnostics must be sent to the GUI event channel or written to standard error (`stderr`) using `eprintln!`.

## Exponent Tools

The application exposes two tools:
1. `ping`
2. `launch_process`

> [!WARNING]
> The `launch_process` tool provides unrestricted local process execution under the user account running the MCP server. There is no security allowlist.

---

### 1. `ping`

Check whether the local Remote Control MCP server is running and responding.
* **Input schema:** Empty object (`{}`)
* **Output:** Returns `"pong"` on success.

---

### 2. `launch_process`

Launch a local process on the host machine. There is no implicit shell execution; command binaries must be called directly.

#### Parameters

* **`process_name`** (string, required): The name or absolute path of the executable to launch (e.g., `"notepad.exe"`, `"git"`).
* **`arguments`** (required):
  * **Windows:** A single raw command-line string (e.g., `"/c \"dir C:\\\""`).
  * **Non-Windows:** An array of discrete argument strings (e.g., `["-c", "ls -l /"]`).
* **`working_directory`** (string, optional): The directory where the process is launched. Defaults to `std::env::temp_dir()`.
* **`environment`** (object, required):
  * **`inherit`** (boolean): If `true`, inherits the parent process's environment variables.
  * **`variables`** (object): Key-value map of environment variables to add or configure. To remove an environment variable, map its key to `null`.
* **`detached`** (boolean, required): If `true`, the MCP server spawns the process and returns immediately without waiting for it to complete.
* **`timeout_ms`** (integer, optional): Bounded execution timeout. Requires `timeout_action`.
* **`timeout_action`** (string, optional): Can be either `"detach"` or `"stop"`.
  * `"detach"`: If the process exceeds the timeout, the MCP server returns immediately and lets the process continue in the background.
  * `"stop"`: If the process exceeds the timeout, the MCP server terminates the process.
  * *Note:* Setting `detached = true` together with `timeout_action = "detach"` is invalid and will fail validation.

#### Process Inputs and Outputs

* **Standard Input (`stdin`):** Subprocesses are spawned with a null stdin (`Stdio::null()`).
* **Standard Output/Error File Redirection:** Output is captured in files generated beneath:
  ```text
  std::env::temp_dir()/RemoteControlMCP
  ```
  These files are named using the host PID, timestamp, and a counter (e.g., `launch-process-1234-1672531199-0.stdout.log`). These files are not deleted automatically.
* **Tail Capture:** The tool returns the final 1,024 bytes of `stdout` and `stderr` lossily decoded as UTF-8. If the output is truncated, it is prefixed with a `[... beginning truncated ...]\n` marker.

#### Subprocess Cleanup and Termination

* **Direct Child Only:** Process termination only stops the immediate child process spawned. Any descendant processes spawned by the child are not terminated.
* **Wait Failures:** If wait or status checking fails, a best-effort attempt is made to terminate the child and reap it synchronously. If synchronous reaping fails, ownership is transferred to a background reaper thread to prevent zombie processes.

#### Result Schema

On successful tool call, a structured JSON result is returned:

* **`status`** (string): The serialised status code of the run:
  * `completed`: Process finished within limits.
  * `detached`: Process was launched detached.
  * `detached_with_stop_timeout`: Process was launched detached with a stop timeout configured.
  * `timed_out_detached`: Process exceeded timeout and was detached.
  * `timed_out_stopped`: Process exceeded timeout and was stopped.
  * `setup_failed`: Directory creation or file redirection setup failed.
  * `launch_process_failed`: Executable could not be spawned (e.g., file not found).
  * `wait_failed`: Status checking or waiting failed.
  * `stop_failed`: Failed to terminate the process on timeout.
* **`error`** (string, optional): Details of the failure (e.g., OS error messages).
* **`pid`** (integer, optional): The OS process identifier.
* **`exit_code`** (integer, optional): The process exit status code.
  * *Note:* Non-zero child exit codes are treated as successful tool executions returning the process details, not MCP errors.
* **`stdout`** / **`stderr`** (string, optional): Lossy UTF-8 tail captures.
* **`stdout_file`** / **`stderr_file`** (string, optional): Absolute file paths to the logs.

Validation errors (e.g., missing process name or invalid parameter combinations) result in immediate MCP validation errors, whereas failures during process execution return a structured result with a failed status (e.g., `launch_process_failed`).

---

## Building

To build the application, run:

```powershell
cargo build
```

## Running directly

```powershell
.\target\debug\remote-control-mcp.exe
```

When run directly from a normal terminal, there is no MCP client feeding `stdin`, so the GUI will remain waiting for a client.

## Automated tests

To run the automated unit and integration test suite:

```powershell
cargo test
```

The suite covers:
* Direct tool behaviour of the `ping` method.
* Correct tool metadata exposure.
* UI event emission and ordering.
* Subprocess execution lifecycle, environment handling, working directories, and null stdin using a self-hosted Rust test helper subprocess.
* Bounded timeout behaviours (`stop` and `detach`).
* Cleanup, best-effort reaping, and classification policies.
* A real MCP initialisation and tool-call sequence over an in-memory duplex connection.
* Concurrency checks verifying that a long-running foreground `launch_process` call does not block other requests like `ping`.

## Testing with MCP Inspector

### Interactive Mode

You can test the application interactively using the Model Context Protocol Inspector:

```powershell
npx -y @modelcontextprotocol/inspector .\target\debug\remote-control-mcp.exe
```

When you run this command:
1. The Inspector web UI launches.
2. The `Remote Control MCP` GUI window appears.
3. The Inspector connects to the application over stdio.
4. The Inspector UI shows both the `ping` and `launch_process` tools.
5. You can invoke either tool and inspect outputs.

### CLI Mode

You can also run the Inspector in non-interactive CLI mode:

**List available tools:**
```powershell
npx -y @modelcontextprotocol/inspector --cli .\target\debug\remote-control-mcp.exe --method tools/list
```

**Call the `ping` tool:**
```powershell
npx -y @modelcontextprotocol/inspector --cli .\target\debug\remote-control-mcp.exe --method tools/call --tool-name ping
```

**Call the `launch_process` tool:**
```powershell
npx -y @modelcontextprotocol/inspector --cli .\target\debug\remote-control-mcp.exe --method tools/call --tool-name launch_process --arguments "{\"process_name\":\"whoami\",\"arguments\":\"\",\"environment\":{\"inherit\":true,\"variables\":{}},\"detached\":false}"
```

## Connect to ChatGPT

The local stdio MCP server can be connected to ChatGPT through an OpenAI Secure MCP Tunnel. For a detailed step-by-step walkthrough of the tunnel setup, see [DEVELOPER_SETUP.md](docs/DEVELOPER_SETUP.md).
