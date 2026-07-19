# Provenance policy

Nexus-Rust is a separately maintained Rust implementation intended to interoperate with the public Nexus add-on ecosystem. Provenance is part of correctness: compatibility is useful only if the published project can account for what it contains.

## Repository boundary

The published Git history is constructed without the proprietary C++ host tree or its assets. The Rust source files were authored for this project, but development was not organized as a formal two-team clean-room process. This document describes the practical source boundary; it does not make a definitive legal conclusion.

The normative ABI reference is the MIT-licensed [RaidcoreGG/Nexus-API](https://github.com/RaidcoreGG/Nexus-API), specifically its public `Nexus.h` declarations. Public documentation and behavior observable through normal use may supply compatibility requirements and test evidence. They do not make a third-party host implementation part of this project.

The game-bind compatibility table was checked against `Nexus-API/Nexus.h` at commit `9b2c53df86c00db6495642bfcff2d0611bd957ef`. It includes every numeric value in that public `EGameBinds` declaration and intentionally retains legacy numeric value `10` for older add-on compatibility. Future table changes must record the public source revision and explain any legacy-only entries.

## Allowed compatibility evidence

- Publicly licensed interface definitions, with the source and license recorded.
- Public documentation and release notes.
- Black-box observations made through ordinary supported use.
- Independently authored tests, traces, and fixtures that contain no copied protected assets or implementation text.

The evidence should explain what behavior is required, not prescribe another host's internal design.

## Excluded material

Do not add proprietary host source or decompiled output; copied implementation text; third-party binaries or libraries without redistribution permission; or artwork, fonts, style presets, icons, names, and other assets without documented rights.

When uncertain, leave the material out and ask for review. Compatibility alone is not a reason to import it.

## Required record for new inputs

Any new third-party asset, generated table, protocol fixture, captured data set, or vendored source must include:

1. Origin URL or other verifiable source.
2. Author or rights holder.
3. License or written permission and any required notice.
4. Exact version, commit, or content hash.
5. Transformations performed and the project files produced.
6. A short explanation of why the input is necessary.

Generated outputs must be reproducible when practical. Dependency and notice inventories must be regenerated and reviewed before a release.

## Review posture

Maintainers should review implementation correctness and provenance separately. A green test suite does not establish redistribution rights, and a documented source does not establish behavioral compatibility. Release decisions may require qualified legal review; this policy is an engineering control, not legal advice.
