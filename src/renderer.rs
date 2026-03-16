use crate::config::ThemeConfig;
use anyhow::{Context, Result};
use fontdue::{Font, FontSettings};
use image::{ImageBuffer, Rgba, RgbaImage};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use vt100::Screen;

/// Parsed RGBA theme ready for rendering.
#[derive(Debug, Clone)]
pub struct Theme {
    pub foreground: Rgba<u8>,
    pub background: Rgba<u8>,
    pub palette: [Rgba<u8>; 16],
}

impl Theme {
    /// Parse a ThemeConfig (hex strings) into an RGBA Theme.
    pub fn from_config(config: &ThemeConfig) -> Result<Self> {
        Ok(Self {
            foreground: parse_hex_color(&config.foreground)?,
            background: parse_hex_color(&config.background)?,
            palette: {
                let mut arr = [Rgba([0, 0, 0, 255]); 16];
                for (i, hex) in config.palette.iter().enumerate() {
                    arr[i] = parse_hex_color(hex)?;
                }
                arr
            },
        })
    }

    /// Fallback dark theme (no config needed).
    pub fn dark() -> Self {
        Self {
            foreground: Rgba([204, 204, 204, 255]),
            background: Rgba([30, 30, 30, 255]),
            palette: [
                Rgba([30, 30, 30, 255]),
                Rgba([204, 0, 0, 255]),
                Rgba([78, 154, 6, 255]),
                Rgba([196, 160, 0, 255]),
                Rgba([52, 101, 164, 255]),
                Rgba([117, 80, 123, 255]),
                Rgba([6, 152, 160, 255]),
                Rgba([211, 215, 207, 255]),
                Rgba([85, 87, 83, 255]),
                Rgba([239, 41, 41, 255]),
                Rgba([138, 226, 52, 255]),
                Rgba([252, 233, 79, 255]),
                Rgba([114, 159, 207, 255]),
                Rgba([173, 127, 168, 255]),
                Rgba([52, 226, 226, 255]),
                Rgba([238, 238, 236, 255]),
            ],
        }
    }
}

fn parse_hex_color(hex: &str) -> Result<Rgba<u8>> {
    let hex = hex.trim_start_matches('#');
    anyhow::ensure!(hex.len() == 6, "Invalid hex color: #{}", hex);
    let r = u8::from_str_radix(&hex[0..2], 16)?;
    let g = u8::from_str_radix(&hex[2..4], 16)?;
    let b = u8::from_str_radix(&hex[4..6], 16)?;
    Ok(Rgba([r, g, b, 255]))
}

/// Renders a vt100 screen buffer to a PNG image.
pub struct Renderer {
    font: Font,
    font_size: f32,
    cell_width: u32,
    cell_height: u32,
    themes: HashMap<String, Theme>,
    default_theme: String,
    padding: u32,
}

impl Renderer {
    pub fn new(
        font_path: &Path,
        font_size: f32,
        theme_configs: &HashMap<String, ThemeConfig>,
        default_theme: &str,
    ) -> Result<Self> {
        let font_data = std::fs::read(font_path)
            .with_context(|| format!("Failed to read font: {:?}", font_path))?;
        let font = Font::from_bytes(font_data, FontSettings::default())
            .map_err(|e| anyhow::anyhow!("Failed to parse font: {}", e))?;

        let metrics = font.metrics('M', font_size);
        let cell_width = metrics.advance_width.ceil() as u32;
        let line_metrics = font.horizontal_line_metrics(font_size).unwrap();
        let cell_height = line_metrics.new_line_size.ceil() as u32;

        // Parse all themes
        let mut themes = HashMap::new();
        for (name, config) in theme_configs {
            match Theme::from_config(config) {
                Ok(theme) => {
                    themes.insert(name.clone(), theme);
                }
                Err(e) => {
                    tracing::warn!("Failed to parse theme '{}': {}", name, e);
                }
            }
        }
        // Always have a fallback
        themes.entry("dark".to_string()).or_insert_with(Theme::dark);

        Ok(Self {
            font,
            font_size,
            cell_width,
            cell_height,
            themes,
            default_theme: default_theme.to_string(),
            padding: 16,
        })
    }

    /// Get a theme by name, falling back to default then "dark".
    pub fn get_theme(&self, name: Option<&str>) -> &Theme {
        let name = name.unwrap_or(&self.default_theme);
        self.themes
            .get(name)
            .or_else(|| self.themes.get(&self.default_theme))
            .or_else(|| self.themes.get("dark"))
            .expect("no themes available")
    }

    /// List available theme names.
    pub fn theme_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.themes.keys().cloned().collect();
        names.sort();
        names
    }

    /// Render raw ANSI bytes to a PNG image.
    /// Returns the path to the saved image and the plain text content.
    pub fn render_bytes(
        &self,
        data: &[u8],
        cols: u16,
        rows: u16,
        output_dir: &Path,
        theme_name: Option<&str>,
    ) -> Result<(PathBuf, String)> {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(data);
        let screen = parser.screen();
        let plain_text = screen
            .rows(0, cols)
            .collect::<Vec<String>>()
            .join("\n")
            .trim_end()
            .to_string();
        let theme = self.get_theme(theme_name);
        let image = self.render_screen(screen, theme)?;

        let id = &uuid::Uuid::new_v4().to_string()[..8];
        let filename = format!("termshot_{}.png", id);
        let path = output_dir.join(&filename);
        image
            .save(&path)
            .with_context(|| format!("Failed to save image to {:?}", path))?;

        Ok((path, plain_text))
    }

    /// Render a vt100 Screen to an RGBA image.
    fn render_screen(&self, screen: &Screen, theme: &Theme) -> Result<RgbaImage> {
        let rows = screen.size().0 as u32;
        let cols = screen.size().1 as u32;

        let content_rows = self.find_content_rows(screen, rows, cols);

        let img_width = cols * self.cell_width + self.padding * 2;
        let img_height = content_rows * self.cell_height + self.padding * 2;

        let mut img: RgbaImage = ImageBuffer::from_pixel(img_width, img_height, theme.background);

        for row in 0..content_rows {
            for col in 0..cols {
                if let Some(cell) = screen.cell(row as u16, col as u16) {
                    let x = col * self.cell_width + self.padding;
                    let y = row * self.cell_height + self.padding;

                    let (fg_color, bg_color) = self.resolve_cell_colors(&cell, theme);

                    // Draw background
                    if bg_color != theme.background {
                        self.draw_rect(&mut img, x, y, self.cell_width, self.cell_height, bg_color);
                    }

                    // Draw character
                    let ch = cell.contents();
                    if !ch.is_empty() && ch != " " {
                        if let Some(c) = ch.chars().next() {
                            self.draw_char(&mut img, x, y, c, fg_color);
                        }
                    }
                }
            }
        }

        Ok(img)
    }

    fn find_content_rows(&self, screen: &Screen, rows: u32, cols: u32) -> u32 {
        let mut last_row_with_content = 0u32;
        for row in 0..rows {
            for col in 0..cols {
                if let Some(cell) = screen.cell(row as u16, col as u16) {
                    let contents = cell.contents();
                    if !contents.is_empty() && contents != " " {
                        last_row_with_content = row + 1;
                        break;
                    }
                }
            }
        }
        (last_row_with_content + 1).min(rows)
    }

    fn resolve_cell_colors(&self, cell: &vt100::Cell, theme: &Theme) -> (Rgba<u8>, Rgba<u8>) {
        let mut fg = self.resolve_color(cell.fgcolor(), true, theme);
        let mut bg = self.resolve_color(cell.bgcolor(), false, theme);

        if cell.inverse() {
            std::mem::swap(&mut fg, &mut bg);
        }

        if cell.bold() {
            // Brighten foreground slightly for bold
            fg = Rgba([
                (fg[0] as u16).min(255) as u8,
                (fg[1] as u16).min(255) as u8,
                (fg[2] as u16).min(255) as u8,
                fg[3],
            ]);
        }

        if cell.dim() {
            fg = Rgba([fg[0] / 2, fg[1] / 2, fg[2] / 2, fg[3]]);
        }

        (fg, bg)
    }

    fn resolve_color(&self, color: vt100::Color, is_foreground: bool, theme: &Theme) -> Rgba<u8> {
        match color {
            vt100::Color::Default => {
                if is_foreground {
                    theme.foreground
                } else {
                    theme.background
                }
            }
            vt100::Color::Idx(idx) => {
                if idx < 16 {
                    theme.palette[idx as usize]
                } else if idx < 232 {
                    // 216-color cube
                    let idx = idx - 16;
                    let r = (idx / 36) % 6;
                    let g = (idx / 6) % 6;
                    let b = idx % 6;
                    Rgba([
                        if r == 0 { 0 } else { 55 + 40 * r },
                        if g == 0 { 0 } else { 55 + 40 * g },
                        if b == 0 { 0 } else { 55 + 40 * b },
                        255,
                    ])
                } else {
                    // Grayscale ramp
                    let level = 8 + 10 * (idx - 232);
                    Rgba([level, level, level, 255])
                }
            }
            vt100::Color::Rgb(r, g, b) => Rgba([r, g, b, 255]),
        }
    }

    fn draw_rect(&self, img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: Rgba<u8>) {
        for dy in 0..h {
            for dx in 0..w {
                let px = x + dx;
                let py = y + dy;
                if px < img.width() && py < img.height() {
                    img.put_pixel(px, py, color);
                }
            }
        }
    }

    fn draw_char(&self, img: &mut RgbaImage, x: u32, y: u32, ch: char, color: Rgba<u8>) {
        let (metrics, bitmap) = self.font.rasterize(ch, self.font_size);
        if bitmap.is_empty() {
            return;
        }

        let line_metrics = self.font.horizontal_line_metrics(self.font_size).unwrap();
        let ascent = line_metrics.ascent;

        let glyph_x = x as i32 + metrics.xmin;
        let glyph_y = y as i32 + (ascent as i32) - metrics.height as i32 - metrics.ymin;

        for gy in 0..metrics.height {
            for gx in 0..metrics.width {
                let alpha = bitmap[gy * metrics.width + gx];
                if alpha == 0 {
                    continue;
                }

                let px = glyph_x + gx as i32;
                let py = glyph_y + gy as i32;

                if px >= 0 && py >= 0 && (px as u32) < img.width() && (py as u32) < img.height() {
                    let px = px as u32;
                    let py = py as u32;

                    if alpha == 255 {
                        img.put_pixel(px, py, color);
                    } else {
                        let bg = img.get_pixel(px, py);
                        let a = alpha as f32 / 255.0;
                        let blended = Rgba([
                            (color[0] as f32 * a + bg[0] as f32 * (1.0 - a)) as u8,
                            (color[1] as f32 * a + bg[1] as f32 * (1.0 - a)) as u8,
                            (color[2] as f32 * a + bg[2] as f32 * (1.0 - a)) as u8,
                            255,
                        ]);
                        img.put_pixel(px, py, blended);
                    }
                }
            }
        }
    }
}
