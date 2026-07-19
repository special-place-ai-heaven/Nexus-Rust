# Contributing to Nexus-Rust

Nexus-Rust is building a reliable Rust host for the public Nexus add-on ABI. Compatibility work is welcome, but every change must preserve a clear implementation and provenance boundary.

## Before starting

- Open an issue before a broad architectural change or a new compatibility promise.
- Keep changes narrow enough to review and verify.
- Describe the public API, documentation, or observable behavior that motivates compatibility work.
- Do not submit third-party host source, decompiled output, binaries, artwork, fonts, style presets, branding, or copied implementation text.
- For any new asset, generated table, protocol fixture, or data file, record its origin, author or owner, license, exact version or hash, and transformations.

See [PROVENANCE.md](PROVENANCE.md) for the project's evidence policy.

## Development setup

The current build target is 64-bit Windows with the MSVC toolchain. Install Visual Studio Build Tools with the C++ workload, a Windows SDK, and Rust 1.97.1 or newer.

```powershell
rustup toolchain install 1.97.1
rustup target add x86_64-pc-windows-msvc --toolchain 1.97.1
cargo +1.97.1 build --workspace
```

## Engineering expectations

- Keep native ownership and callback lifetimes explicit.
- Minimize `unsafe`; document the safety invariant for every unsafe boundary.
- Fail closed across the C ABI and never unwind through foreign code.
- Preserve exact argument order, return conventions, and owner-scoped cleanup where compatibility requires them.
- Add regression tests for behavior changes, including failure and teardown paths.
- Avoid global state when ownership can be represented by types or scoped services.
- Do not hide warnings or weaken workspace lints to land a change.

## Required checks

Run these commands from the repository root:

```powershell
cargo +1.97.1 fmt --all -- --check
cargo +1.97.1 test --workspace --all-features
cargo +1.97.1 clippy --workspace --all-targets --all-features -- -D warnings
cargo +1.97.1 doc --workspace --all-features --no-deps
```

Platform-specific or end-to-end changes should also include focused runtime evidence. State what was exercised, the Windows version and GPU path, and what remains untested.

## Pull request checklist

- Explain the user-visible outcome and compatibility evidence.
- Identify safety, lifecycle, rendering, or rollback risks.
- Add or update tests and documentation.
- List third-party inputs and their provenance, or state that none were introduced.
- Confirm the required checks pass.
- Keep generated and vendored content out of the change unless it is necessary, reproducible, and properly licensed.

By contributing, you confirm that you have the right to submit the work and agree that your contribution is licensed under Apache License 2.0.
