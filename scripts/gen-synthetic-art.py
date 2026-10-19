#!/usr/bin/env python3
"""Rebuild the text-free PNG artwork used by layout tests, using only Python."""
from pathlib import Path
import struct
import zlib

ROOT = Path(__file__).resolve().parents[1] / "test-fixtures/synthetic-art"
# Width, height and PNG color type retain the layout and decoder coverage.
SAMPLES = {
    "sample-a-cover": (535, 512, 6),
    "sample-a-hero": (640, 480, 2),
    "sample-b-cover": (500, 500, 2),
    "sample-b-hero": (320, 240, 2),
    "sample-c-cover": (512, 512, 2),
    "sample-c-hero": (320, 224, 3),
    "sample-d-cover": (512, 512, 2),
    "sample-d-hero": (512, 384, 2),
}


def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(
        ">I", zlib.crc32(kind + data) & 0xFFFFFFFF
    )


def generate():
    ROOT.mkdir(parents=True, exist_ok=True)
    for seed, (name, (width, height, color_type)) in enumerate(SAMPLES.items()):
        rows = bytearray()
        for y in range(height):
            rows.append(0)
            for x in range(width):
                # Quantized gradients, a central disc and a contrasting grid.
                value = ((x * 16 // width) + (y * 16 // height) + seed * 3) % 32
                if (2 * x - width) ** 2 + (2 * y - height) ** 2 < (height // 2) ** 2:
                    value = (value + 12) % 32
                if x % 64 < 4 or y % 64 < 4:
                    value = 31
                rgb = (value * 7, (value * 5 + seed * 23) % 256, 240 - value * 6)
                if color_type == 3:
                    rows.append(value)
                else:
                    rows.extend(rgb)
                    if color_type == 6:
                        rows.append(128 if x < 16 or y < 16 else 255)
        header = struct.pack(">IIBBBBB", width, height, 8, color_type, 0, 0, 0)
        png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
        if color_type == 3:
            palette = bytes(c for value in range(256) for c in (
                (value % 32) * 7, ((value % 32) * 5 + seed * 23) % 256,
                240 - (value % 32) * 6,
            ))
            png += chunk(b"PLTE", palette)
        png += chunk(b"IDAT", zlib.compress(rows, 9)) + chunk(b"IEND", b"")
        (ROOT / f"{name}.png").write_bytes(png)
        print(f"{name}.png: {width}x{height}, type {color_type}, {len(png)} bytes")


if __name__ == "__main__":
    generate()
