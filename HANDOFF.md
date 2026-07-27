# Nexus-Rust engineering handoff

This document is the durable, self-contained handoff for continuing the Rust
implementation. It records the repository state, verified architecture, exact
remaining invariants, and the intended continuation order. All required design
content is included below.

## Stop: use the correct repository

The canonical worktree is:

```text
C:\AI_STUFF\PROGRAMMING\Nexus-Rust-publish
```

The active branch is:

```text
feat/production-addon-runtime
```

At the time of this handoff its parent checkpoint is:

```text
2f0b3ed72a174bf547721abfd4a3e7b374ac0851
```

Do not implement this work in:

```text
C:\AI_STUFF\PROGRAMMING\Nexus
```

That is a different, stale worktree on `rewrite/rust-latest` at `93ffdd7`. It
contains the C++ history and an older copy of the Rust crates. The two branches
have no merge base even though their Rust crate names look alike. Do not merge
or rebase them together.

Additional Git traps:

- Use `origin/main` explicitly as the publication base.
- Local `main` tracks `cpp-reference/main`, not `origin/main`.
- Local `publish/main` tracks the actual `origin/main`.
- `cpp-reference` points to the upstream C++ repository.
- Local tags came from the C++ reference history; `origin` has no tags.
- Never use `git push --tags` or `git push --mirror`.
- Keep the publication history free of the C++ repository's commits and assets.

The public repository is
<https://github.com/special-place-ai-heaven/Nexus-Rust>.

## Mission and acceptance standard

Nexus-Rust is the user's project, not a source-level clone. The compatibility
goal is that existing users and addons should not notice a behavioral break
when moving to the Rust host. Compatibility is about public interfaces and
observable behavior, not copied implementation.

The minimum product target is a safe, behavior-compatible Rust host. The north
star is a more stable and controllable implementation, especially around:

- NVIDIA and auxiliary-overlay swap-chain selection;
- deterministic addon ownership and unload;
- generation-safe callback cleanup;
- bounded queues and memory use;
- panic containment at every native boundary;
- fail-closed ABI behavior;
- recoverable device loss, resize, fullscreen, and render-session reselection;
- clear diagnostics rather than silent no-ops.

Passing unit tests is necessary but not the acceptance criterion. A release
must also work in a real Guild Wars 2 process as a primary and chainloaded
proxy, load representative addons across API revisions, preserve visible UI
and input behavior, and survive unload/reload and graphics lifecycle changes.

Current honest status: the crate-level foundations are substantial, but this
is still an unreleased alpha and not a functioning drop-in addon host. The
native backend, manager, cleanup, watcher, and update building blocks are not
yet composed into the runtime DLL, and the functional Nexus-owned UI has not
been ported.

## Legal and provenance boundary

Read these before compatibility work:

- `README.md`
- `CONTRIBUTING.md`
- `PROVENANCE.md`
- `THIRD_PARTY_NOTICES.md`

The repository is Apache-2.0 and has a clean, independent publication root.
Preserve that boundary. Permitted compatibility evidence is:

- publicly licensed interfaces;
- public documentation and release notes;
- black-box observations from ordinary supported use;
- independently authored tests, traces, and fixtures.

Do not copy proprietary implementation text or import unlicensed binaries,
artwork, fonts, styles, icons, branding, or other assets. The normative addon
ABI source is the MIT-licensed `RaidcoreGG/Nexus-API`; the game-bind table is
tied to public revision
`9b2c53df86c00db6495642bfcff2d0611bd957ef`.

This is an engineering policy, not a legal opinion. The third-party notice
inventory is explicitly incomplete and must be completed before release.

## Branch, toolchain, and CI state

Before this handoff document was added:

- the worktree was clean;
- local and upstream `feat/production-addon-runtime` both pointed at `2f0b3ed`;
- the branch was 10 commits ahead of and 0 behind `origin/main`;
- no pull request existed;
- the latest GitHub Actions run passed:
  <https://github.com/special-place-ai-heaven/Nexus-Rust/actions/runs/29782777222>.

The workspace contract is:

- Rust and Cargo `1.97.1`;
- target `x86_64-pc-windows-msvc`;
- Rust edition 2024;
- Cargo resolver 3;
- 36 crates plus `xtask`;
- tracked `Cargo.lock`;
- `panic = "unwind"` in development and release;
- Clippy `all`, `undocumented_unsafe_blocks`, and `unwrap_used` denied;
- `unsafe_op_in_unsafe_fn` denied;
- missing documentation warned;
- LF line endings enforced by `.gitattributes`.

Foreign/native calls must still catch panics and fail closed. `panic = "unwind"`
does not permit unwinding across a C ABI boundary.

The authoritative CI workflow is `.github/workflows/rust.yml`. It runs these
gates on `windows-latest`:

```powershell
cargo fmt --all --check
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
$env:RUSTDOCFLAGS = "-D warnings"
cargo doc --workspace --no-deps
cargo build -p nexus-runtime --release
cargo run -p xtask --release -- verify-exports target/release/d3d11.dll
cargo run -p xtask --release -- smoke-proxy target/release/d3d11.dll
```

The CI release-profile artifacts are `target/release/d3d11.dll` and
`target/release/d3d11.pdb`. `xtask` verifies 20 named proxy exports and their
ordinals.

The user's orchestration limit is at most four simultaneous Cargo
build/test/check processes. Many agents may inspect or code in parallel, but
coordinate filesystem ownership and let one designated agent run the gates.
This is a working policy, not a repository `.cargo/config.toml` setting.

Commit and push every coherent checkpoint. Do not bundle unrelated milestones
into one large commit.

Never print, paste, or commit a secret. If one is discovered, report only its
configuration name and file location, never its value.

For code intelligence, additively index the canonical worktree with SymForge.
Project IDs are session-specific, so a new agent should call `index_folder`
with `add=true` and the canonical path rather than copy an old project ID.
Start with a low-detail repository map. Before reading a source file, request
its file context; before text search, use symbol-grouped search with references;
before editing, request conventions and an edit plan; after every edit, run
file-impact analysis so the index and affected-consumer report stay current.
Fall back to literal file reads only for documentation, unindexed files, or a
reported SymForge failure.

## What is already implemented

The project contains real implementations, not a stub skeleton:

- public addon ABI revisions v1 through v6 and the legacy flat tables;
- real `GetAddonDef` export resolution;
- D3D9, D3D11, and DXGI proxy/chainload support;
- export verification and proxy smoke tests;
- selected-swap-chain lifecycle and NVIDIA auxiliary-overlay diagnostics;
- ImGui/D3D11 render infrastructure;
- addon loader, manager, ownership, cleanup, and watch foundations;
- Mumble, DataLink, NexusLink, settings, logging, paths, and events;
- texture, localization, font-manager, input-bind, game-bind, WndProc, inline
  hook, WinHTTP, and update transaction primitives;
- bounded and generation-aware registration APIs;
- self-update source selection and planning primitives.

The current runtime composition is also substantial:

- `runtime::initialize` enters `services::initialize`;
- `RuntimeServices::build` owns paths, settings, DataLink, events, NexusLink,
  Mumble, input, game input, `UiHost`, fonts, textures, localization, scaling,
  logging, and the scheduler;
- `dxgi::build_services` installs the `OverlayAdapter`;
- the current render-observer nesting is game input -> fonts -> textures;
- render attachments expose the live swap-chain pointer, D3D11 device, ImGui
  context, HWND, swap-chain ID, and generation;
- Addons-stage frames already advance localization, fonts, textures, and
  `UiHost` callbacks in the intended host order.

Texture is ready for native composition:
`RuntimeTextureCoordinator` implements `TextureServiceFacade`, scopes work to
the selected session, keeps an in-flight guard through lazy acquisition, and
closes/drains during detach.

Localization is ready for native composition:
`RuntimeLocalization` owns the synchronized service, exposes
`backend_service`, cleans an owner synchronously, and latches one later
glyph-refresh frame.

Update ingress is implemented but orphaned:
`UpdateApi` is bounded, FIFO, owner-attributed, generation-exact,
gate-rechecked, cancellable, and atomically drainable. No production
coordinator consumes `try_dequeue`.

## Two important status corrections

### The production backend is composed, but not installed

`ProductionAddonApiBackend` in
`crates/nexus-addon-backend/src/production.rs` implements all 62
`AddonApiBackend` methods.

Its 28 core operations delegate to:

- `UiApi`;
- `LoggingApi`;
- `PathApi`;
- `InlineHookApi`;
- `EventApi`;
- `DataLinkApi`.

Its 34 required operations delegate to:

- update;
- WndProc;
- input binds;
- game binds;
- textures;
- localization;
- fonts.

All seven required adapter implementations exist. Tests provide a compile-time
complete-backend assertion and exact-argument delegation coverage.

The gap is not "implement the production backend." The gap is production
construction and installation. Non-test references are limited to the
definition, implementation, and re-export. Every concrete adapter constructor
is likewise used only in tests.

`nexus_addon_ffi::install_render_session` is complete. It atomically installs
an `Arc<dyn AddonApiBackend>`, builds pinned v1-v6 tables from the selected
swap chain and ImGui context, and returns an `InstalledAddonApi` lease. It is
called only by tests. `AddonManager` and `ManagerRuntime` are also not
constructed by `nexus-runtime`.

### The font coordinator exists; the addon-facing bridge does not

`crates/nexus-runtime/src/fonts.rs` already contains a real
`RuntimeFontCoordinator` and render-session font lifecycle:

- attach, select, advance, GPU-rebuild take, detach, shutdown, and failure;
- a render-thread-local `FontManager`;
- full `FontRebuildRequest` application and fallback behavior;
- ImGui-context, render-thread, and attachment-generation checks;
- a production font observer composed with the D3D11 texture observer;
- a combined lease whose drop order removes font state before texture state;
- panic containment that avoids a second unwind from foreign panic payloads;
- tests for stale leases, drop order, replacement errors, panic containment,
  and stale-publication prevention.

The precise missing invariant is that `FontApi` requires an
`Arc<dyn RenderFontService>`, but `RuntimeFontCoordinator` does not implement
that trait. Only a test recorder does. Arbitrary native caller threads
therefore cannot synchronously and safely reach the render-thread-local
`FontManager`.

Do not describe fonts as "not started." The missing work is the bounded,
synchronous addon-facing bridge plus phase-correct font cleanup.

## First milestone: split font cleanup by lifecycle phase

This should land as a focused checkpoint before production backend wiring.

The cleanup model deliberately has two font domains:

1. `FontCallbacks` in `CallbackRegistrations`, before callback-gate drain.
2. `FontResources` in `OwnedResources`, after callback-gate drain.

Current behavior is incorrect for that contract:

- `FontManager::cleanup_owner` removes owner claims and subscribers together;
- `FontApi::cleanup_owner` calls that combined operation and then removes its
  native subscription receipts;
- the only direct cleanup adapter is `font_resources`, and it calls the
  combined cleanup;
- there is no `FontCallbacks` adapter.

Implement these exact phase operations:

```text
cleanup_owner_callbacks(owner)
    remove only subscribers for the exact owner generation
    abort/drop queued native callback publications for that owner
    remove FontApi subscription receipts
    fence all earlier accepted callback operations
    do not remove font entries or invalidate the atlas

cleanup_owner_resources(owner)
    remove exact-generation owner claims
    sweep entries that are now unreferenced
    include entries made unreferenced during callback cleanup
    invalidate the atlas only if registry contents changed

cleanup_owner(owner)
    call callback cleanup, then resource cleanup
    retain the legacy combined behavior and summed removal result
```

The current receipt map retains subscription IDs, while aborting a staged
publication requires an abort-capable callback/publication object. Extend the
receipt/publication state to retain an owner-scoped abort handle before
claiming that cleanup can abort queued publications. Callback-gate suppression
alone can prevent a later native call, but it is not the same as removing and
fencing the queued publication.

Expose the two synchronous phase barriers through `RenderFontService` and
`RuntimeFontCoordinator`. Add separate cleaner adapters:

- the `FontCallbacks` adapter invokes callback cleanup before gate drain;
- the `FontResources` adapter invokes resource cleanup after gate drain.

Required properties:

- exact `OwnerToken` generation matching;
- idempotency;
- retry safety after partial cleanup;
- no callback can begin after callback cleanup returns;
- font resources remain available until the post-drain resource phase;
- atlas rebuild is requested only for actual resource-registry changes.

Refresh `crates/nexus-addon-cleanup/src/inventory.rs` when this lands. Its
static gap list is stale: recent work already fixed localization
acknowledgement, texture coordinator privacy, and cleaner outer-lock issues.
Do not treat every present `API_GAPS` entry as current truth.

## Second milestone: bounded synchronous render-font bridge

The bridge must be synchronous from the native API's perspective. "Put the
request in a queue and return" is not compatible.

### Required behavioral contract

- Every accepted call completes the matching `FontManager` operation before
  returning.
- Calls on the active render thread execute inline.
- Calls from other threads enter one bounded FIFO and wait for a terminal
  response.
- If older queued work exists when an inline call arrives, drain the older
  work first, then execute the inline operation. Global ordering must remain
  FIFO.
- Commands carry the exact attachment ID that accepted them. Work accepted by
  an old render selection must never execute in a new ImGui context.
- Validate the active session, render thread, ImGui context, and attachment
  again immediately before execution.
- Drain the queue at the beginning of `RuntimeFontCoordinator::advance`,
  before normal font-manager advance and atlas rebuilding.
- A callback can re-enter while `FontManager` is mutably borrowed.
  `try_borrow_mut` failure must reject closed immediately. Never panic, enqueue
  onto the same render thread, or wait on itself.
- Queue full, no active session, stale generation, detach, coordinator failure,
  and shutdown are atomic failures with no partial registration.
- Make command mutation transactional where practical. Panic containment by
  itself cannot undo a mutation that already occurred. If a panic may have
  followed a partial mutation, mark the attachment failed, close admission,
  reject its queued work, invalidate pending publication, and use the existing
  runtime font-failure path rather than returning an ordinary rejection while
  leaving the attachment active.

### Queue and ticket design

Use fully owned commands:

```text
Get
Release
AddMemory
Resize
CleanupCallbacks
CleanupResources
```

File and Windows resource registration copy their bytes first and then become
`AddMemory`; a queued command must not retain a path, borrowed buffer, raw
module handle, or resource pointer.

Each pending command should contain:

- the exact attachment ID;
- owned arguments, callback wrapper, and copied config;
- a one-shot response channel;
- an atomic ticket state:
  `QUEUED`, `CLAIMED`, `CANCELED`, or `COMPLETED`.

Bound both command count and retained bytes. A count limit alone is not a
memory limit: 256 commands carrying large font buffers can retain gigabytes,
and `FontManager::register_memory` currently clones the supplied bytes.
Recommended initial, injectable limits are:

```text
normal command capacity: 256
pre-claim wait timeout: 2 seconds
MAX_ADDON_FONT_BYTES: 16 MiB
MAX_QUEUED_FONT_BYTES: 64 MiB
```

The 16 MiB command limit matches the current native-memory snapshot boundary.
If compatibility requires a larger native addon font, raise the boundary,
per-command, aggregate-queue, and peak-memory tests together. Keep the
separate existing user-font policy deliberate rather than accidentally
letting its larger limit bypass the native boundary. Reserve aggregate bytes
atomically on admission and release the reservation on cancellation,
rejection, or completion. Account for the temporary clone made during manager
registration in peak-memory tests.

Producer algorithm:

1. Lock coordinator state and validate that admission is open for the exact
   active attachment.
2. Prune canceled tickets without reordering live work.
3. Reserve the command slot and byte budget, rejecting atomically if either
   bound would be exceeded.
4. Enqueue as `QUEUED`.
5. Drop the coordinator lock before waiting on the response.
6. On timeout, compare-and-swap `QUEUED -> CANCELED`.
7. If cancellation succeeds, atomically take/drop its payload (or remove it
   under the coordinator lock), release its byte reservation, and return
   `ServiceRejected`; that command must never mutate font state later.
8. If cancellation loses to `CLAIMED` or `COMPLETED`, wait for the real
   terminal result. Never report failure after a mutation may have started.

Consumer algorithm:

1. Under the coordinator lock, take only the FIFO snapshot present at the
   start of the frame drain so producers cannot starve normal font
   advancement, then release the lock.
2. Compare-and-swap each ticket `QUEUED -> CLAIMED`.
3. Skip canceled tickets.
4. Revalidate the attachment and render context.
5. Execute the manager operation under `try_borrow_mut` with panic
   containment.
6. Release the TLS borrow before response delivery and before the initial
   staged publication is finished on its originating API caller.
7. On a contained panic, follow the failed-attachment rule above, resolve the
   current and rejected queued waiters, and release their byte reservations.
8. Otherwise send exactly one terminal result, mark the ticket completed, and
   release its byte reservation.

Never hold the coordinator mutex across a manager operation, callback
publication, response wait/send, detach, or shutdown. The TLS `RefCell` borrow
must be limited to the manager operation. A later already-published manager
notification can invoke native code during that operation; a reentrant font
API call must observe the borrow and fail closed through `try_borrow_mut`.

Detach, reselection, failure, and shutdown close admission for the exact
attachment, cancel all not-yet-claimed tickets, drop their retained payloads,
release their byte reservations, and wake their waiters. Claimed operations
may finish. A stale lease must not close or cancel a newer attachment.

Cleanup commands are unload barriers. Execute them inline only for a caller
validated as the active render thread. Off-thread cleanup must use reserved
control admission and synchronously wait, so a full normal queue cannot
prevent unload forever.

Preserve the existing mixed-thread staged callback design.
`SendFontCallback` already requires `Send`. For the initial `Get`/registration
publication, the service-side callback stages the current font; after the
synchronous service operation returns, `FontApi` finishes and publishes it on
the originating API-caller thread. Once that publication is marked published,
later atlas/rebuild notifications enqueued by the render-thread manager may
drain and invoke the native callback on the render thread. Both paths must keep
the callback-gate and exact owner-generation checks. Do not force all
notifications onto one thread, and do not add unsafe thread-safety erasure.

### Bounded font asset acquisition

For files:

- perform a metadata size precheck;
- open the file and read through `Read::take(MAX + 1)` to handle time-of-check
  changes;
- reject empty, oversized, or failed reads;
- copy into an owned `Vec<u8>` before enqueueing;
- enforce the same deliberate per-addon and aggregate queued-byte limits used
  by memory and resource registration.

For Windows resources:

- reject a null module and a resource ID of zero or greater than `u16::MAX`;
- pin the module with `GetModuleHandleExW(FROM_ADDRESS, ...)`;
- use integer resource type `RT_FONT` (8);
- find and size the resource;
- reject zero or over-limit sizes;
- load and lock it;
- copy into an owned `Vec<u8>` while the module is pinned;
- release the module pin only after the copy;
- enqueue only the owned bytes;
- fail closed on non-Windows targets.

The current file path uses an unbounded read, and resource loading deliberately
returns unsupported. Both must change before exposing the bridge.

### Required bridge tests

- active render-thread calls execute inline;
- off-thread calls synchronously wait and preserve FIFO order;
- an inline call drains older queued work first;
- `Get` stages and publishes the current font on the originating caller before
  the API call returns;
- command and aggregate-byte capacity reject atomically;
- cancellation and detach release retained payloads and byte reservations;
- a timed-out queued ticket never executes later;
- detach/shutdown wakes every waiter;
- stale attachment work cannot cross into a new selection;
- cleanup barriers stay ordered with ordinary operations;
- callback re-entry fails closed without deadlock;
- a panic resolves the waiter and leaves no stale publication;
- empty, oversized, failed, and time-of-check-changing file reads fail closed;
- invalid resource arguments fail closed;
- callback publication and requested GPU rebuild ordering are preserved.

## Third milestone: construct the production backend once

The process composition root is `RuntimeServices::build`. Construct and retain:

1. one shared `Arc<AddressOwnershipIndex>`;
2. an `AddonCallerResolver` using that exact index;
3. one `BackendFailures`;
4. one `NativeMemoryReader`;
5. one shared `NativeCallBoundary`;
6. one shared instance of every domain service and native adapter;
7. `CoreAddonApiServices`;
8. `RequiredAddonApiServices`;
9. one `Arc<ProductionAddonApiBackend>`.

The exact same ownership index must be given to
`ManagerRuntime::with_address_ownership_index`. A second index would break
caller attribution and callback-generation safety.

The backend and `RegistrationCleaner` must reference the exact same service
instances. Duplicate UI, event, input, hook, texture, font, or localization
registries can make cleanup report success while live callbacks remain in the
registry actually used by addons.

Concrete adapter mapping:

```text
UiApi             shared boundary + existing UiHost
LoggingApi        boundary + shared Arc<LogRegistry>
PathApi           boundary + one bounded StablePathStore::from_index
InlineHookApi     boundary + one retained InlineHookService
EventApi          boundary + existing EventService
DataLinkApi       boundary + existing DataLinkService
UpdateApi         boundary
WndProcApi        boundary + raw registry + game-only message sink
InputBindApi      boundary + managed-input registry
GameBindApi       failures + invoker slot + shared scheduler
TextureApi        boundary + RuntimeTextureCoordinator
LocalizationApi   boundary + RuntimeLocalization::backend_service
FontApi           boundary + completed RuntimeFontCoordinator bridge
```

Small plumbing needed:

- add narrow crate-private clone accessors for
  `RuntimeInputServices`' managed-bind and raw-WndProc registries;
- add narrow accessors for `RuntimeGameInput`'s game-only message sink and
  invoker slot;
- store `LogRegistry` as `Arc<LogRegistry>`;
- store the scheduler as `Option<Arc<MinimalScheduler>>`; passing `None` to
  `GameBindApi` silently disables asynchronous bind parity;
- instantiate and retain one `InlineHookService`;
- build `StablePathStore::from_index` once with an explicit bounded addon-path
  capacity;
- add the missing runtime dependencies for addon FFI, manager, loader,
  cleanup, watcher, host, inline hooks, and native memory.

Retain the backend for at least as long as every installed API catalog and
addon. Its stable path/localization returns intentionally rely on adapter-owned
storage.

Do not call `install_render_session` from `RuntimeServices::build`. The live
swap chain and ImGui context do not exist there.

## Fourth milestone: install and own the addon session

Add an outer addon session layer around the existing
game-input -> fonts -> textures chain, but do not put every lifecycle operation
inside the current `RenderSessionObserver::attach`/attachment `Drop`.

Two constraints in the present overlay interface matter:

- `RenderSessionObserver::attach` receives `RenderSessionResources`, not the
  current or maximum `RenderStage`;
- attachment `Drop` currently occurs while the thread-local render state is
  mutably borrowed.

Select whether to compose the addon-aware observer in
`nexus-runtime/src/dxgi.rs::build_services`, where `render_controls` is
available. If the configured maximum stage is lower than `Addons`, install only
the existing downstream observer and never install or activate native addons.
Alternatively, extend the observer interface explicitly; do not pretend the
current `attach` method can perform a stage check it cannot see.

For an addon-enabled selection, the resource-attachment phase is:

1. Attach the downstream observer so game input, fonts, and textures are live.
2. Call `install_render_session` with the exact generation, swap-chain pointer,
   ImGui context, and retained production backend.
3. Obtain `InstalledAddonApi::shared_catalog()`.
4. Give that catalog and the shared ownership index to `ManagerRuntime`.
5. Construct or bind the addon manager in a quiescent, not-yet-activated
   state.
6. Publish a combined attachment marked as awaiting first-frame activation.

Addon discovery/inspection/activation needs an explicitly current ImGui
context because load-time `FontApi::Get` and related calls validate that
identity. Context construction currently restores the prior context, so a raw
stored pointer is not enough. On the first actual `Addons`-stage frame:

1. move lifecycle work outside the mutable `THREAD_STATES` borrow, or add a
   dedicated lifecycle callback that is explicitly safe outside it;
2. enter an RAII current-context scope for the selected ImGui context
   (`D3d11Renderer::with_current_context` provides the existing semantic
   model, but its present borrowing shape may need refactoring);
3. revalidate the render generation and stage;
4. discover, inspect, and activate eligible addons;
5. restore the prior ImGui context on every success, error, and panic path.

If the stage policy drops below `Addons`, close admission and quiesce active
addons before continuing in the lower mode.

If resource attachment or activation fails, unwind only resources owned by
that attempt and return a closed render error. Do not leave a dispatcher
installed without its manager, or a manager active without its downstream
render services.

Teardown order is mandatory:

```text
stop directory-watch and update admission
close every owner callback gate
cancel exact-generation queued updates
request unload for every active addon
drive all cleanup phases and callback drain
invoke native unload
finish unload and release module ownership
retire/drop InstalledAddonApi
drop game-input/font/texture render attachments
destroy ImGui/render resources
```

Unload and cleanup need render-bound services to remain alive. Conversely, the
installed API catalog cannot be retired while native modules may still hold
its pointers.

Do not perform synchronous manager unload/drain from attachment `Drop` while
`THREAD_STATES` is mutably borrowed. Refactor retirement into two phases:

1. under a short render-state borrow, stop new render dispatch and move the
   addon attachment into a local retirement value;
2. release the borrow;
3. with the selected ImGui context explicitly current if cleanup requires it,
   quiesce/unload the manager and retire the installed API;
4. only then drop the downstream attachment and, if needed, reborrow render
   state to finish reselection.

Give the combined attachment an explicit `retire`/`quiesce` operation.
Ordinary `Drop` should only release an already-quiescent attachment or perform
a minimal fail-closed admission stop; it must not hide native callbacks,
manager mutation, or unbounded waits inside the borrowed thread-state path.

### Manager lifetime hazard

Do not blindly create a fresh `AddonManager` on every render reselection. The
manager owns per-signature generation counters. Recreating it may reuse an
`OwnerToken` generation while process-retained state from an earlier session
still exists.

Choose and test one of these designs before wiring reselection:

- retain one process-owned manager and add a quiescent API-catalog rebinding
  operation that preserves generation history; or
- move generation allocation to process-global, session-epoch-aware state.

The required outcome is non-negotiable: old addons are fully drained before a
new dispatcher becomes active, and owner generations never regress or repeat.

### Registration cleaner composition

Construct `RegistrationCleanerBuilder` with all nine real slots:

- inline hooks;
- UI callbacks;
- raw WndProc;
- managed input;
- events;
- textures;
- font callbacks;
- font resources;
- localization.

Use the exact instances used by the backend. Texture, localization, and font
need coordinator-aware runtime adapters rather than duplicate raw session
services.

The update queue is not currently represented as a cleanup domain. Either add
an explicit cleanup slot or make `cancel_owner` a mandatory manager-lifecycle
hook. Process shutdown must close and drain update admission.

Minimum integration proof:

- runtime constructs all adapters and `ProductionAddonApiBackend`;
- an Addons-stage selected render session installs the dispatcher;
- a real API-table shim reaches exactly one real service adapter;
- detach causes closed ABI defaults;
- lower safe modes never install or activate;
- activation happens with the exact ImGui context current;
- reselection drains the old generation before installing the new one;
- a stale installation lease cannot retire a newer dispatcher;
- owner generations remain monotonic across reselection;
- attachment retirement performs no manager mutation while `THREAD_STATES` is
  borrowed;
- shutdown ordering is covered end to end.

## Fifth milestone: manager, watcher, and update coordinator

Compose `ManagerRuntime` from:

- `StdDirectoryScanner`;
- the Windows loader platform;
- loaded-module address resolution;
- the shared address-ownership index;
- the real registration cleaner;
- the installed API catalog.

Drive discovery, inspection, activation, unload, watcher events, update
requests, and diagnostics through one runtime coordinator. Do not mutate the
addon manager from inside locked or borrowed render-callback sections.

`UpdateApi` currently accepts requests that nobody consumes. The coordinator
must define:

- ownership of dequeued work;
- cancellation of queued and claimed owner work;
- duplicate/coalescing policy;
- update planning, download, commit, and rollback transitions;
- cross-process serialization;
- close-and-drain behavior at shutdown.

Directory-watch code exists but is not attached to an active manager.
Self-update and addon-update transaction primitives exist but are not invoked
by production runtime code.

## Major product milestone: functional Nexus-owned UI

Rust does draw UI, but only two diagnostic surfaces:

- a render probe window;
- a noninteractive runtime-status window for hook mode, lifecycle, and DXGI
  selection.

These are the only two production `igBegin` calls found in the canonical Rust
tree. There are no production buttons, checkboxes, selectables, menus, popups,
tables, images, or input controls.

What exists as state but is not rendered:

- Quick Access has a bounded ownership registry, snapshots, visibility policy,
  context items, and notifications;
- Alerts has FIFO/fade state;
- addon, bind, settings/style, logging, and Mumble domain services exist.

Functional view layers still absent:

- window, subwindow, modal, and context-menu controls;
- main window shell;
- Addons page and load/uninstall confirmations;
- Binds page and bind-capture modal;
- Options and style import/export;
- About, Debug, and Log pages;
- persisted EULA/license modal;
- Quick Access rendering;
- Alerts rendering;
- Mumble inspector;
- optional Snowflake decoration.

The old UI has 80 physical files. A prior 78-file count omitted the
`MainWindow/Log` directory due an index ignore-pattern collision.

The current production frame path bypasses `FrameRenderState` and invokes
callbacks directly, so the existing `eula_accepted` and visibility gates are
not active production behavior.

Render from existing service snapshots rather than create parallel mutable
state. Preserve this composition order:

```text
advance localization and fonts
invoke PreRender callbacks
start ImGui frame
if EULA is not accepted:
    render only the EULA modal
else if host UI is visible:
    invoke addon Render callbacks
    render Alerts
    render MainWindow
    render QuickAccess
end and submit ImGui frame
invoke PostRender callbacks
```

Recommended UI order:

1. control primitives and persisted EULA accept/decline;
2. Alerts and Quick Access renderers;
3. main window shell;
4. Addons, Binds, Options, About, Debug, and Log pages and modals;
5. Mumble inspector;
6. retain probe/status windows as explicitly controlled diagnostics.

This is release-gating work. Until it lands, the host can initialize render
infrastructure but cannot provide the expected user-facing Nexus experience.

## Other confirmed missing integrations

### Crash handling

Only `CrashLog` and `CrashStack` paths are present. There is no vectored or
unhandled-exception handler, crash-handler-specific stack capture/walking,
symbol resolution, or crash-stack writer. A separate caller-resolution helper
does use `RtlCaptureStackBackTrace`; it is not a crash handler. Design and test
the missing handler in a sacrificial process; never validate it by crashing
the development host.

### Multibox

Rust parses `-sharearchive` and `-multi` but implements no behavior. Missing
observable behavior is:

- close Guild Wars 2's single-instance mutex on every non-vanilla startup;
- maintain the legacy state bits `1 | 2 | 4`;
- isolate logs per `-mumble` instance;
- serialize self-update across processes with a named mutex.

Do not invent archive or local-path switching. The legacy behavior only
reports the two command-line state bits.

### ArcDPS

Rust has ArcDPS style decoding, a path, and an addon ABI flag, but no complete
module detection/function-table bridge for logging, UI flags/modifiers,
extension add/free/list, and extension polling.

### Packaging

Installation, supported update/rollback UX, release packaging, complete
third-party notices, and a supported binary release remain undone.

## Runtime validation still required

Unit and CI success cannot prove:

- primary and chainloaded startup in the real game;
- D3D9, D3D11, and DXGI forwarding against real clients;
- NVIDIA auxiliary-swap-chain rejection while the intended overlay stays
  visible;
- device loss, resize, fullscreen, and swap-chain reselection;
- representative API v1-v6 addon loading;
- callback cleanup under concurrent unload/reload;
- multi-process behavior;
- crash handling;
- visual and input parity for EULA, main window, Quick Access, alerts, binds,
  scaling, and Escape-to-close.

Build a controlled compatibility matrix and preserve traces for these cases.
Run risky native/crash tests in disposable processes.

## Recommended continuation sequence

Land one checkpoint at a time:

1. Split font callback and resource cleanup, including tests and inventory
   refresh.
2. Implement the bounded synchronous `RenderFontService` bridge and bounded
   file/resource acquisition.
3. Construct and retain the production backend from the process services.
4. Add the outer addon render-session observer and native API installation.
5. Compose manager, cleaner, discovery, watcher, and update coordination.
6. Add lifecycle integration tests for unload-before-retire and monotonic
   owner generations.
7. Port the functional host UI in the order above.
8. Implement crash, multibox, and ArcDPS integration.
9. Complete packaging, legal inventory, and real-game compatibility QA.

Do not begin a later step merely because its crate already compiles. Each step
must satisfy its lifecycle and ownership invariants before the next layer
depends on it.

## Recent checkpoint history

```text
2f0b3ed feat: make localization cleanup synchronous
b8c1273 feat: make addon update requests lifecycle safe
c7cda71 feat: bridge addon textures to render sessions
10c401d fix: make texture replacement failure transactional
a49f6cf feat: implement required addon service adapters
7613294 feat: implement production game bind backend
22085ef feat: implement native input and wndproc backends
236cba0 feat: make native input registrations generation safe
089b377 fix: pass callback gate to addon ffi fixture
c523836 feat: bind addon ownership to callback gates
576d5c2 chore: establish independent Nexus-Rust project
272a4a5 feat: compose production addon api backend
a38b73a fix: add sequence-safe overlay handoff
8ea4773 fix: harden primary swap-chain lifecycle
491b0d0 fix: harden addon-owned native registrations
9c67d3c feat: checkpoint Rust rewrite foundation
464c353 Initial commit
```

`c523836` exposed a stale `CallbackGate` test fixture; `089b377` corrected it.
Nine consecutive checkpoints, from `089b377` through `2f0b3ed` inclusive,
passed GitHub Actions.

## Resume checklist

Before editing:

1. `cd C:\AI_STUFF\PROGRAMMING\Nexus-Rust-publish`.
2. Confirm branch `feat/production-addon-runtime`.
3. Confirm the worktree is clean and compare against `origin/main`, not local
   `main`.
4. Fetch the publication remote with `git fetch --no-tags origin`.
5. Read the provenance and contribution documents.
6. Additively index this exact worktree in SymForge.
7. Inspect the current font-manager, `RenderFontService`, cleaner-domain, and
   runtime-observer symbols before changing them.
8. Claim a narrow checkpoint and coordinate files with other agents.
9. Keep Cargo concurrency at four or fewer.
10. Run the proportionate focused tests, then all CI gates before pushing.
11. Commit and push the checkpoint.

The safest next coding checkpoint is the font callback/resource cleanup split.
It is bounded, testable, and removes a real unload-safety blocker needed by
both the render-font bridge and production backend installation.
