# Mars Documentation

Mars is a package manager for `.agents/` directories. It installs agent profiles and skills from git and local sources into one managed root, tracks ownership in `mars.lock`, and links managed content into tool-specific directories like `.claude/` or `.cursor/`.

## Core Concepts

**Sources** are git repositories or local directories that contain agents and skills. Each source may expose many installable units. Mars resolves versions and **transitive skill dependencies** (skills referenced by installed agents that must also be installed) across sources before installing anything.

**Project root** is the directory containing `mars.toml` and `mars.lock` (typically your repo root).

**Managed root** is the directory Mars installs into (default: `.agents/`, configurable via `settings.managed_root`). It contains installed `agents/` and `skills/`.

**`--root`** points at the managed root to override auto-detection. Mars then resolves the corresponding project root (the parent containing `mars.toml`).

**Lock file** (`mars.lock`) records every managed item with its source, version, and content checksums. It is the authority for what Mars manages. See [lock-file.md](lock-file.md) for format details.

**Links** are symlinks from tool directories (`.claude/agents`, `.cursor/skills`) into the managed root. This lets multiple tools share one set of installed agents and skills without copying.

**Filters** control which items from a source get installed: include specific agents/skills, exclude items, or restrict to agents-only or skills-only. See [configuration.md](configuration.md).

## Dependency Names vs Source Specifiers

- In `mars.toml`, the key under `[dependencies.<NAME>]` is the dependency name.
- `mars add` derives that name from the source specifier by default (for example, `haowjy/meridian-base` becomes `meridian-base`).
- Commands like `mars remove`, `mars override`, `mars upgrade`, and `mars why` operate on dependency names, not source URLs.
- `mars list --status` shows a `SOURCE` column; combine with `--source <name>` to filter to one dependency.

## Quick Start

```bash
# Initialize a mars project and link to Claude
mars init --link .claude

# Add sources
mars add haowjy/meridian-base
mars add haowjy/meridian-dev-workflow

# See what's installed
mars list

# Explain why an item is present
mars why reviewer
```

## Managed Layout

```
project/
  mars.toml              # Dependency config (committed)
  mars.lock              # Ownership registry (committed)
  mars.local.toml        # Dev overrides (gitignored)
  .mars/                 # Internal state (gitignored)
  .agents/               # Managed root
    agents/
    skills/
  .claude/               # Tool directory
    agents -> ../.agents/agents
    skills -> ../.agents/skills
```

## Source Inputs

Mars accepts several source forms:

| Form | Example |
|---|---|
| GitHub shorthand | `owner/repo` or `owner/repo@^1.0` |
| HTTPS URL | `https://github.com/owner/repo` |
| SSH URL | `git@github.com:owner/repo.git` |
| Local path | `../my-agents` or `/absolute/path` |

## Documentation

| Document | Contents |
|---|---|
| [configuration.md](configuration.md) | `mars.toml` reference: all fields, filter modes, settings |
| [commands.md](commands.md) | Full CLI reference: every subcommand with flags and examples |
| [sync-pipeline.md](sync-pipeline.md) | How sync works: resolve, target, diff, apply |
| [conflicts.md](conflicts.md) | Collision handling: naming, unmanaged files, merge, resolution |
| [lock-file.md](lock-file.md) | Lock file format and semantics |
| [local-development.md](local-development.md) | Dev workflows: overrides, local paths, submodules |
| [troubleshooting.md](troubleshooting.md) | `mars doctor`, `mars repair`, common problems |

## Design Constraints

- **Resolve first, then act.** If resolution fails, nothing is mutated.
- **Atomic writes.** Config, lock, and installed files use tmp+rename.
- **Lock is authority.** `mars.lock` defines what Mars manages.
- **Explicit intent.** User intent comes from flags and arguments, not heuristics.
