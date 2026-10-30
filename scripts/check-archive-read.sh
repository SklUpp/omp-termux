#!/usr/bin/env bash
# Real-fixture acceptance check for `omp read` archive support.
#
# Build once, then run:  OMP_BIN=./target/debug/omp scripts/check-archive-read.sh
# Fixtures are generated with the Python stdlib (zipfile/tarfile/gzip/bz2/lzma)
# and the CLI is exercised end to end; nothing is mocked and no Rust test target
# runs. `python3` MUST be on PATH: it is both the fixture generator and the
# runtime prerequisite the archive backend spawns (`python3 -I -B -c`), and no
# Python packages beyond the standard library are used.
set -euo pipefail

if ! command -v python3 >/dev/null 2>&1; then
	echo "check-archive-read: python3 is required (archive backend prerequisite)" >&2
	exit 3
fi

OMP_BIN="${OMP_BIN:-}"
if [[ -z "$OMP_BIN" ]]; then
	for candidate in ./target/debug/omp ./target/release/omp; do
		if [[ -x "$candidate" ]]; then
			OMP_BIN="$candidate"
			break
		fi
	done
fi
if [[ -z "$OMP_BIN" || ! -x "$OMP_BIN" ]]; then
	echo "check-archive-read: set OMP_BIN to a built omp binary" >&2
	exit 2
fi
OMP_BIN="$(cd "$(dirname "$OMP_BIN")" && pwd)/$(basename "$OMP_BIN")"

work="$(mktemp -d "${TMPDIR:-/tmp}/omp-archive-check.XXXXXX")"
trap 'rm -rf "$work"' EXIT

python3 - "$work" <<'PY'
import bz2
import gzip
import io
import lzma
import os
import struct
import sys
import tarfile
import zipfile
import zlib

root = sys.argv[1]
os.makedirs(os.path.join(root, "sub", "nested"), exist_ok=True)


with zipfile.ZipFile(os.path.join(root, "hello.zip"), "w") as zf:
    zf.writestr("docs/notes.txt", "alpha\nbeta\ngamma\ndelta\n")
    zf.writestr("docs/nested/deep.txt", "deep\n")
    zf.writestr("docs/ünïcode.txt", "héllo\n")
    zf.writestr("empty.txt", "")
    zf.writestr("binary.bin", b"\x00\x01\x02binary")
    zf.writestr("invalid-utf8.txt", b"caf\xe9\n")
    zf.writestr("emptydir/", b"")

with zipfile.ZipFile(os.path.join(root, "traversal.zip"), "w") as zf:
    zf.writestr("../evil.txt", "escaped")
    zf.writestr("/abs.txt", "absolute")
    zf.writestr("safe.txt", "safe")

with zipfile.ZipFile(os.path.join(root, "literal.zip"), "w") as zf:
    zf.writestr("inside.txt", "archive\n")

with zipfile.ZipFile(os.path.join(root, "links.zip"), "w") as zf:
    zf.writestr("docs/notes.txt", "alpha\n")
    safe = zipfile.ZipInfo("safe-link")
    safe.external_attr = 0o120777 << 16
    zf.writestr(safe, "docs/notes.txt")
    escape = zipfile.ZipInfo("escape-link")
    escape.external_attr = 0o120777 << 16
    zf.writestr(escape, "../../etc/passwd")

with zipfile.ZipFile(os.path.join(root, "links-darwin.zip"), "w") as zf:
    zf.writestr("docs/notes.txt", "alpha\n")
    darwin = zipfile.ZipInfo("mac-link")
    darwin.create_system = 19  # OS X: st_mode bits are authoritative
    darwin.external_attr = 0o120777 << 16
    zf.writestr(darwin, "docs/notes.txt")
    dos = zipfile.ZipInfo("dos-mode")
    dos.create_system = 0  # DOS: the same bits are junk and must be ignored
    dos.external_attr = 0o120777 << 16
    zf.writestr(dos, "plain payload\n")

with open(os.path.join(root, "literal.zip:1-2"), "w", encoding="utf-8") as handle:
    handle.write("literal\n")

tar_path = os.path.join(root, "hello.tar")
with tarfile.open(tar_path, "w", format=tarfile.PAX_FORMAT) as tf:
    for name, data in (("docs/notes.txt", b"alpha\nbeta\ngamma\ndelta\n"), ("docs/Ω.txt", "Ωmega\n".encode())):
        info = tarfile.TarInfo(name)
        info.size = len(data)
        tf.addfile(info, io.BytesIO(data))
    directory = tarfile.TarInfo("docs/")
    directory.type = tarfile.DIRTYPE
    tf.addfile(directory)

with open(tar_path, "rb") as raw:
    tar_bytes = raw.read()
with gzip.open(os.path.join(root, "hello.tar.gz"), "wb") as stream:
    stream.write(tar_bytes)
with lzma.open(os.path.join(root, "hello.tar.xz"), "wb") as stream:
    stream.write(tar_bytes)
with bz2.open(os.path.join(root, "hello.tar.bz2"), "wb") as stream:
    stream.write(tar_bytes)

with tarfile.open(os.path.join(root, "hardlink.tar"), "w") as tf:
    payload = b"hard target\n"
    base = tarfile.TarInfo("base.txt")
    base.size = len(payload)
    tf.addfile(base, io.BytesIO(payload))
    hard = tarfile.TarInfo("hard.txt")
    hard.type = tarfile.LNKTYPE
    hard.linkname = "base.txt"
    tf.addfile(hard)

with tarfile.open(os.path.join(root, "symlink.tar"), "w") as tf:
    link = tarfile.TarInfo("escape")
    link.type = tarfile.SYMTYPE
    link.linkname = "../../etc/passwd"
    tf.addfile(link)
    loop = tarfile.TarInfo("cycle")
    loop.type = tarfile.SYMTYPE
    loop.linkname = "cycle"
    tf.addfile(loop)
    data = b"inside\n"
    inside = tarfile.TarInfo("inside.txt")
    inside.size = len(data)
    tf.addfile(inside, io.BytesIO(data))

# Declared 65 MiB member with no payload: the index must reject the size
# before any read is attempted.
with open(os.path.join(root, "oversize.tar"), "wb") as handle:
    header = bytearray(512)
    header[:9] = b"big.bin\x00\x00"
    header[100:108] = b"0000000\x00"
    header[108:116] = b"0000000\x00"
    header[116:124] = b"0000000\x00"
    header[124:136] = ("%011o\x00" % (65 * 1024 * 1024)).encode()
    header[136:148] = b"00000000000\x00"
    header[148:156] = b" " * 8
    header[156:157] = b"0"
    header[257:263] = b"ustar\x00"
    header[263:265] = b"00"
    checksum = sum(header)
    header[148:156] = ("%06o\x00 " % checksum).encode()
    handle.write(header)
    handle.write(b"\x00" * 1024)

with gzip.open(os.path.join(root, "notes.txt.gz"), "wb") as stream:
    stream.write(b"single stream\n")
with bz2.open(os.path.join(root, "notes.txt.bz2"), "wb") as stream:
    stream.write(b"bz2 stream\n")
with lzma.open(os.path.join(root, "notes.txt.xz"), "wb") as stream:
    stream.write(b"xz stream\n")

with open(os.path.join(root, "unsupported.rar"), "wb") as handle:
    handle.write(b"Rar!\x1a\x07\x00")

with zipfile.ZipFile(os.path.join(root, "sub", "nested", "suffix.zip"), "w") as zf:
    zf.writestr("inner.txt", "suffix\n")

# --- hostile allocation fixtures (all small on disk) -----------------------

# Every ZIP compression method the backend claims: STORED/DEFLATED go through
# `ZipExtFile`, BZIP2/LZMA through the bounded decoders. A regression that
# silently drops a method shows up as a failed read, not a passing skip.
with zipfile.ZipFile(os.path.join(root, "codecs.zip"), "w") as zf:
    zf.writestr("stored.txt", "stored member\n")
    deflated = zipfile.ZipInfo("deflate.txt")
    deflated.compress_type = zipfile.ZIP_DEFLATED
    zf.writestr(deflated, "deflate member\n")
    bzipped = zipfile.ZipInfo("bzip.txt")
    bzipped.compress_type = zipfile.ZIP_BZIP2
    zf.writestr(bzipped, "bzip member\n")
    lzma_member = zipfile.ZipInfo("lzma.txt")
    lzma_member.compress_type = zipfile.ZIP_LZMA
    zf.writestr(lzma_member, "lzma member\n")

# ZIP LZMA member whose declared dictionary is 1 GiB: the payload offset is
# derived from the local header, then the properties must be rejected before a
# decoder is built. Compressed data and CRC are untouched (CRC covers the
# uncompressed bytes).
bomb_path = os.path.join(root, "lzma-dict.zip")
with zipfile.ZipFile(bomb_path, "w") as zf:
    member = zipfile.ZipInfo("bomb.txt")
    member.compress_type = zipfile.ZIP_LZMA
    zf.writestr(member, b"bomb\n")
with open(bomb_path, "rb") as raw:
    payload_bytes = bytearray(raw.read())
start = payload_bytes.find(b"PK\x03\x04")
name_len = int.from_bytes(payload_bytes[start + 26 : start + 28], "little")
extra_len = int.from_bytes(payload_bytes[start + 28 : start + 30], "little")
payload_at = start + 30 + name_len + extra_len
# ZIP LZMA payload: version(2) props_size(2) props(5: lclppb + dict size)
payload_bytes[payload_at + 5 : payload_at + 9] = (1 << 30).to_bytes(4, "little")
with open(bomb_path, "wb") as handle:
    handle.write(payload_bytes)


def zip_member_region(path):
    """Return the raw bytes, payload start and central-directory offset."""
    raw = bytearray(open(path, "rb").read())
    eocd = raw.rfind(b"PK\x05\x06")
    directory = int.from_bytes(raw[eocd + 16 : eocd + 20], "little")
    name_length = int.from_bytes(raw[26:28], "little")
    extra_length = int.from_bytes(raw[28:30], "little")
    return raw, 30 + name_length + extra_length, directory


def rewrite_zip_member(path, payload, flags=None):
    """Replace the only member's payload, fixing both headers and offsets."""
    raw, payload_at, directory = zip_member_region(path)
    head = bytearray(raw[:payload_at])
    tail = bytearray(raw[directory:])
    size = len(payload).to_bytes(4, "little")
    for header, flag_at, size_at in ((head, 6, 18), (tail, 8, 20)):
        header[size_at : size_at + 4] = size
        if flags is not None:
            header[flag_at : flag_at + 2] = flags.to_bytes(2, "little")
    eocd_at = raw.rfind(b"PK\x05\x06") - directory
    tail[eocd_at + 16 : eocd_at + 20] = (payload_at + len(payload)).to_bytes(4, "little")
    with open(path, "wb") as handle:
        handle.write(head + bytes(payload) + tail)


# ZIP LZMA general-purpose bit 1 says the raw stream carries an end marker.
# Real writers emit both forms, so both must read; a stream that stops before
# the marker it declared must not.
LZMA_PLAIN = b"no eos member\n"
lzma_eos = os.path.join(root, "lzma-eos.zip")
with zipfile.ZipFile(lzma_eos, "w") as zf:
    member = zipfile.ZipInfo("eos.txt")
    member.compress_type = zipfile.ZIP_LZMA
    zf.writestr(member, LZMA_PLAIN)
raw, payload_at, directory = zip_member_region(lzma_eos)
lzma_payload = bytes(raw[payload_at:directory])
probe = zipfile.LZMADecompressor()
if probe.decompress(lzma_payload) != LZMA_PLAIN or not probe.eof:
    raise SystemExit("lzma-eos.zip: the writer did not emit an end marker")

# Same stream with the marker tail removed and bit 1 cleared: the declared
# compressed size delimits the member, so the bounded decoder must accept it.
marker_trim = 0
for cut in range(1, 17):
    probe = zipfile.LZMADecompressor()
    if probe.decompress(lzma_payload[:-cut]) == LZMA_PLAIN and not probe.eof:
        marker_trim = cut
if not marker_trim:
    raise SystemExit("lzma-noeos.zip: no marker-free prefix decodes completely")
lzma_noeos = os.path.join(root, "lzma-noeos.zip")
with zipfile.ZipFile(lzma_noeos, "w") as zf:
    member = zipfile.ZipInfo("noeos.txt")
    member.compress_type = zipfile.ZIP_LZMA
    zf.writestr(member, LZMA_PLAIN)
rewrite_zip_member(lzma_noeos, lzma_payload[:-marker_trim], 0)

# BZIP2 member whose end marker and stream CRC were cut: the decompressed
# bytes are complete, so only an explicit end-of-stream requirement rejects it.
BZIP2_PLAIN = b"bzip member\n"
bzip2_path = os.path.join(root, "truncated-bzip2.zip")
with zipfile.ZipFile(bzip2_path, "w") as zf:
    member = zipfile.ZipInfo("truncated.txt")
    member.compress_type = zipfile.ZIP_BZIP2
    zf.writestr(member, BZIP2_PLAIN)
raw, payload_at, directory = zip_member_region(bzip2_path)
bzip2_payload = bytes(raw[payload_at:directory])
marker_trim = 0
for cut in range(1, 21):
    probe = bz2.BZ2Decompressor()
    try:
        complete = probe.decompress(bzip2_payload[:-cut]) == BZIP2_PLAIN
    except OSError:
        complete = False
    if complete and not probe.eof:
        marker_trim = cut
if not marker_trim:
    raise SystemExit("truncated-bzip2.zip: no complete-but-unterminated prefix")
rewrite_zip_member(bzip2_path, bzip2_payload[:-marker_trim])


def write_zip64(path, entries, directory_size, directory_offset):
    """Emit ZIP64 record + locator + classic EOCD carrying the sentinels."""
    record = struct.pack(
        "<4sQ2H2L4Q",
        b"PK\x06\x06",
        44,
        45,
        45,
        0,
        0,
        entries,
        entries,
        directory_size,
        directory_offset,
    )
    locator = struct.pack("<4sLQL", b"PK\x06\x07", 0, 0, 1)
    eocd = struct.pack(
        "<4s4H2LH",
        b"PK\x05\x06",
        0,
        0,
        0xFFFF,
        0xFFFF,
        0xFFFFFFFF,
        0xFFFFFFFF,
        0,
    )
    with open(path, "wb") as handle:
        handle.write(record)
        handle.write(locator)
        handle.write(eocd)


# Valid ZIP64 empty archive: record at 0, zero-length directory.
write_zip64(os.path.join(root, "empty64.zip"), 0, 0, 0)
# Hostile ZIP64: the record claims a 100 MiB directory, past the index ceiling.
write_zip64(os.path.join(root, "bad64.zip"), 2, 100 * 1024 * 1024, 0)
# Hostile classic end record: 4 GiB directory size without the ZIP64 sentinel.
with open(os.path.join(root, "bad-classic.zip"), "wb") as handle:
    handle.write(
        struct.pack(
            "<4s4H2LH",
            b"PK\x05\x06",
            0,
            0,
            1,
            1,
            0xFFFFFFFE,
            0,
            0,
        )
    )


def xz_with_declared_dictionary(code):
    """XZ stream header + block header declaring an LZMA2 dictionary code."""
    flags = b"\x00\x00"
    stream_header = b"\xfd7zXZ\x00" + flags + struct.pack("<I", zlib.crc32(flags) & 0xFFFFFFFF)
    block = bytearray(b"\x00\x00" + b"\x21" + b"\x01" + bytes([code]))
    padding = b"\x00" * ((-len(block)) % 4)
    block[0] = (len(block) + len(padding) + 4) // 4 - 1
    block.extend(padding)
    block.extend(struct.pack("<I", zlib.crc32(bytes(block)) & 0xFFFFFFFF))
    # Body bytes are never decoded: the memory limit fires at codec init.
    return stream_header + bytes(block) + b"\x00" * 32


# LZMA2 dictionary code 37 = 1.5 GiB, far past the 96 MiB decoder budget.
with open(os.path.join(root, "huge-dict.xz"), "wb") as handle:
    handle.write(xz_with_declared_dictionary(37))
PY

failures=0

# No-extraction proof: every read below must leave the fixture tree untouched.
# A backend that wrote members to disk (or a temp dir) changes this snapshot.
snapshot_work() {
	(cd "$work" && find . -print | LC_ALL=C sort)
}
before_tree="$(snapshot_work)"

report_fail() {
	failures=$((failures + 1))
	echo "FAIL $1: $2" >&2
	echo "----- output -----" >&2
	printf '%s\n' "$3" >&2
	echo "------------------" >&2
}

run_read() {
	(cd "$work" && "$OMP_BIN" read "$1" 2>&1)
}

expect_contains() {
	local label="$1" pattern="$2" output
	if ! output="$(run_read "$3")"; then
		report_fail "$label" "read failed" "$output"
		return
	fi
	if [[ "$output" != *"$pattern"* ]]; then
		report_fail "$label" "missing '$pattern'" "$output"
	fi
}

expect_equals() {
	local label="$1" expected="$2" output
	if ! output="$(run_read "$3")"; then
		report_fail "$label" "read failed" "$output"
		return
	fi
	if [[ "$output" != "$expected" ]]; then
		report_fail "$label" "expected '$expected'" "$output"
	fi
}

expect_missing() {
	local label="$1" pattern="$2" output
	if ! output="$(run_read "$3")"; then
		report_fail "$label" "read failed" "$output"
		return
	fi
	if [[ "$output" == *"$pattern"* ]]; then
		report_fail "$label" "unexpected '$pattern'" "$output"
	fi
}

expect_error() {
	local label="$1" pattern="$2" output
	if output="$(run_read "$3")"; then
		report_fail "$label" "expected an error" "$output"
		return
	fi
	if [[ "$output" != *"$pattern"* ]]; then
		report_fail "$label" "missing error '$pattern'" "$output"
	fi
}

expect_contains "zip root listing" "docs/" "hello.zip"
expect_contains "zip root empty dir" "emptydir/" "hello.zip"
expect_contains "zip dir listing" "notes.txt" "hello.zip:docs"
expect_contains "zip member default" "1|alpha" "hello.zip:docs/notes.txt"
expect_contains "zip member range" "3|gamma" "hello.zip:docs/notes.txt:2-3"
expect_equals "zip member raw range" $'beta\ngamma' "hello.zip:docs/notes.txt:raw:2-3"
expect_equals "zip member raw" $'alpha\nbeta\ngamma\ndelta' "hello.zip:docs/notes.txt:raw"
expect_contains "zip member multi range" "1|alpha" "hello.zip:docs/notes.txt:1-1,3-3"
expect_contains "zip member multi range tail" "3|gamma" "hello.zip:docs/notes.txt:1-1,3-3"
expect_contains "zip UTF-8 member" "héllo" "hello.zip:docs/ünïcode.txt"
expect_contains "zip empty member" "1|" "hello.zip:empty.txt"
expect_contains "zip binary member" "[Cannot read binary archive entry 'binary.bin'" "hello.zip:binary.bin"
expect_contains "zip invalid UTF-8 member" "[Cannot read binary archive entry 'invalid-utf8.txt'" "hello.zip:invalid-utf8.txt"
expect_error "zip directory multi range" "Multi-range line selectors are not supported" "hello.zip:docs:1-1,3-3"
expect_contains "zip root offset listing" "docs/" "hello.zip:2-2"

expect_contains "tar root listing" "docs/" "hello.tar"
expect_contains "tar member range" "1|alpha" "hello.tar:docs/notes.txt:1-1"
expect_contains "tar UTF-8 member" "Ωmega" "hello.tar:docs/Ω.txt"
expect_contains "tar.gz root listing" "docs/" "hello.tar.gz"
expect_contains "tar.gz member" "2|beta" "hello.tar.gz:docs/notes.txt:2-2"
expect_contains "tar.xz root listing" "docs/" "hello.tar.xz"
expect_contains "tar.xz member" "1|alpha" "hello.tar.xz:docs/notes.txt:1-1"
expect_contains "tar.bz2 root listing" "docs/" "hello.tar.bz2"
expect_contains "tar.bz2 member" "2|beta" "hello.tar.bz2:docs/notes.txt:2-2"

expect_contains "single stream listing" "notes.txt (" "notes.txt.gz"
expect_equals "single stream member" "single stream" "notes.txt.gz:notes.txt:raw"
expect_equals "bz2 stream member" "bz2 stream" "notes.txt.bz2:notes.txt:raw"
expect_equals "xz stream member" "xz stream" "notes.txt.xz:notes.txt:raw"

expect_missing "traversal listing hides escape" "../evil.txt" "traversal.zip"
expect_contains "traversal absolute normalizes inside root" "abs.txt" "traversal.zip"
expect_contains "traversal absolute member inside root" "absolute" "traversal.zip:abs.txt"
expect_contains "traversal safe member" "safe" "traversal.zip:safe.txt"
expect_error "traversal member rejected" "not found inside archive" "traversal.zip:../evil.txt"
expect_error "symlink escape rejected" "cannot be materialized" "symlink.tar:escape"
expect_contains "zip symlink resolves inside archive" "alpha" "links.zip:safe-link"
expect_error "zip symlink escape rejected" "cannot be materialized" "links.zip:escape-link"
expect_contains "darwin host symlink resolves" "alpha" "links-darwin.zip:mac-link"
expect_contains "dos host mode bits ignored" "plain payload" "links-darwin.zip:dos-mode"
expect_contains "tar hardlink resolves" "hard target" "hardlink.tar:hard.txt"
expect_error "symlink cycle rejected" "cyclic symlink" "symlink.tar:cycle"
expect_error "declared oversize rejected" "too large" "oversize.tar:big.bin"
expect_error "unsupported backend named" "rar backend" "unsupported.rar:anything"

expect_contains "suffix match notice" "via suffix match" "suffix.zip:inner.txt"
expect_contains "literal colon file wins" "literal" "literal.zip:1-2"

# Bounded codec control: every method must be served, and attacker-declared
# workspace must be rejected before a decoder allocates it.
expect_contains "zip stored member" "1|stored member" "codecs.zip:stored.txt"
expect_contains "zip deflate member" "1|deflate member" "codecs.zip:deflate.txt"
expect_contains "zip bzip2 member" "1|bzip member" "codecs.zip:bzip.txt"
expect_contains "zip lzma member" "1|lzma member" "codecs.zip:lzma.txt"
expect_error "zip lzma dictionary bomb" "LZMA dictionary" "lzma-dict.zip:bomb.txt"
expect_contains "zip lzma eos member" "1|no eos member" "lzma-eos.zip:eos.txt"
expect_contains "zip lzma member without eos marker" "1|no eos member" "lzma-noeos.zip:noeos.txt"
expect_error "zip bzip2 truncated stream rejected" "ended before its end-of-stream marker" "truncated-bzip2.zip:truncated.txt"
expect_contains "zip64 empty listing" "(empty archive directory)" "empty64.zip"
expect_error "zip64 oversized index rejected" "central directory is too large" "bad64.zip"
expect_error "classic oversized index rejected" "central directory is too large" "bad-classic.zip"
expect_error "xz dictionary bomb" "Cannot decompress XZ archive" "huge-dict.xz"

output="$(cd "$work" && env PATH="$work/no-python" HOME="${HOME:-/}" "$OMP_BIN" read hello.zip 2>&1)" || true
if [[ "$output" != *"Cannot start the archive backend"* || "$output" != *"python3"* ]]; then
	report_fail "missing python prerequisite" "spawn failure names python3" "$output"
fi

if [[ "$(snapshot_work)" != "$before_tree" ]]; then
	report_fail "no extraction" "archive reads modified the fixture tree" "$(snapshot_work)"
fi

if [[ "$failures" -ne 0 ]]; then
	echo "check-archive-read: $failures failure(s)" >&2
	exit 1
fi
echo "check-archive-read: all archive expectations passed"
