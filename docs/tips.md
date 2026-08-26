# Tips

## Shell shortcuts

```sh
termshot exec '!!'              # your shell expands !! before termshot sees it
alias tshot='termshot exec'     # then: tshot 'cargo test'
```

Commands run through your login shell, so aliases apply. Bypass one the way you
would interactively:

```sh
termshot exec 'command cat .env.staging'      # bypass a cat -> bat alias
termshot exec --no-prompt 'cat .env.staging'  # non-interactive: no prompt, no aliases
```

## Render captured ANSI

`render` draws a PNG without executing anything, from a file or a pipe:

```sh
cmd --color=always | termshot render -
termshot render output.log --redact
termshot render --tail-lines 30 build.log
```

Many tools drop color when stdout is not a TTY, hence `--color=always`.

## Trim long output

```sh
termshot exec --tail-lines 20 'cargo test'   # last 20 lines
termshot exec --head-lines 15 'dmesg'        # first 15 lines
```

The two flags are mutually exclusive. Without either, the screenshot shows the
whole run, not just the last screenful.

## Recipes

Pentest report finding, framed and timestamped, with secrets masked:

```sh
termshot exec --redact --chrome report --timestamp \
  -o finding-01-portscan.png 'nmap -F 10.20.30.40'
```

Mask something the built-in rules do not know about, on top of them:

```sh
termshot exec --redact \
  --redaction '{"pattern":"TCK-[0-9]+","replacement":"TICKET"}' \
  'cat notes.txt'
```

Before/after for a PR description:

```sh
termshot exec --no-rounded -o before.png 'cargo clippy 2>&1'
# ... apply the fix ...
termshot exec --no-rounded -o after.png 'cargo clippy 2>&1'
termshot compose --layout horizontal --chrome gnome \
  --title 'clippy - before / after' -o pr-fix.png before.png after.png
```

Blog post shots in a consistent theme, square-cornered for embedding:

```sh
export TERMSHOT_THEME=catppuccin-mocha
termshot exec --no-rounded -o step-1.png 'eza --tree --level 2 --color=always'
```

`termshot themes` lists every theme available.

See [redaction.md](./redaction.md) for the `--redaction` format,
[themes.md](./themes.md) for themes and fonts, and
[config.md](./config.md) for defaults you can set once.
