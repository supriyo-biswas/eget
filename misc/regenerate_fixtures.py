#!/usr/bin/env python3
"""Regenerate deterministic archive-security fixtures.

Requires Python 3 and the `7z` and `zstd` commands. All metadata and compression
inputs are fixed so repeated runs produce byte-identical files.
"""

from __future__ import annotations

import base64
import bz2
import gzip
import io
import lzma
import os
import shutil
import struct
import subprocess
import tarfile
import tempfile
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent / "tests/fixtures/archives"
MTIME = 946684800
DATA = b"deterministic fixture payload\n"
SCRIPT = b"#!/usr/bin/env sh\nprintf 'fixture\\n'\n"
ENCRYPTED_7Z = (
    b"N3q8ryccAATv+g8lkAAAAAAAAAA+AAAAAAAAAPXHRIBkD+QzCytbRAWZxm4VsyeKBPDMlZa4q2r3dcM+MVdB9oNlQOaw7FDwUdof8R3sWHJ6bqZsDzulvR40HDOFC78G58AN2F16IaCxQbuXhhX+CqM7rOAND5uqQKIB1P6fwcTPynLIS8liKLtwX80m1lUIngkV5xa9FHlSaynT+jFgk4HQB6b3+Ph73o2j+F4h7VgXBhABCYCAAAcLAQACJAbxBwESUw9xE0Hq7GFc6fuAqF+5BFnbIwMBAQVdABAAAAEADICAgLoKAeIV7FYAAA=="
)


def info(name: str, kind: bytes = tarfile.REGTYPE, mode: int = 0o644) -> tarfile.TarInfo:
    item = tarfile.TarInfo(name)
    item.type = kind
    item.mode = mode
    item.mtime = MTIME
    item.uid = item.gid = 0
    item.uname = item.gname = ""
    return item


def add_bytes(tar: tarfile.TarFile, name: str, data: bytes, mode: int) -> None:
    item = info(name, mode=mode)
    item.size = len(data)
    tar.addfile(item, io.BytesIO(data))


def safe_tar() -> bytes:
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        tar.addfile(info("package", tarfile.DIRTYPE, 0o755))
        tar.addfile(info("package/bin", tarfile.DIRTYPE, 0o755))
        tar.addfile(info("package/empty", tarfile.DIRTYPE, 0o700))
        tar.addfile(info("package/share", tarfile.DIRTYPE, 0o755))
        add_bytes(tar, "package/bin/tool", SCRIPT, 0o755)
        add_bytes(tar, "package/share/data.bin", DATA, 0o640)
        link = info("package/bin/tool-link", tarfile.SYMTYPE, 0o777)
        link.linkname = "tool"
        tar.addfile(link)
        hard = info("package/share/data-hard", tarfile.LNKTYPE, 0o640)
        hard.linkname = "package/share/data.bin"
        tar.addfile(hard)
    return out.getvalue()


def write_tar_variants(raw: bytes) -> None:
    (ROOT / "safe.tar").write_bytes(raw)
    with (ROOT / "safe.tar.gz").open("wb") as output:
        with gzip.GzipFile(filename="", mode="wb", fileobj=output, mtime=0) as compressed:
            compressed.write(raw)
    (ROOT / "safe.tar.bz2").write_bytes(bz2.compress(raw, compresslevel=9))
    (ROOT / "safe.tar.xz").write_bytes(lzma.compress(raw, format=lzma.FORMAT_XZ, preset=9))
    subprocess.run(
        ["zstd", "-q", "-19", "--no-check", "-f", str(ROOT / "safe.tar"), "-o", str(ROOT / "safe.tar.zst")],
        check=True,
    )


def safe_zip() -> None:
    def item(name: str, mode: int) -> zipfile.ZipInfo:
        entry = zipfile.ZipInfo(name, (2000, 1, 1, 0, 0, 0))
        entry.create_system = 3
        entry.external_attr = mode << 16
        entry.compress_type = zipfile.ZIP_DEFLATED
        return entry

    with zipfile.ZipFile(ROOT / "safe.zip", "w", compresslevel=9) as archive:
        archive.writestr(item("package/", 0o40755), b"")
        archive.writestr(item("package/bin/", 0o40755), b"")
        archive.writestr(item("package/empty/", 0o40700), b"")
        archive.writestr(item("package/share/", 0o40755), b"")
        archive.writestr(item("package/bin/tool", 0o100755), SCRIPT)
        archive.writestr(item("package/share/data.bin", 0o100640), DATA)
        archive.writestr(item("package/bin/tool-link", 0o120777), b"tool")


def seven_zip(output: Path, source: Path, *names: str) -> None:
    subprocess.run(
        [
            "7z",
            "a",
            "-bd",
            "-bb0",
            "-y",
            "-t7z",
            "-m0=lzma2",
            "-mx=9",
            "-ms=on",
            "-mmt=off",
            "-mtc=off",
            "-mta=off",
            "-mtm=on",
            "-snl",
            str(output),
            *names,
        ],
        cwd=source,
        check=True,
        stdout=subprocess.DEVNULL,
    )


def set_mtime(path: Path) -> None:
    os.utime(path, (MTIME, MTIME), follow_symlinks=False)


def seven_zip_fixtures() -> None:
    with tempfile.TemporaryDirectory() as temporary:
        source = Path(temporary)
        package = source / "package"
        (package / "bin").mkdir(parents=True)
        (package / "empty").mkdir()
        (package / "share").mkdir()
        (package / "bin/tool").write_bytes(SCRIPT)
        (package / "share/data.bin").write_bytes(DATA)
        (package / "bin/tool-link").symlink_to("tool")
        os.chmod(package / "bin/tool", 0o755)
        os.chmod(package / "share/data.bin", 0o640)
        os.chmod(package / "empty", 0o700)
        for path in [
            package / "bin/tool",
            package / "share/data.bin",
            package / "bin/tool-link",
            package / "bin",
            package / "empty",
            package / "share",
            package,
        ]:
            set_mtime(path)
        seven_zip(ROOT / "safe.7z", source, "package")

        escape = source / "escape"
        escape.symlink_to("../../eget-fixture-outside")
        set_mtime(escape)
        seven_zip(ROOT / "symlink-escape.7z", source, "escape")

    # 7-Zip encryption uses random salt and IV values, so retain a canonical
    # encrypted archive as base64 to keep fixture regeneration deterministic.
    (ROOT / "encrypted.7z").write_bytes(base64.b64decode(ENCRYPTED_7Z))


def malicious_tar(name: str, entries: list[tuple[tarfile.TarInfo, bytes | None]]) -> None:
    with tarfile.open(ROOT / name, "w", format=tarfile.USTAR_FORMAT) as archive:
        for entry, data in entries:
            if data is not None:
                entry.size = len(data)
                archive.addfile(entry, io.BytesIO(data))
            else:
                archive.addfile(entry)


def malicious() -> None:
    malicious_tar("absolute.tar", [(info("/tmp/eget-absolute", mode=0o644), b"bad")])
    malicious_tar("dotdot.tar", [(info("../eget-dotdot", mode=0o644), b"bad")])
    link = info("link", tarfile.SYMTYPE, 0o777)
    link.linkname = "outside"
    malicious_tar("symlink-child.tar", [(link, None), (info("link/child", mode=0o644), b"bad")])
    hard = info("hard", tarfile.LNKTYPE, 0o644)
    hard.linkname = "../../outside"
    malicious_tar("hardlink-escape.tar", [(hard, None)])
    malicious_tar("fifo.tar", [(info("pipe", tarfile.FIFOTYPE, 0o644), None)])
    malicious_tar(
        "conflicting-duplicate.tar",
        [(info("same", tarfile.DIRTYPE, 0o755), None), (info("same", mode=0o644), b"bad")],
    )


def singles() -> None:
    with (ROOT / "single.gz").open("wb") as output:
        with gzip.GzipFile(filename="", mode="wb", fileobj=output, mtime=0) as compressed:
            compressed.write(SCRIPT)
    (ROOT / "single.bz2").write_bytes(bz2.compress(SCRIPT, compresslevel=9))
    (ROOT / "single.xz").write_bytes(lzma.compress(SCRIPT, format=lzma.FORMAT_XZ, preset=9))
    plain = ROOT / "single"
    plain.write_bytes(SCRIPT)
    subprocess.run(["zstd", "-q", "-19", "--no-check", "-f", str(plain), "-o", str(ROOT / "single.zst")], check=True)
    plain.unlink()


def elf(machine: int, kind: int, entry: int, osabi: int = 0, interpreter: bytes | None = None) -> bytes:
    ident = b"\x7fELF" + bytes([2, 1, 1, osabi]) + bytes(8)
    phoff = 64 if interpreter is not None else 0
    phnum = 1 if interpreter is not None else 0
    header = ident + struct.pack("<HHIQQQIHHHHHH", kind, machine, 1, entry, phoff, 0, 0, 64, 56, phnum, 64, 0, 0)
    if interpreter is None:
        return header
    data = interpreter + b"\0"
    program = struct.pack("<IIQQQQQQ", 3, 4, 120, 0, 0, len(data), len(data), 1)
    return header + program + data


def macho(cpu: int, kind: int) -> bytes:
    return struct.pack("<IiiIIIII", 0xFEEDFACF, cpu, 0, kind, 0, 0, 0, 0)


def executable_fixtures() -> None:
    root = ROOT.parent / "executables"
    if root.exists():
        shutil.rmtree(root)
    root.mkdir(parents=True)
    (root / "elf-x86_64-exec").write_bytes(elf(62, 2, 0x400000))
    (root / "elf-aarch64-exec").write_bytes(elf(183, 2, 0x400000))
    (root / "elf-x86_64-pie").write_bytes(elf(62, 3, 0x400000))
    (root / "elf-x86_64-static-pie").write_bytes(elf(62, 3, 0x500000))
    (root / "elf-x86_64-shared").write_bytes(elf(62, 3, 0))
    (root / "elf-x86_64-freebsd").write_bytes(elf(62, 2, 0x400000, 9))
    (root / "elf-x86_64-missing-loader").write_bytes(
        elf(62, 3, 0x400000, interpreter=b"/definitely/missing/ld.so")
    )
    arm = macho(0x0100000C, 2)
    x86_dylib = macho(0x01000007, 6)
    (root / "macho-arm64-exec").write_bytes(arm)
    (root / "macho-arm64-dylib").write_bytes(macho(0x0100000C, 6))
    offset_one = 8 + 2 * 20
    offset_two = offset_one + len(x86_dylib)
    fat = struct.pack(">II", 0xCAFEBABE, 2)
    fat += struct.pack(">iiIII", 0x01000007, 0, offset_one, len(x86_dylib), 0)
    fat += struct.pack(">iiIII", 0x0100000C, 0, offset_two, len(arm), 0)
    (root / "macho-universal-exec").write_bytes(fat + x86_dylib + arm)
    (root / "script-env").write_bytes(SCRIPT)
    (root / "script-absolute").write_bytes(b"#!/bin/sh\necho fixture\n")


def main() -> None:
    if ROOT.exists():
        shutil.rmtree(ROOT)
    ROOT.mkdir(parents=True)
    raw = safe_tar()
    write_tar_variants(raw)
    safe_zip()
    seven_zip_fixtures()
    malicious()
    singles()
    executable_fixtures()


if __name__ == "__main__":
    main()
