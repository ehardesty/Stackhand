# Native dependency license inventory

This inventory covers native code that is built into or shipped with the
terminal prototype. It is part of the packaging evidence for issue #10.

| Component | Revision | License | Source and packaging note |
| --- | --- | --- | --- |
| Ghostty source used by `libghostty-vt-sys` | `a887df42c56f6de86c0fe6da9c4eeca37931e083` | MIT | [ghostty-org/ghostty](https://github.com/ghostty-org/ghostty); linked into the application from the vendored static library |
| `libghostty-vt-sys` | crate `0.2.1`, upstream revision `46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0` | MIT OR Apache-2.0 | [uzaaft/libghostty-rs](https://github.com/uzaaft/libghostty-rs); raw FFI and native build script |
| `libghostty-vt` | crate `0.2.1`, upstream revision `46a9d2ac941ed600cf43c5e6299c8dfd1d3a1ef0` | MIT OR Apache-2.0 | [uzaaft/libghostty-rs](https://github.com/uzaaft/libghostty-rs); safe Rust API |

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
