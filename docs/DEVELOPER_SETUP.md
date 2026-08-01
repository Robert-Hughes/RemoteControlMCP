# OpenAI Secure MCP Tunnel Windows Developer Setup

This guide documents the complete Windows developer setup used to connect this local Streamable HTTP Model Context Protocol (MCP) server to ChatGPT through an OpenAI Secure MCP Tunnel.

## 1. Overview and Scope

This setup allows a ChatGPT client to securely invoke the local `remote-control-mcp.exe` application using an outbound-only polling connection. 

* **Private and Local:** The MCP server remains entirely private within the local network.
* **No Public URLs or Inbound Ports:** No inbound firewall ports or public URLs are opened or exposed.
* **Outbound Polling:** The `tunnel-client` daemon establishes an outbound HTTPS connection to the OpenAI-hosted Secure MCP Tunnel control plane and polls for command dispatches.
* **Independent Application Lifecycle:** `remote-control-mcp.exe` runs independently and listens only on `127.0.0.1:61337`; it does not need to be restarted when the tunnel client starts or stops.
* **Loopback HTTP:** `tunnel-client` connects to `http://127.0.0.1:61337/mcp` over Streamable HTTP. This TCP listener is not reachable from other machines.
* **Continuous Operation:** The tunnel client must remain running continuously for ChatGPT application discovery and for executing all incoming MCP tool calls.

This guide is written specifically for developers using **Windows PowerShell** and targeting the **release executable**.

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
             │ Streamable HTTP over loopback TCP
             ▼ (127.0.0.1:61337/mcp)
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
* **ChatGPT Workspace:** A Pro, Plus, Business, Enterprise, or Education account eligible for Developer mode on the web.
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
   cargo build --release
   ```

3. Confirm that the release executable was built successfully:
   ```powershell
   Test-Path .\target\release\remote-control-mcp.exe
   ```
   This command must return `True`.

> [!IMPORTANT]
> Start the application directly before using `doctor`, the tunnel client, or MCP Inspector:
>
> ```powershell
> .\target\release\remote-control-mcp.exe
> ```
>
> The GUI reports that it is waiting for an MCP client and offers to launch the configured tunnel client after the remaining setup steps are complete.

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
> * The key is stored in a separate access-controlled file, not in the generated profile, application configuration, command line, or repository.
> * The setup below removes inherited file permissions and grants access only to the current Windows account.

---

## 6. Configure the PowerShell Session Securely

In your open PowerShell session, define path and identifier variables to make your commands portable. Use an absolute path to the downloaded tunnel client:

```powershell
$TunnelClient = (Resolve-Path -LiteralPath "C:\path\to\tunnel-client.exe").Path
$TunnelId = "tunnel_<your-tunnel-id>"
$McpEndpoint = "http://127.0.0.1:61337/mcp"
```

Record the tunnel-client executable path for the GUI launch button:

```powershell
$LauncherConfigDirectory = Join-Path $env:APPDATA "RemoteControlMCP"
$TunnelClientPathFile = Join-Path $LauncherConfigDirectory "tunnel-client-path.txt"
New-Item -ItemType Directory -Path $LauncherConfigDirectory -Force | Out-Null
[System.IO.File]::WriteAllText(
    $TunnelClientPathFile,
    $TunnelClient,
    [System.Text.UTF8Encoding]::new($false)
)
```

Next, prompt for the runtime API key, write it without a trailing newline or UTF-8 BOM, and restrict the file to the current Windows account:

```powershell
$KeyDirectory = Join-Path $env:APPDATA "tunnel-client"
$KeyFile = Join-Path $KeyDirectory "remote-control-mcp.key"
New-Item -ItemType Directory -Path $KeyDirectory -Force | Out-Null

$secureRuntimeKey = Read-Host "OpenAI tunnel runtime API key" -AsSecureString
$runtimeKey = [System.Net.NetworkCredential]::new("", $secureRuntimeKey).Password
try {
    [System.IO.File]::WriteAllText(
        $KeyFile,
        $runtimeKey,
        [System.Text.UTF8Encoding]::new($false)
    )
} finally {
    Remove-Variable runtimeKey -ErrorAction SilentlyContinue
    Remove-Variable secureRuntimeKey -ErrorAction SilentlyContinue
}

$keyAcl = Get-Acl -LiteralPath $KeyFile
$keyAcl.SetAccessRuleProtection($true, $false)
foreach ($accessRule in @($keyAcl.Access)) {
    [void]$keyAcl.RemoveAccessRuleSpecific($accessRule)
}
$currentUserSid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
$keyRule = [System.Security.AccessControl.FileSystemAccessRule]::new(
    $currentUserSid,
    [System.Security.AccessControl.FileSystemRights]::FullControl,
    [System.Security.AccessControl.AccessControlType]::Allow
)
[void]$keyAcl.AddAccessRule($keyRule)
Set-Acl -LiteralPath $KeyFile -AclObject $keyAcl

$KeyReference = "file:$KeyFile"
```

> [!NOTE]
> The key remains plaintext at rest so that `tunnel-client` can read it non-interactively, but the file's Windows ACL is restricted to your account. The application checks that this exact non-empty file exists but never reads its contents. `tunnel-client` resolves the `file:` reference itself.

---

## 7. Create or Migrate the HTTP Profile

Initialize a local profile named `remote-control-mcp` that points to the already-running loopback HTTP server:

```powershell
& $TunnelClient init `
    --sample sample_mcp_remote_no_auth `
    --profile remote-control-mcp `
    --tunnel-id $TunnelId `
    --mcp-server-url $McpEndpoint `
    --control-plane-api-key-ref $KeyReference
```

The generated configuration profile will be saved to:
`%APPDATA%\tunnel-client\remote-control-mcp.yaml`

If this machine already has the former stdio profile, migrate it by rerunning the command with `--force`. This preserves the named profile and tunnel ID you supply while replacing the old `mcp.command` binding with `mcp.server_urls`:

```powershell
& $TunnelClient init `
    --sample sample_mcp_remote_no_auth `
    --profile remote-control-mcp `
    --tunnel-id $TunnelId `
    --mcp-server-url $McpEndpoint `
    --control-plane-api-key-ref $KeyReference `
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
    --control-plane.api-key $KeyReference `
    --explain
```

### Expected Results

A successful check will print an overall result of:
```text
RESULT ok
NEXT   tunnel-client run --profile remote-control-mcp
```

The application must be running while `doctor` probes the MCP endpoint. The output should show pass results for configuration checks, including profile loading, tunnel ID, API key availability, and MCP server reachability.

> [!NOTE]
> The following status indicators are expected for this local HTTP tunnel:
> * `mcp_server_reachable   PASS` while `remote-control-mcp.exe` is running
> * `oauth_metadata         PASS` with a message that OAuth metadata is not advertised
> * `codex_plugin           SKIP` (Optional check, not required for basic ChatGPT tunnel operation)

---

## 9. Start the Tunnel Client

Start the tunnel client daemon manually with the protected key-file reference:

```powershell
& $TunnelClient run `
    --profile remote-control-mcp `
    --control-plane.api-key $KeyReference
```

For longer development sessions, increase the maximum lifetime of the local MCP transport connection from the tunnel client's 10-minute default:

```powershell
& $TunnelClient run `
    --profile remote-control-mcp `
    --control-plane.api-key $KeyReference `
    --mcp.connection-max-ttl 24h
```

This reduces HTTP session rotation during a typical development session. It does not repair an already stale MCP session; after restarting the tunnel or local MCP process, start a new ChatGPT conversation so that the new connection receives a fresh MCP `initialize` handshake.

Alternatively, select **Start Secure MCP Tunnel** in the already-running application. It uses the recorded tunnel-client path, the fixed `remote-control-mcp` profile, `%APPDATA%\tunnel-client\remote-control-mcp.key`, and its own displayed HTTP endpoint. It starts the tunnel on an ephemeral loopback health port, waits for `/readyz`, and continues monitoring the tunnel process. While it is waiting, **Cancel tunnel launch** terminates the supervised tunnel-client process. Once connected, **Stop Secure MCP Tunnel** terminates it without closing the local MCP application and leaves the tunnel ready to be started again. Logs are written beneath `%TEMP%\RemoteControlMCP`.

* **Manual launch:** Leave the terminal pane open. The process must remain active to handle connection dispatches.
* **GUI-button launch:** The tunnel client runs without a console window and stops when the GUI closes.
* **Structured Logs:** Manual launches write structured logs to the terminal. GUI-button launches write them beneath `%TEMP%\RemoteControlMCP`.
* **Independent UI:** The local Rust GUI remains open whether the tunnel is connected or not.
* **Connection Status:** The compact GUI panel independently shows whether its supervised tunnel client is running, how many HTTP connections are open, and how many MCP sessions are active. The tunnel can be started, cancelled, or stopped without restarting the local MCP server.
* **Admin Interface:** A manually launched tunnel client exposes its browser-based admin UI at the profile's configured health address. By default, this is:
  ```text
  http://127.0.0.1:8080/ui
  ```
  The GUI button deliberately uses an ephemeral loopback health port so it cannot collide with another local service.

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

1. Stop the `tunnel-client` daemon by pressing `Ctrl+C` in its PowerShell window, or stop `tunnel-client.exe` from Task Manager if it was launched by the GUI button.
2. Close the Rust GUI application when finished. If it launched the tunnel client, closing the GUI also stops that tunnel-client process.
3. The protected runtime key file remains available for the next launch. If you are decommissioning the setup, revoke the key in the OpenAI Platform Dashboard and then delete `%APPDATA%\tunnel-client\remote-control-mcp.key`.

---

## 14. Troubleshooting

### MCP server is not reachable
* **Symptom:** `doctor` reports that `http://127.0.0.1:61337/mcp` is unreachable, or the tunnel starts but cannot initialize MCP.
* **Fix:** Start `remote-control-mcp.exe` first and confirm that its GUI shows the same local endpoint. If it reports a bind error, another copy or process already owns port `61337`; close that process before starting this build.

### Runtime API key file is missing
* **Symptom:** The GUI reports that `%APPDATA%\tunnel-client\remote-control-mcp.key` is missing or `tunnel-client` reports an invalid `file:` API-key reference.
* **Fix:** Repeat the key-file creation and ACL commands in section 6, then run `doctor` with `--control-plane.api-key $KeyReference`.

### Tunnel-client executable path is missing
* **Symptom:** The GUI cannot launch `tunnel-client.exe` or reports that `tunnel-client-path.txt` is invalid.
* **Fix:** Repeat the launcher-path commands in section 6. The file must contain one existing absolute path to `tunnel-client.exe`.

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

* **Cause:** Developer-mode app selection is conversation-scoped. Existing conversations may not support an app that was not selected when their tool context was established.
* **Fix:** Start a new conversation and select the app from the composer’s Developer mode tool list.

### Doctor still reports a stdio target
* **Cause:** The existing `remote-control-mcp` profile still contains the former `mcp.command` configuration.
* **Fix:** Run the HTTP `init` command in section 7 with `--force`, then rerun `doctor`. The MCP server reachability check should pass while the application is running.

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
