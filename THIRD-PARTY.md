# Third-party libraries

Direct dependencies of this app. The complete transitive list (with exact versions) is pinned in [`Cargo.lock`](Cargo.lock).

## Rust crates

| Library | Version | License | Used for |
|---|---|---|---|
| [image](https://crates.io/crates/image) | 0.25 | MIT OR Apache-2.0 | Pure-Rust image decoding (PNG, JPEG, GIF, BMP decoders only) and resizing |
| [jpeg-decoder](https://crates.io/crates/jpeg-decoder) | 0.3 | MIT OR Apache-2.0 | IDCT-scaled JPEG decode for over-budget images |
| [png](https://crates.io/crates/png) | 0.18 | MIT OR Apache-2.0 | Row-streamed PNG decode for over-budget images |
| [log](https://crates.io/crates/log) | 0.4 | MIT OR Apache-2.0 | Logging facade |

## Foundation SDK / KeyOS platform

Provided by the installed Foundation SDK (path dependencies, not crates.io):

| Component | Role |
|---|---|
| `server` (KeyOS) | App runtime, KeyOS service messaging, filesystem API |
| `xous-api-log` | Log output to the KeyOS log server |
| `slint-keyos-platform` (+ `-build`) | [Slint](https://slint.dev) UI runtime and build integration for KeyOS |
| `foundation-themes` | Design tokens and light/dark theming |

The Slint UI toolkit itself is licensed under GPL-3.0-only OR the Slint Royalty-free / commercial licenses; this app is GPL-3.0-or-later.
