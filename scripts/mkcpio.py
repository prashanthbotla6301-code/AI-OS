#!/usr/bin/env python3
"""Create a deterministic ``newc`` initramfs archive.

The image build must not need root just to create /dev/console.  cpio stores
character-device metadata in the archive, so this small writer can add the
kernel console nodes without calling mknod on the host.
"""
from __future__ import annotations

import os
import stat
import struct
import sys
from pathlib import Path

MAGIC = b"070701"
TRAILER = "TRAILER!!!"
EPOCH = int(os.environ.get("SOURCE_DATE_EPOCH") or "0")


def align4(value: int) -> int:
    return (value + 3) & ~3


def field(value: int) -> bytes:
    return f"{value:08x}".encode("ascii")


def record(
    name: str,
    mode: int,
    data: bytes = b"",
    *,
    major: int = 0,
    minor: int = 0,
    rmajor: int = 0,
    rminor: int = 0,
) -> bytes:
    name_bytes = name.encode("utf-8") + b"\0"
    header = b"".join(
        [
            MAGIC,
            field(0),  # inode
            field(mode),
            field(0),  # uid
            field(0),  # gid
            field(1),  # nlink
            field(EPOCH),
            field(len(data)),
            field(major),
            field(minor),
            field(rmajor),
            field(rminor),
            field(len(name_bytes)),
            field(0),  # check
        ]
    )
    assert len(header) == 110
    return header + name_bytes + b"\0" * (align4(110 + len(name_bytes)) - 110 - len(name_bytes)) + data + b"\0" * (align4(len(data)) - len(data))


def entries(root: Path):
    # The archive root itself is represented by all its children. Sorting gives
    # reproducible output independent of filesystem directory order.
    paths = sorted(root.rglob("*"), key=lambda p: p.relative_to(root).as_posix())
    for path in paths:
        rel = path.relative_to(root).as_posix()
        # Device nodes are injected explicitly by main() so an unprivileged
        # build can produce them without host mknod.
        if rel in {"dev", "dev/console", "dev/tty", "dev/null"}:
            continue
        st = path.lstat()
        mode = stat.S_IFMT(st.st_mode) | (st.st_mode & 0o7777)
        if stat.S_ISDIR(st.st_mode):
            yield record(rel, mode)
        elif stat.S_ISREG(st.st_mode):
            yield record(rel, mode, path.read_bytes())
        elif stat.S_ISLNK(st.st_mode):
            yield record(rel, mode, os.readlink(path).encode("utf-8"))
        else:
            raise ValueError(f"unsupported special file in staging tree: {path}")


def main() -> int:
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} STAGING_DIR OUTPUT_CPIO", file=sys.stderr)
        return 2
    root = Path(sys.argv[1]).resolve()
    out = Path(sys.argv[2])
    if not root.is_dir():
        print(f"mkcpio: staging directory does not exist: {root}", file=sys.stderr)
        return 1
    out.parent.mkdir(parents=True, exist_ok=True)
    with out.open("wb") as stream:
        # These nodes are needed before agentos-core mounts devtmpfs. Later the
        # kernel-created devtmpfs hides/replaces them with live device nodes.
        stream.write(record("dev", stat.S_IFDIR | 0o755))
        for name, rmajor, rminor, perms in [
            ("dev/console", 5, 1, 0o600),
            ("dev/tty", 5, 0, 0o666),
            ("dev/null", 1, 3, 0o666),
        ]:
            stream.write(record(name, stat.S_IFCHR | perms, rmajor=rmajor, rminor=rminor))
        for item in entries(root):
            stream.write(item)
        stream.write(record(TRAILER, stat.S_IFREG | 0o644))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
