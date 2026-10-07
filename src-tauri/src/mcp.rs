//! Tauri commands over `monocode_process::mcp`.
use tauri::{AppHandle, State};

use monocode_process::mcp::{self, McpConnection};

#[tauri::command]
pub async fn mcp_add(
    app: AppHandle,
    host: State<'_, crate::harness::HarnessHost>,
    cwd: String,
    provider: String,
    scope: String,
    name: String,
    config: String,
) -> Result<(), String> {
    let host = host.inner().clone();
    let profile = crate::harness::default_claude_mcp_profile(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        mcp::mcp_add(&host, Some(&profile), cwd, provider, scope, name, config)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn mcp_discover(cwd: String) -> Result<Vec<McpConnection>, String> {
    tauri::async_runtime::spawn_blocking(move || mcp::mcp_discover(cwd, None))
        .await
        .map_err(|e| e.to_string())?
}
