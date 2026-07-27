# Nexus-Rust drop-in conformance standard

This document defines *done* for this project and enumerates every known gap between
this Rust host and the C++ reference. It is the queue: work items come from the
register in section 6, and nothing ships until the release gate in section 3 is green.

`HANDOFF.md` tells you how to build and where the traps are. This file tells you what
the result has to *do*. Where the two disagree, this file wins — section 10 lists the
specific places `HANDOFF.md` is now known to be wrong.

---

## 1. The standard

> A user replaces the C++ `d3d11.dll` with this one, launches Guild Wars 2, and notices
> nothing. Their addons load. Their binds work. Their settings and window positions are
> where they left them. Their log looks the same. Then they replace it back, and that
> works too.

Not "equivalent architecture". Not "the same features". *Nothing changed that they can
see.*

### Priority order

1. **All functionality respected.** Every observable behavior the reference has.
2. **Addon compatibility maintained.** Binaries compiled years ago against revisions
   v1–v6 keep working, unrecompiled.
3. **Optimizations, speedups, and fixes** — a bonus, and welcome, subject to rule 2.2.

## 2. The four rules

### 2.1 Frozen contract, free internals

The contract is everything an addon or a user can observe: exported symbols, struct byte
layouts, function-pointer table offsets, string identifiers, numeric enum values, file
formats and their bytes, event names and payloads, window titles, ordering guarantees,
and the timing an addon can detect.

Internals are free. Idiomatic Rust, better data structures, different threading, tighter
locking — all encouraged. Do not mirror the reference's class hierarchy, control flow, or
naming to feel safe. Mirror its *observable edge*.

### 2.2 An observable bug is part of the contract

This is the rule that costs people their weekend. If a defect is detectable from outside,
addons have adapted to it, and "fixing" it is a breaking change.

Fix freely where undetectable. Where a defect is observable, **preserve it bug-for-bug and
record it here as an intentional requirement** — never silently correct it. Precedents
already in this codebase: the legacy `WM_USER` passthrough offset `7997`; "matching the
observable legacy order" in settings notification; the sub-1024×768 minimum-resolution
scaling rule; and `PROVENANCE.md` deliberately retaining `EGameBinds` numeric value `10`.

Three live examples of the trap, all currently "improvements" that break compatibility:

- Promoting an Alt-modified game bind to `WM_SYSKEYDOWN` (register #27). The reference
  sends the *ordinary* message with lParam bit 29. Every other addon's WndProc sees this.
- Rejecting log levels `0` and `6` from addons (#31). The reference emits the line with
  severity `(null)`. Rejecting it silently drops an addon's log output.
- Implementing definition flag `1<<3` ("can create own ImGui context") by passing null
  context/allocators (#39). The reference ignores the flag and always passes non-null.
  Honouring it would null-deref every addon that sets it.

### 2.3 Do not reproduce the reference's in-progress refactor

The C++ tree at HEAD contains a half-finished change whose artifacts look like contract
and are not. `src/Core/Index/Index.cpp:73` anchors the path index with
`CreateIndex(GetModuleHandle(NULL))`, so `EPath::NexusDLL` is **the game executable**.
Consequences at HEAD: the startup banner's `Module:` line prints `Gw2-64.exe`;
`NexusDLL_Old`/`_Update` are `Gw2-64.exe.old`/`.update`; and the ArcDPS "is the system
d3d11 me?" comparison can never be true.

**Faithfully reproducing this would make the Rust self-updater rename the game
executable.** Do not port the `.old`/`.update` naming, the `Module:` banner value, or the
ArcDPS self-comparison from HEAD. Read a *tagged* release or dump a shipped install
first. Keep the DLL anchor for the module path and the executable anchor for the
directory index (which is the reference's observable layout).

The same caution applies to `NexusLink` `Width`/`Height`: at HEAD nothing seeds them
before the first resize, so an addon reading them at load legitimately observes `0`. That
is probably refactor damage, not contract. Until a tagged release settles it, implement
the safe superset — published every frame **and** pre-written at resize before the event —
and never publish `0` once a frame has rendered.

### 2.4 Required divergences

Some differences are mandatory. Reverting them in the name of compatibility causes harm.

| Divergence | Why it must stay |
|---|---|
| `crates/nexus-network/src/update.rs:18-22` points self-update at **this project's** releases | Repointing it at the legacy endpoint makes the Rust host overwrite itself with the C++ DLL and silently undo the swap |
| All icons, banners, style presets, typefaces and 11 locale tables independently authored | The reference's are proprietary; `res/Locales` is an unpopulated submodule there anyway |
| Identity strings in `VERSIONINFO` (CompanyName, LegalCopyright, ProductName) are ours | Reproduce the field *names* and layout, never the reference's values |
| Mumble identity name truncated at a char boundary rather than overflowing a 20-byte array | Do not reproduce a memory-safety defect; truncation is correct for both trees (#11) |

Low-risk deliberate divergences worth recording rather than reverting: the network cache
key, `User-Agent: Nexus/1.0`, and the finite 15/15/30/30s WinHTTP timeouts.

---

## 3. Release gate

Three tiers, strictly ordered. Tier 1 items are prerequisites for *observing* anything
else; until they pass, every other item in this document is untestable.

### Tier 1 — Load and attach

| # | Gate | Status |
|---|---|---|
| 1.1 | Static CRT: the DLL loads with no VC++ redistributable installed | **FAILS** (verified, §4.A1) |
| 1.2 | Overlay attaches without depending on the window-class literal | **FAILS** (#2) |
| 1.3 | `nexus-runtime` actually links the addon subsystem so one addon can load | **FAILS** (#1) |
| 1.4 | Process-wide DXGI interception, or a real GW2 trace proving per-object suffices | **UNPROVEN** (#28) |

### Tier 2 — Addon contract

An addon that works on C++ works unchanged. Requires: #4 unload-export ordering (the API
must stay live across `invoke_native_unload`); #5 caller attribution serving
unattributable callers instead of rejecting them; #6 metadata validation relaxed to
lossy+truncate; #7 discovery degrading per-entry and following symlinks; #12 the seven
host event names with correct payloads and ordering; #10 `NexusLink` font pointers never
published as `0`; #3 API table offsets cross-checked against an MSVC-derived table; #8
the volatile/350-build rule; #20 texture registry lifetime settled.

### Tier 3 — User-visible host

A swapper must not notice. Requires: #15 the host UI (Nexus window, six pages, seven bind
identifiers, EULA gate reading the *existing* `AcceptEULA` key, Quick Access, Alerts); #16
the 17 missing settings keys, observed live; #17 locale tables so no `((000xxx))` tokens
appear; #18 saved `ImGuiStyle` applied and DPI scaling working; #13 the log banner and
shutdown records with per-`-mumble` naming; #14 input consumption fixed; #25 key names
from the OS layout table; #24 Unofficial Extras bind tracking; #23 multibox mutex
released; #19 `AddonConfig.json` read and written; #22 a versioned, notice-complete
release artifact the self-updater can point at.

---

## 4. Cheap gates — build these first

No game, no install, no operator. These are where the value per unit of effort is highest,
because a wrong byte layout or a shifted enum silently corrupts third-party binaries
instead of failing diagnosably.

### A. PE and byte layout

**A1. Import allowlist.** Extend `xtask` — `read_pe_exports` at `xtask/src/main.rs:141`
already has the whole PE parser (`rva_to_offset`, `read_sections`, `read_u32`); the import
directory is data-directory index 1, so this is a small addition, not new machinery.

Verified against `target/release/d3d11.dll` by parsing the import directory (19 modules):

- **Reject:** `VCRUNTIME140.dll` and six `api-ms-win-crt-*` modules are **present today**.
  A machine without the VC++ redistributable cannot map the DLL, so the game's static
  import of `d3d11.dll` fails and *the game does not start at all*. Fix with
  `-C target-feature=+crt-static` (reference: `Nexus.vcxproj:545` links the static CRT).
  Reject `vcruntime*`, `msvcp*`, `ucrtbase*`, `api-ms-win-crt-*`.
- **Permit:** `d3dcompiler_47.dll` and `xinput1_4.dll` — the reference links these too, via
  ImGui's own pragmas. A naive denylist over-rejects here.
- **Decide:** `bcryptprimitives.dll` puts the floor at Windows 10. The reference's
  `PathCch`/`shcore` put it at Windows 8. Record the supported-OS floor as a decision; on
  an unsupported OS the failure is indistinguishable from the CRT bug.
- **Match module names case-insensitively** — the current build imports both `KERNEL32.dll`
  and `kernel32.dll`.

**A2. `VERSIONINFO` present with `FileVersion` == release version.** Verified absent: the
resource data directory is **size 0**, and `VS_VERSION_INFO`/`StringFileInfo` do not appear
in the image. Workspace version is `0.0.0` (`Cargo.toml:44`), so there is nothing to stamp
yet. Not cosmetic — a third-party GW2 addon manager can treat an unversioned file as
unknown and overwrite the swapped DLL.

**A3. Resource inventory.** The reference ships fonts, icons, banners, locale tables and
the third-party notice blob *inside* the image (`res/Nexus.rc`, `res/ResConst.h`), so one
file drop is a complete install. Assert the decision per resource id — an empty resource
directory is currently indistinguishable from "not started".

**A4. The single highest-value gate: an external offset oracle.** API table offsets for
v1–v6 currently pass, and two independent surveys hand-checked them against the headers
(#3). The residual risk is that expectations are *author-computed*, so a shared mistake
passes. Close it: emit the offset table from Rust and diff it in CI against `offsetof`
output from one MSVC translation unit built from the public MIT `RaidcoreGG/Nexus-API`
header. Also add interior offset assertions for v2/v3/v4 (size-only today), and
compile-time asserts on the literal `"GetAddonDef"` and `imgui_version_num() == 18000`.

**A5. Export directory:** name == `d3d11.dll`, 20 non-forwarder exports, no duplicate
ordinals, `AddressOfEntryPoint` not reaching Nexus code.

**A6. TLS callbacks.** The image *has* a TLS directory (size 40), so this is live, not
theoretical: assert `AddressOfCallBacks` is empty or holds only the known CRT callback,
and forbid any `thread_local` with a non-trivial `Drop` in the DLL — a `Drop` taking a
lock during `DLL_THREAD_DETACH` is a loader-lock deadlock in a live game.

### B. Value tables and parser round-trips

Fixtures only. Assert each table **in full, including length** — a set-only or length-only
assertion misses a swapped pair.

- 177 game-bind id/name pairs; the 100 scan-code pairs *both directions*; the 111 default
  binds; six log levels and labels including `(null)` for `0` and `6`; 63 HTTP status
  phrases; four `RenderPhase` and five `QuickAccess`/four position constants; the 0..20
  proxy entry enumeration; MD5 against published vectors.
- Golden round-trips for `Settings.json`, `InputBinds.json`, `GameBinds.xml`,
  `AddonConfig.json` and `.imstyle180` against C++-produced fixtures. This immediately
  catches the three known byte deltas: the XML `encoding` attribute, the 52-vs-53-space log
  continuation indent, and LF-vs-CRLF.
- Bind-string parse table: `"junk+CTRL+H"` keeps Ctrl+H; an unresolvable final token keeps
  modifiers with code `0`; `"(null)"` unbinds.
- The `7997` passthrough offset and its closed range.
- All 41 path-index leaves, and **exactly eight** created directories (the build currently
  creates nine).
- The crash-log line formatter from a synthetic frame list — no crash handler, no crash.

### C. Deterministic harnesses

A stub exe and stub DLLs. Still no GW2.

- `should_consume` matrix: `WM_MOUSEMOVE`, `*BUTTONUP`, middle, X-button, key-up and focus
  messages are **never** consumed, and the keyboard flag derives from `WantTextInput`, not
  `WantCaptureKeyboard` (#14).
- Stub swap-chain vtable: `Present`/`Present1`/`ResizeBuffers` return sentinel `HRESULT`s
  verbatim, native runs exactly once, and a panicking callback still yields one native call.
- Stub `*_chainload.dll`: PATH-resident discovery, the missing self-module guard (today one
  bounce through our own export double-applies `-ggdev` and re-attaches dxgi), and
  per-export recursion scoping (#33).
- Recording `GameMessageSink` asserting exact ordered message tuples for game-bind
  press/release — this is what pins the `WM_SYSKEYDOWN` divergence (#27).
- Load the DLL and assert zero files, zero windows, unchanged thread count *before* the
  first export call.
- Extend `smoke-proxy` beyond its single `D3DPERF_GetStatus` so the D3D11 and DXGI
  resolution paths are covered at all.

---

## 5. How to read the register

`status` is the Rust tree's position against the reference:

| status | meaning |
|---|---|
| `matches` | Observably equivalent. Keep a test so it stays that way. |
| `partial` | Primitive is correct; something about reach, coverage or bytes is not. |
| `diverges` | Implemented and observably different. The most dangerous class. |
| `unreachable` | Correct code with no production caller. Same user-visible result as absent. |
| `absent` | No implementation. |

Where two surveys disagreed on `partial` vs `unreachable`, the **stricter** label was
taken: a byte-perfect serialiser with no construction site produces exactly the same
user-visible result as no serialiser at all.

Counts across 225 surveyed items: 52 `matches`, 43 `partial`, 43 `diverges`, 55 `absent`,
30 `unreachable`, 2 `unknown`. 66 rated high divergence risk.

---

## 6. Conformance register

40 consolidated items, then 17 the completeness critic found that no domain survey covered.
`B` = release blocker.

| # | B | Status | Surface | Reference | Rust |
|---|---|---|---|---|---|
| 1 | ● | unreachable | **Zero addons load.** No scan, no DLL loaded, no load callback, no API table | `HoContext.cpp:64-70`, `Loader.cpp:252-255,389-410` | `nexus-runtime/Cargo.toml:16` depends only on `nexus-addon-backend` — not on `-ffi`/`-loader`/`-manager`/`-watch`/`-cleanup`/`nexus-host`. **Verified.** Root cause of ~20 other `unreachable` items |
| 2 | ● | diverges | Overlay attaches; must not require a window-class literal | Class-agnostic: `Hooks.cpp:232-242`, `PlContext.cpp:34-56`. `ArenaNet_Dx_Window_Class` appears **nowhere** in the C++ tree (verified) | `dxgi.rs:31` + `set_require_expected_game_window(true)`; rejections at `classifier.rs:46-49,98-106` |
| 3 | ● | matches | Byte layout of every addon-facing struct; v1=256B … v6=496B | `ApiV1.h`–`ApiV6.h`, `AddonDefV1.h:28-44` | `nexus-abi/src/api.rs:271-541` + asserts `:552-588`. Needs the A4 external oracle |
| 4 | ● | diverges | An addon's unload routine can still call every API it used at load | `Addon.cpp:418-432`: native unload runs **first**, with the API fully live; sweep after | `manager.rs:1193,1203-1253,1263-1296` inverts it. `GetAddonDirectory` returns NULL and addons concatenate it — a crash vector. `HANDOFF.md:654-665` prescribes the *inverted* order |
| 5 | ● | diverges | API calls work from any thread and any call stack | No caller authentication; attribution failure never denies (`ApiBuilder.cpp:173-215`) | `boundary.rs:147-162` fails closed; `dispatcher.rs:295-308` returns null. Breaks worker threads, timer callbacks, MinHook trampolines |
| 6 | ● | diverges | Non-UTF-8 / long / heap-allocated addon metadata still loads | Opaque NUL-terminated bytes; no provenance check | `definition.rs:430` rejects invalid UTF-8; ceilings reject rather than truncate; `module.rs:312-361` rejects out-of-image definitions |
| 7 | ● | diverges | A symlinked DLL loads; one locked file doesn't disable every addon | Symlinks resolved; one bad entry skipped, never fatal | `discovery.rs:171-173` skips symlinks; `:161-195` returns `Err` for the whole scan, discarding every entry already found |
| 8 | ● | absent | Volatile addons auto-disable after a >350-build game patch | `BuildInfoService.cpp:10-43`, `Addon.cpp:873-897` | `VOLATILE` declared at `addon.rs:120` with **no reader**. Zero hits for `assetcdn`/`latest64` (verified). Turns a controlled refusal into a crash on patch day |
| 9 | ● | diverges | String-returning API functions never return null | No failure path exists (`ApiBuilder.cpp:175-215`) | `paths.rs:69-89` ceiling → `CapacityExceeded` → null. A `strcat` on address zero |
| 10 | ● | partial | `NexusLink` Font/FontBig/FontUI are always dereferenceable | Refreshed before addon callbacks; never NULL while rendering | `fonts.rs:407-431` returns all-zero addresses; `services.rs:677-688` publishes with no non-zero check. Null-deref inside third-party code |
| 11 | ● | diverges | A ≥20-byte character name still publishes identity and UI scale | Name truncated into the fixed array; other fields still published | `identity.rs:62-64` returns `NameTooLong`, publishing nothing → `ui_size` stays 0 → 0.90 scaling |
| 12 | ● | absent | Seven host event identifiers with correct payloads and ordering | `HkConst.h:11`, `AddConst.h:11-15`, `IbApi.cpp:224-227` | **Zero hits for all seven, verified.** Only `EV_MUMBLE_IDENTITY_UPDATED` exists. Resize plumbing has correct pre-native ordering (`detours.rs:1222-1250`) but raises nothing |
| 13 | ● | diverges | `Nexus.log` banner, shutdown records, `-mumble` naming, `-ggconsole` | `Runtime.cpp:92-137`; `SH_DENYWR` | `services.rs:96-102` registers a sink and **writes nothing**; `-ggvanilla` returns before init so no log at all; a complete `ConsoleLogSink` sits unused at `logging.rs:525-591`; continuation indent 52 vs 53 |
| 14 | ● | diverges | UI swallows exactly what the reference swallows | `UiInput.cpp:22-198`: never consumes mouse-move, button-up, middle, X, key-up, focus; keyboard gated on `WantTextInput` | Relay exists and is wired (`message.rs:60-125`). Three defects: over-broad `MessageClass::Mouse`; `WantCaptureKeyboard` not `WantTextInput`; no cursor-visibility term. Symptoms: stuck camera drag, stuck attacks, dead middle-click binds |
| 15 | ● | absent | The whole host UI: `Nexus` window, six pages, seven bind ids, EULA, Quick Access, Alerts | `MainWindow.cpp:30-445`, `UiContext.cpp:326-371` | `ui.rs:185-268` has two probe windows. Zero hits for `AcceptEULA`, `KB_MENU`, `KB_TOGGLEHIDEUI` (verified). Quick Access/Alerts/EULA exist as test-only state |
| 16 | ● | partial | 24 settings keys, exact names and types, observed **live** | `PrefConst.h:11-34`; absent options materialise their default | Only 8 keys exist; 17 absent (verified). Mitigating: `settings.rs:135-137` round-trips byte-identically, so unread keys are **preserved** |
| 17 | ● | partial | No raw `((000xxx))` tokens; 11 locale files written at startup | `LoclApi.cpp:29-405` | Reader matches and is wired; **nothing writes any `*_Main.json`**. Asset gap: the identifier numbers are interface and must be preserved |
| 18 | ● | unreachable | Saved `ImGuiStyle` applied; DPI scaling works | `UiStyle.cpp:49-210`, `Scaling.cpp:59-155` | `style.rs:302-392` complete, zero production callers; `scaling.rs:140-159` correct and unit-tested, **no production caller**; no `GetDpiForWindow`/`WM_DPICHANGED` anywhere (verified). Visible to any user on a 125%/150% display |
| 19 | ● | unreachable | `AddonConfig.json` read at startup, rewritten on change | `CfgManager.cpp:20-248` | Format matches byte-for-byte with a golden test; **no construction site** and no `PathKey` |
| 20 | ● | diverges | An addon-held `Texture*` survives resize/fullscreen/device loss | Process-lifetime records; resize never frees them | `textures.rs:180-205,296-315` retires the service and every ABI record on re-attach. **Settle this before wiring the addon API, not after** — otherwise every addon-held pointer is a use-after-free on first resize |
| 21 | ● | unreachable | Self-update, per-addon auto-update, library install, patch mutex | `Updater.cpp:19-318`, `Addon.cpp:588-1309` | Every `update.rs` public fn has zero callers outside the file. Zero hits for `RCGG-Mutex` (verified). No cadence, no scheduler, no library source. The mutex must land in the **same change** as the updater |
| 22 | ● | absent | `VERSIONINFO`, third-party notices on disk, a downloadable release | `res/Nexus.rc:56-88` | Resource directory **size 0**, no `VS_VERSION_INFO`, version `0.0.0` (all verified). Notices list 4 entries against 129 lockfile packages |
| 23 | ● | absent | A second GW2 client launches; multibox state logged | `Multibox.cpp:128-188`, substring `AN-Mutex-Window-Guild Wars 2` | Options parsed, never read. No mutex code anywhere. Mainstream GW2 workflow |
| 24 | ● | absent | Live GW2 bind tracking, so addon-issued binds press the player's key | `GbApi.cpp:24-99`, Unofficial Extras event | Zero hits for `UNOFFICIAL_EXTRAS` (verified). Every addon-issued game bind presses the **default** key |
| 25 | ● | partial | `"ALT+["`, `"CTRL+NUM 7"`, `"SHIFT+CAPS LOCK"` resolve, on any layout | Full OS key-name set, scan codes 0..255 × {plain, extended}, **active layout** | `bind.rs:343-413` covers 61 names, no punctuation/keypad/lock keys; `&UsKeyNames` hardcoded; `parse_bind_lossy` drops modifiers on error. Same table feeds bind *display* |
| 26 | ● | unknown | Numeric mouse-button codes in `InputBinds.json` | Values live in `Util/Inputs.h` — an **uninitialized submodule**, not verifiable from this tree | `bind.rs:34-48` pins None=0…X2=5. Settle from the public MIT `Nexus.h`. A shifted value rebinds the wrong physical button and looks like user error |
| 27 | ● | diverges | Exact message tuples a game-bind press/release puts on the wire | Alt-modified main key uses **ordinary** `WM_KEYDOWN` with lParam bit 29 (`GbApi.cpp:270-275,374-379`) | `game.rs:706` sets `system: binding.alt` → `WM_SYSKEYDOWN`, declared an intentional correction. Visible to the game *and* every other addon's WndProc — rule 2.2 |
| 28 | ● | partial | Overlay renders even when the swap chain wasn't created via a Nexus export | Process-wide: implementation addresses patched at vtable slots 8/13/22 | Per-instance vtable pointers only; `HookMode::GlobalFallback` accepted but unimplemented. Also detectable by other overlays — needs coexistence testing |
| 29 | | absent | `Crash.log`/`CrashStack.log` with fixed layout | Vectored handler at chain **front**, sees first-chance, does not dismiss | Path keys only; no handler, no stack walk, no writer. Trap: `SetUnhandledExceptionFilter` is quieter but changes what the user sees |
| 30 | | diverges | Escape closes the front-most Nexus window, if the user enabled it | Walks the **live** ImGui stack, skipping the bottom entry | `escape.rs` reproduces the gates and is wired, but the "stack" is *registration order*, and `set_enabled` has no production caller so `CloseOnEscape` is ignored |
| 31 | | matches | 177 bind ids, 100 scan codes, 111 defaults, log levels, 63 HTTP phrases, MD5, enums | Exact-value contracts | Two surveys diffed these programmatically: identical. **But** `logging.rs:52-67` rejects log levels 0 and 6 where C++ emits `(null)` — rule 2.2 |
| 32 | | partial | Five user-facing file formats round-trip; 41 path leaves; 8 directories | `PrefContext.cpp:53-84`, `Index.cpp:17-96`; binds written **synchronously** on mutation | `game.rs:238` writes `encoding="UTF-8"`; binds saved **only at shutdown** so a crash loses the session; LF vs CRLF everywhere; **nine** directories created; **two path anchors in one build** (DLL vs executable) |
| 33 | | partial | Chainload search path, self-module guard, per-export recursion | Module-name resolution (covers PATH); self-module discarded; per-export thread-local | Exe-directory-only lookup; **no self-module check**; a single shared `IN_PROXY_CALL` across all 20 exports |
| 34 | | unreachable | Addon hot-reload; a moved file is a relocation, not a new addon | 5000ms poll **plus** OS notifications via `WM_USER+101` | Debounced watcher, no polling; `hot_reload` has no production caller. Decide `0x0465` deliberately — an addon using it would suddenly start receiving those messages |
| 35 | | partial | Export names and ordinals | 18 names, **no explicit ordinals** — link.exe derives them from the sorted name table | 20 names with hand-chosen `@1..@20`. Name set is a strict superset (fine); ordinals are **not** reference-compatible. Document the pin as "our own ordinals" until measured |
| 36 | | matches | Native return values passed through; no `DllMain`; `-ggdev` on two exports; idempotent shutdown | `Hooks.cpp:218-259`, no `DllMain` anywhere | All reproduced correctly, plus real improvements worth keeping (§9). Two to add: route `report_proxy_failure` to `Nexus.log`; keep `ResizeBuffers1` at slot 39 rather than replicating the reference's never-firing slot-26 hook |
| 37 | | diverges | Three DataLink identities; addon `Share` storage kind; identifier acceptance | Addon `Share` creates **process-local heap**, not a named mapping; identifiers are opaque bytes | `data_link.rs:34-45` routes to `share_public` → a **named Win32 mapping**; `name.rs:55-99` rejects non-UTF-8/>255B; telemetry on the 50ms not 100ms cadence; a non-positive scaling skips the **whole frame's** publish |
| 38 | | partial | ArcDPS: nothing exposed to addons; style presets | **No ArcDPS members in any revision** — matches | Trap: `ArcApi.cpp` looks like a working client but never calls through in the shipped build. "Completing" it would add behaviour the original never had |
| 39 | | matches | Deliberate non-features: function registry, frame counter, two inert flags | No observable contract — and a reimplementation must not add one | Correctly absent. Assert flag `1<<3` still gets non-null context and allocators (see rule 2.2) |
| 40 | | partial | Localization/atlas/style/quick-access reach (folded into #15–#19) | — | — |

### Critic extension — surfaces no domain survey covered

| # | B | Status | Surface | Detail |
|---|---|---|---|---|
| 41 | ● | diverges | An addon depending on a sibling DLL in the GW2 root still loads | Reference uses `LoadLibraryA` — **standard search order**, so dependents resolve from the exe directory and PATH. Addons routinely ship dependencies next to `Gw2-64.exe`. Today: `ERROR_MOD_NOT_FOUND`. Log `GetLastError` with the path — a silent NULL reads as "the Rust version deleted my addon". The A→W change is a genuine improvement; keep it |
| 42 | ● | diverges | **Byte encoding** of the three directory getters | Reference builds the path index from ANSI entry points and hands out ACP bytes, so an addon passing them to `fopen`/`CreateFileA` always hits the right file. Decide explicitly: emit ACP bytes for parity, or keep UTF-8 and document that non-ASCII install paths break narrow-CRT addons. Invisible on every ASCII test machine |
| 43 | ● | absent | Ctrl+C/Ctrl+V in any Nexus or addon text field | ImGui 1.80's Win32 clipboard handlers are compiled into the reference. Pasting an API key is a mainstream flow. Static gate: assert `OpenClipboard`/`SetClipboardText` bound or `io.SetClipboardTextFn` non-null. Likely just `IMGUI_DISABLE_WIN32_DEFAULT_CLIPBOARD_FUNCTIONS` |
| 44 | ● | absent | PE resource directory beyond `VERSIONINFO` | Reference embeds notices (id 1, unpacked at startup), three fonts (101–103), Quick Access icons (201–219 incl. seasonal), page icons (300–306), tier icons (400–403), banners (500–513), locale blobs (701+) |
| 45 | ● | partial | Glyph coverage of the shared atlas | Reference ranges = default + Latin Extended + every char in the loaded locale tables, and the default font adds ChineseFull + Cyrillic. Assert U+00E9, U+0410, U+4E00 resolve. A Russian or Chinese addon UI drawn with Nexus fonts renders as blank boxes — reported as "the Rust version broke addon X" |
| 46 | ● | diverges | What the user sees when the host itself fails | **No `MessageBox` anywhere** in the reference; console only under `-ggconsole`; a fatal exception is logged and **not dismissed**. Rule: the Rust build may be quieter in the log, never louder on screen, and must never be why the game exits. Wrap every `extern "C"` entry in `catch_unwind` (the dispatcher already does — `detours.rs:1119-1146`) |
| 47 | | diverges | Reentrant (de)registration from inside a render callback | The reference **self-deadlocks**, so no addon can depend on it — but a callback deregistered mid-frame must not be invoked again. Snapshot-plus-liveness; `CallbackSlot::deactivate` already exists. Assert phase order Pre→Render→Post and registration order within a phase |
| 48 | | diverges | Host-invented ceilings that silently retire a registration | Reference has **no ceilings and no self-healing**: registration succeeds forever. Every ceiling must be unreachable by a realistic addon or degrade loudly, naming the addon and the limit |
| 49 | | diverges | Loader-lock / TLS side effects | Reference deliberately has **no `DllMain`**, deferring everything to a latch on the first export. See gate A6 — the image has a TLS directory today |
| 50 | | diverges | Minimum Windows version and the non-OS-guaranteed import set | Reference's `PathCch`/`shcore` put the floor at Win8. See gate A1 |
| 51 | | unknown | Frame time and working set | Pin a budget *before* optimizing: measure median and p99 added `Present` cost and working set for both builds on the same machine and scene, and gate CI on a stated margin. Without a number, "it feels worse" is unfalsifiable |
| 52 | | diverges | Downloads work on the same networks the C++ build's did | Reference: static OpenSSL over cpp-httplib, `crypt32` available. A corporate MITM root in the Windows store behaves differently under WinHTTP than under bundled OpenSSL, and `WINHTTP_ACCESS_TYPE_NO_PROXY` reaches nothing behind a system proxy. Invisible on a dev machine, total on a locked-down network |
| 53 | | partial | An addon texture that loaded under the reference still loads | Reference decodes with `stb_image`: lenient, covers PNG/JPG/BMP/TGA/PSD/GIF/HDR/PIC/PNM. Build a corpus from real addons plus damaged files and diff the pass/fail matrix. A silently-unresolved texture looks like an addon with invisible buttons |
| 54 | | diverges | Which addon loads first | Reference order is the filesystem's, and each addon is loaded **inline as discovered**, so a load callback sees only addons that sorted before it. Decides who wins a contested Quick Access shortcut. Sorted-then-batch is probably better — but record the decision and test it |
| 55 | | partial | CJK IME composition and candidate window | ImGui 1.80's Win32 IME support is compiled into the reference. Korean and Chinese are shipped Nexus locales, so this population is explicitly in scope |
| 56 | | diverges | `Nexus.log` with a second client running | Reference opens truncating with write-sharing **denied**, so the second instance logs nowhere rather than corrupting the first file. Multiboxers are the population most likely to try a swap, and a shredded log destroys the first artifact of every bug report |
| 57 | | unknown | Whether the cited self-update and banner values are the shipped contract | See rule 2.3. Do not port `.old`/`.update` naming, the `Module:` banner value, or the ArcDPS self-comparison from HEAD |

---

## 7. Needs observation — operator homework

No amount of source reading settles these.

**From a real shipped C++ binary:** `dumpbin /exports` for the reference's actual ordinals,
plus `dumpbin /imports` on `Gw2-64.exe` and common chainload DLLs — this decides whether
the ordinal divergence (#35) matters at all. `EMouseButtons` numeric values (#26). Hex-dump
`Settings.json`/`AddonConfig.json`/`InputBinds.json`/`Nexus.log` to confirm the CRLF claim,
and `GameBinds.xml` for the XML declaration. Whether a message ending in a newline produces
an extra indent-only log line. Whether the released build implements `RequestUpdate` (HEAD
has it commented out). The reference's four version components (`src/Version.h` is absent
from this worktree).

**From a real GW2 install:** The actual top-level window class across DX11/legacy and
locales (#2). An API trace of a real launch recording every factory/swap-chain creation and
its caller module (#28). Whether the game accepts `WM_SYSKEYDOWN` equivalently (#27).
Black-box coexistence with ArcDPS, Steam, Discord, NVIDIA and RTSS **in both install
orders**. Overlay behaviour across windowed/fullscreen/borderless, resolution, monitor, DPI
change and alt-tab. Two clients for multibox (#23). Whether `NexusLink` Width/Height must be
the buffer size or the client rect — they differ under exclusive fullscreen at a non-native
resolution.

**From the real ecosystem:** Load third-party addons across revisions v1–v6 and confirm
their windows render, accept input and keep their style — an ImGui version or allocator
mismatch shows up as *corruption*, not a clean failure, so no unit test substitutes.
Whether the C++ extension check is case-sensitive (does `Foo.DLL` load?). Whether
`RtlCaptureStackBackTrace` reaches an addon frame from a MinHook trampoline. Screenshots at
fixed resolution/UI-size/DPI for layout comparison.

**Operator decisions, not findings:** Whether to keep Raidcore branding, banners, outbound
links and the copyright footer. Whether to ship bug-for-bug parity for the two visibly
broken C++ paths (uninstall is a commented-out no-op; "check for updates" throws). The asset
programme — independently authored icons, banners, style presets, fonts and all 11 locale
tables, none of which may be copied.

---

## 8. Verification standard

Per rule 2.2 and the `/assay` discipline: **a passing check proves nothing until something
proves it could have failed.** For every item closed, the counterfactual must perturb the
subject, never the measurement — compare `(exit_code, passed + failed)` and require the test
count to be identical and non-zero across both worlds. A green run where the test was
deleted is indistinguishable from a pass at the exit-code level.

For claims of absence, run the flip test: demonstrate the same search finding a control
string. (The zero-hit counts in section 6 marked *verified* were confirmed this way — the
same query finds `EV_MUMBLE_IDENTITY_UPDATED` and the `CloseOnEscape` ABI type names, so
zero means absent, not a broken search.)

No silent caps: if a gate bounds its own coverage, say what was dropped.

## 9. Improvements to keep

Real wins, invisible when nothing goes wrong. Do not "restore compatibility" by reverting
these:

`DXGI_STATUS_OCCLUDED` recognised; `DEVICE_REMOVED`/`RESET` retiring renderer state; an
`after_resize` recreate phase the reference lacks; fail-closed export fallbacks instead of a
null-call fault; a conditional WndProc restore that cannot clobber an overlay installed
after Nexus; `catch_unwind` with a re-resolve fallback so native runs exactly once on panic;
`ResizeBuffers1` hooked at the slot that actually fires; settings/config writers preserving
unknown keys, unparseable entries and original order; temp-file + atomic replace so a crash
mid-save cannot truncate; static CRT linkage additionally removing a DLL-hijack surface;
`LoadLibraryW` accepting paths the ANSI code page cannot express.

## 10. `HANDOFF.md` corrections

| Location | Correction |
|---|---|
| `:654-665` | **Prescribes the inverted teardown order.** Following it guarantees the #4 break — a NULL `GetAddonDirectory` inside an unload export. The reference invokes native unload *first*, API fully live |
| `:176-190`, `:232-259` | Frames the addon gap as construction-only. It is **not linked** at all (#1), and four addon-visible behaviours have no implementation to compose: the volatile/350-build rule, the four lifecycle events, five-interface export detection, and the update cadence |
| `:186-190` | Lists Mumble/DataLink/NexusLink/settings/logging/paths/events as composed. `DataLinkService` and `EventService` are unread `_data_link`/`_events` fields with zero addon-facing exposure; logging never calls `LogRegistry::log`, so `Nexus.log` is created, truncated and left empty |
| `:794-806` | Understates settings and misstates input. `AcceptEULA` has **no storage at all**; 17 further keys absent. Conversely it fails to record that `WindowMessageRouter`, escape-close, managed-bind routing, the `7997` translation and the mouse-reset fix are **already live** — a reader planning from the document alone might rebuild working code |
| `:869-872` | Omits two checkable facts: the release DLL carries **no `VERSIONINFO` at all**, and the workspace version is `0.0.0`, so there is nothing to stamp |
| `:190` | "Self-update source selection and planning primitives" reads as more integrated than it is — zero callers outside `update.rs`. Also fails to record that the endpoint **deliberately** points at this project's own repository (rule 2.4) |
| `:865-867` | Mislabels `AddonInterfaces::ARC_DPS` as an addon ABI flag. It is a host-internal module-detection bitset; the addon-facing `AddonDefinitionFlags` has no ArcDPS bit |
| Missing entirely | The highest-severity item in the whole matrix: the release cdylib links the CRT **dynamically** (gate A1). Every other item is moot until this is fixed |

---

## Provenance and method

Derived from an 8-domain parallel survey of both trees plus a completeness critic, all
reading only: this repository, the C++ reference tree, and the public MIT
`RaidcoreGG/Nexus-API` header (normative per `PROVENANCE.md` where it covers).

The method is **interoperability reimplementation**, not derivation. Capture the interface
and observable contract exactly — names, offsets, values, formats, ordering. Do not
reproduce internal algorithms, class hierarchies, control flow, naming or file
organization; do not copy implementation text or assets. Every item above states *what
behavior is required*, never *how the reference achieves it*. Where an item could not be
settled from permitted sources it is recorded as `unknown` in section 6 or listed in
section 7 rather than guessed.

Facts marked **verified** in section 6 were independently re-checked against the live tree
rather than taken from an agent report — the PE import/resource/TLS directories by parsing
the image, the dependency and literal-absence claims by direct search with a flip test.
