mod config;
mod executor;
mod renderer;
mod server;

use clap::{Parser, Subcommand};
use config::Config;
use renderer::Renderer;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "termshot",
    about = "Capture terminal screenshots with full ANSI rendering",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Path to config file (default: ~/.config/termshot/config.toml)
    #[arg(long, global = true)]
    config: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the MCP server (stdio transport)
    Mcp,

    /// Execute a command and save a terminal screenshot
    Exec {
        /// The command to execute (everything after --)
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,

        /// Terminal width in columns
        #[arg(short = 'c', long, default_value_t = 120)]
        cols: u16,

        /// Terminal height in rows
        #[arg(short = 'r', long, default_value_t = 40)]
        rows: u16,

        /// Timeout in seconds
        #[arg(short, long, default_value_t = 30)]
        timeout: u64,

        /// Hide interactive shell prompt (PS1) from screenshot.
        /// By default the command runs in an interactive login shell so
        /// the prompt is visible. Use --no-prompt to run without it.
        #[arg(long = "no-prompt", default_value_t = false)]
        no_prompt: bool,

        /// Theme name (e.g. dark, adamkadaban, catppuccin-mocha, dracula, nord)
        #[arg(long)]
        theme: Option<String>,

        /// Output file path (default: auto-generated in output dir)
        #[arg(short, long)]
        output: Option<String>,
    },

    /// Render an ANSI file to a PNG screenshot
    Render {
        /// Path to file containing raw ANSI terminal output
        input: String,

        /// Terminal width in columns
        #[arg(short = 'c', long, default_value_t = 120)]
        cols: u16,

        /// Terminal height in rows
        #[arg(short = 'r', long, default_value_t = 40)]
        rows: u16,

        /// Theme name
        #[arg(long)]
        theme: Option<String>,

        /// Output file path
        #[arg(short, long)]
        output: Option<String>,
    },

    /// List available themes
    Themes,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Set up logging to stderr
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("screenshot_mcp=info".parse()?),
        )
        .with_writer(std::io::stderr)
        .init();

    let config = Config::load(cli.config.as_deref())?;

    match cli.command {
        Commands::Mcp => {
            let renderer = Renderer::new(
                &config.font_path,
                config.font_size,
                &config.themes,
                &config.default_theme,
            )?;
            server::run_mcp_server(config, renderer).await?;
        }
        Commands::Exec {
            command,
            cols,
            rows,
            timeout,
            no_prompt,
            theme,
            output,
        } => {
            let renderer = Renderer::new(
                &config.font_path,
                config.font_size,
                &config.themes,
                &config.default_theme,
            )?;

            let cmd_str = command.join(" ");
            let timeout = Duration::from_secs(timeout);

            let exec_result = if !no_prompt {
                executor::execute_command(&cmd_str, &config.shell, rows, cols, timeout).await?
            } else {
                executor::execute_command_simple(&cmd_str, &config.shell, rows, cols, timeout)
                    .await?
            };

            let theme_name = theme.as_deref();
            let (image_path, plain_text) = renderer.render_bytes(
                &exec_result.raw_output,
                cols,
                rows,
                &config.output_dir,
                theme_name,
            )?;

            // If user specified an output path, move the file there
            let final_path = if let Some(out) = output {
                let out = std::path::PathBuf::from(&out);
                std::fs::rename(&image_path, &out)?;
                out
            } else {
                image_path
            };

            // Print screenshot path to stdout (easy to capture), everything else to stderr
            println!("{}", final_path.display());

            let exit_info = if exec_result.timed_out {
                "TIMED OUT".to_string()
            } else {
                match exec_result.exit_code {
                    Some(code) => format!("exit code: {}", code),
                    None => "unknown".to_string(),
                }
            };
            eprintln!("Status: {}", exit_info);
            eprintln!("--- Terminal Output ---");
            eprint!("{}", plain_text);
        }
        Commands::Render {
            input,
            cols,
            rows,
            theme,
            output,
        } => {
            let renderer = Renderer::new(
                &config.font_path,
                config.font_size,
                &config.themes,
                &config.default_theme,
            )?;

            let data = std::fs::read(&input)?;
            let theme_name = theme.as_deref();
            let (image_path, plain_text) =
                renderer.render_bytes(&data, cols, rows, &config.output_dir, theme_name)?;

            let final_path = if let Some(out) = output {
                let out = std::path::PathBuf::from(&out);
                std::fs::rename(&image_path, &out)?;
                out
            } else {
                image_path
            };

            println!("{}", final_path.display());
            eprintln!("--- Terminal Output ---");
            eprint!("{}", plain_text);
        }
        Commands::Themes => {
            let renderer = Renderer::new(
                &config.font_path,
                config.font_size,
                &config.themes,
                &config.default_theme,
            )?;
            let names = renderer.theme_names();
            let default = &config.default_theme;
            for name in &names {
                if name == default {
                    println!("{} (default)", name);
                } else {
                    println!("{}", name);
                }
            }
        }
    }

    Ok(())
}
