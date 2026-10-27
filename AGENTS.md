# hotswap

```stele
kind: system
# purpose:            # ≤200-char scent — fill it in (never auto-generated, §7)
# commands: {}
# invariants: []
# hazards: []
# edges:
#   depends: []
#   decided_by: [§ 0001]
# budget:
```

```stele
kind: system
purpose: hotswap — the swap moment for a mise-installed gigantic tool (drain, kickstart its launchd agents, setup, smoke, then settle or roll back through `mise use -g`); one Rust binary, no daemon, no download code.
commands:
  check: mise run check
  cover: cargo llvm-cov --fail-under-lines 100
  lint: cargo clippy --all-targets -- -D warnings && cargo fmt --check
  test: cargo test
```

The design is `docs/index.html` — the only design source.

<!-- @stele --><!-- @end -->
