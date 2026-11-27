use crate::capture::LineSelection;
use anyhow::{Context, Result, bail};
use image::RgbaImage;
use serde::{Deserialize, Serialize};
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Terminal emulation and drawing are selected together, before parsing bytes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[default]
    Builtin,
    /// Experimental Linux GTK4/VTE renderer; captures the current viewport.
    Vte,
}

impl std::str::FromStr for Backend {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "builtin" => Ok(Self::Builtin),
            "vte" => Ok(Self::Vte),
            _ => bail!("unknown backend {value:?}: use 'builtin' or experimental 'vte'"),
        }
    }
}

impl Backend {
    pub fn validate(self, lines: LineSelection, redaction: bool) -> Result<()> {
        if self == Self::Vte {
            if redaction {
                bail!(
                    "the experimental VTE backend does not support redaction; \
                     use --backend builtin to redact a screenshot"
                );
            }
            if !lines.is_all() {
                bail!(
                    "the experimental VTE backend captures only the current viewport; \
                     head_lines/tail_lines are unsupported; use --backend builtin"
                );
            }
        }
        Ok(())
    }

    /// Check native dependencies and display access before executing a command.
    pub fn check(self) -> Result<()> {
        if self == Self::Vte {
            run_helper(None)?;
        }
        Ok(())
    }
}

pub const VTE_VIEWPORT_NOTICE: &str = "VTE captured only the current viewport; earlier output is not in this PNG. \
     Use --backend builtin to include scrollback.";

const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_NATIVE_PIXELS: u64 = 16_000_000;
const MAX_OUTPUT_BYTES: u64 = MAX_NATIVE_PIXELS * 4 + 4 * 1024 * 1024;
const HELPER_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Serialize)]
pub(crate) struct VteRequest<'a> {
    pub data: &'a [u8],
    pub cols: u16,
    pub rows: u16,
    pub font_size: f32,
    pub font: Option<&'a Path>,
    pub font_bold: Option<&'a Path>,
    pub fallback_fonts: &'a [std::path::PathBuf],
    pub embedded_font: &'a [u8],
    pub foreground: [u8; 4],
    pub background: [u8; 4],
    pub palette: [[u8; 4]; 16],
    pub scale: u32,
}

#[derive(Deserialize)]
struct VteResponse {
    width: u32,
    height: u32,
    cell_width: u32,
    cell_height: u32,
    text: String,
    truncated: bool,
}

pub(crate) struct NativeCapture {
    pub image: RgbaImage,
    pub cell_width: u32,
    pub cell_height: u32,
    pub text: String,
    pub truncated: bool,
}

pub(crate) fn render_vte(request: &VteRequest<'_>) -> Result<NativeCapture> {
    anyhow::ensure!(
        (1..=500).contains(&request.cols) && (1..=500).contains(&request.rows),
        "VTE terminal dimensions must be between 1 and 500"
    );
    anyhow::ensure!(
        request.font_size.is_finite() && request.font_size > 0.0 && request.font_size <= 128.0,
        "VTE font_size must be greater than zero and at most 128 pixels"
    );
    anyhow::ensure!(
        request.data.len() <= MAX_INPUT_BYTES,
        "the experimental VTE backend accepts at most 4 MiB of ANSI input"
    );
    let output = run_helper(Some(request))?;
    let header_end = output
        .iter()
        .position(|byte| *byte == b'\n')
        .context("VTE helper returned no response header")?;
    anyhow::ensure!(
        header_end <= 2 * 1024 * 1024,
        "VTE response header is too large"
    );
    let response: VteResponse =
        serde_json::from_slice(&output[..header_end]).context("invalid VTE response header")?;
    anyhow::ensure!(
        response.width > 0
            && response.height > 0
            && u64::from(response.width) * u64::from(response.height) <= MAX_NATIVE_PIXELS
            && response.cell_width > 0
            && response.cell_height > 0
            && u64::from(response.width)
                == u64::from(request.cols) * u64::from(response.cell_width)
            && u64::from(response.height)
                == u64::from(request.rows) * u64::from(response.cell_height),
        "VTE helper returned invalid image dimensions"
    );
    let mut reader = image::ImageReader::with_format(
        std::io::Cursor::new(&output[header_end + 1..]),
        image::ImageFormat::Png,
    );
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(response.width);
    limits.max_image_height = Some(response.height);
    limits.max_alloc = Some(MAX_NATIVE_PIXELS * 4);
    reader.limits(limits);
    let image = reader.decode().context("invalid VTE PNG")?.into_rgba8();
    anyhow::ensure!(
        image.dimensions() == (response.width, response.height),
        "VTE PNG dimensions do not match its response header"
    );
    Ok(NativeCapture {
        image,
        cell_width: response.cell_width,
        cell_height: response.cell_height,
        text: response.text,
        truncated: response.truncated,
    })
}

fn read_bounded(reader: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= limit,
        "VTE helper output exceeded its limit"
    );
    Ok(bytes)
}

fn run_helper(request: Option<&VteRequest<'_>>) -> Result<Vec<u8>> {
    anyhow::ensure!(
        cfg!(target_os = "linux"),
        "the experimental VTE backend requires Linux"
    );
    let mut command = Command::new("/usr/bin/python3");
    command
        .args(["-I", "-c", include_str!("vte_helper.py")])
        .env("GTK_A11Y", "none")
        .stdin(if request.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if request.is_none() {
        command.arg("--check");
    }
    let mut child = command
        .spawn()
        .context("cannot start the VTE helper: install /usr/bin/python3 and the GTK4/VTE introspection packages")?;
    let input = child.stdin.take();
    let output = child.stdout.take().context("missing VTE stdout pipe")?;
    let errors = child.stderr.take().context("missing VTE stderr pipe")?;
    std::thread::scope(|scope| {
        let writer = scope.spawn(move || -> Result<()> {
            if let (Some(input), Some(request)) = (input, request) {
                let mut input = BufWriter::new(input);
                serde_json::to_writer(&mut input, request).context("cannot write VTE request")?;
                input.flush().context("cannot flush VTE request")?;
            }
            Ok(())
        });
        let stdout = scope.spawn(move || read_bounded(output, MAX_OUTPUT_BYTES));
        let stderr = scope.spawn(move || read_bounded(errors, 64 * 1024));
        let deadline = Instant::now() + HELPER_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => break Err(anyhow::anyhow!("VTE capture timed out after 20 seconds")),
                Err(error) => break Err(error.into()),
            }
        };
        if status.is_err() {
            if let Err(error) = child.kill() {
                tracing::warn!("Failed to stop VTE helper: {error}");
            }
            if let Err(error) = child.wait() {
                tracing::warn!("Failed to reap VTE helper: {error}");
            }
        }
        let output = stdout
            .join()
            .map_err(|_| anyhow::anyhow!("VTE output reader panicked"))??;
        let errors = stderr
            .join()
            .map_err(|_| anyhow::anyhow!("VTE error reader panicked"))??;
        let written = writer
            .join()
            .map_err(|_| anyhow::anyhow!("VTE input writer panicked"))?;
        let status = status?;
        let diagnostic = String::from_utf8_lossy(&errors);
        anyhow::ensure!(status.success(), "VTE helper failed: {}", diagnostic.trim());
        written?;
        if !diagnostic.trim().is_empty() {
            tracing::warn!("VTE: {}", diagnostic.trim());
        }
        Ok(output)
    })
}
