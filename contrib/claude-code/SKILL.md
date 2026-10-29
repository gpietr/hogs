---
name: hogs
description: Find what is using disk space and help the user free it with the `hogs` CLI. Use when the user asks what's filling their disk, wants to free space, or asks whether some directory is safe to delete.
---

# Freeing disk space with hogs

`hogs` scans fast (about 1M files/s) and caches each scan for 10 minutes, so run it freely. Every command
accepts a path, which defaults to the current directory. `hogs --help` shows the full workflow.

hogs only reports facts: on-disk size, % of the parent, file count, the age of the newest change
inside, and the path. Judging what something is and whether it is worth deleting is your job.

1. Start with `hogs ls ~ -d 2`, or the directory the user cares about.
2. Drill down with `hogs ls PATH -d 2`, `hogs dirs PATH`, `hogs files PATH`, `hogs ext PATH`, or
   `hogs find PATH --name 'PATTERN' --type d|f --min-size 1G --older 180d`.
3. Before judging anything unfamiliar, look at it: what is next to it (a `package.json` or
   `Cargo.toml` means a project) and what is inside. Remember that `node_modules` or `target`
   inside an installed application is part of that application, not a rebuildable artifact.
4. Explain each suggestion to the user: what it is, its size, when it last changed, and what
   deleting it costs (for example a rebuild, a re-download or lost data). When a tool owns the data,
   suggest the tool instead of deleting files, e.g. `docker system prune`, `cargo clean`,
   `journalctl --vacuum-size=500M` or `flatpak uninstall --unused`.
5. Stage only the items the user agrees to: `hogs stage PATH... --reason "short reason"`. You can also
   pipe paths in: `hogs find ~ --name node_modules --type d --older 180d --paths | hogs stage --reason "..."`.
6. Tell the user to run `hogs staged apply` in their own terminal. It shows your reasons, re-checks
   every item, and moves the items to the trash after they confirm.

Never delete files yourself (`rm`, `del`, `trash`, ...). `hogs staged apply` deliberately refuses to
run without an interactive terminal. Filesystem roots, the home directory and OS directories
(`/usr`, `/etc`, `/var/lib`, `C:\Windows`, ...) can never be staged.
