# Local Development

When developing agents and skills, you need fast iteration: edit source, see changes immediately, without committing and pushing to git. Mars supports several workflows for this.

## `mars override`

The primary mechanism for local development. Overrides swap a git source for a local path without modifying the shared config.

```bash
mars override base --path ../meridian-base
```

This writes to `mars.local.toml`:

```toml
[overrides.base]
path = "../meridian-base"
```

And re-syncs, using the local path instead of the git URL. The original git spec is preserved internally, so:
- Other developers aren't affected (the shared `mars.toml` still points at git)
- `mars doctor` can validate that the override name matches a real dependency
- Removing the override returns to the git source seamlessly

### Override + Sync Cycle

```bash
# Set up override once
mars override base --path ../meridian-base

# Edit agents/skills in ../meridian-base/
vim ../meridian-base/agents/coder.md

# Re-sync to pick up changes
mars sync
```

Path sources have no version caching, so `mars sync` always reads the latest content from the local path.

### Removing Overrides

Edit `mars.local.toml` directly and remove the override entry, then `mars sync`:

```bash
# Remove the override
vim mars.local.toml   # delete the [overrides.base] section

# Sync back to git source
mars sync
```

## `mars.local.toml`

This file is gitignored (added by `mars init`). Each developer can have different overrides.

```toml
[overrides.base]
path = "../meridian-base"

[overrides.dev-workflow]
path = "../meridian-dev-workflow"
```

Rules:
- Override names must match dependency names in `mars.toml`
- If an override references a non-existent dependency, Mars warns but continues
- Overrides replace the source URL but preserve filter and rename config from `mars.toml`

## Local Path Dependencies

For sources that are always local (not published to git), use `path` directly in `mars.toml`:

```toml
[dependencies.my-agents]
path = "../my-agents"
```

This is appropriate when:
- The source is a sibling directory that won't be published
- You're developing a new source package and haven't pushed it yet
- The source is a git submodule (path is relative to project root)

Path sources:
- Always resolve to the canonical filesystem path
- Have no version constraint (no semver tags to check)
- Don't appear in `mars outdated` output
- Re-read content on every `mars sync` (no caching)

## Working with Submodules

If your agent sources are git submodules:

```bash
# Submodule at ./meridian-base/
git submodule add https://github.com/haowjy/meridian-base

# Reference as path dependency
# mars.toml:
# [dependencies.base]
# path = "./meridian-base"
```

Or use a git URL dependency with `mars override` for local edits:

```bash
# mars.toml points at git
# [dependencies.base]
# url = "https://github.com/haowjy/meridian-base"
# version = "^1.0"

# Override locally to use the submodule checkout
mars override base --path ./meridian-base
```

## Local Package Development

If your project is itself a source package (has `[package]` in `mars.toml`), its own agents and skills are symlinked into the managed root under the `_self` source name (`_self` is the reserved source identifier for items from the current project).

```toml
[package]
name = "my-project-agents"
version = "0.1.0"
```

With this, any agents in `agents/` and skills in `skills/` at the project root are automatically available in the managed root via symlinks. This lets you develop and test agents/skills locally without installing them from an external source.

### Validating Before Publishing

Before publishing a source package, validate its structure:

```bash
mars check
```

This checks frontmatter (the YAML metadata block at the top of agent/skill Markdown files), naming conventions, duplicate names, and skill dependency references. See [commands.md](commands.md#mars-check) for details.

## Workflow Summary

| Scenario | Approach |
|---|---|
| Iterate on a git source locally | `mars override source --path ../local-checkout` |
| Permanent local source | `path = "../source"` in `mars.toml` |
| Git submodule source | `path = "./submodule"` in `mars.toml` or override |
| Develop agents in this project | Add `[package]` to `mars.toml` |
| Validate before publishing | `mars check` |
