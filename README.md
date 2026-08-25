# Remote Control MCP

A lightweight Windows GUI application that also hosts a Model Context Protocol (MCP) server over loopback Streamable HTTP.

## Architecture

This application uses a multi-threaded architecture to separate the user interface from the MCP communication protocol:
* **Main Thread:** Runs an `egui`/`eframe` native Windows GUI that displays server state and a scrolling list of tool requests.
* **Background Thread:** Spawns a dedicated Tokio runtime and serves the `rmcp` MCP endpoint at `http://127.0.0.1:61337/mcp`.
* **Communication:** The background worker sends structured events to the UI thread using a standard library channel (`std::sync::mpsc::channel`).

```text
MCP client
    │
    │ Streamable HTTP over loopback TCP
    ▼
Rust MCP worker thread
    │
    │ std::sync::mpsc
    ▼
egui main thread
```

## MCP instructions

The server sends operating guidance to MCP clients in the `instructions` field of its initialisation response.

The effective instructions have two sources:

* `instructions/GENERAL.md` is committed to the repository and embedded into the executable at compile time. It documents machine-independent use of the Remote Control MCP tools.
* `instructions/LOCAL.md` is gitignored and loaded from the checkout at runtime. It can document programs, paths, and operational guidance specific to the host machine.

The local file is resolved relative to `CARGO_MANIFEST_DIR`, so it does not depend on the process working directory. A successful non-empty load is reported in the GUI and on standard error with the resolved path. A missing, empty, or unreadable local file falls back to general-only instructions and produces a non-fatal GUI warning as well as a standard-error warning.

Changes to `GENERAL.md` require a rebuild. Changes to `LOCAL.md` require a server restart and a new MCP initialisation handshake, but no rebuild.

## GUI request list

The server lifecycle remains in the status area at the top of the window. A fatal server error is shown separately and is never represented as a tool request.

Every invocation that reaches a typed tool handler creates one request row. Rows are ordered by request start and displayed newest-first; subsequent completion, warning, failure, rejection, or background-error events update the originating row in place without moving it. Each row includes a state icon and readable state text, the tool name, a request summary, the local start time in `DD/MM/YYYY HH:MM:SS` format, and a live or frozen elapsed duration. `launch_process` summaries show up to 80 characters of the command line with an ellipsis when truncated. Hovering shows diagnostic launch details including the request ID, full command line, working directory, launch mode, and returned stdout/stderr file paths, allowing subsequent `read_file` rows to be correlated with the launch that produced the files. Detached-process background handling failures update their original `launch_process` row.

The GUI retains at most 500 requests under normal conditions, pruning the oldest finished request first. In-progress requests are never removed to enforce that limit, so the list may temporarily exceed 500 while more than 500 calls overlap. Protocol requests rejected by `rmcp` before typed-handler entry, such as malformed JSON or schema-invalid arguments, cannot receive an application request ID and may not appear in this first version.

## Diagnostics

MCP protocol traffic no longer uses standard input or output. Application lifecycle diagnostics are sent to the GUI event channel or written to standard error (`stderr`).

## Maximum request timeout

The GUI exposes a persistent **Maximum request timeout** setting, expressed in whole seconds. Its default is `0`, which disables the local request deadline. The value is stored as `maximum-request-timeout.txt` in the `RemoteControlMCP` user-config directory and changes apply to subsequent MCP tool requests without restarting the server.

When enabled, the limit applies to every exposed tool. If tool execution has not produced a result by the configured deadline, RemoteControlMCP returns an MCP `isError: true` result explaining that its own request timeout expired. This is useful when the MCP client or transport has a longer opaque timeout of its own: letting RemoteControlMCP fail first gives the model an actionable error instead of a generic transport exception. In the ChatGPT Secure MCP Tunnel setup documented below, an outer tool-call lifetime of approximately 120 seconds has been observed, so configuring RemoteControlMCP to **110 seconds** leaves a deliberate margin. See [Long-running `launch_process` calls fail near the tunnel response deadline](docs/DEVELOPER_SETUP.md#long-running-launch_process-calls-fail-near-the-tunnel-response-deadline).

Timeout results use one consistent message shape: they state which timeout expired and include an `Outcome:` sentence describing what RemoteControlMCP actually did. Server-wide Maximum request timeout failures identify the configured per-request limit and tool; ordinary `launch_process timeout_ms` failures use the same `Outcome:` vocabulary while identifying the caller-configured process timeout instead. Pure async work is reported as cancelled; file-tool timeouts explain that blocking filesystem work may still finish; foreground process timeouts distinguish detached/still-running, stopped, and unconfirmed-stop outcomes. Cooperative `launch_process` timeout results also retain the structured `LaunchProcessResult` (including PID and output-file paths when available), so a detached process can be followed up rather than becoming anonymous background work.

The generic watchdog is a response deadline, not a universal cancellation mechanism. File tools perform their operating-system work through Tokio `spawn_blocking`; once such blocking work has started, dropping the async wait cannot forcibly cancel the underlying OS operation. The bounded file operations may therefore finish in the background after a timeout. In particular, a mutating file operation that was already in progress may still reach its atomic commit. Callers must not interpret a request-timeout result as proof that arbitrary blocking side effects were rolled back.

`launch_process` receives additional cooperative handling because a foreground launch can intentionally remain alive for an arbitrary period and its blocking waiter owns a child process. Merely timing out the async wait could otherwise return an MCP error while leaving the foreground child and blocking worker running. For a foreground launch, RemoteControlMCP therefore calculates a process budget just below the request-wide limit, leaving up to one second of headroom for stop/detach cleanup and response construction. With a 110-second Maximum request timeout the cooperative process budget is **109 seconds**. If `timeout_ms` is omitted or is longer than that budget, the server clamps the effective process timeout to the cooperative budget; an explicitly supplied `timeout_action` is preserved, while an otherwise unbounded foreground launch defaults to `detach`. A shorter caller-supplied process timeout is left unchanged, and already-detached launches are not clamped because their MCP request returns immediately. The outer request watchdog remains active at the full configured limit as a final guard.

## Exposed Tools

The application exposes eight tools: `get_instructions`, `ping`, `launch_process`, `read_file`, `read_binary_file`, `insert_before_line`, `insert_after_line`, and `write_file`.

> [!WARNING]
> The `launch_process` tool provides unrestricted local process execution under the user account running the MCP server. There is no security allowlist.
> The file tools likewise use the full regular-file access available to that account; relative-path handling is a convenience, not a security boundary.

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
* **`timeout_ms`** (integer, optional): Bounded child-process execution timeout. Requires `timeout_action`. When **Maximum request timeout** is enabled, a foreground value that would run too close to or beyond the request deadline is cooperatively reduced as described above; shorter caller-supplied timeouts retain their normal semantics. When calls are routed through an OpenAI Secure MCP Tunnel, see [Long-running `launch_process` calls fail near the tunnel response deadline](docs/DEVELOPER_SETUP.md#long-running-launch_process-calls-fail-near-the-tunnel-response-deadline).
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

Each returned logical line is prefixed with `<line_number>: `, for example `170: let value = 1;`. The prefix is presentation metadata and is not part of the file. Blank line 171 is returned as `171: `. This is the only representation of the selected content; the server does not duplicate it in an unnumbered field.

Returned bytes use lossy UTF-8 conversion. The `lossy_utf8` result field reports whether replacement characters were needed in the selected range.

At most 256 KiB (`256 * 1024` bytes), including line-number prefixes and separators, is returned, and lines are never split. If the next complete numbered line would exceed the limit after one or more lines have fitted, `status` is `truncated` and `next_start_line` identifies the first omitted line for a continuation call using the original `end_line`. If the first requested numbered line itself exceeds the limit, the result has `status = line_too_long` and contains no partial text.

#### Result shape

`content` contains exactly one concise human-readable summary; it never contains file text. The complete typed result is present only in `structuredContent`, with these fields:

* **`status`**: `completed`, `truncated`, `not_found`, `access_denied`, `not_a_file`, `read_failed`, or `line_too_long`.
* **`error`**: Optional filesystem or operating-system detail for runtime failures.
* **`path`**: Resolved absolute path.
* **`requested_start_line`** / **`requested_end_line`**: Original validated range.
* **`actual_start_line`** / **`actual_end_line`**: Returned inclusive range, or `null` when no line was returned.
* **`text`**: Selected logical lines joined by LF and prefixed with `<line_number>: `. Prefixes are presentation metadata, not file content.
* **`eof`**: Whether EOF was reached for a successful read; `null` for runtime failures.
* **`next_start_line`**: Continuation line for `truncated`, otherwise `null`.
* **`lossy_utf8`**: Whether returned bytes required lossy replacement.

Valid requests return ordinary non-error MCP tool results even for structured filesystem failures. Invalid paths or line parameters return MCP invalid-parameter errors.

---

### 4. `insert_before_line` and `insert_after_line`

Insert non-empty text next to an existing 1-based anchor line without replacing or reproducing that line. Use these tools for insertion instead of manufacturing an insertion with `write_file`.

#### Parameters and paths

Both tools have the same input schema:

* **`path`** (string, required): Uses the same absolute, relative, UNC, literal-path, and ambiguous-Windows-path rules as `read_file`.
* **`line`** (positive integer, required): An existing 1-based anchor line. Use the line numbers displayed by `read_file`.
* **`text`** (non-empty string, required): UTF-8 text to insert, limited to 256 KiB when encoded. Supply file content only; never include the `<line_number>: ` presentation prefix returned by `read_file`.

`insert_before_line` places `text` immediately before the anchor line. `insert_after_line` places it immediately after the anchor line. The anchor must exist; empty files and lines beyond EOF return `range_out_of_bounds`. These tools never create files.

The supplied `text` bytes are preserved. When a separator is required at a boundary and `text` does not already provide one, the server uses the anchor line's LF or CRLF terminator, falling back to LF for an unterminated anchor. A leading UTF-8 BOM remains at the beginning of the file. Untouched bytes, permissions, symlink targets, and final-newline state are otherwise handled like `write_file`.

Each tool returns `status`, optional `error`, resolved `path`, `requested_line`, `inserted_bytes`, and the bounded post-edit fields described below for `write_file`. Runtime filesystem failures are structured non-error MCP results. Invalid parameters, including empty or oversized `text`, return MCP invalid-parameter errors.

---

### 5. `write_file`

Replace a strict, 1-based inclusive line range in a local regular file. The replacement may contain fewer lines, more lines, or no lines. An empty `text` therefore deletes the selected range.

#### Parameters and paths

* **`path`** (string, required): Uses the same absolute, relative, UNC, literal-path, and ambiguous-Windows-path rules as `read_file`.
* **`start_line`** / **`end_line`** (positive integers, required): Inclusive range containing at most 500 lines.
* **`expected_text`** (string, required): Exact current logical content of the selected range, without the `<line_number>: ` prefixes returned by `read_file`. Join multiple expected lines with LF and retain empty components for blank lines. The encoded value is limited to 256 KiB.
* **`text`** (string, required): UTF-8 replacement text, limited to 256 KiB when encoded.
* **`create_if_missing`** (boolean, required): Missing files are created only when this is `true`, the requested range is exactly `1-1`, and `expected_text` is empty.

For an existing non-empty file, every requested line must exist. A range extending beyond EOF returns `range_out_of_bounds` and leaves the original unchanged. If the selected logical lines do not exactly equal `expected_text`, the result is `content_mismatch` and the original remains unchanged. An empty existing file has one virtual editable line whose expected content is empty, so range `1-1` can populate it.

Line terminators are normalized only for the precondition: LF and CRLF both separate expected logical lines and are represented by LF in `expected_text`. A leading UTF-8 BOM is excluded. Each selected file line must otherwise match the UTF-8 bytes in the corresponding expected line exactly. Consequently, a range containing invalid UTF-8 cannot be edited with `write_file`; use another local editing mechanism.

Creation never creates parent directories. A missing parent returns `parent_not_found`; a parent path that is not a directory returns `parent_not_a_directory`. A missing file with `create_if_missing = false` returns `not_found`.

The server follows filesystem symlinks or Windows reparse points for existing files and replaces the resolved target while retaining the link. Only regular files are accepted.

#### Text and replacement behaviour

Untouched file bytes are copied exactly, including invalid UTF-8, a leading UTF-8 BOM, blank lines, mixed line endings, and final-newline state. Replacement `text` is written as supplied. When unselected lines follow and non-empty replacement text does not end in LF, the selected range's original LF or CRLF terminator is inserted before the suffix. When the selected range reaches EOF, the replacement controls the final newline exactly.

Filesystem work runs through `spawn_blocking`. Existing files are rewritten to a unique staging file in the resolved target directory and committed only after the full staged write succeeds. On Windows the final replacement uses `ReplaceFileW`; other platforms use same-filesystem rename. Missing-file creation uses create-new commit semantics so a concurrently appearing target is not silently overwritten. Temporary staging files are removed on failure where possible.

#### Result shape

`content` contains exactly one concise human-readable summary and never contains file text. `structuredContent` contains:

* **`status`**: `completed`, `created`, `not_found`, `parent_not_found`, `parent_not_a_directory`, `access_denied`, `not_a_file`, `range_out_of_bounds`, `content_mismatch`, `read_failed`, `write_failed`, or `replace_failed`.
* **`error`**: Optional filesystem or operating-system detail.
* **`path`**: Resolved absolute request path.
* **`requested_start_line`** / **`requested_end_line`**: Original validated range.
* **`replaced_line_count`**: Number of existing lines replaced, or `null` for creation and failures.
* **`inserted_bytes`**: UTF-8 byte length inserted on success; zero for failed results.
* **`post_edit_start_line`** / **`post_edit_end_line`**: Inclusive range of the returned post-edit excerpt, or `null` when no resulting line is available or the mutation failed.
* **`post_edit_text`**: A single numbered view of the resulting file around the first affected line, using the same `<line_number>: ` presentation prefixes as `read_file`. It is empty on failures and when no lines remain.
* **`post_edit_truncated`**: Whether the excerpt omitted a complete line because of its byte limit.

Successful mutation results include at most 11 post-edit lines: the first affected line and up to five lines before and after it. The complete excerpt, including prefixes and separators, is limited to 16 KiB and never splits a line. The excerpt appears only once, in `structuredContent`; it is not duplicated in the textual summary. Callers should inspect it for misplaced or duplicate lines before issuing another positional mutation.

Valid requests return ordinary non-error MCP tool results even for structured filesystem failures. Invalid paths, ranges, or oversized replacement text return MCP invalid-parameter errors.

---

## Building

To build the application, run:

```sh
cargo build
```

This command works identically in PowerShell, bash, and zsh.

## Starting from the executable

**Windows (PowerShell):**

```powershell
.\target\debug\remote-control-mcp.exe
```

**macOS / Linux (bash or zsh):**

```bash
./target/debug/remote-control-mcp
```

The application immediately starts its loopback-only Streamable HTTP endpoint at `http://127.0.0.1:61337/mcp`. Its compact status panel groups the server state with that endpoint, keeps the supervised tunnel state beside its action button, and independently shows the number of open HTTP connections and active MCP sessions. **Start Secure MCP Tunnel** launches the tunnel, **Cancel tunnel launch** terminates it while it is starting, and **Stop Secure MCP Tunnel** disconnects it without closing the application. The adjacent **Start automatically** setting is remembered across application restarts and launches the tunnel once the local listener is ready. **Maximum request timeout** is also persisted; `0` disables it, and changes apply to subsequent tool calls immediately.

The button requires the one-time launcher path, HTTP profile, and runtime-key file configuration documented in [DEVELOPER_SETUP.md](docs/DEVELOPER_SETUP.md). The fixed port allows the same profile to reconnect after application restarts without relaunching the application as a child process.

## Automated tests

To run the automated unit and integration test suite:

```sh
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
* Strict staged `write_file` replacement, explicit creation, line-ending preservation, failure atomicity, schemas, privacy, GUI events, and runtime responsiveness.
* Real MCP initialisation and tool-call sequences over both an in-memory duplex connection and loopback Streamable HTTP.
* Concurrency checks verifying that long-running process and file operations do not block other requests like `ping`.

## Testing with MCP Inspector

### Interactive Mode

You can test the application interactively using the Model Context Protocol Inspector:

**Windows (PowerShell):**

```powershell
.\target\debug\remote-control-mcp.exe
npx -y @modelcontextprotocol/inspector
```

**macOS / Linux (bash or zsh):**

```bash
./target/debug/remote-control-mcp
npx -y @modelcontextprotocol/inspector
```

When you run this command:
1. The Inspector web UI launches.
2. In the Inspector, choose **Streamable HTTP**, enter `http://127.0.0.1:61337/mcp`, and connect.
3. The Inspector connects to the already-running application over loopback TCP.
4. The Inspector UI shows the `get_instructions`, `ping`, `launch_process`, `read_file`, `read_binary_file`, `insert_before_line`, `insert_after_line`, and `write_file` tools.
5. You can invoke any tool and inspect outputs.

### CLI Mode

You can also run the Inspector in non-interactive CLI mode. The commands below use single quotes, which PowerShell and bash/zsh both accept:

**List available tools:**
```sh
npx -y @modelcontextprotocol/inspector --cli http://127.0.0.1:61337/mcp --transport http --method tools/list
```

**Call the `ping` tool:**
```sh
npx -y @modelcontextprotocol/inspector --cli http://127.0.0.1:61337/mcp --transport http --method tools/call --tool-name ping
```

**Call the `launch_process` tool:**
```powershell
npx -y @modelcontextprotocol/inspector --cli http://127.0.0.1:61337/mcp --transport http --method tools/call --tool-name launch_process --tool-arg process_name=whoami.exe --tool-arg 'environment={"inherit":true,"variables":{}}' --tool-arg detached=false
```

On macOS and Linux, replace `whoami.exe` with `whoami`:

```bash
npx -y @modelcontextprotocol/inspector --cli http://127.0.0.1:61337/mcp --transport http --method tools/call --tool-name launch_process --tool-arg process_name=whoami --tool-arg 'environment={"inherit":true,"variables":{}}' --tool-arg detached=false
```

This no-argument example works on any supported system; executable availability differs between systems.

**Call the `read_file` tool:**
```sh
npx -y @modelcontextprotocol/inspector --cli http://127.0.0.1:61337/mcp --transport http --method tools/call --tool-name read_file --tool-arg path=RemoteControlMCP/example.stdout.log --tool-arg start_line=1 --tool-arg end_line=100
```

**Call the `read_binary_file` tool:**
```sh
npx -y @modelcontextprotocol/inspector --cli http://127.0.0.1:61337/mcp --transport http --method tools/call --tool-name read_binary_file --tool-arg path=RemoteControlMCP/example.png
```

Recognised images are returned as native MCP image content. Other binary files are returned as native embedded resources. The hard server limit is 100,000,000 bytes (100 MB); `max_bytes` can request a lower per-call limit. Oversized files are rejected rather than truncated.

**Call the `write_file` tool:**
```sh
npx -y @modelcontextprotocol/inspector --cli http://127.0.0.1:61337/mcp --transport http --method tools/call --tool-name write_file --tool-arg path=RemoteControlMCP/example.txt --tool-arg start_line=1 --tool-arg end_line=1 --tool-arg expected_text= --tool-arg text=updated --tool-arg create_if_missing=true
```

Relative paths resolve beneath the system temporary directory (`%TEMP%` on Windows, `$TMPDIR` on macOS and Linux); Windows also accepts backslash separators.

## Connect to ChatGPT

The loopback HTTP MCP server can be connected to ChatGPT through an OpenAI Secure MCP Tunnel. For a detailed step-by-step walkthrough and migration instructions, see [DEVELOPER_SETUP.md](docs/DEVELOPER_SETUP.md).
