//! Regex-based redaction engine.
//!
//! The redaction pass sits between vt100 parsing and PNG rendering:
//!
//! ```text
//! raw PTY bytes -> vt100 parser -> redaction pass -> render to PNG
//! ```
//!
//! It scans the parsed [`vt100::Screen`] cell by cell, runs a set of regex
//! rules against each row's text, and produces a [`RedactionMap`] describing
//! which cells must be masked. The renderer consults that map and draws bright
//! red blocks (with an optional short label such as `[IP]`) in place of the
//! sensitive characters. The vt100 buffer itself is never mutated, which keeps
//! the pass side-effect free and avoids re-parsing fragile ANSI streams.
//!
//! The engine deliberately never records the *values* it redacts -- only the
//! per-rule match counts -- so audit logs cannot leak the very data they are
//! meant to protect.

use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use vt100::Screen;

/// A single user-configurable redaction rule as read from `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactionRuleConfig {
    /// Unique rule name (also used to override a compiled-in builtin).
    pub name: String,
    /// Regex pattern to match against terminal row text.
    #[serde(default)]
    pub pattern: String,
    /// Text used to derive the audit label / replacement marker.
    #[serde(default = "default_replacement")]
    pub replacement: String,
    /// Whether the rule participates in redaction.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional per-rule redaction block color as `#RRGGBB`. Falls back to the
    /// engine-wide `[redaction] color` and then the built-in red.
    #[serde(default)]
    pub color: Option<String>,
    /// Optional minimum Shannon entropy (bits/char) of the matched text; a
    /// match with lower entropy is ignored. Used by YAML rule files to
    /// suppress low-randomness false positives.
    #[serde(default)]
    pub min_entropy: Option<f64>,
    /// Number of leading matched characters to leave unmasked (partial
    /// redaction). When set, the prefix renders normally and only the
    /// characters after it are blocked out, e.g. `AKIA████████████████`.
    #[serde(default)]
    pub keep_prefix: Option<usize>,
    /// Number of trailing matched characters to leave unmasked (partial
    /// redaction). When set, the suffix renders normally and only the
    /// characters before it are blocked out.
    #[serde(default)]
    pub keep_suffix: Option<usize>,
}

impl RedactionRuleConfig {
    /// Construct a rule with just the core fields set (used for builtins and
    /// ad-hoc/agent-supplied rules); optional fields default to `None`.
    pub fn new(name: &str, pattern: &str, replacement: &str) -> Self {
        Self {
            name: name.to_string(),
            pattern: pattern.to_string(),
            replacement: replacement.to_string(),
            enabled: true,
            color: None,
            min_entropy: None,
            keep_prefix: None,
            keep_suffix: None,
        }
    }
}

fn default_replacement() -> String {
    "[REDACTED]".to_string()
}

fn default_true() -> bool {
    true
}

/// Default redaction block color (bright red, `#d41919`).
pub const DEFAULT_BLOCK_COLOR: [u8; 3] = [212, 25, 25];
/// Default redaction label text color (black).
pub const DEFAULT_LABEL_COLOR: [u8; 3] = [0, 0, 0];

/// The `[redaction]` section of the config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RedactionConfig {
    /// Master switch: when false, `auto` redaction never runs (an explicit
    /// `--redact` / `redact: true` request still forces it on).
    pub enabled: bool,
    /// When true, redact every screenshot without needing an explicit flag.
    /// Off by default: a false positive silently corrupts a screenshot of
    /// ordinary output, which is worse than requiring `--redact` when you are
    /// capturing something sensitive.
    pub auto: bool,
    /// Default redaction block color as `#RRGGBB` (default red).
    #[serde(default)]
    pub color: Option<String>,
    /// Redaction label text color as `#RRGGBB` (default black).
    #[serde(default)]
    pub label_color: Option<String>,
    /// Optional directory of extra rule files (`.toml`, `.yaml`, `.yml`) to
    /// load in addition to the inline `rules` below. The generic YAML rule
    /// format is supported.
    #[serde(default)]
    pub rules_path: Option<String>,
    /// User rule overrides / additions. These are merged on top of the
    /// compiled-in builtin rules, keyed by `name`.
    pub rules: Vec<RedactionRuleConfig>,
}

impl Default for RedactionConfig {
    fn default() -> Self {
        Self {
            // The rules are available (so `--redact` / `redact: true` works out
            // of the box) but do not run unattended: automatic redaction of
            // every screenshot risks masking legitimate output, and a corrupted
            // capture is a worse default than an unmasked one. Opt in with
            // `auto = true` in config when you always want it.
            enabled: true,
            auto: false,
            color: None,
            label_color: None,
            rules_path: None,
            rules: Vec::new(),
        }
    }
}

/// Decide whether redaction should run for a single screenshot.
///
/// * an explicit "no redact" request always wins and disables redaction;
/// * `enabled = false` is the master switch: nothing runs, not even on an
///   explicit request (callers should surface that as an error rather than
///   silently hand back an unredacted image - see
///   [`explicit_request_is_blocked`]);
/// * an explicit "redact" request then forces redaction on, even when `auto`
///   is off;
/// * otherwise redaction runs only when it is both `enabled` and `auto`.
pub fn resolve_should_redact(cfg: &RedactionConfig, redact: bool, no_redact: bool) -> bool {
    if no_redact || !cfg.enabled {
        return false;
    }
    if redact {
        return true;
    }
    cfg.auto
}

/// True when the caller explicitly asked for redaction but the master switch
/// (`[redaction] enabled`) is off, so the request cannot be honored. Callers
/// must fail loudly in that case: quietly writing an unredacted screenshot for
/// someone who typed `--redact` is the worst possible outcome.
pub fn explicit_request_is_blocked(cfg: &RedactionConfig, redact: bool, no_redact: bool) -> bool {
    redact && !no_redact && !cfg.enabled
}

/// Error message for a blocked explicit request, shared by the CLI and MCP.
pub const REDACTION_DISABLED_MSG: &str =
    "redaction was requested but is disabled in config ([redaction] enabled = false); \
     set enabled = true to use --redact / redact: true";

/// A compiled rule ready to match against terminal text.
struct CompiledRule {
    name: String,
    /// On-image label, or `None` to draw a plain block with no text overlay.
    label: Option<String>,
    regex: Regex,
    /// True when the pattern declares a `(?P<redact>...)` group. Such a rule
    /// may match surrounding context (to reject look-alikes such as `std::fs`)
    /// while only the captured group is masked.
    has_redact_group: bool,
    /// Optional predicate; when it returns true for a match the match is
    /// skipped (used to keep obviously non-sensitive values like `127.0.0.1`).
    skip: Option<fn(&str) -> bool>,
    /// When set, ignore matches whose Shannon entropy is below this threshold.
    min_entropy: Option<f64>,
    /// Block color for this rule's redactions.
    block_color: [u8; 3],
    /// Label text color for this rule's redactions.
    label_color: [u8; 3],
    /// Leading matched characters to leave unmasked (partial redaction).
    keep_prefix: usize,
    /// Trailing matched characters to leave unmasked (partial redaction).
    keep_suffix: usize,
}

impl CompiledRule {
    /// Byte ranges of this rule's matches in `line`. When the pattern declares
    /// a `redact` capture group, only that group's span is returned, so a rule
    /// can require surrounding context without masking it.
    fn match_spans(&self, line: &str) -> Vec<(usize, usize)> {
        if self.has_redact_group {
            self.regex
                .captures_iter(line)
                .filter_map(|caps| caps.name("redact").map(|m| (m.start(), m.end())))
                .collect()
        } else {
            self.regex
                .find_iter(line)
                .map(|m| (m.start(), m.end()))
                .collect()
        }
    }
}

/// A compiled set of redaction rules.
pub struct RedactionEngine {
    rules: Vec<CompiledRule>,
}

impl RedactionEngine {
    /// Build an engine by merging the compiled-in builtin rules with any user
    /// overrides / additions from config. A user rule with the same `name` as
    /// a builtin overrides it (letting users tweak a pattern, disable a rule,
    /// or change its replacement); other user rules are appended.
    pub fn from_config(cfg: &RedactionConfig) -> Result<Self> {
        Self::from_config_with_labels(cfg, true)
    }

    /// Like [`from_config`](Self::from_config) but lets the caller suppress the
    /// on-image `[LABEL]` tags, drawing plain solid blocks instead.
    pub fn from_config_with_labels(cfg: &RedactionConfig, show_labels: bool) -> Result<Self> {
        // Start from the builtin defaults, preserving their order.
        let mut order: Vec<String> = Vec::new();
        let mut merged: HashMap<String, RedactionRuleConfig> = HashMap::new();
        for rule in builtin_rules() {
            order.push(rule.name.clone());
            merged.insert(rule.name.clone(), rule);
        }

        // Overlay user rules.
        for user in &cfg.rules {
            if let Some(existing) = merged.get_mut(&user.name) {
                // Override a builtin: an empty pattern means "keep the builtin
                // pattern but apply the other fields" (e.g. just disabling it).
                if !user.pattern.is_empty() {
                    existing.pattern = user.pattern.clone();
                }
                existing.replacement = user.replacement.clone();
                existing.enabled = user.enabled;
                existing.min_entropy = user.min_entropy;
                if user.color.is_some() {
                    existing.color = user.color.clone();
                }
                if user.keep_prefix.is_some() {
                    existing.keep_prefix = user.keep_prefix;
                }
                if user.keep_suffix.is_some() {
                    existing.keep_suffix = user.keep_suffix;
                }
            } else {
                order.push(user.name.clone());
                merged.insert(user.name.clone(), user.clone());
            }
        }

        // Resolve engine-wide default colors.
        let default_block = cfg
            .color
            .as_deref()
            .and_then(parse_hex_rgb)
            .unwrap_or(DEFAULT_BLOCK_COLOR);
        let default_label = cfg
            .label_color
            .as_deref()
            .and_then(parse_hex_rgb)
            .unwrap_or(DEFAULT_LABEL_COLOR);

        let ordered: Vec<RedactionRuleConfig> = order
            .into_iter()
            .map(|name| merged[&name].clone())
            .collect();
        Self::compile(&ordered, default_block, default_label, show_labels)
    }

    /// Build an engine from an explicit list of rules only (no builtins). Used
    /// for agent-driven, ad-hoc redaction where the caller supplies exactly the
    /// patterns to apply.
    pub fn from_rules(rules: &[RedactionRuleConfig]) -> Result<Self> {
        Self::from_rules_with_labels(rules, true)
    }

    /// Like [`from_rules`](Self::from_rules) but lets the caller suppress the
    /// on-image `[LABEL]` tags, drawing plain solid blocks instead.
    pub fn from_rules_with_labels(
        rules: &[RedactionRuleConfig],
        show_labels: bool,
    ) -> Result<Self> {
        Self::compile(rules, DEFAULT_BLOCK_COLOR, DEFAULT_LABEL_COLOR, show_labels)
    }

    /// Compile a fully-resolved rule list into an engine. When `show_labels` is
    /// false, every rule renders as a plain block with no text overlay.
    fn compile(
        rules: &[RedactionRuleConfig],
        default_block: [u8; 3],
        default_label: [u8; 3],
        show_labels: bool,
    ) -> Result<Self> {
        let mut compiled = Vec::new();
        for rc in rules {
            if !rc.enabled || rc.pattern.is_empty() {
                continue;
            }
            let regex = Regex::new(&rc.pattern)
                .with_context(|| format!("invalid regex for redaction rule '{}'", rc.name))?;
            let block_color = rc
                .color
                .as_deref()
                .and_then(parse_hex_rgb)
                .unwrap_or(default_block);
            let label = if show_labels {
                label_for(&rc.name, &rc.replacement)
            } else {
                None
            };
            compiled.push(CompiledRule {
                name: rc.name.clone(),
                label,
                has_redact_group: regex.capture_names().flatten().any(|n| n == "redact"),
                regex,
                skip: builtin_skip(&rc.name),
                min_entropy: rc.min_entropy,
                block_color,
                label_color: default_label,
                keep_prefix: rc.keep_prefix.unwrap_or(0),
                keep_suffix: rc.keep_suffix.unwrap_or(0),
            });
        }
        Ok(Self { rules: compiled })
    }

    /// Number of active (enabled, compiled) rules.
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Names of the active (enabled, compiled) rules.
    pub fn rule_names(&self) -> Vec<&str> {
        self.rules.iter().map(|r| r.name.as_str()).collect()
    }

    /// Validate that every name in `requested` matches an active rule. Returns
    /// an error listing the unknown names so an explicit `--redact-rules`
    /// request fails loudly instead of silently selecting zero rules.
    pub fn validate_rule_names(&self, requested: &[String]) -> Result<()> {
        let unknown: Vec<&str> = requested
            .iter()
            .filter(|name| !self.rules.iter().any(|r| &r.name == *name))
            .map(|s| s.as_str())
            .collect();
        if !unknown.is_empty() {
            let available = self.rule_names().into_iter().collect::<Vec<_>>().join(", ");
            anyhow::bail!(
                "unknown redaction rule(s): {}. Available rules: {}",
                unknown.join(", "),
                available
            );
        }
        Ok(())
    }

    /// Scan a parsed screen and build the map of cells to redact.
    ///
    /// When `only` is `Some`, only rules whose name appears in the list are
    /// applied; otherwise every enabled rule runs.
    pub fn redact_screen(&self, screen: &Screen, only: Option<&[String]>) -> RedactionMap {
        let mut map = RedactionMap::default();
        self.redact_screen_into(screen, only, &mut map);
        map
    }

    /// Scan a parsed screen and merge redactions into an existing map (so
    /// multiple engines / manual redactions can be combined).
    ///
    /// Rules run against *logical* lines: soft-wrapped physical rows are joined
    /// before matching, so a secret that crosses the right margin is still
    /// recognized. Every byte of the joined line keeps a `(row, column)`
    /// back-reference, so a match spanning the wrap masks cells on both rows.
    pub fn redact_screen_into(
        &self,
        screen: &Screen,
        only: Option<&[String]>,
        map: &mut RedactionMap,
    ) {
        let mut counts: HashMap<String, usize> = HashMap::new();

        for (line, positions) in logical_lines(screen) {
            for rule in &self.rules {
                if let Some(names) = only {
                    if !names.iter().any(|n| n == &rule.name) {
                        continue;
                    }
                }

                for (start, end) in rule.match_spans(&line) {
                    let matched = &line[start..end];
                    if let Some(skip) = rule.skip {
                        if skip(matched) {
                            continue;
                        }
                    }
                    if let Some(threshold) = rule.min_entropy {
                        if shannon_entropy(matched) < threshold {
                            continue;
                        }
                    }

                    // Collect the distinct cells this match covers, which may
                    // span more than one physical row when the line wrapped.
                    let mut match_cells: Vec<(u16, u16)> = Vec::new();
                    for &pos in &positions[start..end] {
                        if match_cells.last() != Some(&pos) {
                            match_cells.push(pos);
                        }
                    }
                    if match_cells.is_empty() {
                        continue;
                    }

                    // Partial redaction: leave the first `keep_prefix` and last
                    // `keep_suffix` matched cells visible, blocking only the
                    // middle. If the kept prefix/suffix cover the whole match
                    // there is nothing left to redact, so skip it entirely.
                    let start_idx = rule.keep_prefix.min(match_cells.len());
                    let end_idx = match_cells
                        .len()
                        .saturating_sub(rule.keep_suffix)
                        .max(start_idx);
                    let redact_cells = &match_cells[start_idx..end_idx];
                    if redact_cells.is_empty() {
                        continue;
                    }

                    map.apply_run(
                        redact_cells,
                        rule.label.as_deref(),
                        rule.block_color,
                        rule.label_color,
                    );
                    *counts.entry(rule.name.clone()).or_insert(0) += 1;
                }
            }
        }

        // Stable audit ordering: most frequent first, then alphabetical.
        let mut combined: HashMap<String, usize> = map.counts.iter().cloned().collect();
        for (name, count) in counts {
            *combined.entry(name).or_insert(0) += count;
        }
        let mut audit: Vec<(String, usize)> = combined.into_iter().collect();
        audit.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        map.counts = audit;
    }
}

/// Split a screen into logical lines, joining soft-wrapped physical rows.
///
/// Returns, for each logical line, its text and a parallel vector mapping every
/// *byte* of that text back to the `(row, column)` cell it came from. Wide
/// characters and empty cells both contribute exactly one cell.
fn logical_lines(screen: &Screen) -> Vec<(String, Vec<(u16, u16)>)> {
    let (rows, cols) = screen.size();
    let mut lines = Vec::new();
    let mut row = 0u16;
    while row < rows {
        let mut line = String::new();
        let mut positions: Vec<(u16, u16)> = Vec::new();
        let mut last = row;
        loop {
            append_row(screen, last, cols, &mut line, &mut positions);
            // vt100 marks a row as "wrapped" when its content continues on the
            // next row; keep appending until the logical line ends.
            if last + 1 < rows && screen.row_wrapped(last) {
                last += 1;
            } else {
                break;
            }
        }
        lines.push((line, positions));
        row = last + 1;
    }
    lines
}

/// Append one physical row's text (and its byte -> cell mapping) to `line`.
fn append_row(
    screen: &Screen,
    row: u16,
    cols: u16,
    line: &mut String,
    positions: &mut Vec<(u16, u16)>,
) {
    for col in 0..cols {
        let contents = screen
            .cell(row, col)
            .map(|c| c.contents())
            .unwrap_or_default();
        if contents.is_empty() {
            line.push(' ');
            positions.push((row, col));
        } else {
            for ch in contents.chars() {
                for _ in 0..ch.len_utf8() {
                    positions.push((row, col));
                }
                line.push(ch);
            }
        }
    }
}

/// A single redacted cell. `label_char` carries the glyph to draw over the red
/// block (e.g. the `[`, `I`, `P`, `]` of an `[IP]` tag); `None` means a plain
/// block. `block_color` / `label_color` are the RGB colors the renderer paints.
#[derive(Debug, Clone, Copy)]
pub struct RedactedCell {
    pub label_char: Option<char>,
    pub block_color: [u8; 3],
    pub label_color: [u8; 3],
}

/// The result of a redaction pass: which cells to mask and the per-rule audit
/// counts (never the redacted values themselves).
#[derive(Debug, Default)]
pub struct RedactionMap {
    cells: HashMap<(u16, u16), RedactedCell>,
    /// (rule name, match count), sorted most-frequent first.
    pub counts: Vec<(String, usize)>,
}

impl RedactionMap {
    /// Mask a run of cells, laying a `[LABEL]` tag over the start of the run
    /// when a label is given and it fits. Passing `None` (or a label that does
    /// not fit) draws plain blocks. Cells already redacted by an earlier
    /// (overlapping) match are left untouched. The run may span rows when the
    /// matched text crossed a soft line wrap.
    fn apply_run(
        &mut self,
        cells: &[(u16, u16)],
        label: Option<&str>,
        block_color: [u8; 3],
        label_color: [u8; 3],
    ) {
        let tag_chars: Vec<char> = match label {
            Some(l) if !l.is_empty() => format!("[{}]", l).chars().collect(),
            _ => Vec::new(),
        };
        let use_label = !tag_chars.is_empty() && cells.len() >= tag_chars.len();

        for (i, &(row, col)) in cells.iter().enumerate() {
            let label_char = if use_label && i < tag_chars.len() {
                Some(tag_chars[i])
            } else {
                None
            };
            self.cells.entry((row, col)).or_insert(RedactedCell {
                label_char,
                block_color,
                label_color,
            });
        }
    }

    /// Add a manual (coordinate-based) redaction spanning `[col_start, col_end)`
    /// on `row`, using the default colors. `label` is drawn over the block when
    /// `Some` (and it fits); `None` draws a plain block. Counted in the audit
    /// under the label when present, otherwise under `manual`.
    pub fn add_manual(&mut self, row: u16, col_start: u16, col_end: u16, label: Option<&str>) {
        if col_end <= col_start {
            return;
        }
        let cells: Vec<(u16, u16)> = (col_start..col_end).map(|col| (row, col)).collect();
        self.apply_run(&cells, label, DEFAULT_BLOCK_COLOR, DEFAULT_LABEL_COLOR);
        let name = match label {
            Some(l) if !l.is_empty() => format!("manual:{}", l),
            _ => "manual".to_string(),
        };
        if let Some(entry) = self.counts.iter_mut().find(|(n, _)| n == &name) {
            entry.1 += 1;
        } else {
            self.counts.push((name, 1));
        }
    }

    /// Look up a cell's redaction, if any.
    pub fn get(&self, row: u16, col: u16) -> Option<&RedactedCell> {
        self.cells.get(&(row, col))
    }

    /// True when nothing was redacted.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Total number of redacted cells.
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// A human-readable audit summary such as `3x ipv4, 1x aws_key`.
    pub fn audit_summary(&self) -> String {
        self.counts
            .iter()
            .map(|(name, count)| format!("{}x {}", count, name))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Produce a redaction-safe plain-text rendering of the screen, replacing
    /// masked cells with the full-block glyph so returned text never leaks the
    /// original values while preserving column alignment.
    pub fn redacted_plain_text(&self, screen: &Screen) -> String {
        let (rows, cols) = screen.size();
        let mut out_rows: Vec<String> = Vec::with_capacity(rows as usize);
        for row in 0..rows {
            let mut line = String::new();
            for col in 0..cols {
                if self.cells.contains_key(&(row, col)) {
                    line.push('\u{2588}'); // █
                } else {
                    let contents = screen
                        .cell(row, col)
                        .map(|c| c.contents())
                        .unwrap_or_default();
                    if contents.is_empty() {
                        line.push(' ');
                    } else {
                        line.push_str(contents);
                    }
                }
            }
            out_rows.push(line.trim_end().to_string());
        }
        out_rows.join("\n").trim_end().to_string()
    }
}

/// Short visual label for a rule, used inside the redaction block.
/// Derive the on-image label for a rule, or `None` when the rule should render
/// as a plain solid block with no text overlay.
///
/// Built-in rules map to a short fixed tag (`ipv4` -> `IP`, ...). For any other
/// rule the label is derived from its `replacement` text (e.g.
/// `[REDACTED-TICKET]` -> `TICKET`), falling back to the rule name when the
/// replacement carries no usable text. An *empty* replacement means the caller
/// explicitly wants no label (used by ad-hoc `redact_screenshot` patterns that
/// omit `replacement`), so it returns `None`.
fn label_for(name: &str, replacement: &str) -> Option<String> {
    // An empty replacement is an explicit "no label" request.
    if replacement.is_empty() {
        return None;
    }

    let builtin = match name {
        "ipv4" | "ipv6" => Some("IP"),
        "mac" => Some("MAC"),
        "aws_key" | "aws_secret" | "api_key" => Some("KEY"),
        "private_key" => Some("KEY"),
        "jwt" => Some("JWT"),
        "email" => Some("EMAIL"),
        "hostname" => Some("HOST"),
        _ => None,
    };
    if let Some(l) = builtin {
        return Some(l.to_string());
    }

    // Derive a compact label from the replacement text, e.g.
    // "[REDACTED-IP]" -> "IP", falling back to the rule name.
    let cleaned: String = replacement
        .trim_matches(|c| c == '[' || c == ']')
        .replace("REDACTED", "")
        .trim_matches(|c: char| c == '-' || c == '_' || c.is_whitespace())
        .to_uppercase();
    let base = if cleaned.is_empty() {
        name.to_uppercase()
    } else {
        cleaned
    };
    Some(base.chars().take(6).collect())
}

/// Optional per-match skip predicate for a builtin rule.
fn builtin_skip(name: &str) -> Option<fn(&str) -> bool> {
    match name {
        "ipv4" => Some(is_ignorable_ipv4),
        "ipv6" => Some(is_ignorable_ipv6),
        _ => None,
    }
}

/// Keep obviously non-sensitive IPv4 addresses visible: `0.0.0.0`, the
/// loopback range `127.0.0.0/8`, and the broadcast address.
fn is_ignorable_ipv4(s: &str) -> bool {
    let octets: Vec<&str> = s.split('.').collect();
    if octets.len() != 4 {
        return false;
    }
    match s {
        "0.0.0.0" | "255.255.255.255" => return true,
        _ => {}
    }
    octets[0] == "127"
}

/// Reject IPv6 look-alikes so ordinary source code is never masked.
///
/// A candidate is ignored unless it parses as a real address, and even then
/// short compressed forms such as `d::f` (which `std::fs::read` contains) are
/// dropped: a genuine address carries at least four hex digits and either a
/// group of three or more digits or four or more groups. Loopback (`::1`) and
/// the unspecified address (`::`) are always kept visible, mirroring the IPv4
/// rule.
fn is_ignorable_ipv6(s: &str) -> bool {
    let addr: std::net::Ipv6Addr = match s.parse() {
        Ok(a) => a,
        Err(_) => return true,
    };
    if addr.is_loopback() || addr.is_unspecified() {
        return true;
    }
    let hex_digits = s.chars().filter(|c| c.is_ascii_hexdigit()).count();
    let groups: Vec<&str> = s.split(':').filter(|g| !g.is_empty()).collect();
    let longest = groups.iter().map(|g| g.len()).max().unwrap_or(0);
    hex_digits < 4 || (longest < 3 && groups.len() < 4)
}

/// The compiled-in default rule set. Users can override any of these by name in
/// their config (including disabling one via `enabled = false`).
///
// Built-in redaction rules. Patterns sourced from and inspired by:
// - Betterleaks (MIT) - https://github.com/betterleaks/betterleaks
// Additional patterns are original to this project.
fn builtin_rules() -> Vec<RedactionRuleConfig> {
    let r = |name: &str, pattern: &str, replacement: &str| {
        RedactionRuleConfig::new(name, pattern, replacement)
    };

    vec![
        // Context-guarded IPv4: the address must not be embedded in a longer
        // dotted/alphanumeric token, and a trailing `-<letter>` (as in
        // `1.2.3.4-beta`) marks a version string, not an address. Only the
        // `redact` group is masked; the surrounding context is left alone.
        r(
            "ipv4",
            r"(?:^|[^0-9A-Za-z.])(?P<redact>(?:(?:25[0-5]|2[0-4]\d|1?\d\d?)\.){3}(?:25[0-5]|2[0-4]\d|1?\d\d?))(?:$|[^0-9A-Za-z.\-]|-(?:[^A-Za-z]|$))",
            "[REDACTED-IP]",
        ),
        // IPv6 is deliberately conservative: the loose alternation this rule
        // used to carry matched `::` inside ordinary code (`std::fs::read`,
        // `Error::new`). Matches must now be bounded by non-alphanumeric
        // context and additionally survive `is_ignorable_ipv6`, which parses
        // them as real addresses and rejects code-like look-alikes.
        r(
            "ipv6",
            r"(?i)(?:^|[^0-9A-Za-z:.])(?P<redact>::ffff:(?:\d{1,3}\.){3}\d{1,3}|(?:[0-9a-f]{1,4}:){2,7}[0-9a-f]{1,4}|(?:[0-9a-f]{1,4}:){1,7}:(?:[0-9a-f]{1,4}(?::[0-9a-f]{1,4}){0,6})?|::(?:[0-9a-f]{1,4}(?::[0-9a-f]{1,4}){0,7})?)(?:$|[^0-9A-Za-z:])",
            "[REDACTED-IPv6]",
        ),
        r(
            "mac",
            r"\b(?:[0-9A-Fa-f]{2}[:-]){5}[0-9A-Fa-f]{2}\b",
            "[REDACTED-MAC]",
        ),
        r(
            "aws_key",
            r"\b(?:AKIA|ASIA|AGPA|AIDA|AROA|ANPA)[0-9A-Z]{16}\b",
            "[REDACTED-KEY]",
        ),
        r(
            "aws_secret",
            r#"(?i)(?:aws_secret_access_key|aws_secret)\s*[:=]\s*['"]?(?P<redact>[A-Za-z0-9/+=]{40})['"]?"#,
            "[REDACTED-KEY]",
        ),
        // PEM armor only. This rule used to carry an `[A-Za-z0-9+/]{60,}`
        // catch-all that masked any long alphanumeric run - hashes, git object
        // IDs, base64 fixtures, minified output - which made normal developer
        // screenshots unreadable.
        r(
            "private_key",
            r"-----(?:BEGIN|END)[A-Z0-9 ]*PRIVATE KEY-----",
            "[REDACTED-KEY]",
        ),
        r(
            "jwt",
            r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b",
            "[REDACTED-JWT]",
        ),
        r(
            "email",
            r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b",
            "[REDACTED-EMAIL]",
        ),
        r(
            "hostname",
            r"\b(?:[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?\.)+(?:internal|local|corp|lan|intranet)\b",
            "[REDACTED-HOST]",
        ),
        // A `:` or `=` is required between the key name and the value: without
        // it, prose such as "rotate the api key regularly afterwards" matched.
        r(
            "api_key",
            r#"(?i)(?:api[_-]?key|apikey|access[_-]?token|secret[_-]?key|auth[_-]?token|bearer)\s*[:=]\s*['"]?(?P<redact>[A-Za-z0-9_\-\.]{16,})"#,
            "[REDACTED-KEY]",
        ),
        // --- Additional provider tokens (sourced from / inspired by Betterleaks, MIT) ---
        r(
            "github_token",
            r"\b(?:gh[opsu]_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{82})\b",
            "[REDACTED-TOKEN]",
        ),
        r(
            "slack_token",
            r"\bxox[baprs]-[0-9]{10,13}-[0-9]{10,13}-[A-Za-z0-9]{24,34}\b",
            "[REDACTED-TOKEN]",
        ),
        r(
            "private_key_pem",
            r"-----BEGIN (?:RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----",
            "[REDACTED-KEY]",
        ),
        r(
            "gcp_service_account",
            r#"(?i)"type"\s*:\s*"service_account""#,
            "[REDACTED-KEY]",
        ),
        r(
            "azure_client_secret",
            r"\b[A-Za-z0-9_~.]{3}[0-9]Q~[A-Za-z0-9_~.\-]{31,34}\b",
            "[REDACTED-SECRET]",
        ),
        r(
            "generic_api_key",
            r#"(?i)(?:api[_-]?key|apikey)\s*[:=]\s*['"]?(?P<redact>[A-Za-z0-9_\-]{20,})"#,
            "[REDACTED-KEY]",
        ),
        r(
            "bearer_token",
            r#"(?i)(?:bearer|authorization)\s*[:=]\s*['"]?(?P<redact>[A-Za-z0-9_\-.]{20,})"#,
            "[REDACTED-TOKEN]",
        ),
        r(
            "connection_string",
            r#"(?i)(?:connection.?string|conn.?str)\s*[:=]\s*['"]?(?P<redact>[^'";\s]{20,})"#,
            "[REDACTED-SECRET]",
        ),
        r(
            "discord_token",
            r"\b[MN][A-Za-z0-9]{23,}\.[A-Za-z0-9_-]{6}\.[A-Za-z0-9_-]{27,}",
            "[REDACTED-TOKEN]",
        ),
        r(
            "hashicorp_vault_token",
            r"\bhvs\.[A-Za-z0-9]{24,}",
            "[REDACTED-TOKEN]",
        ),
    ]
}

/// Parse a `#RRGGBB` (or `RRGGBB`) hex string into an RGB triple.
pub fn parse_hex_rgb(hex: &str) -> Option<[u8; 3]> {
    let hex = hex.trim().trim_start_matches('#');
    // Check for ASCII hex digits before slicing: a six-*byte* string that
    // contains a multi-byte character would otherwise panic on a non-char
    // boundary.
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some([r, g, b])
}

/// Shannon entropy of a string in bits per character.
fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts: HashMap<char, usize> = HashMap::new();
    let mut total = 0usize;
    for ch in s.chars() {
        *counts.entry(ch).or_insert(0) += 1;
        total += 1;
    }
    let total = total as f64;
    -counts
        .values()
        .map(|&c| {
            let p = c as f64 / total;
            p * p.log2()
        })
        .sum::<f64>()
}

/// A single rule as declared in a YAML rule file. Only the
/// fields we map are captured; unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
struct YamlRule {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    pattern: Option<String>,
    #[serde(default)]
    regex: Option<String>,
    #[serde(default)]
    min_entropy: Option<f64>,
    #[serde(default)]
    confidence: Option<String>,
}

/// Top-level YAML rule document (`rules:` list).
#[derive(Debug, Clone, Deserialize)]
struct YamlRulesFile {
    #[serde(default)]
    rules: Vec<YamlRule>,
}

/// A `.toml` rule file: either a bare list of `[[rules]]` or a single rule.
#[derive(Debug, Clone, Deserialize)]
struct TomlRulesFile {
    #[serde(default)]
    rules: Vec<RedactionRuleConfig>,
}

impl YamlRule {
    /// Convert a YAML rule into our internal rule format. Returns `None`
    /// when the rule has no usable name/id or pattern.
    fn into_rule(self) -> Option<RedactionRuleConfig> {
        let name = self
            .name
            .clone()
            .or_else(|| self.id.clone())
            .filter(|n| !n.is_empty())?;
        let pattern = self.pattern.or(self.regex).filter(|p| !p.is_empty())?;
        // Map confidence to enabled: only an explicit "false"/"disabled"
        // confidence disables the rule; everything else stays enabled.
        let enabled = !matches!(
            self.confidence
                .as_deref()
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("false") | Some("disabled") | Some("off")
        );
        Some(RedactionRuleConfig {
            name,
            pattern,
            replacement: default_replacement(),
            enabled,
            color: None,
            min_entropy: self.min_entropy,
            keep_prefix: None,
            keep_suffix: None,
        })
    }
}

/// Load extra redaction rules from a directory of `.toml` and `.yaml`/`.yml`
/// files. TOML files use our native rule format (`[[rules]]`); YAML files use
/// the generic YAML rule format (`rules:` list). Malformed files are logged and
/// skipped rather than failing the whole load.
pub fn load_rules_from_dir(dir: &std::path::Path) -> Vec<RedactionRuleConfig> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("Failed to read rules dir {:?}: {}", dir, e);
            return out;
        }
    };
    let mut paths: Vec<std::path::PathBuf> =
        entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to read rules file {:?}: {}", path, e);
                continue;
            }
        };
        match ext.as_deref() {
            Some("toml") => match toml::from_str::<TomlRulesFile>(&contents) {
                Ok(f) => out.extend(f.rules),
                Err(e) => tracing::warn!("Failed to parse TOML rules {:?}: {}", path, e),
            },
            Some("yaml") | Some("yml") => match serde_yaml::from_str::<YamlRulesFile>(&contents) {
                Ok(f) => out.extend(f.rules.into_iter().filter_map(YamlRule::into_rule)),
                Err(e) => tracing::warn!("Failed to parse YAML rules {:?}: {}", path, e),
            },
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> RedactionEngine {
        RedactionEngine::from_config(&RedactionConfig::default()).unwrap()
    }

    /// Parse text into a screen so rules can be exercised end to end.
    fn screen_of(text: &str, cols: u16, rows: u16) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(text.as_bytes());
        parser
    }

    fn redact(text: &str) -> RedactionMap {
        let parser = screen_of(text, 120, 10);
        engine().redact_screen(parser.screen(), None)
    }

    fn count_of(map: &RedactionMap, name: &str) -> usize {
        map.counts
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, c)| *c)
            .unwrap_or(0)
    }

    #[test]
    fn ipv4_matches_real_addresses() {
        let map = redact("host is 192.168.1.42 online");
        assert_eq!(count_of(&map, "ipv4"), 1);
    }

    #[test]
    fn ipv4_ignores_loopback_and_unspecified() {
        assert_eq!(count_of(&redact("localhost 127.0.0.1"), "ipv4"), 0);
        assert_eq!(count_of(&redact("bind 0.0.0.0:8080"), "ipv4"), 0);
        assert_eq!(count_of(&redact("bcast 255.255.255.255"), "ipv4"), 0);
    }

    #[test]
    fn ipv4_rejects_out_of_range_octets() {
        assert_eq!(count_of(&redact("version 999.999.1.1 here"), "ipv4"), 0);
    }

    #[test]
    fn ipv6_matches() {
        let map = redact("addr 2001:0db8:85a3:0000:0000:8a2e:0370:7334 up");
        assert!(count_of(&map, "ipv6") >= 1);
    }

    #[test]
    fn mac_matches_both_separators() {
        assert_eq!(count_of(&redact("mac 00:1a:2b:3c:4d:5e"), "mac"), 1);
        assert_eq!(count_of(&redact("mac 00-1A-2B-3C-4D-5E"), "mac"), 1);
    }

    #[test]
    fn aws_key_matches() {
        let map = redact("key AKIAIOSFODNN7EXAMPLE end");
        assert_eq!(count_of(&map, "aws_key"), 1);
    }

    #[test]
    fn aws_secret_matches_in_context() {
        let input = "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let parser = screen_of(input, 120, 3);
        let map = engine().redact_screen(parser.screen(), None);
        assert_eq!(count_of(&map, "aws_secret"), 1);
        let text = map.redacted_plain_text(parser.screen());
        assert!(text.contains("AWS_SECRET_ACCESS_KEY="));
        assert!(!text.contains("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"));
    }

    #[test]
    fn contextual_rules_preserve_key_names() {
        let input = concat!(
            "API_KEY=abcdefghijklmnopqrstuvwx\n",
            "Authorization=abcdefghijklmnopqrstuvwxyz012345\n",
            "CONNECTION_STRING=postgresql://user:secret@db.example/app"
        );
        let parser = screen_of(input, 120, 5);
        let map = engine().redact_screen(parser.screen(), None);
        let text = map.redacted_plain_text(parser.screen());
        assert!(text.contains("API_KEY="));
        assert!(text.contains("Authorization="));
        assert!(text.contains("CONNECTION_STRING="));
        assert!(!text.contains("abcdefghijklmnopqrstuvwx"));
        assert!(!text.contains("abcdefghijklmnopqrstuvwxyz012345"));
        assert!(!text.contains("postgresql://user:secret@db.example/app"));
    }

    #[test]
    fn jwt_matches() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N";
        let map = redact(&format!("token {}", jwt));
        assert_eq!(count_of(&map, "jwt"), 1);
    }

    #[test]
    fn private_key_marker_matches() {
        let map = redact("-----BEGIN RSA PRIVATE KEY-----");
        assert_eq!(count_of(&map, "private_key"), 1);
    }

    #[test]
    fn email_matches() {
        let map = redact("contact admin@example.com now");
        assert_eq!(count_of(&map, "email"), 1);
    }

    #[test]
    fn hostname_matches_internal_domains() {
        assert_eq!(count_of(&redact("ssh db01.internal"), "hostname"), 1);
        assert_eq!(count_of(&redact("ping fileserver.corp"), "hostname"), 1);
    }

    #[test]
    fn api_key_matches() {
        let map = redact("api_key=abcdef0123456789ABCDEF");
        assert_eq!(count_of(&map, "api_key"), 1);
    }

    #[test]
    fn plain_prose_is_not_redacted() {
        let map = redact("the quick brown fox jumps over the lazy dog");
        assert!(map.is_empty(), "unexpected redactions: {:?}", map.counts);
    }

    /// Realistic non-secret developer output must survive untouched: a false
    /// positive silently corrupts the screenshot people came for. Every line
    /// here was mangled by an earlier version of the rules.
    #[test]
    fn ordinary_developer_output_is_not_redacted() {
        let corpus = [
            "use std::fs::read;",
            "let path = std::path::PathBuf::from(\"/tmp/x\");",
            "std::cout << x << std::endl;",
            "Error::new(ErrorKind::Other, \"boom\")",
            "impl fmt::Display for Config {",
            "Foo::bar::baz(&mut self)",
            "termshot v1.2.3.4-beta released",
            "running 66 tests in 0.41s",
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "-rw-rw-r-- 1 adam adam 56917 Aug 25 13:07 Cargo.lock",
            "commit 25eabf1a9c4f1e0b7d5c3a2f8e6d4b0c9a7f5e31",
            "aGVsbG8gd29ybGQgdGhpcyBpcyBhIGxvbmcgYmFzZTY0IHN0cmluZyBmb3IgdGVzdGluZw==",
            "12:34:56 INFO server started",
            "Compiling termshot v0.1.0 (/home/adam/Desktop/termshot)",
            "rotate the api key regularly and store it somewhere safe",
        ];
        for line in corpus {
            let map = redact(line);
            assert!(
                map.is_empty(),
                "false positive on {:?}: {:?}",
                line,
                map.counts
            );
        }
    }

    /// The tightened IPv6 rule must still catch real addresses.
    #[test]
    fn ipv6_addresses_are_still_redacted() {
        for addr in [
            "inet6 fe80::1cb2:ff:fe00:1/64",
            "connecting to [2001:db8::1]:443",
            "addr 2001:0db8:85a3:0000:0000:8a2e:0370:7334 up",
            "peer fd00:1234:5678::abcd",
        ] {
            let map = redact(addr);
            assert!(count_of(&map, "ipv6") >= 1, "missed IPv6 in {:?}", addr);
        }
    }

    #[test]
    fn ipv6_ignores_loopback_and_unspecified() {
        assert_eq!(count_of(&redact("listening on ::1 port 80"), "ipv6"), 0);
        assert_eq!(count_of(&redact("bind :: port 80"), "ipv6"), 0);
    }

    /// A secret split across a soft line wrap must still be masked: it is
    /// exactly the long tokens (keys, JWTs) that wrap.
    #[test]
    fn wrapped_secret_is_redacted_across_rows() {
        // 24 columns forces "AKIAIOSFODNN7EXAMPLE" to straddle two rows.
        let parser = screen_of("token: AKIAIOSFODNN7EXAMPLE done", 24, 6);
        let map = engine().redact_screen(parser.screen(), None);
        assert_eq!(count_of(&map, "aws_key"), 1, "wrapped key missed");

        let rows: Vec<u16> = (0..6)
            .filter(|&row| (0..24).any(|col| map.get(row, col).is_some()))
            .collect();
        assert!(
            rows.len() >= 2,
            "expected redaction on both wrapped rows, got {:?}",
            rows
        );
        assert!(!map.redacted_plain_text(parser.screen()).contains("AKIA"));
    }

    /// Only *soft* wraps are joined. Two hard-separated lines whose text would
    /// form a match when concatenated must stay unmatched: joining them would
    /// scatter phantom redactions across ordinary output.
    #[test]
    fn hard_newlines_are_not_joined_into_a_match() {
        let rule = RedactionRuleConfig::new("hash", "[a-f0-9]{32}", "HASH");
        let engine = RedactionEngine::from_rules(&[rule]).unwrap();
        // Each half fits well inside the row, so neither line wrapped.
        let parser = screen_of("8846f7eaee8fb117ad06\r\nbdd830b7586c\r\n", 60, 6);
        let map = engine.redact_screen(parser.screen(), None);
        assert!(
            map.is_empty(),
            "hard-separated lines were joined into a match: {}",
            map.audit_summary()
        );
    }

    /// The same two halves *are* one value when the terminal itself wrapped
    /// them, which is the case the join exists for.
    #[test]
    fn soft_wrapped_halves_are_joined_into_a_match() {
        let rule = RedactionRuleConfig::new("hash", "[a-f0-9]{32}", "HASH");
        let engine = RedactionEngine::from_rules(&[rule]).unwrap();
        // 20 columns splits the 32-character hash across two rows.
        let parser = screen_of("8846f7eaee8fb117ad06bdd830b7586c\r\n", 20, 6);
        let map = engine.redact_screen(parser.screen(), None);
        assert_eq!(count_of(&map, "hash"), 1, "wrapped hash missed");
        assert!(map.get(0, 19).is_some(), "first row not masked");
        assert!(map.get(1, 0).is_some(), "second row not masked");
    }

    /// A `redact` capture group lets a rule require context without masking
    /// it: only the group's span is blocked out.
    #[test]
    fn only_the_redact_group_is_masked() {
        // "ip 10.0.0.9" -> the leading space is context, not part of the match.
        let parser = screen_of("ip 10.0.0.9!", 120, 3);
        let map = engine().redact_screen(parser.screen(), None);
        assert!(map.get(0, 2).is_none(), "context space was masked");
        assert!(map.get(0, 3).is_some(), "address start not masked");
        assert!(map.get(0, 10).is_some(), "address end not masked");
        assert!(map.get(0, 11).is_none(), "trailing context was masked");
    }

    #[test]
    fn parse_hex_rgb_rejects_non_ascii() {
        // Six *bytes* but not six chars: must not panic on a slice boundary.
        assert_eq!(parse_hex_rgb("#abc\u{20ac}"), None);
        assert_eq!(parse_hex_rgb("#d41919"), Some([212, 25, 25]));
    }

    #[test]
    fn map_marks_correct_columns() {
        // "ip 10.0.0.9" -> address starts at column 3.
        let parser = screen_of("ip 10.0.0.9", 120, 3);
        let map = engine().redact_screen(parser.screen(), None);
        assert!(map.get(0, 2).is_none()); // the space before
        for col in 3..11 {
            assert!(
                map.get(0, col).is_some(),
                "column {} should be redacted",
                col
            );
        }
    }

    #[test]
    fn keep_prefix_and_suffix_leave_edges_unmasked() {
        // A 32-char hash starting at column 3; keep first 4 visible.
        let mut rule = RedactionRuleConfig::new("hash", "[a-f0-9]{32}", "HASH");
        rule.keep_prefix = Some(4);
        let engine = RedactionEngine::from_rules(&[rule]).unwrap();
        let hash = "8846f7eaee8fb117ad06bdd830b7586c";
        let parser = screen_of(&format!("h: {}", hash), 120, 3);
        let map = engine.redact_screen(parser.screen(), None);
        // Columns 3..7 (the first 4 hash chars) stay visible.
        for col in 3..7 {
            assert!(map.get(0, col).is_none(), "prefix col {} redacted", col);
        }
        // The remaining 28 columns (7..35) are masked.
        for col in 7..35 {
            assert!(map.get(0, col).is_some(), "col {} not redacted", col);
        }
    }

    #[test]
    fn keep_prefix_covering_whole_match_redacts_nothing() {
        let mut rule = RedactionRuleConfig::new("word", "[a-z]{4}", "W");
        rule.keep_prefix = Some(4);
        let engine = RedactionEngine::from_rules(&[rule]).unwrap();
        let parser = screen_of("abcd", 120, 3);
        let map = engine.redact_screen(parser.screen(), None);
        assert!(map.is_empty(), "nothing should be redacted");
    }

    #[test]
    fn keep_suffix_leaves_trailing_chars_unmasked() {
        let mut rule = RedactionRuleConfig::new("digits", "[0-9]{6}", "N");
        rule.keep_suffix = Some(2);
        let engine = RedactionEngine::from_rules(&[rule]).unwrap();
        let parser = screen_of("123456", 120, 3);
        let map = engine.redact_screen(parser.screen(), None);
        // First 4 masked, last 2 visible.
        for col in 0..4 {
            assert!(map.get(0, col).is_some(), "col {} not redacted", col);
        }
        for col in 4..6 {
            assert!(map.get(0, col).is_none(), "suffix col {} redacted", col);
        }
    }

    #[test]
    fn label_is_placed_over_block() {
        let parser = screen_of("ip 10.11.12.13 up", 120, 3);
        let map = engine().redact_screen(parser.screen(), None);
        // "10.11.12.13" spans 11 columns starting at 3, so "[IP]" fits.
        let tag: String = (3..7)
            .filter_map(|c| map.get(0, c).and_then(|rc| rc.label_char))
            .collect();
        assert_eq!(tag, "[IP]");
    }

    #[test]
    fn only_filter_limits_rules() {
        let parser = screen_of("192.168.0.1 admin@example.com", 120, 3);
        let only = vec!["ipv4".to_string()];
        let map = engine().redact_screen(parser.screen(), Some(&only));
        assert_eq!(count_of(&map, "ipv4"), 1);
        assert_eq!(count_of(&map, "email"), 0);
    }

    #[test]
    fn validate_rule_names_rejects_unknown() {
        let engine = engine();
        assert!(engine.validate_rule_names(&["ipv4".to_string()]).is_ok());
        let err = engine
            .validate_rule_names(&["ipv4".to_string(), "not_a_rule".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("not_a_rule"), "error was: {}", err);
    }

    #[test]
    fn redacted_plain_text_hides_value() {
        let parser = screen_of("secret 203.0.113.7 host", 120, 3);
        let map = engine().redact_screen(parser.screen(), None);
        let text = map.redacted_plain_text(parser.screen());
        assert!(!text.contains("203.0.113.7"), "text still leaks: {}", text);
        assert!(text.contains('\u{2588}'));
    }

    #[test]
    fn resolve_should_redact_logic() {
        let mut cfg = RedactionConfig {
            enabled: false,
            auto: false,
            ..Default::default()
        };
        // `enabled = false` is the master switch: nothing runs, and an
        // explicit request is reported as blocked instead of silently ignored.
        assert!(!resolve_should_redact(&cfg, false, false));
        assert!(!resolve_should_redact(&cfg, true, false));
        assert!(explicit_request_is_blocked(&cfg, true, false));
        assert!(!explicit_request_is_blocked(&cfg, false, false));
        // An explicit "no redact" is not a blocked request, just a no-op.
        assert!(!resolve_should_redact(&cfg, true, true));
        assert!(!explicit_request_is_blocked(&cfg, true, true));

        cfg.enabled = true;
        cfg.auto = true;
        assert!(resolve_should_redact(&cfg, false, false));
        assert!(!resolve_should_redact(&cfg, false, true)); // no_redact wins
        assert!(!explicit_request_is_blocked(&cfg, true, false));

        // The default config does NOT auto-redact: rules are available but
        // only run on an explicit request, so a false positive can never
        // silently mangle an ordinary capture.
        let default = RedactionConfig::default();
        assert!(default.enabled);
        assert!(!default.auto);
        assert!(!resolve_should_redact(&default, false, false));
        assert!(resolve_should_redact(&default, true, false));
    }

    #[test]
    fn all_builtin_patterns_are_valid_regexes() {
        // Every compiled-in pattern must be a valid Rust `regex` crate regex.
        for rule in builtin_rules() {
            assert!(
                Regex::new(&rule.pattern).is_ok(),
                "invalid regex for builtin '{}': {}",
                rule.name,
                rule.pattern
            );
        }

        // The full default engine (builtins + labels) must build cleanly and
        // expose every newly added Betterleaks-sourced rule by name.
        let engine = RedactionEngine::from_config(&RedactionConfig::default()).unwrap();
        let names = engine.rule_names();
        for expected in [
            "github_token",
            "slack_token",
            "private_key_pem",
            "gcp_service_account",
            "azure_client_secret",
            "generic_api_key",
            "bearer_token",
            "connection_string",
            "discord_token",
            "hashicorp_vault_token",
        ] {
            assert!(
                names.contains(&expected),
                "missing builtin rule '{}'",
                expected
            );
        }
    }

    #[test]
    fn new_builtin_rules_match_sample_secrets() {
        let engine = RedactionEngine::from_config(&RedactionConfig::default()).unwrap();
        // Each sample should trigger at least one redaction.
        let samples = [
            "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
            "xoxb-1234567890-1234567890-abcdefghijKLMNOPqrstuvwx",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "token = hvs.ABCDEFGHIJKLMNOPQRSTUVWX01",
        ];
        for s in samples {
            let parser = screen_of(s, 120, 3);
            let map = engine.redact_screen(parser.screen(), None);
            assert!(!map.is_empty(), "expected a redaction for sample: {}", s);
        }
    }

    #[test]
    fn user_rule_can_disable_builtin() {
        let cfg = RedactionConfig {
            enabled: true,
            auto: false,
            rules: vec![RedactionRuleConfig {
                name: "email".to_string(),
                pattern: String::new(),
                replacement: default_replacement(),
                enabled: false,
                color: None,
                min_entropy: None,
                keep_prefix: None,
                keep_suffix: None,
            }],
            ..Default::default()
        };
        let engine = RedactionEngine::from_config(&cfg).unwrap();
        let parser = screen_of("admin@example.com", 120, 3);
        let map = engine.redact_screen(parser.screen(), None);
        assert_eq!(count_of(&map, "email"), 0);
    }

    #[test]
    fn overriding_builtin_keeps_partial_redaction_fields() {
        // Overriding a builtin (aws_key) with keep_prefix must preserve the
        // partial-redaction request, leaving the "AKIA" prefix visible.
        let cfg = RedactionConfig {
            enabled: true,
            auto: false,
            rules: vec![RedactionRuleConfig {
                name: "aws_key".to_string(),
                pattern: String::new(),
                replacement: default_replacement(),
                enabled: true,
                color: None,
                min_entropy: None,
                keep_prefix: Some(4),
                keep_suffix: None,
            }],
            ..Default::default()
        };
        let engine = RedactionEngine::from_config(&cfg).unwrap();
        let parser = screen_of("AKIAIOSFODNN7EXAMPLE", 120, 3);
        let map = engine.redact_screen(parser.screen(), None);
        // First 4 columns ("AKIA") stay visible; the rest are masked.
        for col in 0..4 {
            assert!(map.get(0, col).is_none(), "prefix col {} redacted", col);
        }
        for col in 4..20 {
            assert!(map.get(0, col).is_some(), "col {} not redacted", col);
        }
    }

    #[test]
    fn custom_user_rule_is_applied() {
        let cfg = RedactionConfig {
            enabled: true,
            auto: false,
            rules: vec![RedactionRuleConfig {
                name: "ticket".to_string(),
                pattern: r"TICKET-\d+".to_string(),
                replacement: "[REDACTED-TICKET]".to_string(),
                enabled: true,
                color: None,
                min_entropy: None,
                keep_prefix: None,
                keep_suffix: None,
            }],
            ..Default::default()
        };
        let engine = RedactionEngine::from_config(&cfg).unwrap();
        let parser = screen_of("see TICKET-1234", 120, 3);
        let map = engine.redact_screen(parser.screen(), None);
        assert_eq!(count_of(&map, "ticket"), 1);
    }

    #[test]
    fn per_rule_color_is_applied() {
        let mut rule = RedactionRuleConfig::new("ip", r"\b10\.0\.0\.\d+\b", "[REDACTED-IP]");
        rule.color = Some("#ff6600".to_string());
        let cfg = RedactionConfig {
            rules: vec![rule],
            ..Default::default()
        };
        let engine = RedactionEngine::from_config(&cfg).unwrap();
        let parser = screen_of("ip 10.0.0.5 up", 120, 3);
        let map = engine.redact_screen(parser.screen(), Some(&["ip".to_string()]));
        let cell = map.get(0, 3).expect("redacted");
        assert_eq!(cell.block_color, [255, 102, 0]);
    }

    #[test]
    fn engine_wide_color_overrides_default() {
        let cfg = RedactionConfig {
            color: Some("#00ff00".to_string()),
            label_color: Some("#111111".to_string()),
            enabled: true,
            ..Default::default()
        };
        let engine = RedactionEngine::from_config(&cfg).unwrap();
        let parser = screen_of("ip 192.168.1.9 up", 120, 3);
        let map = engine.redact_screen(parser.screen(), None);
        let cell = map.get(0, 3).expect("redacted");
        assert_eq!(cell.block_color, [0, 255, 0]);
        assert_eq!(cell.label_color, [17, 17, 17]);
    }

    #[test]
    fn min_entropy_filters_low_randomness_matches() {
        let mut rule = RedactionRuleConfig::new("token", r"\btok_[A-Za-z0-9]+\b", "[REDACTED]");
        rule.min_entropy = Some(3.0);
        let cfg = RedactionConfig {
            rules: vec![rule],
            ..Default::default()
        };
        let engine = RedactionEngine::from_config(&cfg).unwrap();
        // Low-entropy value is ignored...
        let low = screen_of("tok_aaaaaaaa", 120, 3);
        assert_eq!(
            count_of(&engine.redact_screen(low.screen(), None), "token"),
            0
        );
        // ...but a high-entropy value is redacted.
        let high = screen_of("tok_a8Xk2Qm9Zp", 120, 3);
        assert_eq!(
            count_of(&engine.redact_screen(high.screen(), None), "token"),
            1
        );
    }

    #[test]
    fn manual_coordinate_redaction() {
        let parser = screen_of("secret data here", 120, 3);
        let mut map = RedactionMap::default();
        map.add_manual(0, 0, 6, Some("SECRET"));
        for col in 0..6 {
            assert!(map.get(0, col).is_some(), "column {} redacted", col);
        }
        assert!(map.get(0, 6).is_none());
        let text = map.redacted_plain_text(parser.screen());
        assert!(!text.contains("secret"));
    }

    #[test]
    fn shannon_entropy_bounds() {
        assert_eq!(shannon_entropy(""), 0.0);
        assert_eq!(shannon_entropy("aaaa"), 0.0);
        // Two equally likely symbols -> 1 bit/char.
        assert!((shannon_entropy("abab") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn yaml_rules_load() {
        let dir = std::path::Path::new("target/yaml-test-rules");
        std::fs::create_dir_all(dir).unwrap();
        let yaml = r#"
rules:
  - name: slack_token
    id: kf.slack
    pattern: 'xoxb-[0-9A-Za-z-]+'
    min_entropy: 3.0
    confidence: high
  - id: disabled_rule
    pattern: 'DISABLED-\d+'
    confidence: disabled
  - name: no_pattern
"#;
        std::fs::write(dir.join("kf.yaml"), yaml).unwrap();
        let rules = load_rules_from_dir(dir);
        // The rule with no pattern is dropped; the other two are loaded.
        assert_eq!(rules.len(), 2);
        let slack = rules.iter().find(|r| r.name == "slack_token").unwrap();
        assert_eq!(slack.min_entropy, Some(3.0));
        assert!(slack.enabled);
        let disabled = rules.iter().find(|r| r.name == "disabled_rule").unwrap();
        assert!(!disabled.enabled);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn toml_rules_dir_loads() {
        let dir = std::path::Path::new("target/toml-test-rules");
        std::fs::create_dir_all(dir).unwrap();
        let toml_src = r#"
[[rules]]
name = "ticket"
pattern = 'TICKET-\d+'
replacement = "[REDACTED-TICKET]"
"#;
        std::fs::write(dir.join("extra.toml"), toml_src).unwrap();
        let rules = load_rules_from_dir(dir);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "ticket");
        std::fs::remove_dir_all(dir).ok();
    }
}
