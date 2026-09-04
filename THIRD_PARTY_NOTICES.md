# Third-party notices

`termshot` itself is distributed under the [MIT License](./LICENSE) —
Copyright (c) 2025-2026 Adam Hassan (adamkadaban).

This file records the third-party material that `termshot` **embeds or
redistributes**, together with the attribution each of those licenses requires.
Full license texts live in [`LICENSES/`](./LICENSES) and ship with every
artifact (crates.io `.crate`, GitHub source tarballs, release archives, `.deb`,
`.rpm`).

---

## 1. JetBrains Mono (embedded font) — SIL Open Font License 1.1

`fonts/JetBrainsMono-Regular.ttf` is compiled into the `termshot` binary via
`include_bytes!` (`src/renderer.rs`) and is therefore redistributed inside every
binary, archive, and package this project publishes.

> Copyright 2020 The JetBrains Mono Project Authors
> (https://github.com/JetBrains/JetBrainsMono)
>
> This Font Software is licensed under the SIL Open Font License, Version 1.1.

- Full license text: [`LICENSES/OFL-1.1.txt`](./LICENSES/OFL-1.1.txt)
- Upstream: https://github.com/JetBrains/JetBrainsMono
- SPDX: `OFL-1.1`

The OFL requires that this copyright notice and the license text accompany every
distribution of the Font Software, including when it is bundled inside another
work. `termshot` is *not* a JetBrains product and is not endorsed by JetBrains;
"JetBrains Mono" is a Reserved Font Name under the OFL and is used here only to
identify the unmodified upstream font.

### 1a. `tests/fixtures/limited-ascii.ttf` — OFL-1.1 derivative

`tests/fixtures/generate_fixtures.py` produces `limited-ascii.ttf` as a
`fontTools` subset of the JetBrains Mono file above. It is therefore a
**Modified Version** under the OFL and is covered by the same license and the
same notice. As the OFL's Reserved Font Name clause requires, the generator
rewrites the font's family/full/PostScript name records to `Termshot Test ASCII`
/ `TermshotTestASCII` so the fixture can never be mistaken for, or presented as,
JetBrains Mono.

`tests/fixtures/cjk-fallback.ttf`, `shape-a.ttf`, `shape-b.ttf`,
`color-emoji.ttf`, and `collection.ttc` are built from scratch by the same
script - filled rectangles, generated outlines, and generated name tables - and
contain no third-party material. They are covered by this project's own
[MIT License](./LICENSE). No real emoji, CJK, or Indic font is redistributed
here: the Unicode rendering tests deliberately use synthetic faces so they
assert on the renderer rather than on whatever is installed on the machine
running them.

---

## 2. `rmcp` / `rmcp-macros` — Apache License 2.0

The official Model Context Protocol Rust SDK is statically linked into the
`termshot` binary.

- Copyright: the Model Context Protocol authors
- Upstream: https://github.com/modelcontextprotocol/rust-sdk
- Full license text: [`LICENSES/Apache-2.0.txt`](./LICENSES/Apache-2.0.txt)
- SPDX: `Apache-2.0`

Apache-2.0 §4(a) requires that recipients of a derivative work receive a copy of
the license — satisfied by `LICENSES/Apache-2.0.txt`. §4(b) (state changes) does
not apply: `termshot` links the published crates unmodified and carries no
patched copy. §4(d) (propagate `NOTICE`) does not apply either: the upstream
repository and the published `.crate` files contain no `NOTICE` file, so there
is no notice content to propagate.

`rmcp`, `rmcp-macros`, and `unicode-linebreak` (reached through `cosmic-text`)
are the only Apache-2.0-**only** crates in the dependency graph, and the same
license text covers all three. Every other Apache-licensed dependency is
dual-licensed and is taken under MIT.

---

## 3. Betterleaks — MIT License (redaction rule patterns)

Some of the built-in redaction regexes in `src/redaction.rs` are sourced from or
inspired by [Betterleaks](https://github.com/betterleaks/betterleaks). The MIT
permission notice below is reproduced in full, as MIT requires:

```
MIT License

Copyright (c) 2026 Zachary Rice

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

- Full license text: [`LICENSES/Betterleaks-MIT.txt`](./LICENSES/Betterleaks-MIT.txt)
- SPDX: `MIT`

Additional redaction patterns are original to this project. See
[`docs/redaction.md`](./docs/redaction.md).

---

## 4. Other bundled license texts

Two dependencies are not available under MIT or Apache-2.0, so their notices are
shipped verbatim as well:

| Crate | License | Text |
|---|---|---|
| `foldhash` | `Zlib` | [`LICENSES/Zlib.txt`](./LICENSES/Zlib.txt) |
| `slotmap` | `Zlib` | [`LICENSES/Zlib.txt`](./LICENSES/Zlib.txt) |
| `unicode-ident` | `(MIT OR Apache-2.0) AND Unicode-3.0` | [`LICENSES/Unicode-3.0.txt`](./LICENSES/Unicode-3.0.txt) — the MIT half is covered by [`LICENSE`](./LICENSE) |

---

## 5. Dependency license summary

All 152 non-dev transitive dependency crate versions carry an explicit SPDX
`license` field. Every expression is permissive and compatible with MIT
redistribution. One crate (`self_cell`) offers a copyleft option alongside a
permissive one; this project elects the permissive half, and nothing in the
graph is copyleft-only.

| License expression | Crates |
|---|---:|
| `MIT OR Apache-2.0` | 85 |
| `MIT` | 32 |
| `Apache-2.0 OR MIT` | 11 |
| `Unlicense OR MIT` | 3 |
| `Zlib OR Apache-2.0 OR MIT` | 3 |
| `Apache-2.0` | 3 |
| `Zlib` | 2 |
| `MIT OR Apache-2.0 OR Zlib` | 2 |
| `MIT/Apache-2.0` (legacy slash form for `OR`) | 2 |
| `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | 2 |
| `BSD-3-Clause OR Apache-2.0` | 2 |
| `0BSD OR MIT OR Apache-2.0` | 1 |
| `MIT OR Zlib OR Apache-2.0` | 1 |
| `Apache-2.0 OR BSL-1.0` | 1 |
| `Apache-2.0 OR GPL-2.0-only` | 1 |
| `(MIT OR Apache-2.0) AND Unicode-3.0` | 1 |

**Dual-license elections made by this project**

| Crate(s) | Expression | Elected |
|---|---|---|
| all `MIT OR Apache-2.0` / `Apache-2.0 OR MIT` / `MIT/Apache-2.0` crates | dual | **MIT** |
| `adler2`, `bytemuck`, `fontdue`, `miniz_oxide`, `tinyvec` | includes `Zlib`/`0BSD` | **MIT** |
| `linux-raw-sys`, `rustix` | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | **MIT** |
| `moxcms`, `pxfm` | `BSD-3-Clause OR Apache-2.0` | **Apache-2.0** (see [`LICENSES/Apache-2.0.txt`](./LICENSES/Apache-2.0.txt)) |
| `ryu` | `Apache-2.0 OR BSL-1.0` | **Apache-2.0** |
| `self_cell` | `Apache-2.0 OR GPL-2.0-only` | **Apache-2.0** — the GPL option is never exercised |
| `foldhash`, `slotmap` | `Zlib` only | no election — Zlib applies |
| `unicode-ident` | `(MIT OR Apache-2.0) AND Unicode-3.0` | **MIT** for the code half; Unicode-3.0 applies to the embedded Unicode data and its notice is shipped |
| `rmcp`, `rmcp-macros`, `unicode-linebreak` | `Apache-2.0` only | no election — Apache-2.0 applies |

### 5a. Text shaping stack (`cosmic-text`)

Unicode shaping, system font discovery, and color glyph rasterization come from
`cosmic-text` and the crates it pulls in - `fontdb`, `fontconfig-parser`,
`harfrust`, `swash`, `skrifa`, `read-fonts`, `font-types`, `zeno`, `yazi`,
`unicode-bidi`, `unicode-linebreak`, `unicode-script`, `unicode-segmentation`,
`rangemap`, `slotmap`, `smol_str`, `self_cell`, `linebender_resource_handle`,
`memmap2`, `roxmltree`, and `rustc-hash`. All are permissive, all are covered by
the table above, and all are pure Rust: nothing here links Pango, Cairo, or
FreeType, so the static musl release is unaffected.

The `cargo deny check licenses` allow-list in [`deny.toml`](./deny.toml) encodes
exactly this set, so an unexpected license entering the graph fails CI.
