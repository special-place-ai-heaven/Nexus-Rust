# Nexus-Rust

Nexus-Rust is an independent Rust implementation of a Windows overlay and add-on host compatible with the public Nexus add-on ABI. Its goal is that existing add-ons can run without special handling while the host remains observable, controllable, and robust.

> [!WARNING]
> Nexus-Rust is alpha, unreleased software. It is not yet a supported drop-in replacement and should not be relied on for normal gameplay.

## Project goals

- Preserve the public add-on ABI and the behavior add-ons legitimately depend on.
- Keep ownership, callback lifetime, and shutdown rules explicit at native boundaries.
- Recover predictably from graphics-device, swap-chain, overlay, and third-party interaction failures.
- Make updates, diagnostics, policy, and UI behavior controllable by the people running the software.
- Prefer testable Rust components over hidden global state.

The compatibility target is the public API described by [Nexus-API](https://github.com/RaidcoreGG/Nexus-API). Compatibility does not imply shared implementation or affiliation.

## Current status

The workspace contains substantial foundations for ABI types and shims, add-on loading and lifecycle management, DXGI/D3D11 integration, rendering, input, textures, UI, networking, and runtime composition. The production add-on backend dispatch surface and required service contracts exist.

Important gaps remain:

- Not every production service is wired into the running host end to end.
- Add-on behavior and failure recovery have not yet reached full compatibility coverage.
- Installation, updating, rollback, and release packaging are not ready for users.
- The project has no supported binary release.

Progress claims should be tied to tests or observable compatibility cases. “Compiles” is necessary, but it is not proof of drop-in compatibility.

## Building

The current target is 64-bit Windows using the MSVC toolchain and Direct3D 11. Install Visual Studio Build Tools with the C++ workload, a current Windows SDK, and Rust 1.97.1 or newer.

```powershell
rustup toolchain install 1.97.1
rustup target add x86_64-pc-windows-msvc --toolchain 1.97.1
cargo +1.97.1 build --workspace
```

Run the repository checks before submitting changes:

```powershell
cargo +1.97.1 fmt --all -- --check
cargo +1.97.1 test --workspace --all-features
cargo +1.97.1 clippy --workspace --all-targets --all-features -- -D warnings
cargo +1.97.1 doc --workspace --all-features --no-deps
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for engineering and provenance requirements.

## Compatibility and provenance

This repository is maintained separately from the C++ host. The public MIT-licensed Nexus-API is the normative source for ABI declarations; public documentation and observable behavior may be used to define compatibility tests. Third-party host source, binaries, artwork, fonts, style presets, and branding must not be copied into this project.

The project was not developed under a formal two-team clean-room process. [PROVENANCE.md](PROVENANCE.md) records the practical boundary and the evidence expected for future contributions. [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) starts the dependency notice inventory; it will be completed and verified before any release.

## License and independence

Project-owned code is licensed under [Apache License 2.0](LICENSE).

Nexus-Rust is an independent community project. It is not affiliated with, sponsored by, or endorsed by RaidcoreGG, ArenaNet, or NCSoft. Product and project names belong to their respective owners and are used only to identify compatibility.
