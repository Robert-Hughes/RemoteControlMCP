# Repaint scheduling

Remote Control MCP uses event-driven egui repainting. The UI must not keep a fixed repaint heartbeat alive while idle.

- MCP worker events pass through the UI event forwarder. After forwarding an event into the GUI queue, it calls `egui::Context::request_repaint()`.
- Loading indicators use the local throttled spinner and request their next frame after 33 ms. Do not use `egui::Spinner` directly because egui 0.36 requests immediate repaint continuously while the spinner is visible.
- The busy window/taskbar icon cooldown schedules one repaint for the exact end of the 30-second cooldown.
- Tunnel startup remains responsive because its visible throttled spinner owns the repaint cadence while startup is in progress.
- When none of those conditions applies, the native event loop is allowed to sleep until input or an explicit wakeup arrives.

Do not add an unconditional `request_repaint_after` to `RemoteControlApp::ui`. Features that need future work should own their own wakeup or deadline.
