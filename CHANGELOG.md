# Changelog

All notable changes to Katsuobushi are recorded here, **newest first**. The
format follows [Keep a Changelog]; the project is versioned with Git tags
following [SemVer]. While in `0.x`, any release may break — consumer-facing
breaking and behavioral changes are detailed in [`MIGRATING.md`](MIGRATING.md).

## [0.1.1] — 2026-06-24

### Added

- **`lib.sandbox`: `sandbox:status` preflight.** A bare `sandbox:status` now
  prints an `environment:` block before listing instances, verifying every
  declared secret at its **host** source (the `fromEnv` variable is set, or the
  `fromFile` path is readable) and checking for `/dev/vhost-vsock`. It names the
  exact host variable feeding each guest secret, so "is this host ready to
  launch?" is a single command with no project-specific knowledge required.

### Changed

- **`lib.sandbox`: `sandbox:status` exits non-zero when the preflight fails.**
  Previously the bare command always exited `0`; it now exits with the count of
  missing prerequisites, so its exit status alone is a usable gate. See
  [`MIGRATING.md`](MIGRATING.md#011).
- **Docs:** clarified that the guest always reads `CLAUDE_CODE_OAUTH_TOKEN`
  while `secrets.*.fromEnv` chooses which **host** variable supplies it, and
  documented the agent-harness workaround (a harness scrubs
  `CLAUDE_CODE_OAUTH_TOKEN` from its children, so source it from a
  differently-named host variable, e.g. `HARNESS_OAUTH_TOKEN`). Touches
  `lib/sandbox/README.md`, the `sandbox` skill, and the `sandbox` template.

### Fixed

- **`lib.sandbox`: the guest can push to the 9p sync mirror.** The per-instance
  bare mirror is now shared over 9p with `security_model=mapped-xattr` (was
  `security_model=none`), so files the guest creates are recorded as
  agent-owned. The unprivileged agent could previously never write its
  receive-pack quarantine dir, so `git push` failed and no work crossed the
  sandbox boundary. The mirror's pre-existing directories are also opened so the
  agent can create entries inside them.

## [0.1.0] — 2026-06-23

The first tagged release. Highlights below; consumer-facing migration notes for
everything tracked on untagged `main` up to this tag are in
[`MIGRATING.md`](MIGRATING.md#010).

### Added

- **`lib.sandbox`** — a new library that assembles a [`microvm.nix`] guest which
  boots into a working dev environment where an agent harness (Claude Code by
  default) runs with its blast radius bounded by a real VM. Provides
  `apps.sandbox` (`nix run .#sandbox`), the `sandbox:*` menu commands
  (`start`, `prompt`, `status`, `fetch`, `stop`), `checks.sandbox`, and
  `nixosConfiguration`. Scaffold with `nix flake init -t
  github:cdata/katsuobushi#sandbox`.
- **`sandbox` template** and **`sandbox` agent skill** for the above.
- **`rust` template** for scaffolding Rust projects.
- **Transitive infra dependency inheritance.** Katsuobushi now owns `crane`,
  `nix-filter`, `rust-overlay`, and `microvm`, passing them through to consumers
  so a `lib.rust` consumer flake collapses from six inputs to two.
- **`lib.rust`: `target` argument** on `buildCrate` / `buildTestArchive` for
  cross-compiling to arbitrary triples; **`sourceInclude`** argument for crates
  that do not live under `rust/`.

### Changed

- **`lib.markdown` now uses [Prettier]** instead of `rumdl`, which mishandled
  GFM tables. Scope is now `include` / `exclude` glob lists plus a `name` label
  (replacing `docsDir`); `settings` takes Prettier options; outputs and menu
  commands are namespaced per invocation (`format:<name>` / `lint:<name>`).
- **`lib.rust` input arguments renamed** to match nixpkgs vocabulary:
  `buildInputs` → `nativeBuildInputs` (build tools) and `libraries` →
  `buildInputs` (link libraries); both now default to `[ ]`.
- **`lib.rust` wasm-bindgen version is derived from `Cargo.lock`** rather than
  hard-pinned, failing fast with a copy-pasteable fix on a mismatch. Default
  hashes ship for `0.2.108`.
- **`lib.rust` crate version is derived from `Cargo.toml`** instead of a
  hardcoded `0.1.0`; derivation name prefix derives from `projectId`.

See [`MIGRATING.md`](MIGRATING.md#010) for the full upgrade details.

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/
[SemVer]: https://semver.org/spec/v2.0.0.html
[Prettier]: https://prettier.io
[`microvm.nix`]: https://github.com/astro/microvm.nix
