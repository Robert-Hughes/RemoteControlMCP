# Remote Control MCP

A lightweight, proof-of-concept Windows GUI application that also acts as a Model Context Protocol (MCP) server over stdin/stdout.

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

## Current Limitations

* This is a prototype slice with no file access, configuration files, HTTP, auth, shell execution, or system tray functionality.
* Exposes exactly one tool: `ping`.

## Building

To build the application, run:

```powershell
cd D:\Programming\Internet\RemoteControlMCP
cargo build
```

## Testing with MCP Inspector

You can test the application using the Model Context Protocol Inspector:

```powershell
npx -y @modelcontextprotocol/inspector .\target\debug\remote-control-mcp.exe
```

When you run this command:
1. The Inspector launches.
2. The `Remote Control MCP` GUI window appears.
3. The Inspector connects to the application over stdio.
4. The Inspector UI shows the `ping` tool.
5. You can invoke the `ping` tool.
6. The tool returns `pong`.
7. The GUI activity list updates to show the request and response with timestamps.
