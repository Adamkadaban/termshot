# Install

## Prebuilt with cargo-binstall (recommended)

```sh
cargo binstall termshot
```

This downloads the matching binary from GitHub Releases and installs it into
`~/.cargo/bin`. If no matching binary exists, cargo-binstall can fall back to a
source build.

Install cargo-binstall first if needed:

```sh
cargo install cargo-binstall
```

## Build from crates.io

```sh
cargo install termshot
```

## Prebuilt releases

Every tagged release on
[GitHub releases](https://github.com/Adamkadaban/termshot/releases) publishes:

- Linux `x86_64` and `aarch64` (glibc), plus a fully static `x86_64` musl build
- macOS Intel (`x86_64`) and Apple Silicon (`aarch64`)
- `.deb` packages (`x86_64`, `aarch64`) and an `x86_64` `.rpm`

Archives contain the binary, `LICENSE`, `README.md`, `termshot.example.toml`,
the `docs/` tree, and the man page.

```sh
tar -xzf termshot-<version>-x86_64-unknown-linux-gnu.tar.gz
sudo install -m755 termshot-<version>-x86_64-unknown-linux-gnu/termshot /usr/local/bin/
```

```sh
sudo dpkg -i termshot_<version>_amd64.deb     # Debian / Ubuntu
sudo rpm -i termshot-<version>.x86_64.rpm     # Fedora / RHEL
```

## From source

```sh
git clone https://github.com/Adamkadaban/termshot
cd termshot
cargo build --release
# binary at ./target/release/termshot
```

## Platform support

Linux and macOS only: termshot uses PTYs and POSIX signals. Windows is not
supported (WSL works).

JetBrains Mono is compiled into the binary, so no fonts need to be installed.

## Config bootstrap

On first run termshot creates:

```
~/.config/termshot/
|- config.toml
|- rules/     # redaction rule files (.toml / .yaml)
`- themes/    # user themes, one .toml per theme
```

Packages never write a user config, so upgrades do not overwrite your settings.
A commented reference config ships as `termshot.example.toml` (installed to
`/usr/share/doc/termshot/config.toml.example` by the deb and RPM packages).
See [config.md](./config.md).

## Man page

The deb and RPM packages install
[`docs/man/termshot.1`](./man/termshot.1) to
`/usr/share/man/man1/termshot.1`, so `man termshot` works after install. With
any other method, install it by hand or read it directly:

```sh
sudo install -m644 docs/man/termshot.1 /usr/local/share/man/man1/termshot.1
man ./docs/man/termshot.1
```
