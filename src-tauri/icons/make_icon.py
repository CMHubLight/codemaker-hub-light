"""生成最小 32x32 PNG 图标 + 同名 ICO（无 Pillow 依赖，纯标准库）。

用法：python make_icon.py <icons_dir>
"""
import math
import os
import struct
import sys
import zlib


def make_png(size: int = 32) -> bytes:
    width = height = size
    raw = bytearray()
    for y in range(height):
        raw.append(0)  # 每行 filter=0
        for x in range(width):
            # 圆角矩形底：半径 6
            dx_corner = max(0, min(x, width - 1 - x))
            dy_corner = max(0, min(y, height - 1 - y))
            radius = 6
            if dx_corner < radius and dy_corner < radius:
                cx = radius - dx_corner - 0.5
                cy = radius - dy_corner - 0.5
                if math.hypot(cx, cy) > radius:
                    raw.extend((0, 0, 0, 0))
                    continue
            # 蓝色渐变
            t = y / (height - 1)
            r = int(29 + (59 - 29) * t)
            g = int(78 + (130 - 78) * t)
            b = int(216 + (246 - 216) * t)
            a = 255
            # 白色 C 形环：外径 10、内径 6，右侧开口
            dx = x - size / 2 + 0.5
            dy = y - size / 2 + 0.5
            dist = math.hypot(dx, dy)
            in_ring = 6.0 <= dist <= 10.0
            in_opening = dx > 2.5 and abs(dy) < 3.0
            if in_ring and not in_opening:
                r, g, b, a = 255, 255, 255, 255
            raw.extend((r, g, b, a))

    compressed = zlib.compress(bytes(raw), 9)

    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    header = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", compressed)
        + chunk(b"IEND", b"")
    )


def png_to_ico(png: bytes, size: int = 32) -> bytes:
    icondir = struct.pack("<HHH", 0, 1, 1)
    entry = struct.pack(
        "<BBBBHHII",
        size if size < 256 else 0,
        size if size < 256 else 0,
        0,
        0,
        1,
        32,
        len(png),
        6 + 16,
    )
    return icondir + entry + png


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: make_icon.py <icons_dir>", file=sys.stderr)
        return 2
    icons_dir = sys.argv[1]
    os.makedirs(icons_dir, exist_ok=True)
    png = make_png(32)
    png_path = os.path.join(icons_dir, "icon.png")
    ico_path = os.path.join(icons_dir, "icon.ico")
    with open(png_path, "wb") as handle:
        handle.write(png)
    with open(ico_path, "wb") as handle:
        handle.write(png_to_ico(png, 32))
    print(f"wrote {png_path} ({len(png)} bytes)")
    print(f"wrote {ico_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
