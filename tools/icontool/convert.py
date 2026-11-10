#!/usr/bin/env python3
"""Convert the AROS Mason icon set into the compact form the kernel embeds.

The icons are AROS's own — the "Mason" set from the AROS repository,
`images/IconSets/Mason` — and they are indexed-colour PNGs with a
transparent colour. This script decodes them and writes one `.idat` file per
icon: a small header, the palette, and a run-length encoded index plane.

The embedded form is deliberately not compressed: the kernel has no
inflate, and an icon plane of 46x46 indexes is a couple of kilobytes even
run-length encoded, which is a fair price for being able to draw a real
AROS icon without a decoder in the boot path.

Usage:

    python3 tools/icontool/convert.py <source-directory> <output-directory>

The source directory holds the PNGs as they came from the AROS
repository; the output directory receives the `.idat` files and a
`manifest.txt` listing them, which `graphics/icons` turns into the
`include_bytes!` table.

The AROS artwork is distributed under the AROS project's own licence; see
`graphics/icons/NOTICE` for the attribution and the terms.
"""

import os
import struct
import sys
import zlib


class PngError(Exception):
    """A malformed PNG, or one this script does not know how to read."""


def _chunks(data):
    """Yield the PNG's chunks as (type, payload)."""
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise PngError("not a PNG")
    offset = 8
    while offset + 8 <= len(data):
        (length,) = struct.unpack(">I", data[offset : offset + 4])
        kind = data[offset + 4 : offset + 8]
        payload = data[offset + 8 : offset + 8 + length]
        offset += 12 + length
        yield kind, payload
        if kind == b"IEND":
            return


def _unfilter(raw, width, height, bits_per_pixel):
    """Undo the PNG's per-scanline filters."""
    stride = (width * bits_per_pixel + 7) // 8
    out = bytearray()
    previous = bytearray(stride)
    offset = 0
    for _ in range(height):
        filter_type = raw[offset]
        offset += 1
        line = bytearray(raw[offset : offset + stride])
        offset += stride
        if filter_type == 0:
            pass
        elif filter_type == 1:
            for i in range(bits_per_pixel, stride):
                line[i] = (line[i] + line[i - bits_per_pixel]) & 0xFF
        elif filter_type == 2:
            for i in range(stride):
                line[i] = (line[i] + previous[i]) & 0xFF
        elif filter_type == 3:
            for i in range(stride):
                left = line[i - bits_per_pixel] if i >= bits_per_pixel else 0
                line[i] = (line[i] + ((left + previous[i]) >> 1)) & 0xFF
        elif filter_type == 4:
            for i in range(stride):
                left = line[i - bits_per_pixel] if i >= bits_per_pixel else 0
                up = previous[i]
                up_left = previous[i - bits_per_pixel] if i >= bits_per_pixel else 0
                p = left + up - up_left
                pa, pb, pc = abs(p - left), abs(p - up), abs(p - up_left)
                if pa <= pb and pa <= pc:
                    predictor = left
                elif pb <= pc:
                    predictor = up
                else:
                    predictor = up_left
                line[i] = (line[i] + predictor) & 0xFF
        else:
            raise PngError(f"unknown filter {filter_type}")
        out += line
        previous = line
    return bytes(out)


def decode_indexed_png(data):
    """Decode an indexed-colour PNG into (width, height, palette, indices).

    The palette is a list of (r, g, b) tuples and `transparent` is the
    index of the transparent colour, or None when the image is opaque.
    """
    width = height = depth = colour_type = None
    palette = []
    transparent = None
    idat = bytearray()
    for kind, payload in _chunks(data):
        if kind == b"IHDR":
            width, height, depth, colour_type, _, _, interlace = struct.unpack(
                ">IIBBBBB", payload
            )
            if colour_type != 3:
                raise PngError(f"colour type {colour_type} is not indexed")
            if interlace:
                raise PngError("interlaced images are not supported")
        elif kind == b"PLTE":
            for i in range(0, len(payload), 3):
                palette.append((payload[i], payload[i + 1], payload[i + 2]))
        elif kind == b"tRNS":
            if len(payload) >= 1:
                transparent = payload[0]
        elif kind == b"IDAT":
            idat += payload
    if width is None:
        raise PngError("no header")

    raw = _unfilter(zlib.decompress(bytes(idat)), width, height, depth)
    indices = bytearray()
    if depth == 8:
        indices = bytearray(raw[: width * height])
    elif depth in (1, 2, 4):
        # The scanlines are packed: several pixels share a byte, and the
        # *whole* image is packed, not just its first row.
        per_byte = 8 // depth
        mask = (1 << depth) - 1
        for byte in raw:
            for slot in range(per_byte):
                indices.append((byte >> (8 - depth * (slot + 1))) & mask)
        indices = indices[: width * height]
    else:
        raise PngError(f"bit depth {depth} is not supported")
    if len(indices) != width * height:
        raise PngError(
            f"decoded {len(indices)} pixels, expected {width * height}"
        )
    return width, height, palette, bytes(indices), transparent


def rle_encode(indices):
    """Run-length encode a byte plane into (count, value) pairs."""
    out = bytearray()
    count = 0
    current = None
    for value in indices:
        if value == current and count < 255:
            count += 1
            continue
        if current is not None:
            out.append(count)
            out.append(current)
        current = value
        count = 1
    if current is not None:
        out.append(count)
        out.append(current)
    return bytes(out)


def encode_idat(width, height, palette, indices, transparent):
    """Build the `.idat` payload: header, palette, run-length encoded plane."""
    out = bytearray(b"IDAT")
    out.append(1)  # format version
    out += struct.pack(">HH", width, height)
    out.append(len(palette))
    out.append(255 if transparent is None else transparent)
    for r, g, b in palette:
        out += bytes((r, g, b))
    plane = rle_encode(indices)
    out += struct.pack(">I", len(plane))
    out += plane
    return bytes(out)


def main(argv):
    if len(argv) != 3:
        print(__doc__)
        return 2
    source, target = argv[1], argv[2]
    os.makedirs(target, exist_ok=True)
    names = []
    for name in sorted(os.listdir(source)):
        if not name.endswith(".png"):
            continue
        with open(os.path.join(source, name), "rb") as handle:
            data = handle.read()
        width, height, palette, indices, transparent = decode_indexed_png(data)
        payload = encode_idat(width, height, palette, indices, transparent)
        stem = os.path.splitext(name)[0]
        with open(os.path.join(target, stem + ".idat"), "wb") as handle:
            handle.write(payload)
        names.append(stem)
        print(f"{stem:20s} {width}x{height} {len(palette):3d} colours {len(payload):6d} bytes")
    with open(os.path.join(target, "manifest.txt"), "w") as handle:
        handle.write("\n".join(names) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
