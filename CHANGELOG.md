# Changelog

Caveman style. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versioning: [SemVer](https://semver.org/).

## [Unreleased]

## [0.1.10] - 2026-04-23

### Fixed
- Windows test build no longer compiles POSIX-only symlink fixtures.

## [0.1.9] - 2026-04-23

### Added
- Windows CI job for `cargo fmt --all --check` and `cargo test -q`.
- Windows release artifacts: `mars-windows-x64.exe` binary and PyPI wheel.
- Windows npm package: `@meridian-flow/mars-agents-win32-x64`.
- Windows PowerShell smoke testing guide (`docs/smoke-testing-windows.md`).
- `crate::platform` boundary module for cross-platform operations.

### Changed
- Cache root default now uses OS cache directories (`dirs::cache_dir()`).
- Cache component names use hash suffix for collision prevention.
- Directory replacement uses explicit `replace_generated_dir` with rollback.
- Cache finalization uses `publish_cache_dir_if_absent` for race handling.
- Git invocation centralized in `platform::process::run_git`.
- Source path classification centralized in `platform::path_syntax`.
- POSIX smoke guide renamed to `docs/smoke-testing-posix.md` with platform note.
- `docs/commands.md`: `mars link` described as copy, not symlink.

### Fixed
- Explicit-port URLs (e.g., `git://host:19424/repo.git`) no longer produce cache directories with colons.
- Windows-invalid characters in cache component names are sanitized.
- Windows reserved device names in cache paths are escaped.
- Filesystem errors now include operation name and path in diagnostics.

## [0.1.8] - 2026-04-19

### Added
- `mars version` runs package check before versioning — catches invalid frontmatter and missing SKILL.md before tagging.

## [0.1.7] - 2026-04-19

### Fixed
- `local-shadow` warning suppressed when content checksums match — no noise from diamond dependencies pulling same skill from multiple paths.

## [0.1.6] - 2026-04-19

### Changed
- `ManifestDep` unified for URL and path deps — eliminated `collect_path_manifest_requests` special case.
- Removed dead `ResolvedGraph.id_index` field (internal `ResolverContext.id_index` kept for duplicate detection).

### Fixed
- Filtered deps now resolve version without materializing transitive items.
- `Latest` constraint validation no longer bypassed.
- Constraint syntax comparison uses semver semantics, not string equality.
- Skill lookup checks same package first, then all resolved packages.

### Internal
- Resolver god module (4.4k lines) split into 10 focused modules.
- `ResolverContext` tracks version constraints and materialization filters separately.

## [0.1.4] - 2026-04-18

### Added
- `mars add` auto-inits a missing project at `--root` or cwd before adding a source.
- `mars link` auto-inits a missing project before managing a target directory.
- Smoke coverage for bootstrap and root-discovery flows.

### Changed
- `mars add` and context commands walk up to filesystem root, not git root.
- Walk-up boundary is now filesystem root on all platforms (Unix `/`, Windows `C:\`, UNC paths).
- `mars init` creates project at cwd (or `--root` target) without walking up.
- Auto-init applies to `mars add` and `mars link`; `mars sync` still errors on a missing project.
- `--root` for context commands sets walk-up start path, not direct project target.
- Error message now says "filesystem root" instead of "repository root".
- Windows compatibility documented as first-class invariant in AGENTS.md.

## [0.1.3] - 2026-04-16

### Added
- `mars adopt` moves unmanaged target items into `.mars-src/`, then syncs.
- `.mars-src` is now project-local source for agents and skills.
- Non-package repos can mirror local items across `.agents`, `.claude`, and other targets.
- Smoke coverage and docs for adopt/local source flow.

### Changed
- Sync now reads `.mars-src` local items even without `[package]`.
- Legacy repo-root `agents/` and `skills/` stay supported only for package repos.
- `.mars-src` wins if both local roots define the same item.

### Fixed
- `mars list` now shows adopted/local `.mars-src` items after sync.

## [0.1.2] - 2026-04-16

### Added
- Subpath support. One repo can hold many packages.
- Parser understands more source forms: GitHub, GitLab, generic git, local path.
- Smoke testing guide added.
- Repo now uses `meridian-dev-workflow` through Mars.

### Changed
- Fallback discovery now does explicit paths first, then nearest non-empty layer.
- Same-layer fallback picks first deterministic match.
- `mars add` supports `--subpath`.
- Docs now explain subpath and supported source forms.

### Fixed
- `meridian-dev-workflow` install no longer breaks on mirrored `caveman` layout.
- GitLab-like URLs keep explicit ports.
- Parser clippy failure fixed for release checks.
