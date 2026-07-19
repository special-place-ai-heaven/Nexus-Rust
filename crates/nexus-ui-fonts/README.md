# nexus-ui-fonts

This crate owns the Nexus-compatible host font catalog while leaving Dear ImGui
atlas mutation in the thread-bound UI services. A `FontRebuildRequest` can be
prepared off-thread because it owns optional user bytes. Apply it only on the
render thread, with the target ImGui context current, after the previous frame
and before `NewFrame`.

The catalog preserves the legacy identifiers, sizes, insertion order, and UI
scale selection. Its built-in entries intentionally resolve to Dear ImGui's
compiled-in default font, so the project remains self-contained and does not
redistribute third-party font files. The legacy typeface names therefore name
compatibility roles, not bundled font assets.

`FONT_DEFAULT` is configurable from 1–50 pixels and defaults to 15. When a user
font is supplied, owned bytes replace `FONT_DEFAULT` and are inserted in merge
mode immediately after each of the twelve scale-specific built-in entries. This
preserves the catalog and merge semantics without borrowing a path buffer or
addon-owned memory across an atlas rebuild. Host-configured user-font files are
capped at 128 MiB before the registry copy.

Unknown Mumble UI-size values select the normal handles, matching the legacy
switch default. Every returned handle belongs to one atlas generation and must
be replaced after the next rebuild.
