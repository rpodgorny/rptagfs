# rptagfs

A tag-based virtual filesystem. `rptagfs` mounts a FUSE filesystem that
re-presents a plain source directory as a navigable tree of **tags**, where
each directory level in the source becomes a tag on the files it contains.

This is a Rust reimplementation (using [`fuse3`](https://crates.io/crates/fuse3))
of an earlier Python version.

## Concept

In the source directory, a file's tags are simply the directory components of
its path. A file stored at:

```
music/rock/live/song.mp3
```

is tagged `music`, `rock`, and `live`. Tags are a **set** — order does not
matter and duplicates collapse (`rock/rock/x` → `{rock}`).

The mounted filesystem lets you browse by those tags instead of by physical
layout:

- The **root** lists every tag as a directory, plus two special directories
  (`_all`, `_untagged`).
- Entering a tag directory **filters** to the files carrying that tag and shows
  the remaining co-occurring tags as further subdirectories. Drilling deeper
  intersects tags (logical **AND**): `rock/live/` shows files tagged with both
  `rock` and `live`.
- Matching files appear as entries inside each tag directory.

### Special directories

| Name        | Contents                                  |
|-------------|-------------------------------------------|
| `_all`      | Every file, as a flat list.               |
| `_untagged` | Files that live at the source root (no tags). |

### Example

Source directory:

```
src/
├── music/
│   └── rock/
│       └── song.mp3
├── photos/
│   └── pic.jpg
└── readme.txt
```

Mounted view:

```
mnt/
├── _all/
│   ├── song.mp3
│   ├── pic.jpg
│   └── readme.txt
├── _untagged/
│   └── readme.txt
├── music/
│   ├── rock/
│   │   └── song.mp3
│   └── song.mp3
└── photos/
    └── pic.jpg
```

When two files in different directories share a name, the second one gets a
`.__N` suffix inserted before its extension (`song.mp3` → `song.__1.mp3`) so
every file has a unique name in the flat views.

## Build

Requires a Rust toolchain and FUSE (`libfuse` / `fuse3` + the `fusermount3`
helper) installed on the system. Mounting uses the unprivileged FUSE path, so
no root is needed.

```sh
cargo build --release
```

The binary is produced at `target/release/rptagfs`.

## Usage

```sh
rptagfs <SOURCE_DIR> <MOUNT_POINT> [--show-hidden] [--debug]
```

| Argument / flag   | Description                                          |
|-------------------|------------------------------------------------------|
| `SOURCE_DIR`      | Existing directory to expose by tags.                |
| `MOUNT_POINT`     | Empty directory to mount the tag view onto.          |
| `--show-hidden`   | Include dot-files and dot-directories in the scan.   |
| `--debug`         | Verbose debug logging (default level is `info`).     |

Example:

```sh
mkdir /tmp/mnt
rptagfs ~/files /tmp/mnt
```

Unmount with `Ctrl-C` in the foreground process, or from another shell:

```sh
fusermount3 -u /tmp/mnt
```

## Modifying tags

The filesystem is writable, and edits are reflected back onto the real files in
the source directory:

- **Tag / retag a file** — move it between tag directories. Moving `_all/x` into
  `rock/` adds the `rock` tag; moving it back out removes it. Under the hood the
  file is physically moved within the source tree.
- **Create a tag** — `mkdir` a new directory; it becomes an available tag.
- **Rename a tag** — rename a tag directory to rename the tag everywhere.
- **Rename a file** — rename it within a tag directory.
- **Delete** — `unlink` a file or `rmdir` a tag.

Files and tags inside the special `_all` / `_untagged` directories cannot be
created there directly.

## Development

The [`Justfile`](./Justfile) wraps the common tasks:

```sh
just test     # cargo nextest run
just clippy   # cargo clippy
just audit    # cargo audit
```

The codebase is split into:

- `src/scanner.rs` — walks the source tree and builds the tag/file index.
- `src/tagfs.rs` — the FUSE filesystem implementation and inode bookkeeping.
- `src/main.rs` — CLI parsing and mount setup.

It ships with an extensive unit/integration test suite (`cargo nextest run`).

## Notes & limitations

- The source tree is scanned **once at mount time**. Changes made directly to
  the source directory while mounted are not picked up until remount; changes
  made *through* the mounted filesystem are kept consistent in both the live
  view and on disk.
- Symlinked directories are listed as tag directories but are **not** recursed
  into (avoiding symlink loops); broken symlinks are skipped.
