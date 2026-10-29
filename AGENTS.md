# hotswap

```stele
kind: system
purpose: hotswap — the crate inside each gigantic tool — `upgrade` (bump, drain, setup, kickstart, smoke, settle or roll back, red hold), `forget`, `agent_plist` (its 900 s upgrade agent); no binary or daemon.
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

| node  | kind      | purpose                                                                                                                                                                                  | unfold                                           |
| ----- | --------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------ |
| docs  | container | the design — `index.html` (GitHub Pages) is the only design source — release, install, mechanism, the tool contract, bifrost, next; `page.css` holds only what is hotswap's              | `stele unfold docs` · or read `docs/AGENTS.md`   |
| src   | container | crate source — `lib.rs` is the whole crate — `Manifest`; `upgrade` — mise lookup, red hold, bump, lock, drain → setup → kickstart → smoke, rollback, the record; `forget`; `agent_plist` | `stele unfold src` · or read `src/AGENTS.md`     |
| tests | container | end-to-end suite — `hotswap.rs` re-execs itself as tool `t` linking the crate, walking one temp $HOME through every branch with sh stubs for mise, launchctl and the steps               | `stele unfold tests` · or read `tests/AGENTS.md` |

## Indexes

All invariants: `.stele/index/invariants.md` · all hazards: `.stele/index/hazards.md`

## Engine

`stele` CLI available → `stele root | unfold <id> | invariants --touching <path> | hazards | nodes --kind <k>`. MCP: `stele serve`.
No engine → everything above is complete; nested AGENTS.md files carry the detail (nearest file wins).
<!-- @end -->
