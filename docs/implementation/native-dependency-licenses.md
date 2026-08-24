# Native dependency license inventory

This inventory covers native code that is built into or shipped with the
terminal prototype. It is part of the packaging evidence for issue #10.

| Component | Revision | License | Source and packaging note |
| --- | --- | --- | --- |
| Ghostty source used by `libghostty-vt-sys` | `bebca84668947bfc92b9a30ed58712e1c34eee1d` | MIT | [ghostty-org/ghostty](https://github.com/ghostty-org/ghostty); built as the bundled `libghostty-vt` library |
| `libghostty-vt-sys` | crate `0.1.1`, upstream revision `86338c17d1926c7e863f3c94d70202d3d1d60172` | MIT OR Apache-2.0 | [uzaaft/libghostty-rs](https://github.com/uzaaft/libghostty-rs); raw FFI and native build script |
| `libghostty-vt` | crate `0.1.1`, upstream revision `86338c17d1926c7e863f3c94d70202d3d1d60172` | MIT OR Apache-2.0 | [uzaaft/libghostty-rs](https://github.com/uzaaft/libghostty-rs); safe Rust API |
| `ratatui-ghostty` | crate `0.2.0`, upstream revision `be6d2d89105ca8cb867e7963888f82ba58b52316` | MIT | [jint/ratatui-ghostty](https://codeberg.org/jint/ratatui-ghostty); Ratatui adapter |

The Rust dependency graph also carries the licenses of its transitive crates.
Generate the complete lockfile inventory with:

```sh
./scripts/license-inventory.sh > target/license-inventory.tsv
```

The packaged artifact includes this native inventory in
`share/licenses/native-dependencies.md`. It does not include Zig or a separate
Ghostty installation. The exact Ghostty source and wrapper revisions are also
recorded in `packaging/build-metadata.toml`.
