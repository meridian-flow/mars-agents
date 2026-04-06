# Configuration Reference

## Terminology

- **Project root**: directory containing `mars.toml` and `mars.lock`.
- **Managed root**: directory Mars installs into (default: `.agents/`, configurable via `settings.managed_root`).
- **`--root`**: points to the managed root when you need to override auto-detection.

Mars uses three config files, all at the project root:

| File | Purpose | Committed? |
|---|---|---|
| `mars.toml` | Dependencies, filters, settings | Yes |
| `mars.lock` | Resolved versions, checksums, ownership | Yes |
| `mars.local.toml` | Developer-local overrides | No (gitignored) |

## `mars.toml`

### `[package]` (optional)

Present only in source packages (repos that others depend on). Consumers don't need this section.

```toml
[package]
name = "meridian-base"
version = "1.2.0"
description = "Core agents and skills for meridian"  # optional
```

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Package name, used for dependency resolution |
| `version` | string | yes | Semver version of this package |
| `description` | string | no | Human-readable description |

### `[dependencies]`

Each key is the dependency name (the identifier Mars commands use). Each value specifies the source and optional filters.

`mars add` derives the dependency name from the source specifier by default. Example: `mars add haowjy/meridian-base` creates `[dependencies.meridian-base]`.

```toml
[dependencies.base]
url = "https://github.com/haowjy/meridian-base"
version = "^1.0"

[dependencies.dev]
path = "../my-dev-agents"

[dependencies.ops]
url = "https://github.com/acme/ops-agents"
agents = ["deployer", "monitor"]
skills = ["deploy-flow"]

[dependencies.toolkit]
url = "https://github.com/acme/toolkit"
only_skills = true
```

Commands such as `mars remove`, `mars override`, `mars upgrade`, and `mars why` take dependency names, not source URLs. Use `mars list --status` to see the `SOURCE` column and `mars list --source <name>` to filter by one dependency.

#### Source fields

Each dependency must have exactly one of `url` or `path` (not both, not neither).

| Field | Type | Description |
|---|---|---|
| `url` | string | Git URL (HTTPS, SSH, or GitHub shorthand expanded to HTTPS) |
| `path` | string | Local filesystem path (relative to project root or absolute) |
| `version` | string | Version constraint for git sources (see [Version Constraints](#version-constraints)) |

#### Filter fields

Filters control which agents and skills from a source are installed. Only one filter mode is active at a time.

| Field | Type | Description |
|---|---|---|
| `agents` | string[] | Install only these named agents (include mode) |
| `skills` | string[] | Install only these named skills (include mode) |
| `exclude` | string[] | Install everything except these named items |
| `only_skills` | bool | Install only skills, no agents |
| `only_agents` | bool | Install only agents plus their transitive skill dependencies (skills those agents declare and therefore require) |
| `rename` | table | Rename mappings (see [Renaming](#renaming)) |

#### Filter mode rules

These combinations are **rejected** at config load and CLI parse time:

| Combination | Reason |
|---|---|
| `only_skills` + `only_agents` | Mutually exclusive |
| `only_skills` + `agents` | Category-only conflicts with include list |
| `only_agents` + `skills` | Category-only conflicts with include list |
| `exclude` + `agents`/`skills` | Can't include and exclude simultaneously |
| `exclude` + `only_skills`/`only_agents` | Can't exclude and restrict category |

When no filter fields are set, all agents and skills from the source are installed (**All** mode).

#### Include mode behavior

When `agents` and/or `skills` lists are provided:

- Only named agents and named skills are installed
- If a named agent's **frontmatter** (the YAML metadata block at the top of the Markdown file) declares skill dependencies, those transitive skills are also installed automatically
- Items not found in the source are silently absent (warning at sync time)

#### `only_agents` behavior

- All agents from the source are installed
- Skills referenced by those agents' frontmatter are installed (**transitive skill dependencies**: indirectly required skills pulled in through agent declarations)
- Standalone skills not referenced by any agent are excluded

### `[settings]`

```toml
[settings]
managed_root = ".claude"   # default: ".agents"
links = [".claude", ".cursor"]

[settings.model_visibility]
include = ["opus*", "sonnet*"]  # or use exclude = [...]
```

| Field | Type | Default | Description |
|---|---|---|---|
| `managed_root` | string | `".agents"` | Directory name for managed output under the project root |
| `links` | string[] | `[]` | Directories where `agents/` and `skills/` symlinks are maintained |
| `model_visibility` | table | `{}` | Consumer-only display filter for `mars models list` output |

#### `[settings.model_visibility]`

Controls alias visibility in `mars models list`.

| Field | Type | Description |
|---|---|---|
| `include` | string[] | Glob patterns; only matching aliases are shown |
| `exclude` | string[] | Glob patterns; matching aliases are hidden |

Rules:
- `include` and `exclude` are mutually exclusive (`[settings.model_visibility]` with both is a validation error)
- Consumer-only setting; it does not flow through dependencies
- Display filter only; it does not affect `mars models resolve`
- CLI `mars models list --include/--exclude` overrides this config for that invocation

## Version Constraints

Mars uses [semver](https://semver.org/) for version resolution. Sources tag releases with `v`-prefixed semver tags (e.g., `v1.2.3`).

| Constraint | Meaning | Example |
|---|---|---|
| `^1.0` | Compatible with 1.x (>=1.0.0, <2.0.0) | `version = "^1.0"` |
| `~1.2` | Patch-level changes only (>=1.2.0, <1.3.0) | `version = "~1.2"` |
| `>=0.5.0` | At least this version | `version = ">=0.5.0"` |
| `=1.2.3` | Exact version | `version = "=1.2.3"` |
| `v1.2.3` | Exact version (v-prefix) | `version = "v1.2.3"` |
| *(omitted)* | Latest available (HEAD for untagged repos) | |

Branch or commit pins (non-semver strings) bypass version resolution entirely and fetch the specified ref directly.

## Renaming

Rename mappings let you change the installed name of an item from a source. This is useful for resolving naming collisions or for preferring shorter names.

Renames are set via `mars rename` (which updates the dependency's `rename` field) or by editing `mars.toml` directly:

```toml
[dependencies.base]
url = "https://github.com/haowjy/meridian-base"
rename = { "agents/coder__haowjy_meridian-base.md" = "agents/coder.md" }
```

## `mars.local.toml`

Developer-local overrides. Gitignored by `mars init`. Lets each developer swap a git source for a local checkout without modifying the shared config.

```toml
[overrides.base]
path = "../meridian-base"
```

Each key under `[overrides]` must match a dependency name in `mars.toml`. The override replaces the source URL with a local path for resolution and sync. The original git spec is preserved internally so `mars doctor` can still validate config consistency.

If an override references a dependency name not in config, Mars prints a warning but continues.

See [local-development.md](local-development.md) for workflows.

## Reserved Names

- `_self` is reserved for local package items (`_self` is the synthetic source name for agents/skills coming from the current project when `[package]` is present).
