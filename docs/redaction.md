# Redaction

termshot can mask sensitive data so screenshots are safe to share. Redaction
masks the rendered IMAGE; by default the text returned to the caller keeps the
ORIGINAL content, so an agent can see what was on screen and decide what else to
hide.

Redaction is opt-in. Enable it per run with `--redact` or `--redact-rules a,b`
(CLI), or `redact` / `redaction_rules` (MCP). To make it always-on, set
`auto = true` in the config `[redaction]` section. Use `--redact-text` (or
`redact_text: true`) to also scrub the returned text, and `--no-redact` (or
`redact: false`) to force it off for one run.

`[redaction] enabled` is the master switch. With `enabled = false` no rule ever
runs, and an explicit `--redact` fails with an error rather than handing back an
unredacted image. The shipped default is `enabled = true`, `auto = false`.

## Built-in rules

A set of rules is compiled in and enabled by default:

```
ipv4, ipv6, mac, aws_key, aws_secret, private_key, jwt, email, hostname,
api_key, github_token, slack_token, private_key_pem, gcp_service_account,
azure_client_secret, generic_api_key, bearer_token, connection_string,
discord_token, hashicorp_vault_token
```

Several provider-token patterns (GitHub, Slack, Discord, HashiCorp Vault, GCP,
Azure, and the generic key/token/connection-string matchers) are sourced from
and inspired by [Betterleaks](https://github.com/betterleaks/betterleaks) (MIT).

Matches are covered with a colored block and a short `[LABEL]` tag. Rules run
against logical lines, so a secret that crosses the right margin is masked on
both rows. That works because a capture keeps the terminal's own soft-wrap
information: the raw PTY bytes are sliced, never re-emitted row by row, so a
value the terminal wrapped is still one value to the rules - including in the
line the interactive shell echoes back before it runs your command. Hard
newlines are never joined, so two unrelated lines cannot form a match between
them. Obviously non-sensitive values such as `127.0.0.1`, `::1`, and IPv6
look-alikes in code (`std::fs::read`) are left visible.

The built-ins favor precision over recall: a silently corrupted screenshot is
worse than one you chose not to redact. Review the returned text and use
`redact_screenshot` (MCP) for anything the rules do not cover.

## Custom rules

Every `.toml`, `.yaml`, and `.yml` file in `~/.config/termshot/rules/` (created
on first run) is loaded automatically. Supply an additional directory with
`--rules-path <dir>` or the `[redaction] rules_path` config key; its rules are
layered on top and win on name conflicts.

TOML files use the native `[[rules]]` format:

```toml
[[rules]]
name = "ticket"
pattern = 'TICKET-\d+'
replacement = "[REDACTED-TICKET]"
enabled = true
# Optional per-rule block color, entropy floor, and partial-redaction counts:
# color = "#ff6600"
# min_entropy = 3.5
# keep_prefix = 4
# keep_suffix = 0
```

Wrap the part to mask in a `(?P<redact>...)` group to match surrounding context
without masking it, for example `'(?:^|\s)(?P<redact>TICKET-\d+)\b'`.

Built-ins can be overridden or disabled by name. An override keeps the built-in
label and can change the pattern, color, entropy floor, and `keep_prefix` /
`keep_suffix`. An empty or omitted `pattern` keeps the built-in pattern:

```toml
# Disable a built-in.
[[rules]]
name = "email"
enabled = false

# Keep only the first 4 characters of every AWS key visible.
[[rules]]
name = "aws_key"
keep_prefix = 4
```

YAML files use the Kingfisher rule format (Apache 2.0), a `rules:` list with
`name`/`id` and `pattern`/`regex`:

```yaml
rules:
  - name: ticket
    id: ticket
    pattern: 'TICKET-\d+'
```

## Partial redaction

To reveal only part of a secret, either write a regex that matches just the
sensitive portion, or set `keep_prefix` and/or `keep_suffix` (character counts)
to leave leading and trailing characters visible and mask the middle. For an AWS
key `AKIA...`, `keep_prefix: 4` shows `AKIA` followed by blocks. If the kept
prefix and suffix cover the whole match, nothing is redacted.

These work in config rules and on `redact_screenshot` patterns over MCP:

```json
{
  "pattern": "AKIA[0-9A-Z]{16}",
  "keep_prefix": 4
}
```

## Selective redaction workflow

An agent can capture, read the real output, and mask exactly what matters:

1. Capture with `execute_and_screenshot` (or `render_ansi`). The returned text
   is the original content even when the PNG is masked.
2. Inspect the text and decide what else is sensitive.
3. Call `redact_screenshot` with the screenshot path and a list of redactions,
   each either a regex `pattern` (with optional `replacement`, `keep_prefix`,
   `keep_suffix`) or an explicit cell range (`row`, `col_start`, `col_end`,
   optional `label`). The image is re-rendered in place.

See [mcp.md](./mcp.md#workflow-capture-inspect-selectively-redact).

## Labels and colors

Each block can carry a short tag. For a built-in it is a fixed abbreviation
(`IP`, `KEY`, `JWT`); for a custom rule it is derived from `replacement`, so
`[REDACTED-TICKET]` becomes `TICKET` and an empty `replacement` means no label.
Pass `show_labels: false` (on `execute_and_screenshot` or `redact_screenshot`)
to draw plain solid blocks everywhere.

Block and label colors are set engine-wide with `[redaction] color` and
`[redaction] label_color`, and per rule with a rule's `color` field, all as
`#RRGGBB`. The default block is bright red (`#d41919`) with a black label.
