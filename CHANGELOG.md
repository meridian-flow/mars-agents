# Changelog

Caveman style. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versioning: [SemVer](https://semver.org/).

## [Unreleased]

### Added
- OpenCode availability probe cache (60s TTL, stale-while-revalidate). `mars models list/resolve` no longer synchronously spawns `opencode providers list` + `opencode models` on every call — returns cached result and refreshes in the background. Eliminates ~2s per mars invocation after first probe.
- `mars unlink <target>` top-level subcommand. Removes a managed target directory and its settings entry. Owns its logic directly (not a shim over link).
- `cli::target` shared module for target-name normalization.

### Removed
- `mars link --unlink` flag. Use `mars unlink <target>` instead.

### Fixed
- Windows OpenCode probe cache cold path detects and runs `.bat`/`.cmd` shims, so fake or npm-installed `opencode` no longer skips cache population.
- `mars link --unlink` no longer auto-initializes a project in an empty directory before unlinking.
- `mars unlink` deletes the target directory before saving config, so a failed deletion doesn't leave settings mutated with the directory still on disk.

### Changed
- Skill schema: replaced `invocation: explicit | implicit` enum with two independent booleans `model-invocable` and `user-invocable` (both default true). Per-harness lowering compiles each boolean to native fields: Claude gets both natively, Codex gets `allow_implicit_invocation` for model-invocable, Pi/Cursor get `disable-model-invocation`. Old fields (`invocation`, `disable-model-invocation`, `allow_implicit_invocation`) are hard errors.

### Fixed
- `MERIDIAN_MANAGED=1` now suppresses agent artifacts in managed targets (`.claude/agents/`, `.opencode/agents/`). Previously target sync copied agents from `.mars/` to targets even under managed mode. Introduced `AgentSurfacePolicy` enum, unified three agent cleanup paths into `reconcile_native_agent_surfaces`, and fixed `mars link` to apply the same suppression policy as `mars sync`.
- Native harness cleanup dirs derived from `HarnessKind::all()` instead of a hardcoded list. Removed stale `.cursor` entry that isn't a `HarnessKind` variant.

## [0.2.5] - 2026-05-03

### Changed
- Suppressed `skill-field-dropped` and `agent-field-dropped` warnings for `Dropped` and `MeridianOnly` lossiness classifications. These are expected target-format gaps, not actionable.

## [0.2.4] - 2026-05-03

### Fixed
- Version drift false positives: `compatible_with_resolved` now handles `Latest` vs `Semver` constraints. When a concrete resolved version satisfies the semver constraint, reports `Compatible` instead of `PotentiallyConflicting`. Fixes ~20 spurious "potential version drift" warnings in diamond dependency trees.

## [0.2.3] - 2026-05-02

### Changed
- `.gitignore`: added `.claude/` and `.opencode/` generated artifacts.

## [0.2.2] - 2026-05-02

### Fixed
- Conventional flat-skill packages with root `SKILL.md` plus bootstrap docs now discover both skill and bootstrap docs.
- Native harness skill projection now runs inside `target_sync`, so projected skills stay expected during orphan cleanup and `mars link` can populate native skill dirs.
- Native skill projections now always refreshed on sync even when canonical is Skipped. Diverged projections repaired with warning.
- Windows TOML path escaping: `PathBuf` fields in config serialization now normalize backslashes to forward slashes. Prevents `\U` in `C:\Users\...` from being interpreted as TOML unicode escape sequences.

### Changed
- `ReaderIr` now embeds `ResolvedState` directly — eliminates decompose/reconstruct round-trip between reader and compiler stages. Removed dead `target_registry` field. Renamed `_sync_lock` → `sync_lock` in `LoadedConfig`. Removed redundant nested `dry_run` guard in `finalize()`.

### Added
- Bootstrap doc discovery. Package-level `bootstrap/<doc>/BOOTSTRAP.md` scanned during conventional discovery, flows through resolve/target/diff/apply pipeline, materializes to `.mars/bootstrap/`. Fallback/manifest discovery via `bootstrapDocs`/`bootstrap_docs` keys. `mars list` and `mars export` surface bootstrap docs.
- Skill variant projection. `variants/<harness>/<model>/SKILL.md` hierarchy discovered and validated. Native harness dirs get harness-selected variant; canonical `.mars/skills/<name>/variants/` preserved intact. `mars list` shows variant availability.
- Skill frontmatter compilation. Universal schema (`model-invocable`, `user-invocable`, `allowed-tools`, `license`, `metadata`) compiled to per-harness native fields via typed `SkillProfile` parser and lowering functions. Legacy `invocation`, `disable-model-invocation`, and `allow_implicit_invocation` now hard schema errors; migrate to `model-invocable` / `user-invocable`. Raw fallback for malformed frontmatter. `mars validate` checks skill schema.
- `sync::translate` module — `TranslatedOutput` type wraps `PlannedAction` with optional pre-translated content; `translate()` pass-through establishes insertion point for per-target format lowering.
- `TargetAdapter::write_config_entries` / `remove_config_entries` default-no-op methods + `ConfigEntry`/`ConfigEntryKind` placeholder types in `target/mod.rs`.
- Lock-driven orphan cleanup in `target_sync`: `cleanup_orphans` now iterates lock v2 `previous_managed_paths` directly instead of scanning hardcoded subdirectories (`agents/`, `skills/`, etc.).
- `mars version` CHANGELOG.md integration. Automatically promotes `[Unreleased]` → `[X.Y.Z] - YYYY-MM-DD`, inserts fresh empty `[Unreleased]`, stages alongside `mars.toml`. Warns when `[Unreleased]` section is empty. Silent skip when no CHANGELOG.md exists.
- `compiler::agents` — typed agent-profile schema parser: `AgentMode`, `HarnessKind`, `ApprovalMode`, `SandboxMode`, `EffortLevel`, `HarnessOverrides`, `ModelPolicyEntry`, `FanoutEntry`. `parse_agent_profile()` validates field values, flags legacy `models:`, rejects non-overridable fields in override blocks, collects `AgentDiagnostic` without aborting the sync.
- `compiler::agents::lower` — per-target agent lowering: `lower_to_claude()` (markdown+YAML per agent-compilation-mapping spec), `lower_to_codex()` (TOML `[agent]/[agent.config]/[agent.instructions]`), `lower_to_opencode()` (markdown+YAML), `lower_to_pi()` (simplified markdown). `harness-overrides` merged compile-time (D42). Lossiness classification `Exact/Approximate/Dropped/MeridianOnly` per field per target.
- Dual-surface compilation in `compiler::compile()` — `dual_surface_compile()` after `apply_plan()` writes `.mars/agents/`; scans harness-bound agents and writes native artifacts to `<project_root>/<harness_dir>/agents/<name>.<ext>`; emits lossiness warnings as diagnostics; non-fatal (D9). Universal agents (no `harness:`) produce only `.agents/` artifact.

## [0.1.19] - 2026-04-25

### Added
- Model availability classification. Each model now `runnable`, `unavailable`, or `unknown` based on installed harnesses and provider credentials.
- OpenCode provider probing. `opencode providers list` + `opencode models` detect available models through OpenCode harness.
- `--unavailable` flag. Show unavailable models in default list view.
- Availability fields in JSON output: `availability`, `availability_source`, `runnable_paths`.
- `probe_results.opencode` in JSON when OpenCode probing runs.
- **Three-step model resolve**: `mars models resolve` now tries alias → glob match against alias candidates → passthrough. Older versions work: `opus-4-6` → `claude-opus-4-6`. Unknown models pass through to harness with warning instead of erroring. Exit 0 always (cache is enrichment, not gate).
- **Three-tier `mars models list`**: default shows alias winners; `--all` shows all models matching any alias filter; `--catalog` dumps full models.dev cache.
- `auto_resolve_all()` — returns all alias filter candidates, not just winner. Used by `--all` listing and glob resolve.
- User-provided wildcards in resolve: `mars models resolve "*opus*"` uses pattern as-is; plain text auto-wraps as `*{input}*`.

### Fixed
- Offline mode no longer marks direct-harness models as unknown. Only OpenCode probing suppressed.
- Empty OpenCode provider list correctly classifies as unavailable, not unknown.
- OpenCode model slug matching requires exact match when model probe succeeds.
- Passthrough resolve works when cache unavailable (offline + first run). Cache load failure skips to passthrough instead of erroring.

### Changed
- Default `mars models list` prunes unavailable models. Use `--unavailable` to see them.
- `--all` expands alias candidates, does NOT show raw catalog. Use `--catalog` for that.
- `[settings.model_visibility]` now supports combined `include` + `exclude`.
- Visibility patterns match bare model ID, `provider/model`, or OpenCode slug based on slash count.
- `mars models resolve` includes availability annotation (never pruned).
- `--all` flag on `mars models list` redefined: was "show aliases with unavailable harnesses", now "show all alias-filter candidates across versions". No backwards compat needed.

## [0.1.16] - 2026-04-23

### Fixed
- Source name derivation splits on both `/` and `\` and strips drive prefixes — works cross-platform even when parsing Windows paths on Linux.
- Test assertions for Windows path source names expect last component, not full path.

## [0.1.15] - 2026-04-23

### Fixed
- Local path source name derivation uses `Path::file_name()` instead of string splitting — fixes `mars add`, `mars why`, `mars remove`, `mars override` on Windows.
- Archive cache temp path uses `Path::with_file_name()` instead of string concat.
- Content hash relative paths built from `Path::components()` instead of backslash replacement.

## [0.1.14] - 2026-04-23

### Changed
- `default_dest_path` / `parse_rename_dest` return `DestPath` directly, not `PathBuf`.
- `target_sync` uses `HashSet<String>` for cross-platform path comparison.
- `SourceSubpath` and `DestPath` share `normalize_relative_coordinate()` helper.
- `DestPath::item_name()` method added; `rsplit('/')` duplication removed.
- All `std::fs::canonicalize` replaced with `dunce::canonicalize` project-wide.
- Remaining `Command::new("git")` routed through `platform::process::run_git`.

### Fixed
- Windows 8.3 short-name path mismatches in `find_root` and `merge_override` tests.

## [0.1.13] - 2026-04-23

### Changed
- `DestPath` refactored from `PathBuf`-backed to `String`-backed normalized forward-slash coordinate. Lock keys and map keys now consistent across platforms. `resolve(root)` is the only path to native filesystem paths.
- `default_dest_path` and `parse_rename_dest` return `DestPath` directly, not `PathBuf`.
- `target_sync` uses `HashSet<String>` for cross-platform path comparison.
- `SourceSubpath` and `DestPath` share internal `normalize_relative_coordinate()` helper.
- Added `DestPath::item_name()` method; deduplicated `rsplit('/')` pattern.
- All `std::fs::canonicalize` replaced with `dunce::canonicalize` project-wide.
- Remaining `Command::new("git")` in `version.rs` and `merge/mod.rs` routed through `platform::process::run_git`.

### Fixed
- Windows lock files with backslash paths now normalize to forward slashes on load.
- `mars rename` validates destination path before storing mutation.
- Invalid rename destinations in config return error instead of panic.
- `mars adopt` handles invalid target-relative paths gracefully.
- Cache base filename uses underscore instead of colon for Windows compatibility.
- Doctor target divergence warnings use forward-slash display paths.
- MarsContext canonicalization uses `dunce` to avoid `\\?\` prefix on Windows.
- Rename destination normalization handles backslash paths.
- Path source name derivation uses forward-slash-only splitting for cross-platform consistency.

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
