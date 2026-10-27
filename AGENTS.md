# hotswap

```stele
kind: system
purpose: hotswap — the swap moment for a mise-installed gigantic tool (drain, kickstart its launchd agents, setup, smoke, then settle or roll back through `mise use -g`); one binary, no daemon.
commands:
  check: mise run check
  cover: mise run cover
  lint: cargo clippy --all-targets -- -D warnings && cargo fmt --check
  test: cargo test
```

The design is `docs/index.html` — the only design source.

<!-- @stele -->

## Hazards (0 active)


## Map

| node  | kind      | purpose                                                                                                                                                                              | unfold                                           |
| ----- | --------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------ |
| docs  | container | the design — `index.html` (GitHub Pages) is the only design source — release contract, mechanism, `<tool> setup`, hotswap.toml, next shifts; `page.css` holds only what is hotswap's | `stele unfold docs` · or read `docs/AGENTS.md`   |
| src   | container | crate source — `main.rs` is the whole binary — arg parse, `mise ls` lookup, hotswap.toml, the drain → kickstart → setup → smoke run, rollback, the state record                      | `stele unfold src` · or read `src/AGENTS.md`     |
| tests | container | end-to-end suite — `hotswap.rs` walks one temp $HOME through every branch with sh stubs for mise, launchctl and the tool's commands, asserting exit code, output and stub argv       | `stele unfold tests` · or read `tests/AGENTS.md` |

## Indexes

All invariants: `.stele/index/invariants.md` · all hazards: `.stele/index/hazards.md`

## Engine

`stele` CLI available → `stele root | unfold <id> | invariants --touching <path> | hazards | nodes --kind <k>`. MCP: `stele serve`.
No engine → everything above is complete; nested AGENTS.md files carry the detail (nearest file wins).
<!-- @end -->
