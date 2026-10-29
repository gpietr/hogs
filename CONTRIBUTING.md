# Contributing to hogs

Thanks for helping! Contributions are welcome. hogs deletes files, so changes are reviewed carefully.

## How it works

1. **Open a pull request.** Nobody pushes to `main` directly. The maintainer reviews every PR and decides
   whether, and when, it is merged. A PR can be declined or need changes, even if it is good work, when it
   doesn't fit the project.
2. **For bigger changes, open an issue first.** Examples are a new command, a new platform backend, or
   anything touching deletion. Agreeing on the idea saves you from writing code that won't be merged.

## What every pull request needs

- **A clear description.** Say what the change does and why, how it works in a few sentences, and what
  you deliberately left out. A reviewer should understand it without reading the diff first.
- **Proof that it works.** Add screenshots (for the TUI) or terminal output (for the CLI) that show the
  change working, on each platform it affects (Linux, Windows, macOS). For bug fixes, show the behaviour
  before and after.
- **Passing checks.** CI must be green. Locally:

  ```sh
  cargo fmt --all
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
  ```

- **Tests** for new behaviour, and especially for anything that decides what may be deleted.

Pull requests without a description or without proof of testing will be closed. You are welcome to reopen
them once those are added.

## Ground rules for code

- **Safety first.** Agents must never be able to delete anything themselves. Deletion only happens through
  `hogs staged apply` (which requires an interactive terminal) or the TUI, after re-verification. Don't
  weaken that, the protection guard in `crates/core/src/protect.rs`, or the checks in
  `crates/hogs/src/apply.rs`.
- **Facts, not opinions.** hogs reports sizes, counts, ages and paths. Judging what is safe to delete is
  left to the user and their agent: please don't add hardcoded rules of the form "this folder is junk".
- **Speed matters.** Scanning is the core of the tool. Include before/after timings for changes on hot paths.
- **Match the surrounding code:** its style, comment density and naming.

## License

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE) of this
project.
