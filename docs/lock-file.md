# Lock File

`mars.lock` is the ownership registry for all managed items. It records what Mars installed, where it came from, and what the content looked like. This file is committed to version control.

## Format

TOML format with deterministically sorted keys for clean git diffs.

```toml
version = 1

[dependencies.base]
url = "https://github.com/haowjy/meridian-base"
version = "v1.2.0"
commit = "abc123def456"

[dependencies.local]
path = "/home/dev/my-agents"

[items."agents/coder.md"]
source = "base"
kind = "agent"
version = "v1.2.0"
source_checksum = "sha256:aaa111..."
installed_checksum = "sha256:bbb222..."
dest_path = "agents/coder.md"

[items."skills/review"]
source = "base"
kind = "skill"
version = "v1.2.0"
source_checksum = "sha256:ccc333..."
installed_checksum = "sha256:ddd444..."
dest_path = "skills/review"
```

## Schema

### Top Level

| Field | Type | Description |
|---|---|---|
| `version` | integer | Schema version (currently `1`) |
| `dependencies` | table | Resolved source entries |
| `items` | table | Installed items with checksums |

### `[dependencies.<name>]`

Resolved provenance for each source. Built from the resolved dependency graph, not copied from config.

| Field | Type | Present when | Description |
|---|---|---|---|
| `url` | string | Git source | Git URL |
| `path` | string | Path source | Canonical local path |
| `version` | string | Tagged git source | Resolved version tag (e.g., `v1.2.0`) |
| `commit` | string | Git source | Resolved commit hash |
| `tree_hash` | string | *(reserved)* | Future: deterministic tree hash for verification |

### `[items."<dest_path>"]`

Each item key is the destination path relative to the managed root (e.g., `agents/coder.md`, `skills/review`).

| Field | Type | Description |
|---|---|---|
| `source` | string | Dependency name that provided this item |
| `kind` | string | `"agent"` or `"skill"` |
| `version` | string? | Version from the source's resolved graph node |
| `source_checksum` | string | SHA-256 of the original source content |
| `installed_checksum` | string | SHA-256 of what Mars wrote to disk |
| `dest_path` | string | Destination path (same as the key) |

## Dual Checksums

Each item tracks two checksums:

- **`source_checksum`**: Hash of the content as it exists in the source tree, before any transformations. Used to detect when the source has changed (new version, upstream edit).

- **`installed_checksum`**: Hash of what Mars actually wrote to disk. May differ from `source_checksum` when frontmatter rewriting occurred (`frontmatter` is the YAML metadata block at the top of Markdown agent/skill files; Mars may rewrite skill references there). Used to detect when the user has modified the file locally.

This dual-checksum design enables the [three-way diff](conflicts.md#diff-matrix):
- Source changed? → compare new source hash against `source_checksum`
- Local changed? → compare current disk hash against `installed_checksum`

## Checksums

Checksums use the format `sha256:<hex>`. For agents (single files), this is the SHA-256 of the file content. For skills (directories), this is a deterministic hash of the directory tree.

## The `_self` Source

When a project has a `[package]` section in `mars.toml`, its own agents and skills appear in the lock under `source = "_self"` (`_self` is the reserved synthetic source name for items provided by the current project). The `_self` dependency entry uses `path = "."` to indicate the local project.

```toml
[dependencies._self]
path = "."

[items."skills/local-skill"]
source = "_self"
kind = "skill"
source_checksum = "sha256:..."
installed_checksum = "sha256:..."
dest_path = "skills/local-skill"
```

## Building the Lock

The lock is rebuilt on every sync from two inputs:

1. **Resolved graph** provides source provenance (URL, version, commit) for dependency entries
2. **Apply outcomes** provide checksums for item entries

Items are categorized by their apply action:

| Action | Lock behavior |
|---|---|
| Installed / Updated / Merged / Conflicted | New item entry with computed checksums |
| Kept (local modification preserved) | Carried forward from old lock |
| Skipped | Carried forward from old lock |
| Removed | Excluded from new lock |
| Symlinked (`_self`) | New entry with source checksum |

## Absent Lock File

When `mars.lock` doesn't exist, Mars treats it as empty. The first `mars sync` or `mars add` creates it. A missing lock is not an error.

## Corrupt Lock File

If `mars.lock` fails to parse, Mars reports a `LockError::Corrupt` and suggests running `mars repair`. Repair resets the lock to empty and rebuilds from dependencies.

## Atomic Writes

The lock file is written atomically via tmp+rename to prevent corruption from interrupted writes. Keys are sorted (by `IndexMap` insertion order, which the build function ensures is sorted) for deterministic output.
