> [!WARNING]
> **Early alpha: expect bugs.** hogs can delete files and has had very little real-world use. It moves files
> to the trash by default, but please try it on scratch data first, keep backups, and read what you are about
> to apply. It comes with no warranty ([MIT license](https://github.com/gpietr/hogs/blob/main/LICENSE)).

First public build of **hogs**, a fast disk usage analyzer that you use together with your coding agent:
the agent explains and stages, and only you delete. See the [README](https://github.com/gpietr/hogs#readme)
for usage.

### Downloads

| Platform | File |
|---|---|
| Linux x86_64 (static, any distribution) | `hogs-*-x86_64-unknown-linux-musl.tar.gz` |
| Windows x86_64 | `hogs-*-x86_64-pc-windows-msvc.zip` |
| macOS Apple Silicon | `hogs-*-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `hogs-*-x86_64-apple-darwin.tar.gz` |

Each file has a `.sha256` checksum next to it. The binaries are not signed, so SmartScreen or Gatekeeper may warn.

### Known limitations

- **Windows:**
  - The fast MFT scanner needs an Administrator terminal; otherwise hogs walks folders, much more slowly.
  - There is no mount-point check before deleting, and the TUI has barely been tried in a real Windows console.
- **macOS:** builds and passes tests on CI, but has hardly been used.
- **Applying from the TUI** always moves to the trash. For permanent deletion, use `hogs staged apply --permanent`.

Please [report problems](https://github.com/gpietr/hogs/issues) with the output or a screenshot.
