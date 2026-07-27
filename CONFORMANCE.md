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

### 2.2 Fix every defect; the interface is frozen

**No bugs. This is a better host, not a transcription.** Every defect in the reference gets
fixed — memory-safety faults, dead code paths, features that throw, races, the lot.

What is frozen is the **interface**, not the behaviour behind it: names, byte offsets,
numeric values, file formats, and ordering that a third party *transmits, stores, or
compares*. A constant is not a bug. `EGameBinds` value `10`, the `WM_USER` passthrough
offset `7997`, and the sub-1024×768 scaling rule are contract, and keeping them is not
"preserving a bug" — it is implementing the protocol.

The narrow case that needs care: a defect expressed **through** the interface, where the
wrong value *is* the wire format. There the fix is still the goal, but it needs a
measurement first, because "correct" and "what the other party parses" can differ.

Worked through, this is a much smaller set than it first appears. Of the three cases
originally filed here as "preserve the bug", two dissolve on inspection:

| Case | Verdict |
|---|---|
| ~~Log levels `0` and `6` rejected (#31)~~ | **Fixed.** The reference is lenient and emits the line with severity `(null)`; Rust was strict and *lost an addon's log output*. Rust's strictness was the defect, so matching the reference was both the fix and the compatible choice. Three rejection points were involved, not one — see the #31 register row |
| Definition flag `1<<3`, "can create own ImGui context" (#39) | **Already correct.** The reference ignores the flag and always passes a non-null context and allocators. "Implementing" it by passing nulls would null-deref every addon that sets it. Ignoring it is the safe reading, not a bug — assert it so nobody "fixes" it |
| Alt-modified game bind promoted to `WM_SYSKEYDOWN` (#27) | **Genuinely open.** The reference sends the *ordinary* message with lParam bit 29. This is read by the game itself and by every other addon's WndProc. Settle it with the operator test in §7: if GW2 accepts both equivalently the improvement stays; if not, the ordinary message is required. One measurement, then it is closed either way |

Where Rust is **already better and staying that way**: `ResizeBuffers1` hooked at the slot
that actually fires rather than the reference's dead slot-26 hook (#36); Mumble identity
truncated at a char boundary instead of overflowing a 20-byte array (#11); `IsGameplay`
driven by a real counter where the reference latches it at `1` forever (#37); atomic
temp-file writes so a crash mid-save cannot truncate a user's config. §9 lists the rest.

Two reference features are outright broken — uninstall is a commented-out no-op, and "check
for updates" throws. **Implement both properly.** There is no compatibility argument for
reproducing a throw.

The single rule: **fix it, and if the fix changes a byte a third party reads, prove the
change is transparent before shipping it.** Record the proof in the register row.

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
| No third-party brand identity anywhere in the artifact | Decided: we recreate the functionality, not the brand. See §2.5 |
| Mumble identity name truncated at a char boundary rather than overflowing a 20-byte array | Do not reproduce a memory-safety defect; truncation is correct for both trees (#11) |

Low-risk deliberate divergences worth recording rather than reverting: the network cache
key, `User-Agent: Nexus/1.0`, and the finite 15/15/30/30s WinHTTP timeouts.

### 2.7 Teardown order: the API stays live across the unload export

The reference invokes the addon's unload export **first**, with the whole API functional for
that addon, and only then sweeps host-side references and frees the module
(`src/Host/Addons/Addon.cpp:418-432`: `Unload()` → `CleanupRefs` → `EV_ADDON_UNLOADED` →
`FreeLibrary`). Real unload routines rely on this: they deregister callbacks, release
textures and ask for their addon directory.

`HANDOFF.md:654-665` prescribes the inverse, and the implementation followed it, so
`request_unload` closed API admission before the export ran. A string getter then returned
null, and addons concatenate that — a crash, not a no-op.

**Fixed:** admission now closes inside `invoke_native_unload`, after the export returns.
Note this is distinct from the *loader* gate, which `request_shutdown` closes immediately and
should: that gate stops the host calling **into** the addon, which is correct while it tears
down, whereas admission governs the addon calling **out** to the host.

**Outstanding:** the host-side registration sweep (`drain_registrations`) still runs before
the export instead of after. The observable difference is narrow — an addon deregistering
something already swept gets a void no-op either way — but a registration *created* during
unload would survive where the reference sweeps it, leaving a callback that can fire after
`FreeLibrary`. The fix has a natural seam: `Module::complete_host_cleanup` already requires
`UnloadCallbackComplete` and takes a closure, and is currently invoked with a no-op. Moving
the sweep there needs a timeout parameter on `finish_unload`, which is why it is a separate
checkpoint.

### 2.8 Settled: load order is deterministic, and batching is unobservable

The reference's order is the filesystem's — `std::filesystem::directory_iterator` over the
addon directory — and each addon is **loaded inline as it is discovered**
(`src/Host/Loader/Loader.cpp:252-387`). Load order decides who wins a contested Quick Access
shortcut or DataLink identifier, and the order of `EV_ADDON_LOADED`.

**Decision: keep the deterministic sorted order, and load in it.** `normalize_discovery`
already sorts by lowercased path and rejects duplicates
(`nexus-addon-manager/src/discovery.rs:248-265`, tested by
`discovery_is_case_insensitively_sorted_and_rejects_duplicates`). Filesystem order is not
guaranteed across machines or filesystems; sorted order makes a contested-identifier outcome
reproducible instead of luck. On NTFS the two usually coincide anyway, since directory
enumeration is already roughly lexicographic.

**Batching versus inline loading is unobservable through the ABI, which settles the harder
half of this question.** The concern is that an addon's load callback sees a different set of
peers under batch loading than under the reference's interleaving. But `AddonAPI_t` exposes
**no addon-enumeration function at all** — verified against the vendored header, zero matches
for any addon-listing entry point. An addon can therefore learn about its peers only through
`EV_ADDON_LOADED`, whose ordering is preserved as long as loading follows the same sorted
sequence. So the visible contract is the *order*, not the interleaving.

Two discovery divergences remain open under #7 and are not affected by this decision:
symlinks are skipped where the reference resolves them, and the extension test is
case-insensitive where the reference's likely is not, so `Foo.DLL` loads here.

### 2.9 What actually blocks installing the addon API

`HANDOFF.md` frames the remaining addon work as "compose and install". Composition now
exists (`ProductionAddonApiBackend::compose`, register #1). Enumerating what `compose`
demands against what `nexus-runtime` owns pins the blocker to **two missing services**, not
to composition:

| Service `compose` requires | Runtime status |
|---|---|
| `Arc<UiHost>`, `Arc<LogRegistry>`, `Arc<EventService>`, `Arc<DataLinkService>`, `Option<Arc<MinimalScheduler>>` | present in `services.rs` |
| `Arc<StablePathStore>` | constructible from the existing `PathIndex` |
| `Arc<RawWndProcRegistry>`, `Arc<ManagedInputBinds>` | present in `input.rs` |
| `Arc<Mutex<Option<GameInvoker>>>` | present in `game_input.rs` |
| `Arc<dyn GameOnlyMessageSink>` | present via the game-input sink |
| `Arc<Mutex<LocalizationService>>` | present in `services.rs` |
| `Arc<dyn TextureServiceFacade>` | **`RuntimeTextureCoordinator` already implements it** |
| `Arc<InlineHookService>` | **absent** — never constructed in the runtime. Trivial to add |
| `Arc<dyn RenderFontService>` | **absent, and this is the real blocker** |

`RuntimeFontCoordinator` exposes only render-side operations — `advance`,
`take_gpu_rebuild`, `selected_addresses`, `active_identity`, `shutdown` — so the
addon-facing font surface does not exist in the runtime at all, and it is not merely
unwired. That is `HANDOFF.md`'s second milestone (the bounded synchronous render-font
bridge, specified at `HANDOFF.md:351-535`).

**But the milestone is smaller than that specification implies, which is worth knowing
before anyone starts it.** `ImGuiFontManager` is `FontManager<ImGuiFontAtlasBackend>`
(`nexus-ui-fonts/src/lib.rs:593`), and `FontManager` already implements every addon-facing
operation the trait needs: `get`, `register_memory`, `release`, `resize`, `cleanup_owner`,
`cleanup_owner_callbacks` and `cleanup_owner_resources`
(`nexus-ui-services/src/fonts.rs:410-687`). `FontSession` already owns one such manager per
attachment (`nexus-runtime/src/fonts.rs:108-116`).

So the bridge is **routing, not reimplementation**: the work is the bounded FIFO, the ticket
and one-shot response, inline-versus-queued dispatch with older-work-first draining,
re-validating the attachment immediately before execution, and the transactional
partial-mutation failure path. None of the font operations themselves need writing.

The borrow discipline also has precedent to copy rather than invent:
`nexus-addon-cleanup/src/direct.rs:105-116` already routes the two cleanup phases through
`try_borrow_mut` and maps a failed borrow to a closed `Busy` rejection, which is exactly the
re-entrancy rule the specification demands.

**One architectural fact determines the rest of the design, and it is easy to miss.** The
`FontManager` does **not** live in the coordinator's `Mutex`. `FontCoordinatorState` holds
only metadata — `attachment_id`, `identity`, `render_thread`
(`nexus-runtime/src/fonts.rs:94-106`). The manager lives inside `FontSession` in a
**thread-local** `FONT_SESSIONS`, which `advance` reaches through
`FONT_SESSIONS.try_with(...)` plus `try_borrow_mut` (`fonts.rs:350-360`). The type's own doc
comment says as much: "whose native font manager remains render-thread local".

Two consequences follow, and they are not choices:

1. **The bounded queue is structural, not an optimisation.** A call from a non-render thread
   cannot touch the manager at all, because the manager is thread-local to the render thread.
   Queue-and-wait is the only way an off-thread call can complete synchronously.
2. **The inline path needs no new locking.** A call already on the render thread with the
   matching context and attachment reaches the manager the same way `advance` does, and
   `try_borrow_mut` failure is precisely the re-entrancy case the specification says must
   reject closed rather than wait or panic.

So the implementation shape is fixed: validate context and attachment on the current thread,
then either execute inline through `FONT_SESSIONS` or enqueue an owned command with a
one-shot response and block until the render thread drains it at the top of `advance`, before
`session.advance`.

**The inline half is built.** `RuntimeFontCoordinator::with_active_manager` is the seam every
add-on-facing font operation routes through. It revalidates the active session, the current
ImGui context, the attachment and the failed flag immediately before execution; maps a failed
`try_borrow_mut` to a closed `Reentrant` rejection; and on a contained panic marks the
attachment failed rather than returning an ordinary rejection over a possible partial
mutation. Its test covers the positive path against a real manager, re-entrancy, an off-thread
call and a detached attachment, and is flip-tested — removing the `try_borrow_mut` guard makes
the re-entrant case panic with `RefCell already borrowed`.

**The queue is built too.** `enqueue_for_render_thread` accepts an owned command from any
other thread and blocks until the render thread settles it, so an off-thread call is still
synchronous from the native API's perspective. It is bounded on **both** command count (256)
and retained bytes (32 MiB), because a count limit alone is not a memory limit —
`FontManager::register_memory` copies the bytes it is given, so a few hundred queued font
files would retain gigabytes. Each command carries the attachment that accepted it and is
canceled rather than executed if that attachment has been superseded, so work can never run
in an ImGui context it was not accepted under. `advance` drains the queue **before**
`session.advance`, keeping global order FIFO and making a registration visible to the same
frame's atlas rebuild. Detach and shutdown cancel every waiter immediately instead of leaving
callers blocked to their own deadlines.

**The adapter is built, so the bridge is complete.** `RuntimeFontBridge` implements
`RenderFontService` over all nine operations. It decides inline-versus-queued *before*
dispatch — a command can only be consumed once, so both paths cannot be attempted with the
same closure — and collapses both closed error families onto `ServiceRejected`, which is
sound because every variant is already atomic and the legacy ABI has no error channel for
these operations.

File and Windows-resource bytes are copied on the calling thread **before** any command is
queued, so a queued command never retains a path, a borrowed buffer, a module handle or a
resource pointer, and the queue's byte accounting sees the real payload. All three
registration sources funnel through one owned-bytes path.

Its test drives the adapter through the trait from both thread contexts — inline on the
render thread, queued and drained from another — plus a missing font file refused closed and
every operation rejecting closed once the attachment is gone.

**What remains is installation, not the bridge:** `nexus-runtime` must gain dependency edges
on the addon crates, construct an `InlineHookService`, and call
`ProductionAddonApiBackend::compose` with these services. The bridge keeps a `dead_code`
allowance on its constructor until that call exists.

**A partial font bridge must not be installed.** Returning a rejection from `get` hands an
addon a null `ImFont*`, and the host itself pushes those fonts unchecked (#10), so a
half-bridge is worse than no wiring: it converts "addons do not load" into "addons load and
crash". The bridge lands complete, or the API is not installed.

### 2.6 Settled: texture records are process-lifetime, GPU views are not

`ROADMAP.md` requires this decided **before** the addon API is wired, because if ABI
records die with a render session then every addon-held `Texture*` is a use-after-free on
the first resize, and retrofitting means redoing the API surface. Deciding it (#20):

**An addon-visible `Texture*` and the identifier map live for the process. The GPU view
inside the record is replaceable.**

The evidence settles what would otherwise be a judgement call. `GpuTexture` owns an
`ID3D11ShaderResourceView` (`nexus-textures/src/backend.rs:82-85`) and `TextureService::new`
takes the `GpuBackend` at construction (`service.rs:332`). An SRV is created from the
**device**, not from the swap chain — so a back-buffer resize or a render-target rebuild does
**not** invalidate it. Only genuine device loss (`DEVICE_REMOVED`/`DEVICE_RESET`, yielding a
new device) does.

That has a direct consequence: retiring the texture service on session re-attach is a
**defect, not a necessity**. Today `RuntimeTextureCoordinator::attach_service`
(`nexus-runtime/src/textures.rs:181-205`) replaces the active session and `retire_session`
drops the previous `TextureService`, taking `RegistryState.registry` — and therefore every
`Arc<TextureEntry>` and every `Box<TextureStorage>` an addon holds a pointer into — with it.
The reference frees none of this: records are process-lifetime allocations owned by one
host-wide registry, and neither a resize nor a rebuild clears the identifier map
(`src/Graphics/Textures/TxLoader.cpp:48-63,137-183`).

Required shape:

| Concern | Lifetime |
|---|---|
| `TextureStorage` — the record whose address an addon caches | **Process.** Its address must never change and it must never be freed |
| The identifier → handle map | **Process.** A resize must not make `Textures_Get` miss for an identifier that resolved before |
| The `ID3D11ShaderResourceView` | **Per device.** Retained across resize; re-uploaded on device loss, writing the new address into the *existing* record |
| Decode/download workers and queues | **Per session.** These legitimately belong to the service |

`TextureHandle` is already `Arc<TextureEntry>` and `as_abi_ptr` already documents the
pointer as valid "while this handle or its registry entry lives", so the record's stability
is in place; what is missing is an owner outside the session keeping handles alive. Retention
is therefore **unbounded by design**, matching the reference, which has no ceiling and frees
no record — see #48 on host-invented ceilings.

**Implemented for the resize case.** `RuntimeTextureCoordinator` now holds a process-scoped
`retained` table keyed by identifier. Every handle it hands out is kept alive there, and a
`get` that misses the current session falls back to it, so a record that resolved once keeps
resolving after the session that created it is retired. The first handle seen for an
identifier wins — replacing it would move a record an add-on already cached, which is the
thing the table exists to prevent.

Flip-tested: removing only the retention makes the test fail with *"the identifier must still
resolve after a re-attach"*, with the run count identical across both worlds, so the
counterfactual moved the subject and not the instrument.

**Still outstanding: device loss.** On `DEVICE_REMOVED`/`DEVICE_RESET` the retained SRV
becomes invalid and must be re-uploaded into the *existing* record rather than replaced. The
retained handle would currently hand an add-on a stale view. Resize, fullscreen toggle and
render-target rebuild are covered because those keep the device.

### 2.5 Brand identity is removed; protocol identifiers are not

**Decision: no third-party brand identity ships in this artifact.** We reimplement the
functionality, not the badging.

**The product is `Tessera`.** A tessera is one tile in a mosaic — many independent pieces
composing a single coherent surface, which is what an addon host is. It appropriates neither
vendor's identity, which rules out GW2-native terms (Waypoint, Sigil, Mistward) for the same
reason it rules out Raidcore's: `README.md:62` disclaims affiliation with ArenaNet and NCSoft
too, and that disclaimer has to stay true.

The `nexus-*` crate names **stay**. They name the protocol the crates implement, the way a
crate called `http-parser` does — they are invisible in the shipped artifact, and renaming 36
crates plus every `use` path is churn with no user-visible benefit. If that ever changes it
is a mechanical rename, not a design decision.

This needs one line drawn carefully, because some strings that *look* like branding are
wire format. The word "Nexus" is welded into the addon ABI and the on-disk layout: an addon
calls `DataLink.Get("DL_NEXUS_LINK")` with that literal, and `<GW2>/addons/Nexus/` holds
every user's `Settings.json`, `InputBinds.json`, `GameBinds.xml` and `AddonConfig.json`.
Renaming those does not de-brand anything — it breaks every addon and abandons every user's
preferences, which is the exact failure §1 exists to prevent.

So: **an identifier a third party transmits, stores, or opens by name is protocol and stays
verbatim. Everything that identifies the vendor is removed.**

| Remove — vendor identity | Replace with |
|---|---|
| `VERSIONINFO` `CompanyName "Raidcore"`, `LegalCopyright "© 2021-2026 Raidcore"`, `ProductName "Nexus"`, `FileDescription` (`res/Nexus.rc:74-79`) | `ProductName "Tessera"`, our own `CompanyName`/`LegalCopyright`, and a `FileDescription` that does not use a trademarked game name. Reproduce the field *names* and layout only |
| `https://raidcore.gg/Legal` — the licence modal's link | Our own terms, or reconsider the gate. The `AcceptEULA` **settings key name stays** (#15) or every user re-prompts on upgrade |
| `https://discord.gg/raidcore` — the About page link | Our own, or nothing |
| `https://api.raidcore.gg/addonlibrary`, `/arcdpslibrary` — the in-game library sources | Our own catalogue source. Also removes an operational dependency on a third party's servers. **See the open question below** |
| `https://api.raidcore.gg` — the updater endpoint | Already ours (§2.4) |
| All logos, banners, icons, style presets, typefaces, the copyright footer | Independently authored (§5 of `ROADMAP.md`) |
| The overlay window title and any in-UI product name | Our name. Not ABI — addons register their *own* ImGui window names |
| `AddonInterfaces::RAIDCORE` (`nexus-abi/src/addon.rs:143`) | Free to rename: this is a **host-internal** module-detection bitset, absent from every API revision and from `AddonDefinitionFlags` |

| Keep verbatim — protocol, not brand | Why |
|---|---|
| `DL_NEXUS_LINK`, `DL_NEXUS_LINK_<pid>`, `DL_MUMBLE_LINK`, `DL_MUMBLE_LINK_IDENTITY` | Addons pass these literals to `DataLink.Get`/`.Share`. Hard ABI |
| `<GW2>/addons/Nexus/` and every file in it | Holds all existing user state. Renaming abandons it |
| `Nexus.log`, `Nexus_<value>.log` | User-visible artifact and the first thing in every bug report |
| `AN-Mutex-Window-Guild Wars 2` | **ArenaNet's** object name, matched by substring. Required for multibox (#23) |
| `MumbleLink` | The game's own shared-memory object |
| `assetcdn.101.arenanetworks.com/latest64/101` | The game's own build endpoint, required by the volatile rule (#8) |
| `arcdps_integration64.dll`, `.imstyle180`, `THIRDPARTYSOFTWAREREADME.TXT` | Reserved/observable names carrying no vendor identity |
| All `EV_*`, `KB_*` and API-table identifiers | No vendor identity, and all are ABI |

**One judgment call: `RCGG-Mutex-Patch-Nexus`** (`Updater.cpp:19-24`) is a vendor-branded
*named system object* — the cross-process lock preventing two clients patching at once.
Rename it. The only way that regresses is a user multiboxing two separate installs, one C++
and one Rust, both self-updating simultaneously; that is vanishingly narrow and
self-inflicted, and it does not justify shipping someone else's brand in a kernel object
name. Recorded as a decision, not an oversight.

**Open question, functional rather than brand:** the in-game addon library is a real feature
(browse and install without leaving the game). Removing the vendor's catalogue URLs does not
remove the feature — it needs a source. Either we host a catalogue, or the page ships
present-but-empty. Decide before Phase 6; do not silently drop the feature.

---

## 3. Release gate

Three tiers, strictly ordered. Tier 1 items are prerequisites for *observing* anything
else; until they pass, every other item in this document is untestable.

### Tier 1 — Load and attach

| # | Gate | Status |
|---|---|---|
| 1.1 | Static CRT: the DLL loads with no VC++ redistributable installed | **PASSES** — gated in CI (§4.A1) |
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

**Status: closed.** `.cargo/config.toml` sets `-C target-feature=+crt-static` for the MSVC
target, `xtask verify-imports` enforces the result, and CI runs it. The import surface went
from **18 modules to 11**; the smoke test loads the static-CRT DLL and forwards a call.

Proven with a flip test rather than asserted: the same gate binary exits `0` on the
static-CRT artifact and `1` on the preserved dynamic-CRT artifact, naming all seven runtime
modules. The counterfactual perturbed the build's linkage, never the measurement.

The `--target` concern turned out not to apply on toolchain 1.97.1: the workspace's
proc-macro crates (`syn`, `quote`, `serde_derive`, `thiserror-impl`, `windows-implement`,
`windows-interface`, `zerocopy-derive`) all build with the flag in effect, so no explicit
`--target` is needed and the CI artifact paths are unchanged. **Should a future toolchain
change that, the fix is `--target x86_64-pc-windows-msvc` — and note the artifact then moves
to `target/x86_64-pc-windows-msvc/release/`, which four workflow steps reference.**

The original finding, against the pre-fix artifact (19 raw import entries, 18 unique):

- **Reject:** `VCRUNTIME140.dll` plus six `api-ms-win-crt-*` modules were **present** before
  the fix. Precisely: `VCRUNTIME140.dll` is the fatal one — it ships only with the Visual
  C++ redistributable, so without it the OS refuses to map the DLL, the game's static import
  of `d3d11.dll` fails, and *the game does not start at all*. The `api-ms-win-crt-*`
  forwarders and `ucrtbase.dll` are Windows 10 components and do resolve there; they are not
  independently fatal. They still fail the gate, because **a static-CRT build emits none of
  them, so any hit proves the flag did not reach that build** — which is the stronger and
  more useful signal. Fix: `-C target-feature=+crt-static` (reference: `Nexus.vcxproj:545`
  links the static CRT). Reject `vcruntime*`, `msvcp*`, `msvcr*`, `ucrtbase*`,
  `api-ms-win-crt-*`.
- **Permit:** `d3dcompiler_47.dll` and `xinput1_4.dll` — the reference links these too, via
  ImGui's own pragmas. A naive denylist over-rejects here.
- **Decide:** `bcryptprimitives.dll` puts the floor at Windows 10. The reference's
  `PathCch`/`shcore` put it at Windows 8. Record the supported-OS floor as a decision; on
  an unsupported OS the failure is indistinguishable from the CRT bug.
- **Match module names case-insensitively** — the current build imports both `KERNEL32.dll`
  and `kernel32.dll`.

**A2. `VERSIONINFO` present with `FileVersion` == package version. Status: closed.** The
workspace version is now `0.1.0`, `crates/nexus-runtime/build.rs` generates the resource
script from `CARGO_PKG_VERSION` and embeds it via `embed-resource`, and
`xtask verify-version` asserts the embedded `VS_FIXEDFILEINFO` matches. CI runs it.

The resource directory went from **size 0 to 880 bytes**; the image reports
`FileVersion 0.1.0.0`, `ProductName "Tessera"`, and a `FileDescription` carrying no
trademarked game name (§2.5).

The script is **generated rather than checked in**, so the embedded version cannot drift
from the package version — there is only one source of truth. Flip-tested: the same gate
binary exits `0` on the real image and `1` on a copy with only the 8-byte resource data
directory zeroed.

Not cosmetic — a third-party GW2 addon manager can treat an unversioned file as unknown and
overwrite the swapped DLL.

Two follow-ups this does **not** cover: the four `StringFileInfo` values are asserted only
by the binary `VS_FIXEDFILEINFO` comparison, not string-by-string; and per #22 the release
version must eventually equal what the update endpoint advertises.

**A3. Resource inventory.** The reference ships fonts, icons, banners, locale tables and
the third-party notice blob *inside* the image (`res/Nexus.rc`, `res/ResConst.h`), so one
file drop is a complete install. Assert the decision per resource id — an empty resource
directory is currently indistinguishable from "not started".

**A4. The single highest-value gate: an external offset oracle. Status: closed for
revision 6.** `cargo run -p xtask -- verify-abi` emits the offsets Rust actually computes
into a C++ translation unit of `static_assert`s against the vendored MIT upstream header
and compiles it with MSVC. **60 layout facts confirmed.** Nothing is linked or run:
compilation succeeding *is* the agreement, and a mismatch is a compiler error naming the
field. CI runs it.

This removes the human from the loop, which was the whole point — every other layout check
compares Rust against expectations a human read off a header, so a shared mistake passed
silently.

Flip-tested: injecting `+ 8` into one sub-table offset yields
`error C2338: static assertion failed: 'AddonApiV6 sub-table at upstream member
GameBinds_PressAsync'`. Three unit tests additionally guard the vacuous-pass mode, because
a translation unit that asserts *nothing* also compiles: the table must hold ≥50 facts,
each fact must emit exactly one assertion, and the generated unit must include the
vendored header.

**Scope boundary — what this cannot settle.** `vendor/nexus-api/Nexus.h` declares
`NEXUS_API_VERSION 6` and exactly one `AddonAPI_t`, so **revisions 1–5 are absent from the
public header** and remain checked against hand-read expectations in `nexus-abi`'s own
tests. Separately, upstream `NexusLinkData_t` **ends at `FontUI` (40 bytes)** where the
host publishes 56 with the quick-access fields — the public header is behind the host
struct, so only the shared prefix is externally verified. The boundary itself is asserted,
so a future header that grows the struct is noticed rather than silently leaving the tail
unchecked.

Still outstanding from this gate: interior offset assertions for v2/v3/v4 (size-only
today), and compile-time asserts on the literal `"GetAddonDef"` and
`imgui_version_num() == 18000`.

**A5. Export directory:** name == `d3d11.dll`, 20 non-forwarder exports, no duplicate
ordinals, `AddressOfEntryPoint` not reaching Nexus code.

**A6. TLS callbacks and load-time purity. Status: closed, with one suggestion corrected.**
`xtask smoke-proxy` now asserts, before calling any export, that the image registers no TLS
callback beyond Rust's own and that loading it creates no window and no `addons` directory.
CI runs it.

**The original suggestion — "assert `AddressOfCallBacks` is empty" — is impossible to
satisfy in Rust and was measured to be so.** Every Rust `cdylib` carries exactly one
callback: the standard library's thread-local destructor hook, emitted into `.CRT$XLB`
whether or not any `thread_local` exists. Our proxy has one at a fixed address. The gate
therefore asserts *exactly one*, so nothing **additional** can be registered.

**And the hazard that motivated it is not machine-checkable here.** A `thread_local` whose
`Drop` takes a lock is a genuine loader-lock deadlock during `DLL_THREAD_DETACH` — but std's
single hook services every thread-local, so the callback count does not move when one is
added. That stays a **review rule**, not a gate. Recorded so nobody re-derives the check and
concludes it covers the hazard.

Flip-tested by splicing a second entry into the callback array (8 bytes changed): the gate
reports `the image registers 2 TLS callbacks`. The window check was flip-tested against a
fixture that calls `CreateWindowExW` in `DllMain`.

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
- Load the DLL and assert no window and no created directory *before* the first export
  call — **done** (§4.A6). Note the survey's "unchanged thread count" is **not** sound and
  was dropped after measurement: `LoadLibrary` makes Windows start its own loader worker
  threads, so a proxy that runs no code at all still moved the count by +2. Attributing a
  thread to the module needs each thread's Win32 start address tested against the module's
  address range; a raw count fails on innocent input, which is worse than no check.
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
| 1 | ● | unreachable | **Zero addons load.** No scan, no DLL loaded, no load callback, no API table | `HoContext.cpp:64-70`, `Loader.cpp:252-255,389-410` | `nexus-runtime` still depends only on `nexus-addon-backend` among the addon crates. **Progress:** `ProductionAddonApiBackend::compose` is now the single wiring point for all 13 adapters and is tested by serving a real path call through a fully composed backend — it previously had no non-test constructor at all. Remaining: the runtime dependency edges plus a call to `compose` with its own services. Root cause of ~20 other `unreachable` items |
| 2 | ● | diverges | Overlay attaches; must not require a window-class literal | Class-agnostic: `Hooks.cpp:232-242`, `PlContext.cpp:34-56`. `ArenaNet_Dx_Window_Class` appears **nowhere** in the C++ tree (verified) | `dxgi.rs:31` + `set_require_expected_game_window(true)`; rejections at `classifier.rs:46-49,98-106` |
| 3 | ● | matches | Byte layout of every addon-facing structure: API tables v1-v6, AddonDefinition, Version, InputBind, Texture, Mumble data/context/identity, NexusLink | `ApiV1.h`–`ApiV6.h`, `AddonDefV1.h:28-44` | **Revision 6 now externally verified** by `xtask verify-abi`: MSVC checks 60 facts against the vendored MIT header (§4.A4). v1–v5 are absent from the public header and remain author-computed; upstream `NexusLinkData_t` stops at `FontUI` so its quick-access tail is likewise unverified |
| 4 | ● | partial | An addon's unload routine can still call every API function it used during load | Order is: invoke the addon's unload export while the complete API is functional for it, then sweep host-side registrations pointing into the module, then release it (`Addon.cpp:418-432`) | **The crash vector is fixed.** `request_unload` no longer closes addon-to-host API admission; `invoke_native_unload` closes it once the export has returned, so `GetAddonDirectory` no longer hands back a null pointer that addons concatenate. Flip-tested. **Still diverging:** the host-side registration sweep still runs *before* the unload export rather than after, so a registration an addon creates during its own unload would survive where the reference sweeps it — see §2.7 |
| 5 | ● | diverges | API calls work from any thread and any call stack | No caller authentication; attribution failure never denies (`ApiBuilder.cpp:173-215`) | `boundary.rs:147-162` fails closed; `dispatcher.rs:295-308` returns null. Breaks worker threads, timer callbacks, MinHook trampolines |
| 6 | ● | diverges | Non-UTF-8 / long / heap-allocated addon metadata still loads | Opaque NUL-terminated bytes; no provenance check | `definition.rs:430` rejects invalid UTF-8; ceilings reject rather than truncate; `module.rs:312-361` rejects out-of-image definitions |
| 7 | ● | diverges | A symlinked DLL loads; one locked file doesn't disable every addon | Symlinks resolved; one bad entry skipped, never fatal | `discovery.rs:171-173` skips symlinks; `:161-195` returns `Err` for the whole scan, discarding every entry already found |
| 8 | ● | absent | Volatile addons auto-disable after a >350-build game patch | `BuildInfoService.cpp:10-43`, `Addon.cpp:873-897` | `VOLATILE` declared at `addon.rs:120` with **no reader**. Zero hits for `assetcdn`/`latest64` (verified). Turns a controlled refusal into a crash on patch day |
| 9 | ● | partial | String-returning API functions never return null | No failure path exists; each requested name is interned and its pointer stays valid forever (`ApiBuilder.cpp:175-215`) | **The interning ceiling no longer refuses.** It is now an advisory threshold: a name past it still interns to a valid, stable pointer and the event is counted for diagnostics, so a getter cannot hand back a null that an addon concatenates. `CapacityExceeded` is removed as unreachable. **Still outstanding:** a getter can return null when caller attribution fails — that is #5, and these read-only queries own no resource so they should be served without attribution |
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
| 20 | ● | diverges | Texture API: NULL-on-first-miss polling, and a `Texture*` that stays dereferenceable for the process lifetime across resize, fullscreen toggle and device loss | Records are process-lifetime allocations owned by one host-wide registry; neither a resize nor a rebuild frees them or clears the identifier map (`TxLoader.cpp:48-63,137-183`) | **Lifetime now decided — see §2.6.** An SRV is device-scoped, not swap-chain-scoped, so retiring the service on re-attach was a defect rather than a necessity. **Process-scoped retention now implemented and flip-tested** (§2.6); device-loss re-upload is still outstanding. Also still to fix: `service.rs:668-694` queues a cached hit instead of dispatching inline, and the caps sit below what C++ accepted |
| 21 | ● | unreachable | Self-update, per-addon auto-update, library install, patch mutex | `Updater.cpp:19-318`, `Addon.cpp:588-1309` | Every `update.rs` public fn has zero callers outside the file. Zero hits for `RCGG-Mutex` (verified). No cadence, no scheduler, no library source. The mutex must land in the **same change** as the updater |
| 22 | ● | partial | ~~`VERSIONINFO`~~, third-party notices on disk, a downloadable release | `res/Nexus.rc:56-88` | **`VERSIONINFO` done** — `FileVersion 0.1.0.0`, resource directory 880 bytes, gated by `xtask verify-version` (§4.A2). Still absent: notices list 4 entries against 129 lockfile packages, and there is no tagged, stamped, checksummed release job |
| 23 | ● | absent | A second GW2 client launches; multibox state logged | `Multibox.cpp:128-188`, substring `AN-Mutex-Window-Guild Wars 2` | Options parsed, never read. No mutex code anywhere. Mainstream GW2 workflow |
| 24 | ● | absent | Live GW2 bind tracking, so addon-issued binds press the player's key | `GbApi.cpp:24-99`, Unofficial Extras event | Zero hits for `UNOFFICIAL_EXTRAS` (verified). Every addon-issued game bind presses the **default** key |
| 25 | ● | partial | `"ALT+["`, `"CTRL+NUM 7"`, `"SHIFT+CAPS LOCK"` resolve, on any layout | Full OS key-name set, scan codes 0..255 × {plain, extended}, **active layout** | `bind.rs:343-413` covers 61 names, no punctuation/keypad/lock keys; `&UsKeyNames` hardcoded; `parse_bind_lossy` drops modifiers on error. Same table feeds bind *display* |
| 26 | ● | unknown | Numeric mouse-button codes in `InputBinds.json` | Values live in `Util/Inputs.h` — an **uninitialized submodule**, not verifiable from this tree | `bind.rs:34-48` pins None=0…X2=5. Settle from the public MIT `Nexus.h`. A shifted value rebinds the wrong physical button and looks like user error |
| 27 | ● | diverges | Exact message tuples a game-bind press/release puts on the wire | Alt-modified main key uses **ordinary** `WM_KEYDOWN` with lParam bit 29 (`GbApi.cpp:270-275,374-379`) | `game.rs:706` sets `system: binding.alt` → `WM_SYSKEYDOWN`, declared an intentional correction. Visible to the game *and* every other addon's WndProc — rule 2.2 |
| 28 | ● | partial | Overlay renders even when the swap chain wasn't created via a Nexus export | Process-wide: implementation addresses patched at vtable slots 8/13/22 | Per-instance vtable pointers only; `HookMode::GlobalFallback` accepted but unimplemented. Also detectable by other overlays — needs coexistence testing |
| 29 | | absent | `Crash.log`/`CrashStack.log` with fixed layout | Vectored handler at chain **front**, sees first-chance, does not dismiss | Path keys only; no handler, no stack walk, no writer. Trap: `SetUnhandledExceptionFilter` is quieter but changes what the user sees |
| 30 | | diverges | Escape closes the front-most Nexus window, if the user enabled it | Walks the **live** ImGui stack, skipping the bottom entry | `escape.rs` reproduces the gates and is wired, but the "stack" is *registration order*, and `set_enabled` has no production caller so `CloseOnEscape` is ignored |
| 31 | | matches | 177 bind ids, 100 scan codes, 111 defaults, log levels, 63 HTTP phrases, MD5, enums | Exact-value contracts; dispatch is a plain `msg->Level <= sink.Level` (`LogApi.cpp:40`) and `StringFrom` renders anything outside `CRITICAL..TRACE` as `(null)` (`LogConst.cpp:18-31`) | Two surveys diffed these tables programmatically: identical. **The log-level rejection is fixed.** It had *three* rejection points, not the one the survey named: `nexus-addon-backend/src/logging.rs` refused any level outside 1..5, `nexus-platform`'s `log()` returned early for a non-message level, and `allows()` gated on `is_message()` so such a record was filtered at every sink even if admitted. All three now match the reference's plain numeric compare. `legacy_label()` already rendered `(null)` correctly |
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
| 54 | | diverges | Which addon loads first — decides who wins a contested Quick Access shortcut or DataLink identifier, and `EV_ADDON_LOADED` order | Filesystem order, each addon loaded **inline as discovered** (`Loader.cpp:252-387`) | **Decided — see §2.8.** Sorted-by-lowercased-path and batched, already implemented and tested. Deterministic across machines where filesystem order is not. Batching is unobservable because `AddonAPI_t` exposes no addon-enumeration function (verified against the vendored header), so peers are visible only via `EV_ADDON_LOADED`, whose order the sorted sequence preserves |
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

**Settled, no longer open:** branding is removed and the product is `Tessera` (§2.5). The two
broken reference paths (no-op uninstall, throwing update check) are **implemented properly**,
not reproduced (§2.2). The asset programme is independently authored throughout — icons,
banners, style presets, typefaces and all 11 locale tables, none copied (`ROADMAP.md` §5).

**Still open, and functional rather than cosmetic:** where the in-game addon library's
catalogue comes from once the vendor's URLs are gone (§2.5). Decide before Phase 6.

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
