<h1 align="center">termshot</h1>

<p align="center">
  Terminal screenshots as PNGs, with ANSI colors fully rendered.
</p>

<p align="center">
  <a href="https://github.com/Adamkadaban/termshot/releases"><img alt="Latest release" src="https://img.shields.io/github/v/tag/Adamkadaban/termshot?label=release&sort=semver&color=brightgreen"></a>&nbsp;
  <a href="./LICENSE"><img alt="License" src="https://img.shields.io/badge/license-MIT-blue"></a>&nbsp;
  <img alt="Made with vibes" src="https://img.shields.io/badge/made_with-vibes-ff69b4">
</p>

---

Run a command, get a PNG that looks like your terminal. Works as a
standalone CLI and as an MCP server for AI agents. Built for pentest
reports, blog posts, and PR descriptions.

<p align="center">
  <img src="docs/assets/hero.png" alt="termshot capturing a colorized 'ls -la' of the source tree with the shell prompt visible, framed in a GNOME window with the command as its title" width="700">
</p>

```sh
termshot exec --chrome gnome --title 'termshot - ls src/' 'ls --color=always -la src/'
```

## Highlights

**Real ANSI rendering** - colors, bold, italic, underline, Unicode, rendered
at 2x resolution with your own font. Commands run in a PTY through your login
shell, so your prompt, aliases, and `PATH` are in the image.

<p align="center">
  <img src="docs/assets/ansi.png" alt="Screenshot of a colorized 'git log --graph --oneline' with orange commit hashes rendered next to the shell prompt" width="700">
</p>

```sh
termshot exec 'git log --graph --oneline --color=always -n 6'
```

**Redaction** - opt-in masking of secrets in the image (`--redact`, or
`auto = true` in config). The text returned to the caller keeps the originals,
so an agent can inspect it and selectively redact.
See [docs/redaction.md](./docs/redaction.md).

<p align="center">
  <img src="docs/assets/redaction.png" alt="Screenshot of command cat .env.staging in a GNOME window. The variable names AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, GITHUB_TOKEN, and STRIPE_SECRET_KEY remain visible while their values are masked by red blocks. The prefixes AKIA, ghp_, and sk_live_ remain visible to identify each secret type." width="700">
</p>

```sh
termshot exec --redact --chrome gnome 'command cat .env.staging'
```

**Themes** - 11 built-in (using the bundled JetBrains Mono), plus any themes you
drop in `~/.config/termshot/themes/`, each with its own fonts.

<p align="center">
  <img src="docs/assets/themes.png" alt="The same colorized 'git log' command rendered in three built-in themes - catppuccin-mocha, dracula, and nord - stacked with thin dividers to show their different backgrounds and palettes" width="700">
</p>

```sh
for theme in catppuccin-mocha dracula nord; do
  termshot exec --theme "$theme" --no-rounded -o "$theme.png" \
    'git log --oneline --color=always -n 4'
done
termshot compose --divider 4 -o themes.png \
  catppuccin-mocha.png dracula.png nord.png
```

**Your prompt, whatever it is** - commands run through your login shell, so the
prompt in the image is the one your own shell config draws. Prompts are
independent of termshot themes, which only set colors, font, and background:
the four panes below are four different shell prompts in the one built-in
`dark` theme.

<p align="center">
  <img src="docs/assets/prompts.png" alt="Four stacked terminal panes, all rendered in the built-in dark theme, each running the same 'git status -sb' in the same demo repository behind a different real shell prompt, with a trailing shell comment naming the style: a distro-style bash PS1 with the user in green and the path in blue; Oh My Zsh's robbyrussell theme with its green arrow, cyan directory, blue git:(main) marker and yellow dirty cross; Oh My Zsh's bira theme, a two-line prompt whose box-drawing corners bracket the user, path and a yellow branch tag with a red dirty dot; and a personal zsh prompt that appends the branch as ~(main). The identical two lines of output under each prompt show that the prompt comes from the shell, not from termshot" width="820">
</p>

Starship, Powerlevel10k, Oh My Zsh themes, or a hand-rolled `PS1` show up the
same way. The image uses isolated shell configs so it does not modify the
user's normal setup.

```sh
TERMSHOT_SHELL=/path/to/shell-wrapper termshot exec \
  --theme dark -o prompt.png 'git status -sb'
termshot compose --divider 4 -o prompts.png prompt-*.png
```

**Chrome frames** - title bar presets with optional timestamp. Good for
reports and blog posts.

<p align="center">
  <img src="docs/assets/chrome.png" alt="An 'nmap -F localhost' service scan showing port 3000 open, rendered with a GNOME-style title bar, rounded corners, and a UTC timestamp watermark in the corner like a pentest report screenshot" width="700">
</p>

```sh
termshot exec --chrome gnome --timestamp 'nmap -F localhost'
```

**Composition** - combine screenshots side by side or stacked (tmux-style)
for before/after comparisons.

<p align="center">
  <img src="docs/assets/compose.png" alt="A side-by-side before/after of 'git status -su' while staging a feature in a rate-limiter crate: the left pane lists five modified files and three untracked ones in red, the right pane shows the same eight files staged in green after git add, both with the shell prompt and command visible, joined by a thin vertical tmux-style divider in one window" width="820">
</p>

```sh
termshot exec -o before.png 'git status -su'
git add -A
termshot exec -o after.png 'git status -su'
termshot compose --layout horizontal --chrome gnome \
  --title 'git status - before / after staging' -o compose.png before.png after.png
```

**Accessible by default** - every PNG embeds its terminal text (redacted, when
redaction ran) in a UTF-8 `Description` chunk so screen readers can read the
screenshot. Turn it off with `--no-description` or `embed_description = false`.

**MCP server** - four tools for agent workflows:
`execute_and_screenshot`, `render_ansi`, `redact_screenshot`,
`compose_screenshots`.

## Install

```sh
git clone https://github.com/Adamkadaban/screenshot-mcp termshot
cd termshot
cargo build --release
# binary at ./target/release/termshot
```

Tagged releases also publish Linux (x86_64, aarch64, static musl) and macOS
(Intel, Apple Silicon) binaries plus `.deb` and `.rpm` packages.

Linux and macOS only: termshot uses PTYs and POSIX signals.

## Usage

```sh
termshot exec 'ls --color=always -la'                          # basic screenshot
termshot exec --chrome gnome --theme dracula 'git log'         # chrome + theme
termshot exec --redact 'cat credentials.txt'                   # mask secrets
termshot exec --no-rounded 'ls --color=always'                 # square corners
termshot compose -o diff.png before.png after.png              # stack two shots
termshot themes                                                # list themes

# render pre-captured ANSI without executing anything, from a file or a pipe
cmd --color=always | termshot render -
termshot render output.log --redact
```

## Tips

```sh
termshot exec '!!'                            # your shell expands !! first
termshot exec 'command cat .env.staging'      # bypass a cat -> bat alias
termshot exec --no-prompt 'cat .env.staging'  # no prompt, no aliases
alias tshot='termshot exec'
```

## MCP server

Add to your MCP client config:

```json
{
  "mcpServers": {
    "termshot": {
      "command": "/path/to/termshot",
      "args": ["mcp"]
    }
  }
}
```

## Docs

- [docs/mcp.md](./docs/mcp.md) - MCP setup, tool reference, agent workflows
- [docs/themes.md](./docs/themes.md) - built-in and user themes, fonts, fallback
- [docs/redaction.md](./docs/redaction.md) - rules, custom rules, partial redaction
- [docs/config.md](./docs/config.md) - `config.toml` reference

## License

[MIT](./LICENSE)
