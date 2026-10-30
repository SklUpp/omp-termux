"""Read-only archive index/member backend for the Rust `read` tool.

Invoked as `python3 -I -B -c <this source> <json-request>`. Only the standard
library is used: zipfile, tarfile, gzip, bz2, lzma. Nothing is ever extracted
to disk: member bytes are returned on stdout after one framed JSON header line
(`{"ok":...}\n` followed by the raw member payload).

The Rust side owns rendering, line selection, truncation and cancellation
(killing this process group). This module owns container bounds: entry count,
central-directory/tar size, declared and *actual* decompressed member bytes,
path length, link depth, root-escape rejection, and refusing encrypted,
truncated, sparse, device/FIFO and unsupported-codec members explicitly.

Decoder *workspace* is bounded too, because it is attacker-declared and
allocated before any output cap can apply: the effective ZIP end record
(including its ZIP64 record) is validated before `zipfile` may allocate the
index, a guarded reader refuses oversized index reads, ZIP BZIP2/LZMA members
are decoded here with `max_length` steps and a validated LZMA dictionary, and
XZ streams run through `LZMADecompressor(memlimit=...)` instead of
`LZMAFile`, which has no limit.
"""

import bz2
import gzip
import io
import json
import lzma
import os
import posixpath
import stat
import struct
import sys
import tarfile
import zipfile
import zlib

MAX_ENTRIES = 1_000_000
MAX_INDEX_BYTES = 64 * 1024 * 1024
MAX_MEMORY_BYTES = 256 * 1024 * 1024
MAX_MEMBER_BYTES = 64 * 1024 * 1024
MAX_PATH_BYTES = 4096
MAX_LINK_DEPTH = 40
LIST_DEFAULT = 500
# Bounds one framed header even when the selector asks for an absurd limit;
# rendered output is byte-capped far below this by the caller.
LIST_CAP = 5000
READ_CHUNK = 128 * 1024
TAR_BLOCK = 512

# Decoder workspace ceilings. XZ/LZMA dictionaries are attacker-declared in the
# stream header, so they must be bounded before any decompression starts:
# `LZMAFile`/`ZipExtFile` build decoders with no limit. 96 MiB covers a
# standard `xz -9` (64 MiB dictionary) plus decoder overhead.
XZ_DECODER_MEMLIMIT = 96 * 1024 * 1024
ZIP_LZMA_DICT_LIMIT = 64 * 1024 * 1024
ZIP_METHOD_STORED = 0
ZIP_METHOD_DEFLATED = 8
ZIP_METHOD_BZIP2 = 12
ZIP_METHOD_LZMA = 14
# General-purpose bit 1 has a method-specific meaning. For LZMA (APPNOTE 5.2)
# it says the raw stream carries an end-of-stream marker; without it the
# declared compressed size delimits the stream. LZMA producers in the wild use
# both, so only the flagged form may demand an end marker.
ZIP_LZMA_EOS_FLAG = 0x0002
ZIP_UTF8_NAME_FLAG = 0x0800
ZIP64_EOCD = b"PK\x06\x06"
ZIP64_LOCATOR = b"PK\x06\x07"
# version-made-by hosts whose high 16 attribute bits are a real Unix st_mode.
ZIP_UNIX_HOSTS = (3, 19)  # Unix, OS X/Darwin
ZIP64_EOCD_SIZE = 56
ZIP64_LOCATOR_SIZE = 20
ZIP_EOCD = b"PK\x05\x06"
ZIP_LOCAL_HEADER = b"PK\x03\x04"
U32_MAX = 0xFFFFFFFF
U16_MAX = 0xFFFF

COMPRESSED_FORMATS = {
    "tar.gz": "gz",
    "tar.bz2": "bz2",
    "tar.xz": "xz",
    "gz": "gz",
    "bz2": "bz2",
    "xz": "xz",
}
ZIP_FORMATS = {"zip"}


class Fail(Exception):
    def __init__(self, message, kind="message"):
        super().__init__(message)
        self.kind = kind


def fail(message, kind="message"):
    raise Fail(message, kind)


def byte_len(text):
    return len(text.encode("utf-8"))


def check_path_bytes(value, field):
    size = byte_len(value)
    if size > MAX_PATH_BYTES:
        fail("Archive %s exceeds %d bytes" % (field, MAX_PATH_BYTES))


def normalize_lookup(raw):
    """Return `""` for the root, `None` when the path escapes via `..`."""
    if not raw:
        return ""
    parts = []
    for part in raw.replace("\\", "/").split("/"):
        if not part or part == ".":
            continue
        if part == "..":
            return None
        parts.append(part)
    return "/".join(parts)


def normalize_entry(raw):
    normalized = normalize_lookup(raw)
    if not normalized:
        return None
    return normalized


def is_absolute_link(target):
    if target.startswith("/") or "\0" in target:
        return True
    return len(target) >= 2 and target[0].isalpha() and target[1] == ":"


def node(is_dir=False, size=0, storage=None):
    return {"is_dir": is_dir, "size": size, "storage": storage}


def upsert(entries, path, entry):
    existing = entries.get(path)
    if existing is None:
        entries[path] = entry
        return entry
    if existing["is_dir"] and not entry["is_dir"]:
        entries[path] = entry
        return entry
    if not existing["is_dir"] and entry["is_dir"]:
        return None
    entries[path] = entry
    return entry


def ensure_parent_directories(entries):
    for path in list(entries):
        parts = path.split("/")
        for index in range(1, len(parts)):
            parent = "/".join(parts[:index])
            if parent in entries:
                continue
            entries[parent] = node(is_dir=True)
            if len(entries) > MAX_ENTRIES:
                fail("Archive has too many entries (> %d limit)" % MAX_ENTRIES)
    if len(entries) > MAX_ENTRIES:
        fail("Archive has too many entries (> %d limit)" % MAX_ENTRIES)


def resolve_link(entries, archive_path):
    """Rewrite a path through archive-internal links, never the host."""
    resolved = archive_path
    seen = set()
    rewrites = 0
    while resolved not in seen:
        seen.add(resolved)
        replacement = None
        end = len(resolved)
        while end > 0:
            entry = entries.get(resolved[:end])
            if entry is not None and entry["storage"] is not None:
                storage = entry["storage"]
                if storage[0] == "link" and (entry["is_dir"] or storage[2]):
                    suffix = resolved[end + 1 :]
                    target = storage[1]
                    if suffix:
                        replacement = (target + "/" + suffix) if target else suffix
                    else:
                        replacement = target
                    break
            end = resolved.rfind("/", 0, end)
        if replacement is None:
            return resolved
        rewrites += 1
        if rewrites > MAX_LINK_DEPTH:
            break
        resolved = replacement
    fail("Archive path '%s' crosses a cyclic symlink" % archive_path)


class Archive:
    def __init__(self, entries, zip_file=None, tar_file=None, handle=None, size=0):
        self.entries = entries
        self.zip_file = zip_file
        self.tar_file = tar_file
        # Original read-only handle + size, kept so ZIP members whose codec
        # needs bounded control (BZIP2/LZMA) can be read without re-opening
        # the path or trusting `ZipExtFile`'s unbounded decompressor call.
        self.handle = handle
        self.size = size

    def get_node(self, raw_path):
        normalized = normalize_lookup(raw_path)
        if normalized is None:
            return None
        if normalized == "":
            return ("", "", node(is_dir=True))
        resolved = resolve_link(self.entries, normalized)
        if resolved == "":
            return (normalized, resolved, node(is_dir=True))
        entry = self.entries.get(resolved)
        if entry is None:
            return None
        return (normalized, resolved, entry)

    def list_directory(self, normalized, resolved):
        if resolved:
            entry = self.entries.get(resolved)
            if entry is None:
                fail("Archive path '%s' not found" % normalized)
            if not entry["is_dir"]:
                fail("Archive path '%s' is not a directory" % normalized)
        prefix = resolved + "/" if resolved else ""
        children = {}
        for path, entry in self.entries.items():
            if resolved:
                if not path.startswith(prefix) or path == resolved:
                    continue
                relative = path[len(prefix) :]
            else:
                relative = path
            segment = relative.split("/")[0]
            if not segment:
                continue
            child_path = normalized + "/" + segment if normalized else segment
            if child_path in children:
                continue
            source_child = resolved + "/" + segment if resolved else segment
            resolved_child = resolve_link(self.entries, source_child)
            child_entry = self.entries.get(resolved_child) if resolved_child else None
            is_dir = resolved_child == "" or (child_entry is not None and child_entry["is_dir"]) or "/" in relative
            if is_dir:
                size = 0
            elif child_entry is not None:
                size = child_entry["size"]
            else:
                size = entry["size"]
            children[child_path] = {
                "name": segment,
                "is_dir": is_dir,
                "size": size,
            }
        return sorted(children.values(), key=lambda item: item["name"].lower())

    def read_file(self, normalized, resolved, entry):
        if entry["is_dir"]:
            fail("Archive path '%s' is a directory" % normalized)
        storage = entry["storage"]
        if storage is None:
            fail("Archive file '%s' has no readable storage" % normalized)
        if storage[0] == "link":
            fail(
                "Archive symlink '%s' cannot be materialized from target '%s'"
                % (normalized, storage[1])
            )
        declared = entry["size"]
        if declared > MAX_MEMBER_BYTES:
            fail(
                "Archive member '%s' is too large to extract in memory (%d > %d limit)"
                % (normalized, declared, MAX_MEMBER_BYTES)
            )
        if storage[0] == "buffer":
            data = storage[1]
        elif storage[0] == "zip":
            data = read_zip_member(self, storage[1], normalized)
        elif storage[0] == "tar":
            data = read_tar_member(self.tar_file, storage[1], normalized)
        else:
            fail("Archive member '%s' has unsupported storage" % normalized)
        if len(data) != declared:
            fail("Archive member '%s' is truncated (%d of %d bytes)" % (normalized, len(data), declared))
        return data


def read_zip_member(archive, info, label):
    if info.file_size > MAX_MEMBER_BYTES or info.compress_size > MAX_MEMBER_BYTES:
        fail(
            "Archive member '%s' is too large to extract in memory (%d > %d limit)"
            % (label, max(info.file_size, info.compress_size), MAX_MEMBER_BYTES)
        )
    method = info.compress_type
    if method in (ZIP_METHOD_STORED, ZIP_METHOD_DEFLATED):
        # These two are bounded per call inside `ZipExtFile`: stored returns at
        # most the requested slice and deflate forwards `max_length=n`
        # (zipfile/__init__.py `_read1`). BZIP2/LZMA take the branch below
        # because `ZipExtFile` calls `decompress(data)` with no output bound
        # and, for LZMA, builds a decoder from attacker-declared filters.
        return read_zip_stream(archive.zip_file, info, label)
    payload = read_zip_payload(archive, info, label)
    if method == ZIP_METHOD_BZIP2:
        data = decode_bzip2(payload, label)
    elif method == ZIP_METHOD_LZMA:
        data = decode_zip_lzma(
            payload, label, bool(info.flag_bits & ZIP_LZMA_EOS_FLAG)
        )
    else:
        fail("Unsupported ZIP compression method %d for '%s'" % (method, label))
    if info.CRC != (zlib.crc32(data) & U32_MAX):
        fail("Archive member '%s' is corrupt (CRC mismatch)" % label)
    return data


def read_zip_stream(zf, info, label):
    chunks = []
    total = 0
    try:
        with zf.open(info) as handle:
            while True:
                chunk = handle.read(READ_CHUNK)
                if not chunk:
                    break
                total += len(chunk)
                if total > MAX_MEMBER_BYTES:
                    fail(
                        "Archive member '%s' is too large to extract in memory (> %d limit)"
                        % (label, MAX_MEMBER_BYTES)
                    )
                chunks.append(chunk)
    except NotImplementedError as error:
        fail("Unsupported ZIP compression method for '%s': %s" % (label, error))
    except (zipfile.BadZipFile, EOFError, OSError) as error:
        fail("Archive member '%s' is truncated or corrupt: %s" % (label, error))
    return b"".join(chunks)


def pread_exact(fd, offset, length, label):
    """Positional read that never advances the shared file offset."""
    if length <= 0:
        return b""
    data = os.pread(fd, length, offset)
    if len(data) == length:
        return data
    # Short read (pipe-like or offset-adjacent EOF): finish in place.
    buffer = bytearray(data)
    position = offset + len(data)
    while len(buffer) < length:
        chunk = os.pread(fd, length - len(buffer), position)
        if not chunk:
            fail("Archive member '%s' is truncated" % label)
        buffer.extend(chunk)
        position += len(chunk)
    return bytes(buffer)


def read_zip_payload(archive, info, label):
    """Read one member's compressed bytes after validating its local header.

    The BZIP2/LZMA path bypasses `ZipFile.open`, so the header checks that
    `zipfile` performs there (ZIP64-aware name agreement, entry overlap) are
    reproduced here; without them a crafted archive could point a later member
    at an earlier member's bytes or hide a header inside another member.
    """
    if archive.handle is None:
        fail("Archive member '%s' has no readable storage" % label)
    if info.header_offset < 0 or info.header_offset + 30 > archive.size:
        fail("Archive member '%s' is truncated or corrupt: bad local header offset" % label)
    fd = archive.handle.fileno()
    header = pread_exact(fd, info.header_offset, 30, label)
    if header[:4] != ZIP_LOCAL_HEADER:
        fail("Archive member '%s' is truncated or corrupt: bad local header" % label)
    local_method = int.from_bytes(header[8:10], "little")
    if local_method != info.compress_type:
        fail("Archive member '%s' is corrupt: local header method mismatch" % label)
    name_length = int.from_bytes(header[26:28], "little")
    extra_length = int.from_bytes(header[28:30], "little")
    local_name = pread_exact(fd, info.header_offset + 30, name_length, label)
    # `zipfile` decodes the local name with the *central* directory's UTF-8
    # flag; a mismatch between the two records is a BadZipFile upstream.
    if info.flag_bits & ZIP_UTF8_NAME_FLAG:
        try:
            local_name = local_name.decode("utf-8")
        except UnicodeDecodeError:
            fail(
                "Archive member '%s' is corrupt: local file name is not valid UTF-8"
                % label
            )
    else:
        local_name = local_name.decode("cp437")
    if local_name != info.orig_filename:
        fail(
            "Archive member '%s' is corrupt: file name in directory and local header differ"
            % label
        )
    data_start = info.header_offset + 30 + name_length + extra_length
    compressed = info.compress_size
    if compressed > MAX_MEMBER_BYTES:
        fail(
            "Archive member '%s' is too large to extract in memory (%d > %d limit)"
            % (label, compressed, MAX_MEMBER_BYTES)
        )
    if data_start + compressed > archive.size:
        fail("Archive member '%s' is truncated" % label)
    # `zipfile` sets `_end_offset` to the next local header (or the central
    # directory); a member whose data crosses it overlaps the next entry.
    end_offset = getattr(info, "_end_offset", None)
    if (
        end_offset is not None
        and data_start + compressed > end_offset
        and end_offset != info.header_offset
    ):
        fail(
            "Archive member '%s' overlaps the next entry (possible zip bomb)"
            % label
        )
    return pread_exact(fd, data_start, compressed, label)


def decode_bounded(factory, data, label, require_eof=True):
    """Decompress an in-memory payload in `max_length`-bounded steps.

    `require_eof` says whether the codec is self-delimiting: BZIP2 always is,
    ZIP LZMA only when its general-purpose bit 1 is set. A codec without an end
    marker ends when the declared compressed size runs out, so its output size
    and CRC are the completeness proof instead.
    """
    output = bytearray()
    decoder = factory()
    pending = data
    while True:
        if decoder.eof:
            if not require_eof:
                # Self-delimiting stream inside a size-delimited member:
                # anything after the marker is outside the payload we trust.
                return output
            pending = decoder.unused_data
            if not pending:
                return output
            fail(
                "Archive member '%s' is corrupt: trailing data after the end-of-stream marker"
                % label
            )
        if decoder.needs_input and not pending:
            if require_eof:
                fail(
                    "Archive member '%s' is truncated or corrupt: compressed stream "
                    "ended before its end-of-stream marker" % label
                )
            return output
        chunk = decoder.decompress(pending, READ_CHUNK)
        pending = b""
        if chunk:
            if len(output) + len(chunk) > MAX_MEMBER_BYTES:
                fail(
                    "Archive member '%s' is too large to extract in memory (> %d limit)"
                    % (label, MAX_MEMBER_BYTES)
                )
            output.extend(chunk)
        elif not decoder.needs_input and not decoder.eof:
            # Neither output nor an input request: the decoder cannot make
            # progress, so report it instead of returning clipped bytes.
            fail("Archive member '%s' is corrupt: decoder made no progress" % label)


def decode_bzip2(data, label):
    try:
        return decode_bounded(bz2.BZ2Decompressor, data, label)
    except OSError as error:
        fail("Archive member '%s' is truncated or corrupt: %s" % (label, error))


def decode_zip_lzma(data, label, eos_marker):
    if len(data) < 4:
        fail("Archive member '%s' is truncated or corrupt: short LZMA header" % label)
    props_size = int.from_bytes(data[2:4], "little")
    if props_size != 5 or len(data) < 9:
        fail("Archive member '%s' has unsupported LZMA properties" % label)
    properties = data[4:9]
    packed = properties[0]
    if packed >= 9 * 5 * 5:
        fail("Archive member '%s' has invalid LZMA properties" % label)
    lc = packed % 9
    rest = packed // 9
    lp = rest % 5
    pb = rest // 5
    if lc + lp > 4 or pb > 4:
        fail("Archive member '%s' has invalid LZMA properties" % label)
    dictionary = int.from_bytes(properties[1:5], "little")
    if dictionary > ZIP_LZMA_DICT_LIMIT:
        fail(
            "Archive member '%s' declares an LZMA dictionary of %d bytes (> %d limit)"
            % (label, dictionary, ZIP_LZMA_DICT_LIMIT)
        )
    filters = [
        {
            "id": lzma.FILTER_LZMA1,
            "dict_size": max(dictionary, 4096),
            "lc": lc,
            "lp": lp,
            "pb": pb,
        }
    ]
    try:
        return decode_bounded(
            lambda: lzma.LZMADecompressor(format=lzma.FORMAT_RAW, filters=filters),
            data[9:],
            label,
            require_eof=eos_marker,
        )
    except lzma.LZMAError as error:
        fail("Archive member '%s' is truncated or corrupt: %s" % (label, error))


def read_tar_member(tf, info, label):
    if info.size > MAX_MEMBER_BYTES:
        fail(
            "Archive member '%s' is too large to extract in memory (%d > %d limit)"
            % (label, info.size, MAX_MEMBER_BYTES)
        )
    try:
        handle = tf.extractfile(info)
    except (tarfile.TarError, EOFError, OSError) as error:
        fail("Archive member '%s' could not be read: %s" % (label, error))
    if handle is None:
        fail("Archive member '%s' is not a regular file" % label)
    chunks = []
    total = 0
    try:
        while True:
            chunk = handle.read(READ_CHUNK)
            if not chunk:
                break
            total += len(chunk)
            if total > MAX_MEMBER_BYTES:
                fail(
                    "Archive member '%s' is too large to extract in memory (> %d limit)"
                    % (label, MAX_MEMBER_BYTES)
                )
            chunks.append(chunk)
    finally:
        handle.close()
    return b"".join(chunks)


# ---------------------------------------------------------------------------
# ZIP
# ---------------------------------------------------------------------------


class GuardedReader:
    """Read-only wrapper that refuses any single read above the index ceiling.

    `zipfile` allocates exactly what the end record claims *before* any caller
    check can run, so an end record that claims a huge (or ZIP64
    sentinel-derived) central directory would allocate first and fail later.
    Rejecting the oversized read fails before the allocation happens, and the
    same open handle feeds both this guard and the parser (no path TOCTOU).
    """

    def __init__(self, handle, size):
        self._handle = handle
        self._size = size

    def readable(self):
        return True

    def seekable(self):
        return True

    def fileno(self):
        return self._handle.fileno()

    def read(self, size=-1):
        if size is None or size < 0:
            fail("Archive backend refused an unbounded index read")
        if size > MAX_INDEX_BYTES:
            fail(
                "Archive central directory read of %d bytes exceeds %d limit"
                % (size, MAX_INDEX_BYTES)
            )
        return self._handle.read(size)

    def seek(self, offset, whence=0):
        return self._handle.seek(offset, whence)

    def tell(self):
        return self._handle.tell()

    def close(self):
        self._handle.close()


def check_index_limits(entries, directory_size, directory_offset, size):
    if entries > MAX_ENTRIES:
        fail("Archive has too many entries (> %d limit)" % MAX_ENTRIES)
    if directory_size > MAX_INDEX_BYTES:
        fail(
            "Archive central directory is too large (%d > %d limit)"
            % (directory_size, MAX_INDEX_BYTES)
        )
    if directory_offset + directory_size > size:
        fail("Invalid ZIP archive: central directory is out of range")


def zip_precheck(handle, size):
    """Validate the *effective* end record before `zipfile` builds the index."""
    if size < 22:
        return
    tail_len = min(size, 22 + U16_MAX)
    handle.seek(size - tail_len)
    tail = handle.read(tail_len)
    index = tail.rfind(ZIP_EOCD)
    if index < 0 or index + 22 > len(tail):
        # Let `zipfile` produce its canonical "not a zip file" error.
        return
    comment_length = int.from_bytes(tail[index + 20 : index + 22], "little")
    eocd_abs = size - tail_len + index
    if eocd_abs + 22 + comment_length > size:
        return
    entries = int.from_bytes(tail[index + 10 : index + 12], "little")
    directory_size = int.from_bytes(tail[index + 12 : index + 16], "little")
    directory_offset = int.from_bytes(tail[index + 16 : index + 20], "little")
    if entries == U16_MAX or directory_size == U32_MAX or directory_offset == U32_MAX:
        # `zipfile` keeps the classic record when no ZIP64 locator is present
        # (a legit classic archive may sit exactly at the 65535 boundary), so
        # the sentinel values are then checked literally — which still rejects
        # the 0xFFFFFFFF size/offset sentinels.
        if zip64_check(handle, size, eocd_abs):
            return
    check_index_limits(entries, directory_size, directory_offset, size)


def zip64_check(handle, size, eocd_abs):
    """Validate the ZIP64 directory size/count/offset like `zipfile` resolves it.

    Returns False when no ZIP64 locator is present, matching `zipfile`'s
    fallback to the classic end record.
    """
    locator_pos = eocd_abs - ZIP64_LOCATOR_SIZE
    if locator_pos < 0:
        return False
    handle.seek(locator_pos)
    locator = handle.read(ZIP64_LOCATOR_SIZE)
    if len(locator) != ZIP64_LOCATOR_SIZE or locator[:4] != ZIP64_LOCATOR:
        return False
    disk = int.from_bytes(locator[4:8], "little")
    reloff = int.from_bytes(locator[8:16], "little")
    disks = int.from_bytes(locator[16:20], "little")
    if disk != 0 or disks > 1:
        fail("ZIP64 archives that span multiple disks are not supported")
    record_pos = eocd_abs - ZIP64_LOCATOR_SIZE - ZIP64_EOCD_SIZE
    if record_pos < 0 or reloff > record_pos:
        fail("Invalid ZIP64 archive: corrupt end of central directory locator")
    extrasz = record_pos - reloff
    record = None
    if reloff + ZIP64_EOCD_SIZE <= size:
        handle.seek(reloff)
        record = handle.read(ZIP64_EOCD_SIZE)
    if record is None or len(record) != ZIP64_EOCD_SIZE or record[:4] != ZIP64_EOCD:
        # Prepended data (self-extracting stub) shifts every recorded offset.
        handle.seek(record_pos)
        record = handle.read(ZIP64_EOCD_SIZE)
        if len(record) != ZIP64_EOCD_SIZE or record[:4] != ZIP64_EOCD:
            fail("Invalid ZIP64 archive: end of central directory record not found")
        extrasz = 0
    record_size = int.from_bytes(record[4:12], "little")
    entries = int.from_bytes(record[32:40], "little")
    directory_size = int.from_bytes(record[40:48], "little")
    directory_offset = int.from_bytes(record[48:56], "little")
    # Limits first: they are the security-relevant check, and they must fire
    # before the structural cross-checks so the reported reason is accurate.
    check_index_limits(entries, directory_size, directory_offset, size)
    if record_size + 12 != ZIP64_EOCD_SIZE + extrasz:
        fail("Invalid ZIP64 archive: corrupt end of central directory record")
    if directory_offset + directory_size != reloff:
        fail("Invalid ZIP64 archive: corrupt end of central directory record")
    return True


def index_zip(path):
    handle = open(path, "rb")
    size = os.fstat(handle.fileno()).st_size
    try:
        zip_precheck(handle, size)
        zf = zipfile.ZipFile(GuardedReader(handle, size))
    except Fail:
        handle.close()
        raise
    except (zipfile.BadZipFile, EOFError, OSError) as error:
        handle.close()
        fail("Not a valid ZIP archive: %s" % error)
    infos = zf.infolist()
    if len(infos) > MAX_ENTRIES:
        zf.close()
        fail("Archive has too many entries (> %d limit)" % MAX_ENTRIES)
    start_dir = getattr(zf, "start_dir", None)
    if start_dir is not None:
        index_size = size - start_dir
        if index_size > MAX_INDEX_BYTES:
            zf.close()
            fail(
                "Archive central directory is too large (%d > %d limit)"
                % (index_size, MAX_INDEX_BYTES)
            )
    entries = {}
    symlinks = []
    for info in infos:
        name = info.filename
        check_path_bytes(name, "member path")
        if info.flag_bits & 0x1 or info.compress_type == 99:
            fail("Encrypted ZIP member '%s' is not supported" % name)
        normalized = normalize_entry(name)
        if not normalized:
            continue
        declared = max(info.file_size, info.compress_size)
        if declared > MAX_MEMBER_BYTES:
            zf.close()
            fail(
                "Archive member '%s' is too large to extract in memory (%d > %d limit)"
                % (normalized, declared, MAX_MEMBER_BYTES)
            )
        # Only Unix-created entries carry a real st_mode; DOS/Windows zips put
        # junk in the high word. Upstream gates on version-made-by host:
        # 3 = Unix, 19 = OS X (zip.ts HOST_UNIX/HOST_OSX).
        mode = (
            (info.external_attr >> 16) & 0xFFFF
            if info.create_system in ZIP_UNIX_HOSTS
            else 0
        )
        file_type = mode & 0o170000
        is_dir = (
            name.endswith("/")
            or name.endswith("\\")
            or file_type == stat.S_IFDIR
            or bool(info.external_attr & 0x10)
        )
        is_symlink = file_type == stat.S_IFLNK and not is_dir
        if is_dir:
            entry = node(is_dir=True)
        else:
            entry = node(size=info.file_size, storage=("zip", info))
        if upsert(entries, normalized, entry) is not None and is_symlink:
            symlinks.append((normalized, info))
    archive = Archive(entries, zip_file=zf, handle=handle, size=size)
    for symlink, info in symlinks:
        entry = entries.get(symlink)
        if (
            entry is None
            or entry["storage"] is None
            or entry["storage"][0] != "zip"
            or entry["storage"][1] is not info
        ):
            continue
        if info.file_size > MAX_PATH_BYTES:
            zf.close()
            fail("Archive symlink target exceeds %d bytes" % MAX_PATH_BYTES)
        payload = read_zip_member(archive, info, symlink)
        try:
            target = payload.decode("utf-8")
        except UnicodeDecodeError:
            zf.close()
            fail("Failed to read ZIP archive: symlink target is not UTF-8")
        check_path_bytes(target, "link target")
        portable = target.replace("\\", "/")
        resolved_target = None
        if not is_absolute_link(portable):
            resolved_target = normalize_lookup(
                posixpath.join(posixpath.dirname(symlink), portable)
            )
        entry["size"] = 0
        if resolved_target is None:
            entry["storage"] = ("link", portable, False)
        else:
            entry["storage"] = ("link", resolved_target, True)
    ensure_parent_directories(entries)
    return archive


# ---------------------------------------------------------------------------
# TAR
# ---------------------------------------------------------------------------


def _iter_tar(tf):
    try:
        for member in tf:
            yield member
    except (tarfile.TarError, EOFError, OSError) as error:
        fail("Not a valid tar archive: %s" % error)


def tar_sparse(member):
    probe = getattr(member, "issparse", None)
    if probe is not None:
        try:
            if probe():
                return True
        except Exception:
            return True
    headers = getattr(member, "pax_headers", None) or {}
    return any(key.startswith("GNU.sparse.") for key in headers)


def index_tar(tf):
    entries = {}
    hardlinks = []
    count = 0
    members = _iter_tar(tf)
    for member in members:
        count += 1
        if count > MAX_ENTRIES:
            fail("Archive has too many entries (> %d limit)" % MAX_ENTRIES)
        raw_name = member.name
        check_path_bytes(raw_name, "member path")
        normalized = normalize_entry(raw_name)
        if not normalized:
            continue
        if member.ischr() or member.isblk() or member.isfifo():
            fail("Archive member '%s' is a device or FIFO and cannot be read" % normalized)
        if tar_sparse(member):
            fail("Archive member '%s' is a sparse file and cannot be read" % normalized)
        if member.isdir():
            upsert(entries, normalized, node(is_dir=True))
            continue
        if member.issym():
            portable = member.linkname.replace("\\", "/")
            check_path_bytes(portable, "link target")
            if is_absolute_link(portable):
                entry = node(storage=("link", portable, False))
            else:
                target = normalize_lookup(
                    posixpath.join(posixpath.dirname(normalized), portable)
                )
                if target is None:
                    entry = node(storage=("link", portable, False))
                else:
                    entry = node(storage=("link", target, True))
            upsert(entries, normalized, entry)
            continue
        if member.islnk():
            target = normalize_entry(member.linkname)
            if target is None:
                fail("Archive hard link '%s' has an invalid target" % normalized)
            upsert(entries, normalized, node())
            hardlinks.append((normalized, target))
            continue
        if member.isreg():
            if member.size > MAX_MEMBER_BYTES:
                fail(
                    "Archive member '%s' is too large to extract in memory (%d > %d limit)"
                    % (normalized, member.size, MAX_MEMBER_BYTES)
                )
            upsert(entries, normalized, node(size=member.size, storage=("tar", member)))
            continue
        # Unknown typeflags are skipped, matching the TypeScript tar reader.
    resolve_hardlinks(entries, hardlinks)
    ensure_parent_directories(entries)
    return Archive(entries, tar_file=tf)


def resolve_hardlinks(entries, hardlinks):
    pending = [(path, target) for path, target in hardlinks if path in entries]
    while pending:
        remaining = []
        progressed = False
        for path, target in pending:
            entry = entries.get(path)
            if entry is None:
                continue
            target_entry = entries.get(target)
            if target_entry is None:
                remaining.append((path, target))
                continue
            if target_entry["is_dir"]:
                fail("Archive hard link '%s' targets directory '%s'" % (path, target))
            storage = target_entry["storage"]
            if storage is None:
                remaining.append((path, target))
                continue
            entry["size"] = target_entry["size"]
            entry["storage"] = storage
            progressed = True
        if not progressed:
            path, target = remaining[0]
            fail("Archive hard link '%s' targets missing member '%s'" % (path, target))
        pending = remaining


# ---------------------------------------------------------------------------
# Compressed streams
# ---------------------------------------------------------------------------


def looks_tar(data):
    if len(data) < TAR_BLOCK:
        return False
    header = data[:TAR_BLOCK]
    if header == b"\0" * TAR_BLOCK:
        return True
    if not header[:100].split(b"\0", 1)[0]:
        return False
    field = header[148:156].replace(b"\0", b" ").strip() or b"0"
    try:
        expected = int(field, 8)
    except ValueError:
        return False
    unsigned = sum(header[:148]) + 8 * 32 + sum(header[156:])
    signed = sum(byte - 256 if byte > 127 else byte for byte in header[:148])
    signed += 8 * 32 + sum(byte - 256 if byte > 127 else byte for byte in header[156:])
    return expected in (unsigned, signed)


def drain_stream(stream):
    output = bytearray()
    while True:
        chunk = stream.read(READ_CHUNK)
        if not chunk:
            break
        if len(output) + len(chunk) > MAX_MEMORY_BYTES:
            fail(
                "Archive is too large to read in memory (> %d limit)"
                % MAX_MEMORY_BYTES
            )
        output.extend(chunk)
    return bytes(output)


def xz_decoder():
    return lzma.LZMADecompressor(
        format=lzma.FORMAT_AUTO, memlimit=XZ_DECODER_MEMLIMIT
    )


def decompress_xz(handle):
    """Incremental XZ decode with an explicit decoder-memory budget.

    `lzma.LZMAFile` exposes no `memlimit`, so a stream header may declare a
    multi-gigabyte dictionary that liblzma would allocate before any output cap
    could apply. Each concatenated stream gets its own budgeted decoder, and
    output is pulled in `max_length`-bounded steps.
    """
    output = bytearray()
    decoder = xz_decoder()
    pending = b""
    while True:
        if decoder.eof:
            # The XZ format allows zero-byte stream padding between and after
            # streams; a stream itself always starts with the 0xFD magic.
            pending = (decoder.unused_data or handle.read(READ_CHUNK)).lstrip(b"\x00")
            while not pending:
                chunk = handle.read(READ_CHUNK)
                if not chunk:
                    break
                pending = chunk.lstrip(b"\x00")
            if not pending:
                break
            decoder = xz_decoder()
            continue
        if decoder.needs_input and not pending:
            pending = handle.read(READ_CHUNK)
            if not pending:
                # `decoder.eof` is handled above, so the stream is incomplete.
                fail(
                    "Archive is truncated or corrupt: XZ stream ended before its end marker"
                )
        try:
            chunk = decoder.decompress(pending, READ_CHUNK)
        except lzma.LZMAError as error:
            fail(
                "Cannot decompress XZ archive (decoder memory limit %d bytes): %s"
                % (XZ_DECODER_MEMLIMIT, error)
            )
        pending = b""
        if chunk:
            if len(output) + len(chunk) > MAX_MEMORY_BYTES:
                fail(
                    "Archive is too large to read in memory (> %d limit)"
                    % MAX_MEMORY_BYTES
                )
            output.extend(chunk)
        elif not decoder.needs_input and not decoder.eof:
            # A decoder that neither yields bytes nor wants input cannot make
            # progress: report it instead of returning a clipped stream.
            fail("Cannot decompress XZ archive: decoder made no progress")
    return bytes(output)


def decompress_stream(path, codec):
    handle = open(path, "rb")
    try:
        size = os.fstat(handle.fileno()).st_size
        if size > MAX_MEMORY_BYTES:
            fail(
                "Archive is too large to read in memory (%d > %d limit)"
                % (size, MAX_MEMORY_BYTES)
            )
        if codec == "xz":
            return decompress_xz(handle)
        if codec == "gz":
            # GzipFile/BZ2File forward our read size as the codec's
            # `max_length` (compression/_common/_streams.py
            # `DecompressReader.read`), so each call is output-bounded.
            with gzip.GzipFile(fileobj=handle) as stream:
                return drain_stream(stream)
        if codec == "bz2":
            with bz2.BZ2File(handle) as stream:
                return drain_stream(stream)
        fail("Unsupported compression codec '%s'" % codec)
    except Fail:
        raise
    except (EOFError, OSError, lzma.LZMAError, ValueError) as error:
        fail("Archive is truncated or corrupt: %s" % error)
    finally:
        handle.close()


STEM_EXTENSIONS = sorted(
    [
        "tar.gz",
        "tar.bz2",
        "tar.xz",
        "tar.zst",
        "tar.z",
        "tgz",
        "tbz2",
        "tbz",
        "txz",
        "tzst",
        "zip",
        "jar",
        "war",
        "ear",
        "apk",
        "whl",
        "ipa",
        "xpi",
        "vsix",
        "nupkg",
        "cbz",
        "tar",
        "asar",
        "rar",
        "cbr",
        "7z",
        "iso",
        "cab",
        "cpio",
        "rpm",
        "ar",
        "a",
        "lib",
        "deb",
        "lzh",
        "lha",
        "arj",
        "gz",
        "bz2",
        "xz",
        "zst",
        "z",
        "lzma",
    ],
    key=len,
    reverse=True,
)


def stem_name(archive_path):
    base = posixpath.basename(archive_path.replace("\\", "/"))
    lower = base.lower()
    for extension in STEM_EXTENSIONS:
        if len(lower) > len(extension) + 1 and lower.endswith("." + extension):
            return base[: len(base) - len(extension) - 1]
    return base or "data"


def index_compressed(path, codec):
    inner = decompress_stream(path, codec)
    if looks_tar(inner):
        try:
            tf = tarfile.open(
                fileobj=io.BytesIO(inner),
                mode="r:",
                encoding="utf-8",
                errors="replace",
            )
        except (tarfile.TarError, EOFError, OSError) as error:
            fail("Not a valid tar archive: %s" % error)
        return index_tar(tf)
    if len(inner) > MAX_MEMBER_BYTES:
        fail(
            "Archive member is too large to extract in memory (%d > %d limit)"
            % (len(inner), MAX_MEMBER_BYTES)
        )
    name = stem_name(path)
    check_path_bytes(name, "member path")
    normalized = normalize_entry(name) or "data"
    entries = {normalized: node(size=len(inner), storage=("buffer", inner))}
    ensure_parent_directories(entries)
    return Archive(entries)


def index_plain_tar(path):
    size = os.path.getsize(path)
    if size > MAX_MEMORY_BYTES:
        fail(
            "Archive is too large to read in memory (%d > %d limit)"
            % (size, MAX_MEMORY_BYTES)
        )
    try:
        tf = tarfile.open(path, mode="r:", encoding="utf-8", errors="replace")
    except (tarfile.TarError, EOFError, OSError) as error:
        fail("Not a valid tar archive: %s" % error)
    return index_tar(tf)


def open_archive(path, fmt):
    if fmt in ZIP_FORMATS:
        return index_zip(path)
    if fmt == "tar":
        return index_plain_tar(path)
    codec = COMPRESSED_FORMATS.get(fmt)
    if codec is None:
        fail("Unsupported archive format '%s'" % fmt, kind="unsupported")
    return index_compressed(path, codec)


# ---------------------------------------------------------------------------
# Framing
# ---------------------------------------------------------------------------


def emit(payload):
    body = payload.pop("_payload", None)
    header = json.dumps(payload, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    out = sys.stdout.buffer
    out.write(header)
    out.write(b"\n")
    if body:
        out.write(body)
    out.flush()


def describe_directory(archive, found, offset, limit):
    normalized, resolved, entry = found
    items = archive.list_directory(normalized, resolved)
    if offset is not None and offset > 1:
        items = items[offset - 1 :]
    total = len(items)
    effective = limit if limit is not None and limit > 0 else LIST_DEFAULT
    shown = min(effective, LIST_CAP)
    if total >= effective:
        reached = effective
    elif total > shown:
        # Entry cap, not the caller's limit: still report a hit so a truncated
        # listing never reads as complete.
        reached = shown
    else:
        reached = None
    items = items[:shown]
    return {
        "ok": True,
        "type": "dir",
        "entries": items,
        "total": total,
        "limit": reached,
    }


def run(request):
    fmt = request["format"]
    archive_path = request["archive"]
    raw_path = request.get("path") or ""
    offset = request.get("offset")
    limit = request.get("limit")
    archive = open_archive(archive_path, fmt)
    found = archive.get_node(raw_path)
    if found is None:
        return {"ok": False, "kind": "not_found", "error": "not found inside archive"}
    normalized, resolved, entry = found
    if entry["is_dir"]:
        return describe_directory(archive, found, offset, limit)
    data = archive.read_file(normalized, resolved, entry)
    payload = {
        "ok": True,
        "type": "file",
        "path": normalized,
        "size": len(data),
        "_payload": data,
    }
    return payload


def main():
    if len(sys.argv) != 2:
        emit({"ok": False, "error": "archive helper expects one JSON request argument"})
        return
    try:
        request = json.loads(sys.argv[1])
        if not isinstance(request, dict):
            fail("archive helper request must be a JSON object")
        for key in ("format", "archive"):
            if not isinstance(request.get(key), str) or not request[key]:
                fail("archive helper request is missing '%s'" % key)
        emit(run(request))
    except Fail as error:
        emit({"ok": False, "kind": error.kind, "error": str(error)})
    except MemoryError:
        emit({"ok": False, "error": "Archive is too large to read in memory"})
    except Exception as error:  # noqa: BLE001 - every failure is framed, never a silent empty result
        emit({"ok": False, "error": "Archive backend failed: %s" % error})


if __name__ == "__main__":
    main()
