# Third-party notices

SS Writer is licensed under the MIT License (see `LICENSE`). It is built from open-source
components; each one stays under its own license, which is stated in the package it comes from
(`Cargo.toml` `license` field and the `LICENSE*` files of the crate on crates.io).

## Components shipped inside the repository (`third_party/`)

| Component | License | Notes |
| --- | --- | --- |
| mermaid-rs-renderer | MIT | Vendored with local fixes (`third_party/mermaid-rs-renderer/FIXES.md`). |
| mitex-spec | Apache-2.0 | Vendored with one patch for Typst 0.15 (`third_party/mitex-spec`). |

## Fonts

| Font | License |
| --- | --- |
| Cascadia Mono | SIL Open Font License 1.1 |
| New Computer Modern (math and text) | GUST Font License 1.0 (full text in `assets/fonts/LICENSE-NewCM.txt`) |

## Rust dependencies

`THIRD-PARTY-NOTICES.txt`, installed next to `SSWriter.exe` (and included in the portable
zip), lists every Rust crate compiled into SS Writer with its exact version, SPDX license
expression, repository and the full text of its license files. The same list, with versions,
is in `Cargo.lock` at the root of the source repository (https://github.com/isolmaz/ss-writer).
