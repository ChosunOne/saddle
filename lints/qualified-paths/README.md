# qualified_paths

A Dylint library holding Saddle's source style checks. `qualified_paths` (D148) catches a path
is imported where its module keeps the imports, never written inline.

`absolute_paths` allows `crate::`, `self::` and `super::` because they start with a keyword, and
`items_after_statements` only looks after statements, so both leave the shape this repository
forbids unmentioned.

```rust
fn purpose() -> Purpose {
    crate::tui::login::Purpose::Model // flagged
}
```

```rust
use crate::tui::login::Purpose;

fn purpose() -> Purpose {
    Purpose::Model
}
```

## Running it

The workspace declares the library in `[workspace.metadata.dylint]`, so from the repository root:

```sh
cargo dylint --all
```

It needs `cargo-dylint` and `dylint-link` (`cargo install cargo-dylint dylint-link`), and the
nightly `rust-toolchain.toml` here pins, with `rustc-dev` and `llvm-tools-preview`. Those are the
lint's requirements, not the project's: `cargo build`, `cargo clippy` and `cargo nextest` stay on
stable.

The lint reports through the compiler rather than a source scan, so it sees types, patterns and
macro expansions the way rustc does, and a `use` statement is never reported.

`no_source_comments` (D198) denies line, block, and Rustdoc comments in Rust files
loaded by the compiler. It uses the Rust lexer, so comment markers in strings and
a file's shebang are not reported. Keep API and design prose in Markdown.
