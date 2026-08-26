# Redaction

termshot can mask sensitive data so screenshots are safe to share. Redaction
masks the rendered IMAGE; by default the text returned to the caller keeps the
ORIGINAL, unredacted content so an agent can see exactly what was on screen and
decide what else to hide.

## Built-in rules

A set of rules is compiled in and enabled by default, matching common secret
formats:

```
ipv4, ipv6, mac, aws_key, aws_secret, private_key, jwt, email, hostname,
api_key, github_token, slack_token, private_key_pem, gcp_service_account,
azure_client_secret, generic_api_key, bearer_token, connection_string,
discord_token, hashicorp_vault_token
```

Several provider-token patterns (GitHub, Slack, Discord, HashiCorp Vault, GCP,
Azure, and the generic key/token/connection-string matchers) are sourced from
and inspired by [Betterleaks](https://github.com/betterleaks/betterleaks) (MIT).

Matches are covered with a colored block and a short `[LABEL]` tag. Obviously
non-sensitive values such as `127.0.0.1`, `::1`, and IPv6 look-alikes inside
ordinary code (`std::fs::read`) are left visible.

Rules run against *logical* lines: rows joined by a soft wrap are matched as one
string, so a key or JWT that crosses the right margin is still masked on both
rows.

Redaction is **opt-in**: it does not run unless you ask for it. Enable it per
run with `--redact` or `--redact-rules a,b` (CLI), or `redact` /
`redaction_rules` (MCP). To make it always-on, set `auto = true` in the config
`[redaction]` section; every `exec` and `render` then masks its PNG. Use
`--redact-text` (or `redact_text: true`) to also scrub the returned text, and
`--no-redact` (or `redact: false`) to force it off for one run.

Precision over recall: the built-ins are tuned to avoid masking normal
developer output (source code, hashes, semantic versions, base64 fixtures),
because a silently corrupted screenshot is worse than one you chose not to
redact. Review the returned text and use `redact_screenshot` (MCP) for anything
the rules do not cover.

## Turning redaction off

`[redaction] enabled` is the master switch. With `enabled = false` no rule ever
runs, and an explicit `--redact` / `redact: true` fails with an error rather
than handing back an unredacted image. To keep the rules available but off by
default, leave `enabled = true` and `auto = false` (the shipped default) and
ask for redaction per run.

## Custom rules

Drop rule files into `~/.config/termshot/rules/` - created on first run, like
`themes/` - and every `.toml`, `.yaml`, and `.yml` file in it is loaded
automatically. Supply an *additional* directory with `--rules-path <dir>` (or
the `[redaction] rules_path` config key); its rules are layered on top and win
on name conflicts. TOML files use the native `[[rules]]` format; YAML files use the
Kingfisher rule format (a `rules:` list with `name`/`id` and `pattern`/`regex`).
Kingfisher is Apache 2.0 licensed, and its YAML rule schema is compatible here.

A native TOML rule file is a list of `[[rules]]` entries:

```toml
[[rules]]
name = "ticket"
pattern = 'TICKET-\d+'
replacement = "[REDACTED-TICKET]"
enabled = true
# Optional: wrap the part to mask in a (?P<redact>...) group to match
# surrounding context without masking it, e.g.
#   pattern = '(?:^|\s)(?P<redact>TICKET-\d+)\b'
# Optional: per-rule block color, entropy floor, and partial-redaction counts.
# color = "#ff6600"
# min_entropy = 3.5
# keep_prefix = 4
# keep_suffix = 0
```

Individual built-ins can be overridden or disabled by name. Overriding a
built-in keeps its label and lets you change the pattern, color, entropy floor,
and the `keep_prefix` / `keep_suffix` partial-redaction counts. An empty
`pattern` keeps the built-in pattern (useful when you only want to disable it or
add a `keep_prefix`):

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

A YAML rule file uses the generic (Kingfisher) format:

```yaml
rules:
  - name: ticket
    id: ticket
    pattern: 'TICKET-\d+'
```

## Partial redaction

To reveal only part of a secret, either write a regex that matches just the
sensitive portion, or set `keep_prefix` and/or `keep_suffix` (character counts).
Those leave the leading and trailing characters visible and mask only the
middle. For a hash like `8846f7eaee8fb117ad06bdd830b7586c`, `keep_prefix: 4`
renders `8846` followed by blocks; for an AWS key `AKIA...`, `keep_prefix: 4`
shows `AKIA` followed by blocks.

`keep_prefix` / `keep_suffix` work in config rules and on `redact_screenshot`
patterns over MCP:

```json
{
  "pattern": "AKIA[0-9A-Z]{16}",
  "keep_prefix": 4
}
```

If the kept prefix and suffix cover the whole match, nothing is left to redact
and the match is skipped.

## Labels

Each redaction block can carry a short text tag drawn over the blocks. For a
built-in the tag is a fixed abbreviation (for example `IP`, `KEY`, `JWT`); for a
custom rule it is derived from the `replacement` text (for example
`[REDACTED-TICKET]` becomes `TICKET`). An empty `replacement` means "no label"
(a plain block).

Turn labels off entirely with `show_labels: false` on `execute_and_screenshot`
or `redact_screenshot` to draw plain solid blocks with no text overlay.

## Colors

Redaction block and label colors are configurable engine-wide via
`[redaction] color` and `[redaction] label_color`, and per rule via a rule's
`color` field, all as `#RRGGBB`. The default block is bright red (`#d41919`)
with a black label.

## Agent-driven workflow

See [mcp.md](./mcp.md#agent-workflow-selective-redaction) for the
capture -> inspect -> `redact_screenshot` flow that lets an agent selectively
mask output after reading it.
