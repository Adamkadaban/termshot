<h1 align="center">termshot</h1>

<p align="center">
  Terminal screenshots as PNGs, with ANSI colors fully rendered.
</p>

<p align="center">
  <a href="./LICENSE"><img alt="License" src="https://img.shields.io/badge/license-MIT-blue"></a>
  <img alt="Made with vibes" src="https://img.shields.io/badge/made_with-vibes-ff69b4">
  <img alt="Rust" src="https://img.shields.io/badge/rust-stable-orange">
</p>

---

Run a command, get a PNG that looks like your terminal. Works as a
standalone CLI and as an MCP server for AI agents. Built for pentest
reports, blog posts, and PR descriptions.

<p align="center">
  <img src="docs/assets/hero.png" alt="termshot capturing a colorized 'ls -la' of the source tree with the shell prompt visible, framed in a GNOME window with the command as its title" width="700">
</p>

```sh
termshot exec --chrome gnome --title 'termshot — ls src/' 'ls --color=always -la src/'
```

## Highlights

**Real ANSI rendering** - colors, bold, italic, underline, Unicode.
Rendered at 2x resolution with your own font.

<p align="center">
  <img src="docs/assets/ansi.png" alt="Screenshot of a colorized 'git log --graph --oneline' with orange commit hashes rendered next to the shell prompt" width="700">
</p>

```sh
termshot exec 'git log --graph --oneline --color=always -n 6'
```

**Redaction** - opt-in masking of secrets in the image (`--redact`, or
`auto = true` in config). Text returned to the caller preserves originals so
agents can inspect and selectively redact.

<p align="center">
  <img src="docs/assets/redaction.png" alt="Screenshot of an accidentally cat'd .env.staging file where the AWS key, secret, database URL, GitHub token, and Stripe key are each partially masked by red blocks labeled AWSKEY, SECRET, DBURL, GITHUB, and STRIPE - the identifying prefix stays visible so you can see the secret type but not its value" width="700">
</p>

```sh
# `command cat` bypasses the shell alias: termshot runs your interactive shell,
# where `cat` is often `bat`/`batcat` and would add its own frame and line numbers
termshot exec --redact --chrome gnome 'command cat .env.staging'

# the labelled, prefix-preserving blocks above come from custom rules using
# `keep_prefix`: drop a .toml in ~/.config/termshot/rules/ and it is picked up
# automatically - see docs/redaction.md
```

Set `[redaction] enabled = false` in the config to turn redaction off entirely;
with it off, an explicit `--redact` fails loudly instead of quietly writing an
unredacted image.

**Themes** - 11 built-in (using the bundled JetBrains Mono), plus user themes
with custom fonts at `~/.config/termshot/themes/`.

<p align="center">
  <img src="docs/assets/themes.png" alt="The same colorized 'git log' command rendered in three themes - adamkadaban, dracula, and nord - stacked with thin dividers to show their different backgrounds and palettes" width="700">
</p>

```sh
for theme in adamkadaban dracula nord; do
  termshot exec --theme "$theme" --no-rounded -o "$theme.png" \
    'git log --oneline --color=always -n 4'
done
termshot compose --divider 4 -o themes.png adamkadaban.png dracula.png nord.png
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
  --title 'git status — before / after staging' -o compose.png before.png after.png
```

**MCP server** - four tools for agent workflows:
`execute_and_screenshot`, `render_ansi`, `redact_screenshot`,
`compose_screenshots`.

## Install

```sh
git clone https://github.com/Adamkadaban/termshot
cd termshot
cargo build --release
# binary at ./target/release/termshot
```

### Debian/Ubuntu package

Release builds publish a `.deb` for `x86_64` and `aarch64`. It installs the
binary to `/usr/bin/termshot`, the man page to
`/usr/share/man/man1/termshot.1.gz`, and a sample config to
`/usr/share/doc/termshot/config.toml.example`.

The package does not write a user config. On first run any command that touches
config - for example `termshot themes` - bootstraps
`~/.config/termshot/` (including `config.toml`), so your settings are never
overwritten by upgrades.

## Usage

```sh
# basic screenshot
termshot exec 'ls --color=always -la'

# with chrome and theme
termshot exec --chrome gnome --theme dracula 'git log --oneline'

# auto-redact secrets in the image
termshot exec --redact 'cat credentials.txt'

# square (un-rounded) corners; corners are rounded by default
termshot exec --no-rounded 'ls --color=always'

# render pre-captured ANSI without executing anything: from a pipe or a file
cmd --color=always | termshot render -
termshot render output.log --redact

# side-by-side comparison
termshot compose -o diff.png before.png after.png

# list themes
termshot themes
```

`termshot render` reads raw ANSI data (from a file, or from stdin with `-`) and
renders it to a PNG **without executing anything** - handy for piping the output
of a command you have already run, or for previously saved logs. Bare `\n` line
endings from non-TTY output are handled automatically. It takes the same
`--redact` / `--no-redact` flags as `exec`.

### Your shell, your environment

`termshot exec` runs the command in a real PTY through your shell as a **login
+ interactive** shell (`$SHELL -l -i`), so it sources your profile
(`~/.bashrc`, `~/.zshrc`, ...) and inherits the environment you normally work
in: prompt (PS1), aliases and shell functions, `PATH`, exports, and shell
options. That is what makes a screenshot look like *your* terminal.

It also means aliases apply. If `cat` is aliased to `bat`/`batcat`, then
`termshot exec 'cat file'` screenshots *bat's* framed, line-numbered output.
Bypass an alias the same way you would interactively:

```sh
termshot exec 'command cat .env.staging'   # ignore the alias, run the real cat
termshot exec '\cat .env.staging'           # same, via backslash
termshot exec --no-prompt 'cat .env.staging'  # non-interactive: no aliases at all
```

The screen is reset before the command runs, so shell startup banners (MOTD,
version notices) stay out of the image and every capture shows exactly one
prompt: the one in front of your command. The trailing prompt the shell draws
afterwards is removed too, including the upper lines of a multi-line PS1.

`--no-prompt` runs the command non-interactively instead (`$SHELL -c`), so
there is no PS1 in the image and interactive-only startup files are not
sourced. Set `shell` in the config (or `TERMSHOT_SHELL`) to capture with a
different shell.

**Accessibility** - every PNG embeds its terminal text (the redacted version
when redaction ran) in a `Description` metadata chunk, so screen readers can
read the screenshot. Disable per run with `--no-description`, or globally with
`embed_description = false`.

### Known limitations

- **One font, no fallback chain.** Every glyph is rasterized from the single
  configured font (bundled JetBrains Mono by default). Codepoints that font does
  not cover - CJK, emoji, and some box-drawing or Powerline glyphs used by tools
  like `bat`, `btop`, or `eza` - render as `.notdef` boxes. Point `font` /
  `font_bold` at a face with wider coverage (a Nerd Font, for example) when
  capturing such output.
- **Unix only.** termshot uses PTYs and POSIX signals; Linux and macOS are
  supported, Windows is not.

## Tips

```sh
# screenshot the last command you ran (bash/zsh history expansion)
termshot exec '!!'

# alias for quick screenshots
alias tshot='termshot exec'
tshot 'git status'

# render pre-captured ANSI output from a file or pipe
cmd --color=always | termshot render -
termshot render output.log
```

`!!` is expanded by your shell before termshot ever sees it, so you get a
screenshot of your previous command line and its output.

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

- [docs/mcp.md](./docs/mcp.md) - MCP server setup, tool reference, and
  agent workflow examples
- [docs/themes.md](./docs/themes.md) - theme format, built-in list, user
  themes, and font config
- [docs/redaction.md](./docs/redaction.md) - redaction rules, custom YAML
  format, partial redaction, and labels
- [docs/config.md](./docs/config.md) - full `config.toml` reference

## License

[MIT](./LICENSE)
