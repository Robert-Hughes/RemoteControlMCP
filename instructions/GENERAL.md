# Remote Control MCP instructions

Use this server to launch local processes and to read or modify regular files on the host machine.

## Choosing a tool

- Use `ping` only to check that the MCP connection is responding.
- Use `launch_process` to start an executable. The server does not add a shell implicitly; select a shell executable explicitly when the command needs shell syntax, built-ins, pipes, redirection, or variable expansion.
- Use `read_file` to read a bounded range of lines from a regular file.
- Use `write_file` to replace a precise range of lines in a regular file, or to create a missing file when creation is explicitly enabled.

## Launching processes

- Set `working_directory` deliberately when the process depends on relative paths. If omitted, it defaults to the host temporary directory.
- Subprocess standard input is unavailable. Do not rely on interactive terminal prompts.
- Use detached execution for work that must continue after the tool call returns. The result provides `stdout_file` and `stderr_file` paths that can be inspected later with `read_file`.
- For bounded foreground execution, provide both `timeout_ms` and `timeout_action`. Do not combine `detached = true` with `timeout_action = "detach"`.
- Inspect the returned status, exit code, stdout, and stderr. A non-zero process exit code is reported as a completed tool invocation rather than as an MCP protocol error.
- A stop timeout terminates only the immediate child process; descendant processes may continue running.
- Environment variables inherit from the server by default. Set `inherit = false` only when a clean environment is required, then provide every needed variable explicitly.

## Reading and writing files

- Prefer absolute paths. Relative file paths resolve against the host temporary directory.
- File paths are interpreted literally. Environment-variable syntax, `~`, wildcards, and shell expressions are not expanded.
- File tools accept regular files only and use the operating-system permissions of the account running the server.
- Line ranges are one-based and inclusive, and each request may cover at most 500 lines.
- When `read_file` returns `truncated`, continue from `next_start_line` while retaining the original end line.
- `write_file` replaces exactly the selected range. Use the narrowest correct range and read the relevant lines first when the current contents are not already known.
- Missing files are created only when `create_if_missing = true` and the requested range is `1-1`. Parent directories are never created automatically.
- File access and process execution are not sandboxed. Confirm paths and targets before performing destructive operations.
