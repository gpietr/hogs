## What this changes

<!-- What does this PR do, and why? How does it work, in a few sentences? What did you deliberately leave out? -->

## Proof that it works

<!--
Required. Screenshots (TUI) or terminal output (CLI) showing the change working,
on every platform it affects. For bug fixes, show before and after.
PRs without this will be closed.
-->

Tested on:
- [ ] Linux
- [ ] Windows
- [ ] macOS

## Checklist

- [ ] `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass
- [ ] New behaviour has tests
- [ ] This does not let agents delete anything, and does not weaken the protection guard or the apply checks
- [ ] I have read [CONTRIBUTING.md](../CONTRIBUTING.md)
