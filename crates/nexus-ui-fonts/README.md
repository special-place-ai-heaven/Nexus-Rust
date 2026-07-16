# nexus-ui-fonts

This crate owns the exact Nexus host font catalog while leaving Dear ImGui atlas
mutation in the existing thread-bound UI services. A FontRebuildRequest can be
prepared off-thread because it owns optional user bytes. Apply it only on the
render thread, with the target ImGui context current, after the previous frame
and before NewFrame.

The embedded files come directly from res/Fonts:

| Mumble UI size | Menomonia | Menomonia Big | Fira Sans |
|---|---:|---:|---:|
| Small | 16 | 22 | 15 |
| Normal | 18 | 24 | 16 |
| Large | 20 | 26 | 17.5 |
| Larger | 22 | 28 | 19.5 |

FONT_DEFAULT uses embedded Inter at a configurable size clamped to 1–50 pixels,
defaulting to 15. When a user font is supplied, owned bytes replace FONT_DEFAULT
and are inserted in merge mode immediately after each of the twelve embedded
scale-specific inputs. This preserves the legacy insertion semantics without
borrowing a path buffer or addon-owned memory across an atlas rebuild.
Host-configured user-font files are capped at 128 MiB before the registry copy.

Unknown Mumble UI-size values select the normal handles, matching the legacy
switch default. Every returned handle belongs to one atlas generation and must
be replaced after the next rebuild.
