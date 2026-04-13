# Changelog

Caveman style. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versioning: [SemVer](https://semver.org/).

## [Unreleased]

### Added
- Target divergence detection: `mars sync` detects when `.agents/` files diverge from `.mars/` canonical state. Missing files re-copied; manual edits warned but preserved.
- `mars doctor` target health check: compares `.agents/` against lock checksums. Reports missing and divergent files with actionable suggestions.
- Checksum integrity enforcement: mandatory checksums for write actions, post-write verification, lock building rejects empty checksums.

### Changed
- Conflict strategy unified: both agents AND skills use source-wins + warn. Three-way merge no longer triggers on sync conflicts. Local modifications overwritten with diagnostic warning.
- All items are copies, no symlinks. `_self` local package items copied to `.mars/` like dependency items. Local source edits require `mars sync` to propagate.
- `mars resolve` acquires sync lock — concurrent resolve + sync now safe.
- `mars models alias` uses proper config load/save instead of raw `fs::write`.
- Cross-platform file locking: `libc::flock` on Unix, `windows_sys::LockFileEx` on Windows. No external crate.

### Fixed
- `mars check` no longer false-warns when agents reference skills provided by dependencies.
