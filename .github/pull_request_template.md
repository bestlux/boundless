## Summary

Describe the change and the user or maintainer impact.

## Related issue

Link the issue this PR addresses, if any.

## Validation

- [ ] `cargo fmt`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `./scripts/dev/test-suite.ps1 -Profile quick` (when feasible)
- [ ] Not run, with reason explained below

## Notes for review

Call out anything that needs special attention, including Windows-specific behavior, packaging impact, compatibility assumptions, or follow-up work.

## Checklist

- [ ] The diff stays focused on one problem
- [ ] Tests were added or updated when needed
- [ ] Docs were updated when behavior or commands changed
- [ ] Security-sensitive changes were called out explicitly
