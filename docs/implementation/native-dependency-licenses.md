# Native dependency license inventory

This inventory covers native code that is built into or shipped with the
terminal prototype. It is part of the packaging evidence for issue #10.

| Component | Revision | License | Source and packaging note |
| --- | --- | --- | --- |
| Ghostty `vt-1.0.1` source used by `libghostty-vt-sys` | `108cacb17ed8990840cd14abfce43d8ed8503ca1` | MIT | [ghostty-org/ghostty](https://github.com/ghostty-org/ghostty); linked into the application from the vendored static library |
| `libghostty-vt-sys` | crate `0.2.1`, upstream revision `cd4d497e8c698b27054a677ce19d1bd1a1a4d8bf` | MIT OR Apache-2.0 | [uzaaft/libghostty-rs](https://github.com/uzaaft/libghostty-rs); raw FFI and native build script |
| `libghostty-vt` | crate `0.2.1`, upstream revision `cd4d497e8c698b27054a677ce19d1bd1a1a4d8bf` | MIT OR Apache-2.0 | [uzaaft/libghostty-rs](https://github.com/uzaaft/libghostty-rs); safe Rust API |

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

The Rust dependencies use an exact upstream Git revision of the third-party
`libghostty-rs` bridge. This revision embeds Ghostty `vt-1.0.1` and retains
runtime byte/line scrollback limits, native memory measurements, and
caller-driven incremental compression. Both wrapper crates still report version
`0.2.1`. Their Git revision and native source are pinned; `Cargo.lock` and the
packaging source stamp must agree with this inventory. Snapshot persistence is
not enabled. The bridge requires Rust 1.96, and the native build requires Zig
0.17.0.

## Host decoder snapshot

`src/host/decoder/` contains the production Unix decoder from Crossterm 0.29.0,
upstream commit `36d95b26a26e64b0f8c12edfe11f410a6d56a812`,
`src/event/sys/unix/parse.rs`. The import splits the unchanged source at function
boundaries and excludes upstream tests. The full MIT notice is retained in
`src/host/decoder/LICENSE.crossterm`.

The local adapter changes only the event representation, the raw-mode assumption
(cooked newline branches are removed), and bracketed-paste opener recognition.
Framing and query replies remain private
inside `host/`. Malformed zero mouse and cursor coordinates use checked subtraction
instead of upstream's unsigned subtraction, so invalid input cannot panic.
`io::Error::other` and rustfmt account for mechanical differences. The rest of the
key, Kitty, mouse, focus, and UTF-8 grammar follows the pinned source. Updates to
Crossterm require comparing this snapshot with upstream and rerunning protocol and
framing compatibility checks. The snapshot can be removed when an upstream public
incremental decoder provides these bounds without a second global event source.
