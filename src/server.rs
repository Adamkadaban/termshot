use crate::config::Config;
use crate::executor;
use crate::renderer::Renderer;
use rmcp::{
    handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::*,
    schemars, tool, tool_handler, tool_router,
    service::ServiceExt,
    transport::io::stdio,
    ErrorData as McpError,
    ServerHandler,
};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExecuteAndScreenshotParams {
    /// Shell command to execute. The command is run in an interactive shell,
    /// so the PS1 prompt (username, directory, etc.) will be visible in the
    /// screenshot.
    pub command: String,

    /// Terminal width in columns. Defaults to server config value (120).
    #[serde(default)]
    pub cols: Option<u16>,

    /// Terminal height in rows. Defaults to server config value (40).
    #[serde(default)]
    pub rows: Option<u16>,

    /// Maximum time in seconds to wait for the command to finish. Defaults to 30.
    #[serde(default)]
    pub timeout_secs: Option<u64>,

    /// If true, run the command in an interactive login shell so the PS1
    /// prompt is rendered. If false, run the command directly (no prompt).
    /// Defaults to true.
    #[serde(default)]
    pub show_prompt: Option<bool>,

    /// Theme name for rendering. Uses default theme from config if not specified.
    #[serde(default)]
    pub theme: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RenderAnsiParams {
    /// Path to a file containing raw terminal output with ANSI escape sequences.
    pub input_path: String,

    /// Terminal width in columns for rendering. Defaults to 120.
    #[serde(default)]
    pub cols: Option<u16>,

    /// Terminal height in rows for rendering. Defaults to 40.
    #[serde(default)]
    pub rows: Option<u16>,

    /// Theme name for rendering. Uses default theme from config if not specified.
    #[serde(default)]
    pub theme: Option<String>,
}

#[derive(Clone)]
pub struct ScreenshotServer {
    config: Arc<Config>,
    renderer: Arc<Renderer>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl ScreenshotServer {
    pub fn new(config: Config, renderer: Renderer) -> Self {
        Self {
            config: Arc::new(config),
            renderer: Arc::new(renderer),
            tool_router: Self::tool_router(),
        }
    }

    /// Execute a shell command and capture a screenshot of the terminal output.
    ///
    /// Runs the command in a PTY (pseudo-terminal) so that all ANSI escape
    /// sequences (colors, formatting, cursor movement) are captured and
    /// rendered into a PNG image that looks like a real terminal.
    ///
    /// When show_prompt is true (default), the command is run inside an
    /// interactive login shell, so the screenshot includes the PS1 prompt
    /// with username, hostname, current directory, etc.
    ///
    /// Returns the path to the saved PNG screenshot and the plain text output.
    #[tool(name = "execute_and_screenshot")]
    async fn execute_and_screenshot(
        &self,
        Parameters(params): Parameters<ExecuteAndScreenshotParams>,
    ) -> Result<CallToolResult, McpError> {
        let cols = params.cols.unwrap_or(self.config.default_cols);
        let rows = params.rows.unwrap_or(self.config.default_rows);
        let timeout = Duration::from_secs(
            params.timeout_secs.unwrap_or(self.config.default_timeout_secs),
        );
        let show_prompt = params.show_prompt.unwrap_or(true);

        let result = if show_prompt {
            executor::execute_command(&params.command, &self.config.shell, rows, cols, timeout)
                .await
        } else {
            executor::execute_command_simple(
                &params.command,
                &self.config.shell,
                rows,
                cols,
                timeout,
            )
            .await
        };

        let exec_result = result.map_err(|e| {
            McpError::internal_error(format!("Command execution failed: {}", e), None)
        })?;

        let theme_name = params.theme.as_deref();
        let (image_path, plain_text) = self
            .renderer
            .render_bytes(
                &exec_result.raw_output,
                cols,
                rows,
                &self.config.output_dir,
                theme_name,
            )
            .map_err(|e| McpError::internal_error(format!("Rendering failed: {}", e), None))?;

        let exit_info = if exec_result.timed_out {
            "TIMED OUT".to_string()
        } else {
            match exec_result.exit_code {
                Some(code) => format!("exit code: {}", code),
                None => "exit code: unknown".to_string(),
            }
        };

        Ok(CallToolResult::success(vec![
            Content::text(format!("Screenshot saved to: {}", image_path.display())),
            Content::text(format!("Status: {}", exit_info)),
            Content::text(format!("--- Terminal Output ---\n{}", plain_text)),
        ]))
    }

    /// Render a file containing raw ANSI terminal output to a PNG screenshot.
    ///
    /// Takes a path to a file that contains terminal output with ANSI escape
    /// sequences and renders it to a PNG image. Useful for rendering
    /// previously captured output.
    #[tool(name = "render_ansi")]
    async fn render_ansi(
        &self,
        Parameters(params): Parameters<RenderAnsiParams>,
    ) -> Result<CallToolResult, McpError> {
        let cols = params.cols.unwrap_or(self.config.default_cols);
        let rows = params.rows.unwrap_or(self.config.default_rows);
        let theme_name = params.theme.as_deref();

        let data = std::fs::read(&params.input_path).map_err(|e| {
            McpError::internal_error(
                format!("Failed to read file '{}': {}", params.input_path, e),
                None,
            )
        })?;

        let (image_path, plain_text) = self
            .renderer
            .render_bytes(&data, cols, rows, &self.config.output_dir, theme_name)
            .map_err(|e| McpError::internal_error(format!("Rendering failed: {}", e), None))?;

        Ok(CallToolResult::success(vec![
            Content::text(format!("Screenshot saved to: {}", image_path.display())),
            Content::text(format!("--- Terminal Output ---\n{}", plain_text)),
        ]))
    }
}

#[tool_handler]
impl ServerHandler for ScreenshotServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "Terminal screenshot MCP server. Use execute_and_screenshot to run \
                 commands and capture PNG screenshots of the terminal output, including \
                 PS1 prompt, colors, and full ANSI rendering. Use render_ansi to render \
                 previously captured terminal output from a file.",
            )
    }
}

/// Start the MCP server on stdio.
pub async fn run_mcp_server(config: Config, renderer: Renderer) -> anyhow::Result<()> {
    tracing::info!("Starting screenshot-mcp server (stdio transport)");
    let server = ScreenshotServer::new(config, renderer);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
