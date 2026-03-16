# Redaction Stretch Goal

## Overview

During pentests, terminal screenshots may contain sensitive information that
should not be shared in reports or stored long-term: IP addresses, credentials,
API keys, session tokens, internal hostnames, etc.

The goal is to add an automated redaction layer that can sanitize screenshots
before they are saved -- either by modifying the ANSI data before rendering,
or by post-processing the rendered image.

## Approach: ANSI-level Redaction (Preferred)

Modify the terminal buffer **before** rendering to PNG. This is cleaner than
image-level redaction because:

- No risk of partial character overlap / bleed-through
- Can replace sensitive text with `[REDACTED]` or `████████` blocks
- Works at the semantic level (knows what characters are being redacted)

### Pipeline

```
raw PTY bytes
  -> vt100 parser (build screen buffer)
  -> redaction pass (scan buffer cells, replace matches)
  -> render to PNG
```

### Redaction Engine Options

1. **Regex-based rules** (config file)
   - User defines patterns in `config.toml`:
     ```toml
     [[redaction.rules]]
     name = "ipv4"
     pattern = '\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b'
     replacement = "[REDACTED-IP]"

     [[redaction.rules]]
     name = "aws_key"
     pattern = 'AKIA[0-9A-Z]{16}'
     replacement = "[REDACTED-KEY]"
     ```
   - Fast, deterministic, no external dependencies
   - Limited to known patterns

2. **Local LLM-based redaction** (stretch within the stretch)
   - Send the plain-text screen contents to a local LLM (e.g., Ollama, llama.cpp)
   - Prompt: "Identify all sensitive information (IPs, credentials, tokens, etc.)
     and return their exact positions"
   - LLM returns spans to redact
   - **Advantages**: catches things regex can't (e.g., passwords in context)
   - **Security**: must be a LOCAL model -- never send pentest data to cloud APIs
   - **Config**:
     ```toml
     [redaction.llm]
     enabled = true
     endpoint = "http://localhost:11434/api/generate"
     model = "llama3.2:3b"
     # System prompt for redaction task
     prompt = "Identify sensitive information..."
     ```

### Implementation Plan

1. Add `redaction` section to `Config` / `ConfigFile`
2. After vt100 parsing, extract plain text with cell positions
3. Run regex rules against the text, collect spans to redact
4. (Optional) Send text to local LLM, collect additional spans
5. For each span, replace the corresponding cells in the vt100 screen
   buffer with redaction characters (e.g., `█` in red background)
6. Render the modified buffer to PNG

### MCP Tool Extension

Add a `redact` parameter to `execute_and_screenshot`:

```json
{
  "command": "nmap -sV 192.168.1.0/24",
  "redact": true,
  "redaction_rules": ["ipv4", "mac_address"]
}
```

Or have it always-on via config:

```toml
[redaction]
enabled = true
# Apply to all screenshots by default
auto = true
```

### CLI Extension

```
termshot exec --redact -- nmap -sV 192.168.1.0/24
termshot exec --redact-rules ipv4,credentials -- cat /etc/shadow
```

## Considerations

- Redaction should be **visually obvious** (colored blocks, not just whitespace)
- Keep an audit log of what was redacted (without the actual values)
- Consider a "redaction preview" mode that highlights what would be redacted
  without actually removing it
- For LLM-based redaction, add a confidence threshold -- low-confidence
  matches should be flagged for human review rather than auto-redacted
- The LLM endpoint must be configurable and must default to localhost only
