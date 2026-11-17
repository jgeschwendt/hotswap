# hotswap

```stele
kind: system
purpose: hotswap — the crate in each gigantic tool — `ensure`, `install`, `upgrade` on packslip and its own version dirs (drain, repoint, swap, smoke, settle or roll back), `forget`, `agent_plist`, `root`
commands:
  check: mise run check
  cover: mise run cover
  e2e: cargo test -- --ignored
  lint: cargo clippy --all-targets -- -D warnings && cargo fmt --check
  test: cargo test
```

The design is `docs/index.html` — the only design source.

<!-- @stele -->

## Hazards (0 active)


## Map

| node  | kind      | purpose                                                                                                                                                                           | unfold                                           |
| ----- | --------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------ |
| docs  | container | the design — `index.html` (GitHub Pages) is the only design source — release, install, mechanism, the tool contract, bifrost, next; `page.css` holds only what is hotswap's       | `stele unfold docs` · or read `docs/AGENTS.md`   |
| src   | container | crate source — `lib.rs` is the whole crate — packslip pin and bootstrap, `Manifest`, `ensure` (layout, migration off mise), `install`, `upgrade`, `forget`, `agent_plist`, `root` | `stele unfold src` · or read `src/AGENTS.md`     |
| tests | container | end-to-end suite — `hotswap.rs` re-execs itself as tool `t`, walking a temp $HOME through every branch on sh stubs and a fake mise; `--ignored` runs real packslip and grove      | `stele unfold tests` · or read `tests/AGENTS.md` |

## Indexes

All invariants: `.stele/index/invariants.md` · all hazards: `.stele/index/hazards.md`

## Engine

`stele` CLI available → `stele root | unfold <id> | invariants --touching <path> | hazards | nodes --kind <k>`. MCP: `stele serve`.
No engine → everything above is complete; nested AGENTS.md files carry the detail (nearest file wins).
<!-- @end -->
