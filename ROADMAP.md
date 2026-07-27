# Nexus-Rust roadmap

`CONFORMANCE.md` says *what must be true* — 57 register items and a three-tier release
gate. This document says *in what order, grouped how, and blocked on what*.

Read the two together. Register numbers (`#1`, `#42`) refer to `CONFORMANCE.md` section 6;
gate names (`A1`, `B`, `C`) to section 4.

---

## 1. The shape of the problem

Three facts about the dependency graph determine everything below.

**Almost nothing is observable yet.** Four Tier-1 items gate the entire register: the DLL
cannot load without a VC++ redistributable (`A1`), the overlay will not attach unless the
window class matches a literal (`#2`), the addon subsystem is not linked (`#1`), and
interception may not be process-wide (`#28`). Until those pass, no live test of anything
else means anything. **This is why the phases are strictly ordered rather than a flat
backlog.**

**One missing dependency edge accounts for ~20 items.** `nexus-runtime` depends on
`nexus-addon-backend` and none of `-ffi`/`-loader`/`-manager`/`-watch`/`-cleanup`/
`nexus-host`. A large fraction of the register reads `unreachable` — correct code, no
production caller. Those items are *cheap once #1 lands* and impossible before. Do not
work them individually first.

**Two tracks have long lead times and no code dependencies.** The asset programme (11
locale tables, icons, banners, typefaces, style presets — all independently authored) and
the operator homework (things only a real shipped C++ binary and a real GW2 install can
answer). Both block later phases. **Both start now, in parallel, before any engineering
phase completes.** Discovering the asset lead time during Phase 4 would stall the UI port
outright.

### Ordering constraints that are easy to get wrong

| Constraint | Why |
|---|---|
| `#20` texture-registry lifetime is decided **before** the addon API is wired | If records die with a render session, every addon-held `Texture*` is a use-after-free on the first resize. Retrofitting this after addons load means redoing the API surface |
| `#4` teardown order is fixed **as part of** wiring the addon lifecycle | It is the same code. Landing the lifecycle with the inverted order bakes in a crash vector, and `HANDOFF.md` actively prescribes the wrong order |
| `#21` patch mutex lands **in the same commit** as the self-updater | A self-updater without the cross-process lock corrupts multiboxers' installs on the first concurrent launch |
| `A4` offset oracle before `#3` is called done | The offsets pass today, but the expectations are author-computed. A shared mistake is the one bug class that silently corrupts third-party binaries |
| Asset programme before `#15`/`#17`/`#18` | The UI cannot ship with `((000xxx))` tokens or a substituted style |

---

## 2. Workstreams

Nine tracks. The letter is a stable handle for commits and checkpoints.

| WS | Track | Depends on | Parallel-safe |
|---|---|---|---|
| **A** | Load and attach | — | Serial, first |
| **B** | Static conformance gates | A1 only | Highly — mostly independent fixtures |
| **C** | Addon subsystem wiring | A, B/A4 | Serial — one subsystem, one owner |
| **D** | Addon-observable services | C | Highly — one item per service |
| **E** | Host UI | C, I | Moderately — page by page |
| **F** | Platform robustness | A | Highly |
| **G** | Release engineering | A1, A2 | Moderately |
| **H** | Operator homework | — (external) | Runs throughout |
| **I** | Asset programme | — (authoring) | Runs throughout |

---

## 3. Phases

### Phase 0 — Unblock (start everything else)

**Goal:** the DLL loads on a clean machine, and the two long-lead tracks are in motion.

| Item | WS | Size | Notes |
|---|---|---|---|
| ~~`A1` static CRT + import allowlist gate~~ | A | S | **Done.** `.cargo/config.toml` plus `xtask verify-imports`, wired into CI. 18 imports → 11, flip-tested against the preserved dynamic artifact |
| ~~`A2` `VERSIONINFO` + workspace version~~ | G | S | **Done.** Workspace at `0.1.0`; `build.rs` generates the resource from `CARGO_PKG_VERSION`, `xtask verify-version` asserts it in CI |
| Dispatch `H` operator homework | H | — | See section 4 — hand off immediately |
| Kick off `I` asset programme | I | — | See section 5 — longest lead item in the project |

**Trap scoped, then measured.** The workspace contains proc-macro crates (`syn`, `quote`,
`serde_derive`, `thiserror-impl`, `windows-implement`, `windows-interface`,
`zerocopy-derive`), and `crt-static` in `.cargo/config.toml` applies to host artifacts too
when no `--target` is passed. That predicts broken proc-macro builds. **On toolchain 1.97.1
it does not happen** — all of them build with the flag in effect, so no `--target` is needed
and the CI artifact paths are unchanged.

The underlying footgun is still real and is why the gate exists: **a build that does not pick
the flags up produces a dynamically-linked DLL with no other outward sign.** The gate is not
optional polish; it is the only thing that makes the fix stick. If a future toolchain does
break proc macros, the fix is `--target x86_64-pc-windows-msvc`, which moves the artifact to
`target/x86_64-pc-windows-msvc/release/` and requires updating four workflow steps.

**Exit:** reached. `xtask verify-imports` and `verify-version` both pass on a release build and
both run in CI; the image carries `FileVersion 0.1.0.0`. Phase 0's engineering half is closed;
`H` and `I` remain outstanding and are the long poles.

---

### Phase 1 — Static truth (no game, no install)

**Goal:** every byte-level and value-level contract is asserted by a test that would fail
if it broke. This is the highest value per unit of effort in the project, because a wrong
offset or a shifted enum corrupts third-party binaries *silently* instead of failing
diagnosably — and none of it needs a game.

| Item | WS | Size | Notes |
|---|---|---|---|
| ~~`A4` MSVC offset oracle~~ | B | M | **Done for revision 6.** `xtask verify-abi` has MSVC confirm 60 facts against the vendored MIT header, in CI. v1–v5 are not in the public header, so they stay author-computed — see the `CONFORMANCE.md` §4.A4 scope note |
| `A4` interior offsets for v2/v3/v4; `GetAddonDef` and ImGui-version asserts | B | S | Size-only today |
| `A5` export directory assertions | B | S | Partly exists |
| `A6` TLS callbacks + no-`Drop`-`thread_local` rule | B | S | The image has a TLS directory today, so this is live |
| `A3` resource inventory decisions | B/G | S | Assert the decision per id; empty is currently indistinguishable from unstarted |
| `B` value tables in full, including length | B | M | 177 bind ids, 100 scan codes both ways, 111 defaults, log levels, 63 HTTP phrases, enums, MD5 vectors |
| `B` golden round-trips for five file formats | B | M | Catches the three known byte deltas: XML `encoding`, 52-vs-53-space indent, LF vs CRLF |
| `B` bind-string parse table | B | S | Feeds `#25` |
| `B` path index: 41 leaves, **exactly eight** directories | B | S | Nine created today; two anchors in one build |
| `C` `should_consume` matrix | B | M | Pins `#14`'s three defects |
| `C` stub swap-chain vtable harness | B | M | Sentinel `HRESULT`s verbatim, native runs once, panic-safe |
| `C` stub chainload harness | B | M | `#33`: PATH discovery, self-module guard, per-export recursion |
| `C` recording `GameMessageSink` | B | S | Pins `#27`'s `WM_SYSKEYDOWN` divergence |
| `C` pre-export purity + `smoke-proxy` widening | B | S | Zero files/windows/threads before the first export call |

Several of these can only be *finalized* once `H` answers land (CRLF, mouse-button values,
ordinals). Build the harness with the question marked, not blocked — then close it when the
answer arrives.

**Exit:** gates A, B and C green in CI. Register rows for `#3`, `#31`, `#32`, `#33`, `#35`,
`#36` carry tests that would fail if the contract broke.

---

### Phase 2 — Make one addon load

**Goal:** a fixture DLL goes through the full lifecycle, and the reference's *forgiving*
behaviour is reproduced. This is the tier-2 core and it is mostly one subsystem, so it
wants one owner rather than a fan-out.

Settle **first**, before writing lifecycle code:

1. `#20` texture registry lifetime — do ABI records outlive render sessions?
2. `#4` teardown order — native unload first, API fully live, sweep after.
3. `#54` load order — filesystem-inline vs sorted-batch, recorded as a decision with a test.

Then, in order:

| Item | WS | Size | Notes |
|---|---|---|---|
| `#1` link the addon subsystem | C | M | The dependency edge plus a real construction site. Unblocks ~20 items |
| `#4` unload-export ordering | C | M | Contradicts `HANDOFF.md:654-665` |
| `#5` caller attribution serves unattributable callers | C | M | Serve with a null owner; reserve rejection for owner-scoped cleanup, never read-only queries |
| `#6` metadata validation → lossy + truncate | C | S | Keep the bounded string reads; relax definition-in-image to bounded readability |
| `#7` discovery degrades per entry, follows symlinks | C | S | One locked file must not disable every addon |
| `#9` string getters never return null | C | S | Ceiling degrades to a leaked-but-valid allocation |
| `#41` dependent-DLL search order | F | S | Addons ship dependencies next to `Gw2-64.exe` |
| `#42` ABI string encoding decision | C | M | ACP bytes for parity, or UTF-8 with a documented break. Invisible on ASCII machines |
| `#47` reentrant (de)registration safety | C | M | Snapshot-plus-liveness; `CallbackSlot::deactivate` exists |
| `#48` ceilings unreachable or loud | C | S | The reference has none |

**Exit:** an integration test drives a fixture through inspect → activate →
`request_unload` → drain → native unload → finish, with the API live throughout the unload
export. Five hostile-metadata fixtures load.

---

### Phase 3 — Addon-observable services

**Goal:** everything an addon can see, once it can load. Highly parallel — one item per
service, minimal shared code.

| Item | WS | Size | Notes |
|---|---|---|---|
| `#12` seven host event identifiers | D | M | Pin each literal as a `nexus-abi` constant with a string-equality test. Inter-addon discovery via LOADED/UNLOADED is a published pattern |
| `#10` `NexusLink` font pointers never zero | D | S | Null-deref inside third-party code today |
| `#8` volatile / 350-build auto-disable | D | M | Turns a patch-day crash into a controlled refusal |
| `#19` `AddonConfig.json` construction site + `PathKey` | D | S | Format already golden-tested |
| `#37` DataLink share kind, identifier acceptance, cadences | D | M | Addon `Share` must be process-local heap, not a named mapping |
| `#11` Mumble identity truncation | D | S | Do not reproduce the array overflow |
| `#24` Unofficial Extras bind tracking | D | M | Without it every addon-issued bind presses the *default* key |
| `#25` `KeyNameResolver` over the OS layout table | D | M | Also fixes the Binds page display |
| `#27` game-bind message tuples | D | S | Revert to the ordinary message with bit 29 — rule 2.2 |
| `#31` log levels 0 and 6 pass through | D | XS | One-line; silent loss of an addon's log line |
| `#34` hot reload + polling backstop | D | M | Event-only watchers miss network and virtualised paths |
| `#53` texture decoder corpus diff | D | M | Silent non-resolution looks like invisible buttons |
| `#39` assert the do-not-implement items | D | XS | Flag `1<<3` must still get non-null context and allocators |

**Exit:** Tier 2 of the release gate is green.

---

### Phase 4 — Host UI

**Goal:** a swapper does not notice. The largest single chunk, and **gated on the asset
programme**, which is why `I` starts in Phase 0.

| Item | WS | Size | Notes |
|---|---|---|---|
| `#18` DPI scaling | E | S | `update_dpi` is correct and unit-tested with **no production caller**. Independent of the rest of the UI and visible to every user on a 125%/150% display — do this first |
| `#18` saved `ImGuiStyle` applied | E | S | Reject non-1044-byte payloads without touching the live style |
| `#16` 17 missing settings keys, observed live | E | M | Subscribe, don't read once, or Options toggles appear broken until restart |
| `#15` EULA gate reading the **existing** `AcceptEULA` key | E | S | A differently-named key re-prompts on upgrade — exactly the visible break to avoid |
| `#15` `Nexus` window + six pages | E | L | Addons, Options, Binds, Log, Debug, About |
| `#15` Quick Access + Alerts | E | M | State exists, unreachable |
| `#17` locale tables shipped | E | M | Blocked on `I`. Identifier numbers are interface |
| `#43` clipboard | E | S | Pasting an API key is mainstream; likely one build define |
| `#45` atlas glyph coverage | E | M | Blocked on `I`. Reported as "the Rust version broke addon X" |
| `#55` CJK IME | E | S | Korean and Chinese are shipped locales |
| `#30` escape-close uses the live ImGui stack + honours the setting | E | S | |
| `#14` input consumption fixed | E/F | M | Gate C already pins it |

**Exit:** Tier 3 of the release gate is green.

---

### Phase 5 — Platform robustness

Parallel with 3 and 4; depends only on Phase 0.

| Item | WS | Size | Notes |
|---|---|---|---|
| `#46` never louder on screen | F | M | No `MessageBox` anywhere; wrap every `extern "C"` entry in `catch_unwind`; never be why the game exits |
| `#13` log banner, shutdown records, `-mumble` naming, `-ggconsole` | F | M | The only identity surface checkable without the UI, and the first artifact of every bug report. A complete `ConsoleLogSink` already sits unused |
| `#56` log write-sharing denied | F | S | A shredded log destroys every multiboxer's bug report |
| `#23` multibox mutex released | F | S | Mainstream GW2 workflow |
| `#29` crash log | F | M | Split the line formatter (unit-testable, no crash) from the handler. Test only in a **sacrificial child process** |
| `#49` TLS / loader-lock discipline | F | S | Gate A6 |
| `#50` supported-OS floor decision | F | S | `bcryptprimitives` implies Win10; the reference floor is Win8 |
| `#2` window-class admission fallback | A | M | Keep the class as a preference signal, never a gate |
| `#28` process-wide interception | A | L | **Blocked on `H`** — a real trace decides whether this is needed. Do not build the global fallback speculatively |
| `#32` one path anchor; write-through binds | F | M | Shutdown-only saves lose a session's binds to any crash |

---

### Phase 6 — Release engineering

| Item | WS | Size | Notes |
|---|---|---|---|
| `#22` notices generated and diffed in CI | G | M | 4 entries against 129 lockfile packages today. Licence-blocking for a distributed binary |
| `#22` tag-triggered release: stamp, checksum, notices, sign | G | M | Unsigned changes AV/SmartScreen behaviour versus the C++ build |
| `#21` self-updater **plus** patch mutex, one commit | G | L | Keep the endpoint pointed at this project — rule 2.4 |
| `#21` addon update cadence + provider selection | G | M | 3600s/300s with an injected clock |
| `#52` network trust and proxy model | G | M | Invisible on a dev machine, total on a corporate network |
| `#51` performance budget | G | M | Measure both builds, same machine and scene; gate CI on a stated margin. Without a number "it feels worse" is unfalsifiable |

---

### Phase 7 — Live validation

Only meaningful once Tiers 1–3 are green. Everything in `CONFORMANCE.md` section 7 that
needs a real install, plus:

- Real third-party addons across revisions v1–v6. An ImGui version or allocator mismatch
  shows up as **corruption, not a clean failure**, so no unit test substitutes.
- Coexistence with ArcDPS, Steam, Discord, NVIDIA and RTSS **in both install orders**.
- Windowed / fullscreen / borderless, resolution, monitor, DPI change, alt-tab.
- Two clients for multibox.
- Screenshots at fixed resolution/UI-size/DPI for layout comparison.

---

## 4. WS-H — operator homework (dispatch now)

None of this is answerable from source. Each unanswered item leaves a register row
`unknown` or a gate provisional.

**Needs a real shipped C++ Nexus binary to read:**

1. `dumpbin /exports` on it — the reference's actual ordinals. Plus `dumpbin /imports` on
   `Gw2-64.exe` and every common chainload DLL. **This decides whether `#35` matters at
   all**; if everything imports by name, the current ordinal pin is fine as a
   self-consistency gate.
2. `EMouseButtons` numeric values (`#26`) — or read them from the public MIT `Nexus.h`.
   A shifted value rebinds the wrong physical button and looks like user error.
3. Hex-dump `Settings.json`, `AddonConfig.json`, `InputBinds.json`, `Nexus.log` to settle
   CRLF, and `GameBinds.xml` for the declaration.
4. A **tagged** release, to settle `#57` and the `NexusLink` Width/Height question — HEAD
   is mid-refactor and reproducing it renames the game executable.
5. The four version components (`src/Version.h` is absent from this worktree).
6. Whether the released build implements `RequestUpdate` (HEAD has it commented out).

**Needs a real GW2 install to run:**

7. The actual top-level window class across DX11/legacy and locales (`#2`).
8. An API trace of a real launch recording every factory/swap-chain creation and its caller
   module — **decides whether `#28` needs building at all**, which is an L-sized item.
9. Whether the game accepts `WM_SYSKEYDOWN` equivalently to bit 29 (`#27`).
10. Whether `Foo.DLL` loads (`#7` case sensitivity).

Items 1 and 8 are the highest leverage: each can delete a large piece of work.

**Decided, no longer blocking:** branding is removed entirely and the product is `Tessera`
(`CONFORMANCE.md` §2.5). The two broken reference paths — the no-op uninstall and the
throwing update check — are implemented properly rather than reproduced (§2.2).

**Still open:** where the in-game addon library's catalogue comes from, now that the
vendor's URLs are gone. This is a hosting question, not a branding one, and the feature
should not be silently dropped for want of an answer. Needed before Phase 6.

---

## 5. WS-I — asset programme (longest lead time)

Every one of these is proprietary in the reference and must be **independently authored**.
None can be copied. `res/Locales` is an unpopulated submodule in the C++ tree anyway.

| Asset | Blocks | Note |
|---|---|---|
| English strings for every `((000xxx))` identifier | `#17`, all of Phase 4 | The identifier *numbers* are interface — addons call `Translate` with them — so they must be preserved exactly. The English text is ours to write |
| 10 further locale tables | `#17` | de, fr, es, cn, kr, br, cz, it, pl, ru |
| Three typefaces with Latin-Extended + Cyrillic + CJK coverage | `#45` | Without coverage, a Russian or Chinese addon's UI renders as blank boxes |
| Quick Access icons (incl. seasonal), page icons, tier icons, banners | `#15`, `#44` | |
| Two built-in style presets | `#18`, `#38` | The reference's are base64 proprietary style data |
| `Tessera` wordmark, application icon, About-page content | `#15`, `A2` | Replaces the removed vendor identity (`CONFORMANCE.md` §2.5). Small, but it gates the About page and the stamped `VERSIONINFO` |

**Start with the English string table.** It is on the critical path for the entire UI phase,
it is the largest single authoring job, and translation cannot begin until it exists.

---

## 6. What a checkpoint is

One iteration of the loop produces exactly one checkpoint:

1. One commit, scoped to one register item or one gate.
2. `cargo fmt --check`, `cargo clippy` (workspace lints: `all`/`undocumented_unsafe_blocks`/
   `unwrap_used` denied), and the full test suite green — **exit codes captured directly**,
   never through a pipe.
3. CI verified green by conclusion, not by absence of failure.
4. The `CONFORMANCE.md` register row updated to its new status, with the test that now
   defends it.
5. A test that would **fail** if the contract broke. Per section 8 of `CONFORMANCE.md`: a
   passing check proves nothing until something proves it could have failed. The
   counterfactual perturbs the subject, never the measurement — compare
   `(exit_code, passed + failed)` and require the test count identical and non-zero across
   both worlds.

No silent scope reduction. If an item turns out bigger than its size here, split it and say
so in the register rather than landing half of it as done.

## 7. Sequencing summary

```
Phase 0  ──┬─────────────────────────────────────────────────────►  (A1, A2)
           │
   WS-H  ──┼──────────────────── runs throughout ─────────────────►  (answers gate 1,8,26,35,57)
   WS-I  ──┼──────────────────── runs throughout ─────────────────►  (gates Phase 4)
           │
Phase 1  ──┴──►  static gates A/B/C          ─┐
                                              ├──►  Phase 2  ──►  Phase 3  ──┐
Phase 5  ─────►  platform robustness         ─┘                              ├──►  Phase 7
                                                    Phase 4  ────────────────┤
                                                    Phase 6  ────────────────┘
```

Phase 1 and Phase 5 can run alongside each other from the start. Phase 2 is the narrow
waist — it is serial, single-owner, and everything in Phase 3 and 4 waits on it. Phase 6
can start any time after Phase 0 but cannot finish before the artifact is worth releasing.

**Immediate next three checkpoints:** `A1` (static CRT + import gate), `A2` (version
stamping), then `A4` (the offset oracle) while `H` and `I` are in flight.
