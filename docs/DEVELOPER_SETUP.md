# OpenAI Secure MCP Tunnel Windows Developer Setup

This guide documents the complete Windows developer setup used to connect this local stdio Model Context Protocol (MCP) server to ChatGPT through an OpenAI Secure MCP Tunnel.

## 1. Overview and Scope

This setup allows a ChatGPT client to securely invoke the local `remote-control-mcp.exe` application using an outbound-only polling connection. 

* **Private and Local:** The MCP server remains entirely private within the local network.
* **No Public URLs or Inbound Ports:** No inbound firewall ports or public URLs are opened or exposed.
* **Outbound Polling:** The `tunnel-client` daemon establishes an outbound HTTPS connection to the OpenAI-hosted Secure MCP Tunnel control plane and polls for command dispatches.
* **Child Process Lifecycle:** The `tunnel-client` daemon launches `remote-control-mcp.exe` as a child process and communicates with it exclusively over `stdin`/`stdout` using the MCP JSON-RPC protocol.
* **Exclusive Stdout:** Standard output (`stdout`) of the Rust application is strictly reserved for MCP JSON-RPC messages; diagnostic logs are redirected to `stderr` or the GUI.
* **Continuous Operation:** The tunnel client must remain running continuously for ChatGPT application discovery and for executing all incoming MCP tool calls.

This guide is written specifically for developers using **Windows PowerShell** and targetting the **Debug executable**.

### Architecture

```text
       ┌───────────┐
       │  ChatGPT  │
       └─────┬─────┘
             │
             ▼
 ┌───────────────────────┐
 │     OpenAI-hosted     │
 │   Secure MCP Tunnel   │
 └───────────┬───────────┘
             ▲
             │ outbound HTTPS polling
             │ (port 443)
 ┌───────────┴───────────┐
 │   tunnel-client.exe   │  (Windows Host)
 └───────────┬───────────┘
             │
             │ stdio parent-child pipeline
             ▼ (stdin/stdout)
 ┌───────────────────────┐
 │remote-control-mcp.exe │  (Local Rust GUI App)
 └───────────────────────┘
```

---

## 2. Prerequisites

Before starting, ensure you have:

* **Windows Environment:** Windows operating system with PowerShell installed.
* **Rust Toolchain:** Cargo and `rustc` installed and available in your environment path.
* **Local Repository:** A local clone of the `RemoteControlMCP` repository.
* **OpenAI Platform Organisation:** Access to an OpenAI Platform developer organisation.
* **ChatGPT Workspace:** A ChatGPT account (plus, team, or enterprise) eligible for Developer mode.
* **Required Tunnel Permissions:**
  * **Tunnels Read + Manage:** Needed to create, edit, or delete a tunnel endpoint in the Platform settings.
  * **Tunnels Read + Use:** Needed to run the `tunnel-client` daemon locally and select the tunnel when creating the ChatGPT application.
  * *Note: Platform tunnel permissions are managed via Organisation RBAC roles. They are separate from ChatGPT Developer mode settings.*
* **Workspace Association:** The tunnel must be associated with both the owning Platform organisation and the target ChatGPT workspace.
* **Tunnel Client CLI:** The `tunnel-client.exe` binary downloaded from the Platform settings page or official release.

---

## 3. Build and Validate the Local MCP Server

First, compile and validate the Rust GUI application locally:

1. Open a PowerShell session and navigate to the repository root:
   ```powershell
   cd C:\path\to\RemoteControlMCP
   ```

2. Run the validation sequence:
   ```powershell
   cargo fmt --check
   cargo check
   cargo test
   cargo clippy --all-targets -- -D warnings
   cargo build
   ```

3. Confirm that the debug executable was built successfully:
   ```powershell
   Test-Path .\target\debug\remote-control-mcp.exe
   ```
   This command must return `True`.

> [!IMPORTANT]
> Do not manually run the compiled `remote-control-mcp.exe` binary. The `tunnel-client` daemon will launch it automatically as a child process and manage its standard input and output streams.

---

## 4. Create the OpenAI Tunnel

To set up the tunnel in the OpenAI control plane:

1. Open [Platform Tunnel Settings](https://platform.openai.com/settings/organization/tunnels).
2. Select **Create Tunnel**.
3. Provide a recognisable development name, such as:
   ```text
   Remote Control MCP - Development
   ```
4. Associate the tunnel with the correct Platform organisation and the target ChatGPT workspace.
5. Copy the generated `tunnel_id`. This ID is structured like `tunnel_<your-tunnel-id>` (e.g. `tunnel_abc123...`).

> [!WARNING]
> If a tunnel is only associated with the Platform organisation and not linked to your ChatGPT workspace, it will not appear in the ChatGPT app configuration settings.

---

## 5. Create the Runtime API Key

> [!IMPORTANT]
> The runtime API key is not displayed with the tunnel ID on the tunnel configuration page. It must be created separately.

1. Navigate to the [Platform API Keys Settings](https://platform.openai.com/settings/organization/api-keys).
2. Create a new API key.
3. Restrict its scope to the minimum permissions required to run and use tunnels:
   * **Tunnels Read + Use**
4. Copy the API key and store it securely (e.g. in a credential manager). Do not use an Admin API key for general tunnel runtime execution.

> [!CAUTION]
> **Security Requirement:** Runtime API keys are secrets.
> * They must never be committed to source control or pasted into documentation, screenshots, issue reports, or shared logs.
> * If a key is ever exposed in terminal transcripts or shell histories, revoke and replace it immediately.
> * The generated profile references `env:CONTROL_PLANE_API_KEY` rather than storing the literal key.
> * Remove the environment variable from your session once testing is complete.

---

## 6. Configure the PowerShell Session Securely

In your open PowerShell session, define path and identifier variables to make your commands portable:

```powershell
$TunnelClient = "C:\path\to\tunnel-client.exe"
$TunnelId = "tunnel_<your-tunnel-id>"
$McpExe = (Resolve-Path ".\target\debug\remote-control-mcp.exe").Path
```

Next, prompt for the runtime API key securely without exposing it in your PowerShell command history:

```powershell
$secureRuntimeKey = Read-Host "OpenAI tunnel runtime API key" -AsSecureString
$env:CONTROL_PLANE_API_KEY = [System.Net.NetworkCredential]::new("", $secureRuntimeKey).Password
Remove-Variable secureRuntimeKey
```

Verify that the environment variable is set without printing its value to the terminal:

```powershell
if ($env:CONTROL_PLANE_API_KEY) {
    "Runtime API key is set"
} else {
    throw "CONTROL_PLANE_API_KEY is not set"
}
```

> [!NOTE]
> The `$env:CONTROL_PLANE_API_KEY` environment variable is scoped strictly to the current PowerShell process and its child processes. The validation (`doctor`) and daemon (`run`) commands must be run within this same PowerShell session.

---

## 7. Create the stdio Profile

Initialize a local profile named `remote-control-mcp` to tell the tunnel client how to invoke the Rust application.

### Windows Path Escaping Pitfall

Windows single backslashes in paths can be consumed as escape characters by the tunnel-client command parser. For instance:
`C:\path\to\RemoteControlMCP\target\debug\remote-control-mcp.exe`
can be incorrectly parsed as:
`C:pathtoRemoteControlMCPtargetdebugremote-control-mcp.exe`

To avoid this, use the following PowerShell script to double all backslashes inside the command argument:

```powershell
$McpCommand = $McpExe.Replace("\", "\\")

& $TunnelClient init `
    --sample sample_mcp_stdio_local `
    --profile remote-control-mcp `
    --tunnel-id $TunnelId `
    --mcp-command $McpCommand
```

The generated configuration profile will be saved to:
`%APPDATA%\tunnel-client\remote-control-mcp.yaml`

If you need to overwrite an existing profile to update the tunnel ID or command path, run the command with the `--force` flag:

```powershell
& $TunnelClient init `
    --sample sample_mcp_stdio_local `
    --profile remote-control-mcp `
    --tunnel-id $TunnelId `
    --mcp-command $McpCommand `
    --force
```

> [!WARNING]
> The `--force` flag replaces the existing profile file immediately. Do not include it in first-run commands.

---

## 8. Validate the Profile with Doctor

Verify that the profile is fully operational before connecting:

```powershell
& $TunnelClient doctor `
    --profile remote-control-mcp `
    --explain
```

### Expected Results

A successful check will print an overall result of:
```text
RESULT ok
NEXT   tunnel-client run --profile remote-control-mcp
```

The output should show pass results for configuration checks, including profile loading, tunnel ID, API key availability, and local target executables.

> [!NOTE]
> The following status indicators are normal and expected for local stdio tunnels:
> * `mcp_server_reachable   SKIP` (A stdio child target is not probed as a network endpoint)
> * `oauth_metadata         SKIP` (Local stdio targets do not expose OAuth metadata URLs)
> * `codex_plugin           SKIP` (Optional check, not required for basic ChatGPT tunnel operation)

---

## 9. Start the Tunnel Client

Start the tunnel client daemon in the active PowerShell session (where `$env:CONTROL_PLANE_API_KEY` is set):

```powershell
& $TunnelClient run --profile remote-control-mcp
```

* **Keep Running:** Leave this terminal pane open. The process must remain active to handle connection dispatches.
* **Structured Logs:** A large volume of structured JSON-RPC and lifecycle logs will stream to the terminal.
* **Automatic UI Launch:** The local Rust GUI application will launch automatically.
* **Initialisation Handshake:** The status label in the Rust GUI will transition to `Connected` once the MCP handshake completes.
* **Admin Interface:** The local tunnel client exposes a browser-based admin UI. By default, this is available at:
  ```text
  http://127.0.0.1:8080/ui
  ```
  *(If your profile health listener configuration differs, use the exact admin UI URL reported by `doctor`)*

---

## 10. Enable ChatGPT Developer Mode

1. Open [ChatGPT](https://chatgpt.com) in your web browser.
2. Go to **Settings** → **Security and login**.
3. Enable **Developer mode**.

*Note: If you are using a managed workspace, the workspace administrator must permit Developer mode before you can toggle this setting.*

---

## 11. Create the ChatGPT Developer-Mode App

Ensure `tunnel-client run` is still active, then:

1. Open **Settings** → **Plugins** (or the corresponding Apps/Connectors manager).
2. Click the plus button to add a new developer-mode application.
3. Configure the metadata:
   * **Name:** `Remote Control MCP`
   * **Description:** `Connects ChatGPT to the local Remote Control MCP development server. Use the ping tool to verify the connection.`
4. Under **Connection**, select **Tunnel**.
5. Choose your tunnel (`Remote Control MCP - Development`) from the dropdown or paste the `tunnel_id`.
6. Select **no authentication** (or the equivalent non-authenticated setup) since this proof-of-concept MCP server does not implement OAuth.
7. Click **Create app**.
8. Verify that ChatGPT detects the exposed tool:
   * `ping`

The newly created application will appear in your workspace draft list.

---

## 12. Test from a New ChatGPT Conversation

> [!IMPORTANT]
> Creating or enabling a developer-mode app does not retroactively add it to an active, existing chat session. You must start a new conversation.

1. Open a **new chat** in ChatGPT.
2. Click the **+** button in the composer box.
3. Select **More** or **Developer mode**.
4. Choose **Remote Control MCP** from the tools list.
5. Send the following prompt:
   ```text
   Use only the Remote Control MCP app. Call its ping tool and report the exact response.
   ```

### Expected Results

* **ChatGPT Output:** `pong`
* **Rust GUI Logging:**
  ```text
  Connected
  Tool 'ping' requested by client
  Tool 'ping' responded with 'pong'
  ```

This confirms the complete path of execution:
`ChatGPT` → `OpenAI Tunnel Control Plane` → `local tunnel-client` → `remote-control-mcp.exe` → `ping tool` → `pong response`.

---

## 13. Stop and Clean Up

When you are finished testing:

1. Stop the `tunnel-client` daemon by pressing `Ctrl+C` in the PowerShell window.
2. Remove the runtime key from the active PowerShell environment variables:
   ```powershell
   Remove-Item Env:CONTROL_PLANE_API_KEY -ErrorAction SilentlyContinue
   ```
3. Close the Rust GUI application window if it does not shut down automatically when the parent process exits.
4. Revoke the API key in the OpenAI Platform Dashboard if you do not plan to reuse it immediately or suspect it was exposed.

---

## 14. Troubleshooting

### MCP executable reported as missing
* **Symptom:** The console reports that the path to `remote-control-mcp.exe` cannot be found or is corrupted (e.g. displaying as `C:pathtoRemoteControlMCPtargetdebug...`).
* **Cause:** The tunnel-client parser stripped the single backslashes in the command path.
* **Fix:** Use double backslashes in your command during initialization (`--mcp-command $McpCommand` with path replacements).

### `CONTROL_PLANE_API_KEY` is not set
* **Symptom:** `invalid control_plane.api_key reference "env:CONTROL_PLANE_API_KEY"`
* **Fix:** Make sure you set the environment variable in the *same* PowerShell session you are running `doctor` or `run` in. If you open a new window, you must set the variable again.

### Profile already exists
* **Symptom:** `profile "remote-control-mcp" already exists`
* **Fix:** If you want to update the profile with new arguments, run the `init` command with the `--force` flag.

### Tunnel does not appear in ChatGPT
* **Check:**
  * Is `tunnel-client run` active and showing successful polling logs?
  * Did you associate the tunnel with the correct ChatGPT workspace in the OpenAI Platform Tunnels settings?
  * Does the app creator account have both `Tunnels Read` and `Tunnels Use` permissions?
  * Is **Developer mode** enabled in ChatGPT?
  * Do all checks in `doctor --explain` show as passed?

### App works in a new chat but not an old chat
* **Cause:** Developer-mode tools are bound strictly to the chat session they are added to. They cannot be activated in a conversation that was started before the app was added to the composer.
* **Fix:** Start a new conversation and enable the tool.

### Doctor shows stdio or OAuth checks as skipped
* **Explanation:** `mcp_server_reachable` and `oauth_metadata` being marked as `SKIP` is standard behaviour for stdio-based profiles. It does not indicate a configuration error.

### Large volume of startup logs
* **Explanation:** Verbose structured logging is normal when the tunnel client initiates. Check the doctor status and the status of your Rust GUI rather than relying on log volume as a health indicator.

---

## References

* [OpenAI Secure MCP Tunnels Guide](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)
* [OpenAI Developer Mode Guide](https://developers.openai.com/api/docs/guides/developer-mode)
* [OpenAI ChatGPT Apps SDK Guide](https://developers.openai.com/apps-sdk/deploy/connect-chatgpt)
* [OpenAI Platform Tunnels Console](https://platform.openai.com/settings/organization/tunnels)
* [OpenAI Platform API Keys Console](https://platform.openai.com/settings/organization/api-keys)
* [ChatGPT Connector Settings Console](https://chatgpt.com/plugins)
