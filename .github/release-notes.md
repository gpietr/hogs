> [!WARNING]
> **Early alpha: expect bugs.** hogs can delete files and has had very little real-world use. It moves files
> to the trash by default, but please try it on scratch data first, keep backups, and read what you are about
> to apply. It comes with no warranty ([MIT license](https://github.com/gpietr/hogs/blob/main/LICENSE)).

**hogs** is a fast disk usage analyzer that you use together with your coding agent: the agent explains and
stages, and only you delete. See the [README](https://github.com/gpietr/hogs#readme) for usage.

### What's new in 0.1.0-alpha.2

- **See what is growing.**
  - hogs now keeps a compact history of its scans: every folder with its exact size, plus files of 1 MiB or more.
  - `hogs diff PATH [--since 1d] [--files]` shows what grew or shrank as a tree, largest change first.
  - `hogs history [PATH]` lists past scans and how sizes changed.
  - In the TUI, press `c` to compare with an earlier scan. That adds a change column and a "largest growth first" sort.
- **Progress while deleting.** `hogs staged apply` shows a live status line and reports each item as it finishes.
  The TUI shows a progress dialog. The staged list is now updated item by item, so an interrupted apply leaves
  it accurate.
- **Windows:** the classic console window showed `?` instead of some bar and marker characters. It now gets
  simpler characters that it can display. Windows Terminal keeps the full set.
- **Fixes:**
  - Failed writes, for example when the disk is full, no longer leave temporary files behind.
  - A staged-list write error no longer stops an apply half-way.
  - History entries are size-checked when they are read.

### Downloads

| Platform | File |
|---|---|
| Linux x86_64 (static, any distribution) | `hogs-*-x86_64-unknown-linux-musl.tar.gz` |
| Windows x86_64 | `hogs-*-x86_64-pc-windows-msvc.zip` |
| macOS Apple Silicon | `hogs-*-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `hogs-*-x86_64-apple-darwin.tar.gz` |

Each file has a `.sha256` checksum next to it. The binaries are not signed, so SmartScreen or Gatekeeper may warn.

### Known limitations

- **History starts empty.** `hogs diff` needs an earlier scan, so it is useful only after hogs has been used for a while.
- **Windows:**
  - The fast MFT scanner needs an Administrator terminal; otherwise hogs walks folders, much more slowly.
  - There is no mount-point check before deleting.
- **macOS:** builds and passes tests on CI, but has hardly been used.
- **Applying from the TUI** always moves to the trash. For permanent deletion, use `hogs staged apply --permanent`.

Please [report problems](https://github.com/gpietr/hogs/issues) with the output or a screenshot.
