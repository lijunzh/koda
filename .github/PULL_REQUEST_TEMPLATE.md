## What does this PR do?
<!-- One-paragraph summary. Link to issue if applicable: Fixes #123 -->


## Changes
<!-- List the key changes. Be specific about files and functions. -->

- 
- 

## Type
- [ ] Bug fix
- [ ] New feature
- [ ] Refactor (no behavior change)
- [ ] Docs / tests only
- [ ] Performance
- [ ] Security fix

## Files changed
<!-- Key files touched — helps LLM reviewers focus. -->

| File | What changed |
|------|--------------|
| `src/` | |

## Testing
<!-- How did you verify this works? -->

- [ ] `cargo test` passes
- [ ] `cargo clippy -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] Manually tested: <!-- describe -->

## Checklist
- [ ] No files over 600 lines
- [ ] No `unwrap()` in non-test code (use `?` or `.unwrap_or`)
- [ ] Destructive tools require confirmation in Normal mode
- [ ] New tools added to `SAFE_PREFIXES` or `WRITE_TOOLS` comment as appropriate
- [ ] CHANGELOG.md updated (if user-facing)

<!-- For LLM reviewers: focus on security (path traversal, shell injection,
     approval bypass), correctness, and DRY violations. Check that new tools
     are properly gated by the approval system in src/approval.rs. -->
