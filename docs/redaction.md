# Redaction

Redaction masks the rendered IMAGE. The text returned to the caller keeps the
ORIGINAL content by default, so an agent can see what was on screen and decide
what else to hide.

## Default behavior

Redaction is opt-in. Enable it per run with `--redact` or `--redact-rules a,b`
(CLI), or `redact` / `redaction_rules` (MCP); set `auto = true` in the config
`[redaction]` section for always-on. `--redact-text` (`redact_text: true`) also
scrubs the returned text, `--no-redact` (`redact: false`) forces redaction off
for one run, and `[redaction] enabled = false` is the master switch under which
no rule runs and an explicit `--redact` errors instead of returning an
unredacted image. Defaults are `enabled = true`, `auto = false`.

Matches are covered with a colored block and a short `[LABEL]` tag. Rules run
against logical lines, so a secret that wrapped across the right margin is
masked on both rows. Non-sensitive values such as `127.0.0.1`, `::1`, and IPv6
look-alikes in code (`std::fs::read`) are left visible.

## Built-in rules

Compiled in and enabled by default:

```
ipv4, ipv6, mac, aws_key, aws_secret, private_key, private_key_pem, jwt, email,
hostname, api_key, github_token, slack_token, discord_token, gcp_service_account,
azure_client_secret, generic_api_key, bearer_token, connection_string,
hashicorp_vault_token
```

Several provider-token patterns (GitHub, Slack, Discord, HashiCorp Vault, GCP,
Azure, and the generic key/token/connection-string matchers) are sourced from
and inspired by [Betterleaks](https://github.com/betterleaks/betterleaks) (MIT).
They favor precision over recall: review the returned text and add a manual
redaction for anything they miss.

## Custom rules

Every `.toml`, `.yaml`, and `.yml` file in `~/.config/termshot/rules/` is loaded
automatically. Add another directory with `--rules-path <dir>` or the
`[redaction] rules_path` key; its rules layer on top and win name conflicts.

```toml
[[rules]]
name = "ticket"
# Wrap the part to mask in (?P<redact>...) to match context without masking it:
pattern = '(?:^|\s)(?P<redact>TICKET-\d+)\b'
replacement = "[REDACTED-TICKET]"
enabled = true
# Optional: color = "#ff6600", min_entropy = 3.5, keep_prefix = 4, keep_suffix = 0
```

Built-ins are overridden or disabled by reusing their name: `name = "email"` with
`enabled = false` turns one off, and `name = "aws_key"` with `keep_prefix = 4`
overrides one field while keeping the built-in label and pattern.

YAML files use the Kingfisher rule format (Apache 2.0), a `rules:` list with
`name`/`id` and `pattern`/`regex`.

## Manual redaction (CLI and MCP)

The CLI takes one per repeatable `--redaction '<JSON>'` on `exec` and `render`;
MCP takes a list in `redactions` on `execute_and_screenshot`, `render_ansi`, and
`redact_screenshot`. All decode the same JSON. Either a regex pattern, matched
against every line the screenshot shows:

```json
{ "pattern": "[a-f0-9]{32}", "replacement": "HASH", "keep_prefix": 4, "color": "#d41919" }
```

or an explicit cell range on one row (`label` and `color` optional):

```json
{ "row": 3, "col_start": 12, "col_end": 44, "label": "SECRET" }
```

```sh
termshot exec --redaction '{"pattern":"[a-f0-9]{32}","keep_prefix":4}' 'secretsdump.py 10.20.30.40'
```

Only `pattern` (or a full `row`/`col_start`/`col_end` range) is required.
Coordinates are 0-based and counted in the image as rendered, so row 0 is its
top visible line; out-of-range values are an error naming the rendered
dimensions rather than a block that quietly covers nothing. Specifications apply
in the order given (patterns first, then cell ranges) and mask the image even
without `--redact` / `redact: true`, which additionally runs the built-in rules.
Combining them with `--no-redact` (`redact: false`) is refused, as is invalid
JSON, an unknown field, a bad regex, or a bad color. See
[mcp.md](./mcp.md#workflow-capture-inspect-redact) for the agent workflow.

## Partial redaction

To reveal part of a secret, write a regex matching only the sensitive portion,
or set `keep_prefix` / `keep_suffix` (character counts, valid in config rules
and manual specifications alike): for `AKIA...`, `keep_prefix: 4` shows `AKIA`
followed by blocks. If they cover the whole match, nothing is redacted.

## Labels and colors

Each block can carry a short tag: a fixed abbreviation for a built-in (`IP`,
`KEY`, `JWT`), the `replacement` for a custom or pattern rule (`[REDACTED-TICKET]`
becomes `TICKET`, an empty value means no label), or `label` for a coordinate
range (untagged ranges become `[REDACTED]`). Pass `show_labels: false` over MCP
for plain solid blocks.

Colors are `#RRGGBB`: engine-wide with `[redaction] color` and
`[redaction] label_color` (see [config.md](./config.md)), and per rule or manual
redaction with a `color` field. The default is bright red (`#d41919`).
