# Conflict and Collision Handling

Mars handles three types of conflicts: naming collisions between sources, unmanaged file collisions, and content conflicts during updates.

## Naming Collisions

When two sources expose an item with the same destination path (e.g., both have `agents/coder.md`), Mars auto-renames both items.

### Auto-Rename

Both colliding items are suffixed with `__{owner}_{repo}` derived from the source URL or dependency name:

```
agents/coder.md  (from base and from dev)
  → agents/coder__haowjy_meridian-base.md
  → agents/coder__haowjy_meridian-dev-workflow.md
```

Skills follow the same pattern:

```
skills/review  (collision)
  → skills/review__alice_agents
  → skills/review__bob_agents
```

### Frontmatter Rewriting

When skills are auto-renamed, Mars rewrites agent frontmatter (the YAML metadata block at the top of an agent Markdown file) to reference the new skill names. If agent `coder` declares `skills: [review]` and `review` was renamed to `review__alice_agents`, the installed agent file is updated to reference `review__alice_agents`.

The `source_checksum` in the lock file tracks the pre-rewrite hash; `installed_checksum` tracks the post-rewrite hash. This distinction lets Mars detect whether the user modified the file vs. whether Mars's own rewriting changed it.

### Resolving with `mars rename`

Auto-renamed items can be given preferred names:

```bash
mars rename agents/coder__haowjy_meridian-base.md agents/coder.md
```

This adds a `rename` mapping in `mars.toml` for the dependency. The rename persists across syncs. If the other colliding source is later removed, the rename mapping still works (it just applies a no-op mapping).

## Unmanaged File Collisions

When sync would install an item at a path where an unmanaged file already exists (not tracked in `mars.lock`), Mars skips the install and warns:

```
warning: source `base` collides with unmanaged path `agents/custom.md` — leaving existing content untouched
```

The item is removed from the target state, so the unmanaged file is preserved. This protects user-created local agents and skills from being overwritten.

During `mars repair` with a corrupt lock file, unmanaged collisions are handled more aggressively: the colliding path is removed and sync retries, since there's no lock to distinguish managed from unmanaged files.

## Content Conflicts (Three-Way Merge)

When both the source and local disk have changed for a managed item, Mars attempts a three-way merge using the cached base version.

### Diff Matrix

| Source changed? | Local changed? | Action |
|---|---|---|
| No | No | Skip (unchanged) |
| Yes | No | Update (clean overwrite) |
| No | Yes | Keep local modification |
| Yes | Yes | **Conflict** — attempt merge |

"Local changed" is determined by comparing the current disk hash against `installed_checksum` in the lock file. "Source changed" compares the new source hash against `source_checksum` in the lock.

### Merge Process

1. The base version (what Mars originally installed) is cached in `.mars/cache/bases/`
2. Mars performs a three-way merge: base → local (disk) + source (new)
3. If the merge succeeds cleanly, the merged content is installed
4. If the merge has conflicts, conflict markers are written into the file

### Conflict Markers

Conflicted files contain standard git-style markers:

```
<<<<<<< local
your local modification
=======
new source content
>>>>>>> source
```

### Resolving Conflicts

1. Edit the conflicted file to resolve the markers
2. Run `mars resolve` (or `mars resolve <file>` for a specific file)
3. Mars verifies no conflict markers remain and updates `installed_checksum` in the lock

```bash
# Edit the file to fix conflicts
vim .agents/agents/coder.md

# Mark as resolved
mars resolve agents/coder.md

# Or resolve all at once
mars resolve
```

If conflict markers are still present, `mars resolve` reports the file as still conflicted and exits with code 1.

### Force Overwrite

`mars sync --force` skips the merge and overwrites local modifications. The baseline shifts to `source_checksum`, so any file that differs from the original source content is treated as unmodified and overwritten. Use this when you want to discard all local changes and match the source exactly.

## Exit Codes

`mars sync` and `mars resolve` exit with code 1 when unresolved conflicts remain. Use `mars list --status` to see which items are conflicted, or `mars doctor` to check for conflict markers.
