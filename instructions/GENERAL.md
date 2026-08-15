# Remote Control MCP instructions

Use this server to launch local processes and to read or modify regular files on the host machine.

## Choosing a tool

- Call `get_instructions` once at the start of your session. The instructions are static; there is no need to call it again before each tool.
- Use `ping` only to check that the MCP connection is responding.
- Use `launch_process` to start an executable. The server does not add a shell implicitly; select a shell executable explicitly when the command needs shell syntax, built-ins, pipes, redirection, or variable expansion.
- Use `read_file` for text files when line-oriented access is useful.
- Use `read_binary_file` for images and other bounded binary files. Recognised images are returned as native MCP image content; other binary files are returned as native embedded resources.
- Use `write_file` to replace a precise range of lines in a regular file, or to create a missing file when creation is explicitly enabled.

## Launching processes

- Set `working_directory` deliberately when the process depends on relative paths. If omitted, it defaults to the host temporary directory.
- Subprocess standard input is unavailable. Do not rely on interactive terminal prompts.
- Use detached execution for work that must continue after the tool call returns. The result provides `stdout_file` and `stderr_file` paths that can be inspected later with `read_file`.
- For bounded foreground execution, provide both `timeout_ms` and `timeout_action`. Do not combine `detached = true` with `timeout_action = "detach"`.
- Inspect the returned status, exit code, stdout, and stderr. A non-zero process exit code is reported as a completed tool invocation rather than as an MCP protocol error.
- A timed-out foreground launch is returned with `isError = true` and a summary of what happened; retry with a larger `timeout_ms` when the process needs more time.
- A stop timeout terminates only the immediate child process; descendant processes may continue running.
- Environment variables inherit from the server by default. Set `inherit = false` only when a clean environment is required, then provide every needed variable explicitly.

## Reading and writing files

- Prefer absolute paths. Relative file paths resolve against the host temporary directory.
- File paths are interpreted literally. Environment-variable syntax, `~`, wildcards, and shell expressions are not expanded.
- File tools accept regular files only and use the operating-system permissions of the account running the server.
- `read_file` is for text. Line ranges are one-based and inclusive, and each request may cover at most 500 lines. When it returns `truncated`, continue from `next_start_line` while retaining the original end line.
- `read_binary_file` reads the complete binary file or rejects it; it never truncates silently. Its hard server limit is 100,000,000 bytes (100 MB). The optional `max_bytes` argument may request a smaller limit but cannot raise the server limit.
- Do not use `read_file` to ferry a binary file as textual base64 merely to work around the binary API.
- `write_file` replaces exactly the selected range. Use the narrowest correct range and read the relevant lines first when the current contents are not already known.
- Missing files are created only when `create_if_missing = true` and the requested range is `1-1`. Parent directories are never created automatically.
- File access and process execution are not sandboxed. Confirm paths and targets before performing destructive operations.
