# Troubleshooting

## `mars doctor`

The primary diagnostic tool. Checks six areas and reports all issues at once.

```bash
mars doctor
```

### What it checks

| Area | What's validated |
|---|---|
| Config | `mars.toml` parses without errors |
| Lock file | `mars.lock` parses without errors |
| File integrity | Every locked item exists on disk |
| Conflict markers | Agent files don't contain `<<<<<<<` / `>>>>>>>` |
| Config-lock consistency | Every dependency in config has a lock entry |
| Skill references | Every agent's declared skill deps exist on disk |
| Link health | Symlinks exist, point to correct managed root, not broken |

### Exit codes

| Code | Meaning |
|---|---|
| 0 | All checks passed |
| 2 | Issues found (printed to stderr) |

### Example output

```
  agents/coder.md has unresolved conflict markers
  dependency `dev` is in config but not in lock — run `mars sync`
  agent `coder` references missing skill `planning` — add a source that provides it
  link `.claude` — .claude/agents points to ../other (expected .agents/agents)
```

### Symlinked items

Doctor skips validation for individually symlinked items (not directory-level links, but single-file symlinks inside `agents/` or `skills/`). It reports them as informational and moves on. `_self` symlinks (`_self` is Mars' reserved source name for this project's own package items) are silently skipped.

## `mars repair`

When things are broken beyond what `mars sync` can fix.

```bash
mars repair
```

### What it does

1. Loads the lock file. If corrupt, resets to empty and warns.
2. Runs a forced sync (`--force` mode): overwrites all managed files from dependencies.
3. During corrupt-lock recovery, removes unmanaged collision paths and retries (bounded to 1024 retries).

### When to use

- Lock file is corrupt (parse errors)
- Files were manually deleted or moved
- Managed root is in an inconsistent state
- `mars sync` reports errors that don't resolve

### What it doesn't do

- Fix config errors (you need to fix `mars.toml` manually)
- Recover deleted sources (the git cache may need to re-fetch)
- Preserve local modifications (forced sync overwrites everything)

## Common Problems

### "no mars.toml found"

Mars auto-detects from the current directory, then maps the managed root back to the project root (directory containing `mars.toml`), stopping at the nearest `.git` boundary. Solutions:

- Run `mars init` to create the config
- Use `--root <path>` to point directly at the managed root (for example, `.agents/`) when auto-detection picks the wrong location
- Make sure you're inside the git repository that contains the project root

### "dependency `X` has both `url` and `path`"

Each dependency must have exactly one source type. Edit `mars.toml` to keep only `url` or `path`.

### "only_skills and only_agents are mutually exclusive"

Invalid filter combination. A dependency can restrict to skills-only or agents-only, not both. Pick one or remove both for all items.

### "lock file error" / "failed to parse mars.lock"

The lock file is corrupt. Run:

```bash
mars repair
```

This resets the lock and rebuilds from dependencies.

### "collides with unmanaged path"

A managed item would overwrite a file that Mars doesn't own. Options:
- Move the unmanaged file out of the way
- Rename the managed item: `mars rename agents/conflict.md agents/other-name.md`
- If this is during repair, the file is removed automatically

### Missing files on disk

If `mars doctor` reports missing files:

```bash
mars sync      # Reinstall from dependencies
# or
mars repair    # Full rebuild if sync doesn't fix it
```

### Conflict markers in files

Edit the files to resolve conflicts, then:

```bash
mars resolve                     # Resolve all
mars resolve agents/coder.md    # Resolve specific file
```

### Link points to wrong target

```bash
mars link --unlink .claude   # Remove old link
mars link .claude            # Re-create correct link
```

Or with force:

```bash
mars link .claude --force    # Replace whatever exists
```

### "override `X` references a dependency not in mars.toml"

The `mars.local.toml` has an override for a dependency that doesn't exist in config. Either:
- Add the dependency to `mars.toml`
- Remove the stale override from `mars.local.toml`

### Cache taking too much space

```bash
mars cache info     # See disk usage
mars cache clean    # Remove all cached sources
```

The cache is rebuilt automatically on the next sync.

### Stale lock after dependency changes

If you edited `mars.toml` manually:

```bash
mars sync    # Re-resolve and update lock
```

If `mars sync` fails with frozen mode in CI:

```bash
# Locally:
mars sync          # Update lock
git add mars.lock
git commit -m "Update lock file"
```

## Diagnostic Workflow

For any issue, start with:

```bash
mars doctor          # What's wrong?
mars sync            # Can sync fix it?
mars repair          # Nuclear option: rebuild everything
```

If the problem persists after repair, check:
1. Is `mars.toml` valid? (`mars doctor` reports config errors)
2. Are git sources accessible? (network, auth)
3. Is the managed root writable? (permissions)
