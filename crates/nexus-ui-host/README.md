# nexus-ui-host

`nexus-ui-host` is the renderer-independent ownership and state layer for the
Nexus UI compatibility surface. It deliberately contains no D3D or ImGui
backend. A render thread consumes owned snapshots and decides how to draw them.

## Legacy parity

- Render phases preserve insertion order. A callback duplicate is ignored only
  within the same phase, while deregistration removes that identity from all
  four phases. Pre- and post-render remain available when the main UI is hidden;
  options callbacks are requested separately.
- Close-on-Escape preserves the first registration for a duplicate window name,
  ignores key repeats, skips ImGui window-stack index zero, and closes the
  topmost visible registered window.
- Alerts are FIFO and compare duplicates only with the current front. A match
  restarts that front's fade without replacing its type or owner.
- Quick Access uses lexicographic shortcut and context-item order, retains the
  first duplicate, reparents orphan context items, preserves notification-key
  insertion order, and implements the exact gameplay/combat visibility table.

All queues, registries, and strings are bounded. Registration methods return an
explicit duplicate, capacity, or validation result instead of silently growing
without limit.

## Owner cleanup and native boundaries

Obtain one `OwnerHandle` per addon generation through `UiHost::owner` and use it
for every registration. `cleanup_owner_generation` closes the shared generation
gate first, drains activity on other threads, then removes state from every
registry. Stale snapshots therefore skip callbacks and pointers instead of
entering unloaded addon code.

If cleanup is requested reentrantly from the owner's own callback, it reports
`quiescent: false` instead of deadlocking. The loader must defer DLL unload until
`wait_owner_quiescent` reports true.

Native functions and legacy `bool*` visibility storage enter through
`NativeRenderCallback` and `NativeVisibilityPointer`. Their constructors bind
the native resource to an `OwnerHandle` and are unsafe because the addon must
keep code and storage valid until generation quiescence; all later calls, reads,
and writes remain contained behind that exact safe owner gate.

Managed callback panics are caught per registration. A bounded panic budget
disables only the failing registration, and every callback is invoked after all
registry locks have been released.
