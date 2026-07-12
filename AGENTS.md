# AGENTS.md

Instructions for future coding agents working in this repository.

## Project scope

* This is a lightweight Rust Windows GUI application.
* The GUI runs on the main thread using eframe/egui.
* The MCP server runs on a dedicated background thread using a single-threaded Tokio runtime.
* MCP transport uses stdin/stdout.
* Stdout is exclusively reserved for MCP protocol messages.
* Diagnostics must go to stderr or the GUI event channel.
* Avoid unnecessary dependencies and abstractions.

## Formatting

After changing Rust code, run:

```powershell
cargo fmt
```

Before reporting completion, verify formatting with:

```powershell
cargo fmt --check
```

## Compilation

Run:

```powershell
cargo check
```

## Linting

Run:

```powershell
cargo clippy --all-targets -- -D warnings
```

All warnings in project code must be fixed rather than suppressed unless there is a clear documented reason.

## Tests

There are no automated tests in this initial slice.

If tests are added later, run them with:

```powershell
cargo test
```

Do not add tests merely to satisfy a process requirement. Add them when the architecture and behaviours to test are clear.

## Full validation sequence

Expected validation sequence:

```powershell
cargo fmt --check
cargo check
cargo clippy --all-targets -- -D warnings
cargo build
```

If tests exist in the future, include `cargo test` before Clippy.

## MCP Inspector

Run this command:

```powershell
npx -y @modelcontextprotocol/inspector .\target\debug\remote-control-mcp.exe
```

The Inspector connects over stdio. No diagnostic output may be written to stdout.

## Change discipline

* Do not add functionality beyond the requested scope.
* Do not add dependencies without a clear need.
* Do not commit changes unless explicitly asked.
* Review `git diff` and `git status --short` before reporting completion.
* Never claim an interactive test succeeded unless it was actually performed.
