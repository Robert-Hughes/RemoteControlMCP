# Remote Control MCP instructions

Use this server to launch local processes and to read or modify regular files on the host machine.

## Choosing a tool

- Call `get_instructions` once at the start of your session. The instructions are static; there is no need to call it again before each tool.
- Use `ping` only to check that the MCP connection is responding.
- Use `launch_process` to start an executable. The server does not add a shell implicitly; select a shell executable explicitly when the command needs shell syntax, built-ins, pipes, redirection, or variable expansion.
- Use `read_file` for text files when line-oriented access is useful.
- Use `read_binary_file` for images and other bounded binary files. Recognised images are returned as native MCP image content; other binary files are returned as native embedded resources.
- Use `insert_before_line` or `insert_after_line` to add text without replacing or reproducing an existing line.
- Use `write_file` to replace a precise range of lines in a regular file, or to create a missing file when creation is explicitly enabled.

## Launching processes

- Set `working_directory` deliberately when the process depends on relative paths. If omitted, it defaults to the host temporary directory.
- Subprocess standard input is unavailable. Do not rely on interactive terminal prompts.
- Use detached execution for work that must continue after the tool call returns. The result provides `stdout_file` and `stderr_file` paths that can be inspected later with `read_file`.
- For bounded foreground execution, provide both `timeout_ms` and `timeout_action`. Do not combine `detached = true` with `timeout_action = "detach"`.
- The server may be configured with a Maximum request timeout for all tool calls. For foreground launches it cooperatively shortens an omitted or overly long process timeout so process cleanup can finish before that request deadline. If no process timeout/action was supplied, reaching this server limit detaches the process by default so it can continue running. Use detached execution deliberately when work is expected to outlive the request limit.
- Inspect the returned status, exit code, stdout, and stderr. A non-zero process exit code is reported as a completed tool invocation rather than as an MCP protocol error.
- A foreground launch that reaches either its own process timeout or the server request limit is returned with `isError = true`; both timeout causes use the same `Outcome:` wording to report whether the process was detached, stopped, or could not be confirmed stopped.
- Server-generated Maximum request timeout errors name the tool and include an `Outcome:` describing whether work was cancelled, detached/still running, stopped, or may still be completing in blocking filesystem code.
- A stop timeout terminates only the immediate child process; descendant processes may continue running.

## Reading and writing files

- Prefer absolute paths. Relative file paths resolve against the host temporary directory.
- File paths are interpreted literally. Environment-variable syntax, `~`, wildcards, and shell expressions are not expanded.
- File tools accept regular files only and use the operating-system permissions of the account running the server.
- `read_file` is for text. Line ranges are one-based and inclusive, and each request may cover at most 500 lines. Every returned logical line is prefixed with `<line_number>: `; this prefix is presentation metadata and is not part of the file. When it returns `truncated`, continue from `next_start_line` while retaining the original end line.
- `read_binary_file` reads the complete binary file or rejects it; it never truncates silently. Its hard server limit is 100,000,000 bytes (100 MB). The optional `max_bytes` argument may request a smaller limit but cannot raise the server limit.
- Do not use `read_file` to ferry a binary file as textual base64 merely to work around the binary API.
- `insert_before_line` and `insert_after_line` anchor non-empty text to an existing 1-based line. Pass file content only in `text`; never copy the `<line_number>: ` prefixes returned by `read_file`. Do not simulate insertion by replacing and reproducing an adjacent line.
- `write_file` replaces exactly the selected range only when `expected_text` matches its current unnumbered logical content. Copy the content shown by `read_file` without the `<line_number>: ` prefixes, join multiple expected lines with LF, and preserve blank lines. Use an empty `text` to delete the matched range. A mismatch is rejected without changing the file.
- After a successful mutation, inspect `post_edit_text`. It is the only post-edit content representation and uses the same `<line_number>: ` presentation prefixes as `read_file`; check it for misplaced or duplicate lines before making another positional edit.
- Missing files are created only when `create_if_missing = true` and the requested range is `1-1`. Parent directories are never created automatically.
- File access and process execution are not sandboxed. Confirm paths and targets before performing destructive operations.
