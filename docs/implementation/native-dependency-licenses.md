# Native dependency license inventory

This inventory covers native code that is built into or shipped with the
terminal prototype. It is part of the packaging evidence for issue #10.

| Component | Revision | License | Source and packaging note |
| --- | --- | --- | --- |
| Ghostty source used by `libghostty-vt-sys` | `521cf2c5d30bd48028ca61546c64e7913764073d` | MIT | [ghostty-org/ghostty](https://github.com/ghostty-org/ghostty); linked into the application from the vendored static library |
| `libghostty-vt-sys` | crate `0.2.1`, upstream revision `326be19c26da0a1bae0873101452b257cbf09d4b` | MIT OR Apache-2.0 | [uzaaft/libghostty-rs](https://github.com/uzaaft/libghostty-rs); raw FFI and native build script |
| `libghostty-vt` | crate `0.2.1`, upstream revision `326be19c26da0a1bae0873101452b257cbf09d4b` | MIT OR Apache-2.0 | [uzaaft/libghostty-rs](https://github.com/uzaaft/libghostty-rs); safe Rust API |

Stackhand now owns the small Crossterm input and Ratatui render adapters. The
former `ratatui-ghostty` dependency is not in the package. The Rust dependency
graph also carries the licenses of its transitive crates.
Generate the complete lockfile inventory with:

```sh
./scripts/license-inventory.sh > target/license-inventory.tsv
```

The packaged artifact includes this native inventory in
`share/licenses/native-dependencies.md`. It does not include Zig or a separate
Ghostty library. The exact Ghostty source and wrapper revisions are also
recorded in `packaging/build-metadata.toml`.

The Rust dependencies use an exact Git revision from upstream
[PR #115](https://github.com/uzaaft/libghostty-rs/pull/115), rather than the
published crates. This provides safe runtime byte/line scrollback limits and
native memory measurements. Both wrappers and their native source are pinned;
`Cargo.lock` and the packaging source stamp must agree with this inventory.
Ghostty's incremental compression remains caller-driven. Snapshot persistence
is not enabled. The pinned native build requires Zig 0.17.0.
