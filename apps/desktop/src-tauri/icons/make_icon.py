"""Draws the placeholder app icon (two linked rings) as a 1024x1024 PNG.

Usage: python3 make_icon.py app-icon.png
Then:  npm run tauri icon src-tauri/icons/app-icon.png   (from apps/desktop)
"""
import math, struct, sys, zlib

SIZE, SS = 1024, 2  # output size, supersampling factor


def rounded_square(x, y, half, radius):
    dx, dy = abs(x) - (half - radius), abs(y) - (half - radius)
    return math.hypot(max(dx, 0), max(dy, 0)) + min(max(dx, dy), 0) - radius


def ring(x, y, cx, cy, r, thickness):
    return abs(math.hypot(x - cx, y - cy) - r) - thickness / 2


def sample(x, y):
    """Returns (r, g, b, a) for a point in [-1, 1] x [-1, 1]."""
    if rounded_square(x, y, 0.86, 0.2) > 0:
        return (0, 0, 0, 0)
    t = (y + 1) / 2
    bg = (int(38 + 12 * t), int(99 - 30 * t), int(235 - 40 * t))
    left = ring(x, y, -0.2, 0.0, 0.3, 0.12)
    right = ring(x, y, 0.2, 0.0, 0.3, 0.12)
    if left <= 0 or right <= 0:
        return (255, 255, 255, 255)
    return (*bg, 255)


def main(path):
    rows = []
    n = SIZE * SS
    for py in range(SIZE):
        row = bytearray([0])
        for px in range(SIZE):
            acc = [0, 0, 0, 0]
            for sy in range(SS):
                for sx in range(SS):
                    x = ((px * SS + sx + 0.5) / n) * 2 - 1
                    y = ((py * SS + sy + 0.5) / n) * 2 - 1
                    r, g, b, a = sample(x, y)
                    acc[0] += r * a; acc[1] += g * a; acc[2] += b * a; acc[3] += a
            a = acc[3] / (SS * SS)
            if acc[3]:
                row += bytes((round(acc[0] / acc[3]), round(acc[1] / acc[3]), round(acc[2] / acc[3]), round(a)))
            else:
                row += b"\0\0\0\0"
        rows.append(bytes(row))

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(b"".join(rows), 9))
    png += chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "app-icon.png")
