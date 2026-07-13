# Remote Control MCP

A lightweight, Windows GUI application that also acts as a Model Context Protocol (MCP) server over stdin/stdout.

## Architecture

This application uses a multi-threaded architecture to separate the user interface from the MCP communication protocol:
* **Main Thread:** Runs an `egui`/`eframe` native Windows GUI that displays server state and a scrolling list of tool requests.
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

## GUI request list

The server lifecycle remains in the status area at the top of the window. A fatal server error is shown separately and is never represented as a tool request.

Every invocation that reaches a typed tool handler creates one request row. Rows are ordered by request start and displayed newest-first; subsequent completion, warning, failure, rejection, or background-error events update the originating row in place without moving it. Each row includes a state icon and readable state text, the tool name, a privacy-conscious request summary, the local start time in `DD/MM/YYYY HH:MM:SS` format, and a live or frozen elapsed duration. Detached-process background handling failures update their original `launch_process` row.

The GUI retains at most 500 requests under normal conditions, pruning the oldest finished request first. In-progress requests are never removed to enforce that limit, so the list may temporarily exceed 500 while more than 500 calls overlap. Protocol requests rejected by `rmcp` before typed-handler entry, such as malformed JSON or schema-invalid arguments, cannot receive an application request ID and may not appear in this first version.

## Critical Stdout Rule

Standard output (`stdout`) is strictly reserved for MCP protocol messages.
* **Never** print diagnostic, debug, or application output to `stdout` (e.g. using `println!`). Doing so will corrupt the protocol stream and cause the MCP client to disconnect.
* Diagnostics must be sent to the GUI event channel or written to standard error (`stderr`) using `eprintln!`.

## Exposed Tools

The application exposes three tools:
1. `ping`
2. `launch_process`
3. `read_file`

> [!WARNING]
> The `launch_process` tool provides unrestricted local process execution under the user account running the MCP server. There is no security allowlist.
> The `read_file` tool likewise has unrestricted read access to regular files that account can access; its relative-path base is a convenience, not a security boundary.

---

### 1. `ping`

Check whether the local Remote Control MCP server is running and responding.
* **Input schema:** Empty object (`{}`)
* **Text content:** Returns `pong` on success.
* **Structured content:** Returns `{ "message": "pong" }`.
* **Output schema:** Advertises a matching MCP object schema with the required string property `message`.

---

### 2. `launch_process`

Launch a local process on the host machine. There is no implicit shell execution; command binaries must be called directly.

#### Parameters

* **`process_name`** (string, required): The name or absolute path of the executable to launch (e.g., `"notepad.exe"`, `"git"`).
* **`arguments`** (optional):
  * Omit this field to launch the executable with no arguments.
  * The generated schema intentionally has no default, so clients should omit the property for no arguments and MCP Inspector initially displays it as blank.
  * **Windows:** A single raw command-line string when present (e.g., `"/c echo hello"`). An empty string is equivalent to no arguments.
  * **Non-Windows:** An array of discrete argument strings when present (e.g., `["--version"]`). An empty array is equivalent to no arguments.
  * A shell is used only when the caller explicitly selects a shell executable, such as `cmd.exe`; the server never adds an implicit shell.
* **`working_directory`** (string, optional): The directory where the process is launched. Defaults to `std::env::temp_dir()`.
* **`environment`** (object, required):
  * **`inherit`** (boolean, optional): Defaults to `true`, inheriting the parent process's environment variables. Explicitly setting it to `false` clears the inherited environment before applying `variables`.
  * **`variables`** (object, required): Key-value map of environment variables to add or configure. A `null` value removes that variable.
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
* **Wait Failures:** If wait or status checking fails, a best-effort attempt is made to terminate the child. A failed termination is followed by one non-blocking status check; if the child is still running or its status is unknown, ownership is transferred to a background reaper so the MCP response does not wait indefinitely.

#### Result Schema

On a successfully handled tool call, `content` contains one concise outcome summary and `structuredContent` contains the complete typed JSON result described below. Captured `stdout` and `stderr` are not duplicated into the text summary.

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

### 3. `read_file`

Read a bounded, 1-based inclusive line range from a local regular file. The tool is read-only and performs blocking filesystem work away from the single-threaded MCP runtime.

#### Parameters and paths

* **`path`** (string, required): An absolute path or an ordinary relative path. Absolute drive and UNC paths are supported where the operating system permits them. Relative paths resolve against `std::env::temp_dir()`.
* **`start_line`** (positive integer, required): First line to return, using 1-based numbering.
* **`end_line`** (positive integer, required): Last line to return, inclusive. It must be at least `start_line`, and the requested span may contain at most 500 lines.

The server uses normal filesystem permissions and does not sandbox reads. Relative `..` components and filesystem symlinks or Windows reparse points follow normal operating-system behaviour. Only regular files are accepted; directories, devices, and named pipes are rejected. The path is interpreted literally: `%VARIABLE%`, `$env:VARIABLE`, `~`, wildcards, and shell expressions are not expanded. Ambiguous Windows drive-relative and root-relative forms such as `C:some-file.txt` and `\some-file.txt` are rejected.

#### Text, encoding, and size limits

Files are scanned incrementally by LF boundaries rather than loaded in full. A selected line loses its terminating LF and an immediately preceding CR, so LF and CRLF files produce the same logical text. Blank lines and an unterminated final line are preserved. A UTF-8 BOM is removed only at the start of line 1.

Returned bytes use lossy UTF-8 conversion. The `lossy_utf8` result field reports whether replacement characters were needed in the selected range.

At most 256 KiB (`256 * 1024` raw logical-line bytes) is returned, and lines are never split. If the next complete line would exceed the limit after one or more lines have fitted, `status` is `truncated` and `next_start_line` identifies the first omitted line for a continuation call using the original `end_line`. If the first requested line itself exceeds the limit, the result has `status = line_too_long` and contains no partial text.

#### Result shape

`content` contains exactly one concise human-readable summary; it never contains file text. The complete typed result is present only in `structuredContent`, with these fields:

* **`status`**: `completed`, `truncated`, `not_found`, `access_denied`, `not_a_file`, `read_failed`, or `line_too_long`.
* **`error`**: Optional filesystem or operating-system detail for runtime failures.
* **`path`**: Resolved absolute path.
* **`requested_start_line`** / **`requested_end_line`**: Original validated range.
* **`actual_start_line`** / **`actual_end_line`**: Returned inclusive range, or `null` when no line was returned.
* **`text`**: Unnumbered selected file text, with logical lines joined by LF.
* **`eof`**: Whether EOF was reached for a successful read; `null` for runtime failures.
* **`next_start_line`**: Continuation line for `truncated`, otherwise `null`.
* **`lossy_utf8`**: Whether returned bytes required lossy replacement.

Valid requests return ordinary non-error MCP tool results even for structured filesystem failures. Invalid paths or line parameters return MCP invalid-parameter errors.

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
* Correlated request lifecycle emission, update-in-place GUI state, ordering, retention, timestamps, durations, and privacy boundaries.
* Subprocess execution lifecycle, environment handling, working directories, and null stdin using a self-hosted Rust test helper subprocess.
* Bounded timeout behaviours (`stop` and `detach`).
* Cleanup, best-effort reaping, and classification policies.
* Incremental `read_file` line selection, path handling, encoding, complete-line limits, continuation, response schemas, and GUI events.
* A real MCP initialisation and tool-call sequence over an in-memory duplex connection.
* Concurrency checks verifying that long-running process and file operations do not block other requests like `ping`.

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
4. The Inspector UI shows the `ping`, `launch_process`, and `read_file` tools.
5. You can invoke any tool and inspect outputs.

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
npx -y @modelcontextprotocol/inspector --cli .\target\debug\remote-control-mcp.exe --method tools/call --tool-name launch_process --tool-arg process_name=whoami.exe --tool-arg 'environment={"inherit":true,"variables":{}}' --tool-arg detached=false
```

This no-argument example is suitable for a typical Windows installation; executable availability differs between systems.

**Call the `read_file` tool:**
```powershell
npx -y @modelcontextprotocol/inspector --cli .\target\debug\remote-control-mcp.exe --method tools/call --tool-name read_file --tool-arg path=RemoteControlMCP\example.stdout.log --tool-arg start_line=1 --tool-arg end_line=100
```

## Connect to ChatGPT

The local stdio MCP server can be connected to ChatGPT through an OpenAI Secure MCP Tunnel. For a detailed step-by-step walkthrough of the tunnel setup, see [DEVELOPER_SETUP.md](docs/DEVELOPER_SETUP.md).
