# hogs

**Find what's hogging your disk, fast, and decide what to delete together with your coding agent.**

> [!WARNING]
> **hogs is early alpha software.** It can delete files, and it has had very little real-world use.
> By default it moves files to the trash rather than deleting them. Even so, try it on scratch data
> first, keep backups, and read what you are about to apply. It comes with no warranty (see [LICENSE](LICENSE)).

hogs is a disk usage analyzer in the spirit of WizTree and ncdu, made for working with coding agents like
Claude Code. hogs gathers the facts: where the space goes, how big things are and how long they have gone
unchanged. Your agent works out what each item is and whether deleting it is a good idea. **Only you
delete anything:** agents can stage items with a reason, and you review and apply them.

```
 hogs   ~/projects/old-dashboard  106.1 MiB on disk · 5 files · scanned 1s ago · sort: size
┌ old-dashboard ─────────────────────────────────────────────────────────┐┌ details ───────────────────────────────────────┐
│✗ 106.0 MiB ██████████ 100.0%     2y node_modules/                      ││node_modules                                    │
│   16.0 KiB              0.0%     2y src/                               ││~/projects/old-dashboard/node_modules           │
│    4.0 KiB              0.0%     2y package.json                       ││                                                │
│                                                                        ││on disk       106.0 MiB (106.0 MiB apparent)    │
│                                                                        ││files         3 · 3 entries directly inside     │
│                                                                        ││newest change 2y ago (2024-10-29)               │
│                                                                        ││                                                │
│                                                                        ││✗ staged by claude-code                         │
│                                                                        ││  “JS dependencies of an app untouched for 2    │
│                                                                        ││years; npm install restores them”               │
│                                                                        ││                                                │
│                                                                        ││Largest files inside                            │
│                                                                        ││ 52.0 MiB     2y                                │
│                                                                        ││react-dom/cjs/react-dom.development.js          │
│                                                                        ││ 31.0 MiB     2y  @babel/core/lib/index.js      │
│                                                                        ││ 23.0 MiB     2y  typescript/lib/typescript.js  │
└ 1/3 ───────────────────────────────────────────────────────────────────┘└────────────────────────────────────────────────┘
 ↑↓ move · → open · ← back · s stage · Tab staged (1, 106.0 MiB) · a apply · / filter · o sort · ? help · q quit
```

## Why hogs

- **Fast.** All timings below are with the file system already cached in memory.
  - **Linux:** a parallel scanner using raw `getdents64` and `statx`. It walks 850k entries in about 0.2 s,
    about 10× faster than `du`, and its totals match `du` byte for byte.
  - **Windows:** run as Administrator, hogs reads the NTFS master file table directly, like WizTree.
    A full `C:\` with 1.2 million files takes **3.7 s**; walking the folders takes 458 s.
- **Built for agents, Unix style.** Plain commands with readable output, plus `--paths` / `-0` for piping.
  Any coding agent that can run shell commands can use hogs, with no special setup.
- **Safe by design.** Agents can't delete. Every deletion is checked again just before it happens, and
  system directories and home folders can never be deleted.
- **Repeated commands are quick.** Each scan is saved for 10 minutes, so a series of commands
  scans the disk only once.

## Install

Download the archive for your platform from the
**[Releases page](https://github.com/gpietr/hogs/releases)**, extract it, and put `hogs` (or `hogs.exe`)
somewhere on your `PATH`.

| Platform | Status |
|---|---|
| Linux (x86_64) | Main development platform. The binary is static and runs on any distribution. |
| Windows (x86_64) | Tested on CI. The fast MFT scanner needs an Administrator terminal; otherwise hogs walks folders. |
| macOS (Apple Silicon, Intel) | Builds and passes tests on CI. Hardly used yet. |

Or build it yourself with [Rust](https://rustup.rs):

```sh
cargo install --git https://github.com/gpietr/hogs hogs
```

The binaries are not signed, so Windows SmartScreen and macOS Gatekeeper may warn about them.

## Using it

### The interactive browser

Run `hogs` (or `hogs tui PATH`) in a terminal.

| Key | Action |
|---|---|
| `↑` `↓` `PgUp` `PgDn` `g` `G` | move |
| `→` / `Enter`, `←` / `Backspace` | open a folder, go up |
| `s` / `Space` | stage or unstage the selected item |
| `Tab` | the staged list: who staged each item and why. `Space` leaves an item out, `u` unstages it |
| `a` | apply: re-check the staged items and move them to the trash, after you confirm |
| `/`, `o`, `r`, `?`, `q` | filter by name, change the sort (size, oldest change, files, name), rescan, help, quit |

Items staged by an agent show up live, marked `✗`, with the agent's reason. Folders containing staged items get `•`.

### Commands

```
$ hogs ls ~/projects -n 4
 437.2 MiB 100.0%       16 files  today  ~/projects/
 190.0 MiB  43.5%        4 files  today  ~/projects/ml-notebook/
 106.1 MiB  24.3%        5 files     2y  ~/projects/old-dashboard/
  82.0 MiB  18.8%        4 files  today  ~/projects/rust-cli/
  59.0 MiB  13.5%        3 files    40d  ~/projects/website/

$ hogs find ~/projects --name node_modules --type d --older 1y
1 matches, 106.0 MiB on disk in total.
 106.0 MiB  24.3%        3 files     2y  ~/projects/old-dashboard/node_modules/

$ hogs find ~/projects --name node_modules --type d --older 1y --paths | hogs stage --reason "untouched for 2 years"
staged ~/projects/old-dashboard/node_modules (106.0 MiB)
```

Each row shows the on-disk size, the share of the parent folder, the file count, the age of the newest
change anywhere inside, and the path.

| Command | What it does |
|---|---|
| `hogs ls [PATH] [-d DEPTH] [-n N]` | a folder's contents, largest first |
| `hogs dirs [PATH]` / `hogs files [PATH]` | the largest folders or files below a path |
| `hogs ext [PATH]` | space used per file extension |
| `hogs find [PATH] --name GLOB --type f\|d --min-size 1G --older 180d` | search; matching folders are not searched further |
| `hogs stage PATH... --reason TEXT` | stage paths for deletion (paths can also come from stdin). Deletes nothing |
| `hogs staged [list\|apply\|remove\|clear]` | review the staged list, or apply it: to the trash, or `--permanent` |
| `hogs scan [PATH]` | rescan and print a summary; `--backend auto\|generic\|mft` picks the scanner |

Global options: `--paths` / `-0` (print only paths), `--rescan`, `--max-age 10m`.

## Working with coding agents

Ask your agent something like *"What's taking up space in my home folder, and what could I delete?"*.
Agents find the workflow in `hogs --help`. For Claude Code, you can also install the included skill:

```sh
mkdir -p ~/.claude/skills/hogs && cp contrib/claude-code/SKILL.md ~/.claude/skills/hogs/
```

The agent explores with the commands above, explains its suggestions and stages what you agree to. Then
**you** apply them with `hogs staged apply`, or with `a` in the browser. `hogs staged apply` refuses to run
without an interactive terminal, so an agent's shell can't run it. Items staged from inside Claude Code are
labelled `claude-code`.

## Safety

- **Staging deletes nothing.** Applying happens only in a terminal you are using, after a confirmation.
- **Every item is checked again before deletion.** hogs refuses an item that:
  - no longer resolves to the staged path (for example a parent folder was swapped for a symlink);
  - is or contains a mount point;
  - has changed since it was staged.
- **Some paths can never be staged or deleted:**
  - filesystem roots;
  - your home folder and its parents;
  - OS directories such as `/usr`, `/etc`, `/var/lib`, `C:\Windows`, `Program Files` and `ProgramData`.
- **Files go to the trash or Recycle Bin by default.** The space is freed only once you empty it.
  `--permanent` deletes directly, after you type a confirmation.
- **Under `sudo`, hogs keeps acting for you.** It uses your staged list and saved scans and protects your
  home folder. Trashed items do go to root's trash, though, and hogs warns you about that.

## Where hogs keeps data

| What | Linux | Windows | macOS |
|---|---|---|---|
| Staged list | `~/.local/share/hogs/` | `%LOCALAPPDATA%\hogs\` | `~/Library/Application Support/hogs/` |
| Saved scans | `~/.cache/hogs/` | `%LOCALAPPDATA%\hogs\cache\` | `~/Library/Caches/hogs/` |

Set `HOGS_DATA_DIR` and `HOGS_CACHE_DIR` to override these locations.

## Contributing

Contributions are welcome through pull requests. Each PR needs a clear description and screenshots or
terminal output showing that it works. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE). hogs is provided "as is", without warranty of any kind, and the authors are not liable for
any damage or data loss arising from its use.
