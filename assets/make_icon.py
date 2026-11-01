"""Regenerates the app icon: icon.ico, icon.png and icon_rgba.bin (needs Pillow).

Run from anywhere: python assets/make_icon.py

Each size is drawn separately rather than scaled from one large image, with bars
snapped to whole pixels, so small sizes stay sharp. The tile is bright and fills the
canvas so it reads at full size on both dark and light taskbars.
"""
from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).parent
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
SUPERSAMPLE = 8

TILE_TOP = (56, 189, 248)
TILE_BOTTOM = (29, 78, 216)
BAR = (255, 255, 255, 255)
# Height of each bar, as a share of the area between the margins.
BAR_HEIGHTS = (0.50, 1.00, 0.72)


def layout(size):
    """Whole-pixel (left, right, top, bottom) for each bar at this icon size."""
    margin = max(2, round(size * 0.15))
    area = size - margin * 2
    gap = max(1, round(area * 0.09))
    width = (area - gap * 2) // 3
    # Any spare pixels go to the outer margins, to keep the bars centered.
    left = (size - (width * 3 + gap * 2)) // 2
    bottom = size - margin
    bars = []
    for i, height in enumerate(BAR_HEIGHTS):
        x = left + i * (width + gap)
        bars.append((x, x + width, bottom - round(area * height), bottom))
    return bars


def draw(size):
    s = SUPERSAMPLE
    big = size * s

    gradient = Image.new("RGB", (1, big))
    for y in range(big):
        t = y / (big - 1)
        gradient.putpixel(
            (0, y),
            tuple(round(a + (b - a) * t) for a, b in zip(TILE_TOP, TILE_BOTTOM)),
        )
    tile = gradient.resize((big, big))

    mask = Image.new("L", (big, big), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (0, 0, big - 1, big - 1), radius=round(big * 0.22), fill=255
    )
    icon = Image.merge("RGBA", (*tile.split(), mask))

    canvas = ImageDraw.Draw(icon)
    for left, right, top, bottom in layout(size):
        radius = 0 if size < 32 else round((right - left) * s * 0.22)
        canvas.rounded_rectangle(
            (left * s, top * s, right * s - 1, bottom * s - 1), radius=radius, fill=BAR
        )
    return icon.resize((size, size), Image.LANCZOS)


def main():
    images = {size: draw(size) for size in SIZES}
    largest = images[SIZES[-1]]
    largest.save(
        HERE / "icon.ico",
        format="ICO",
        sizes=[(s, s) for s in SIZES],
        append_images=[images[s] for s in SIZES[:-1]],
    )
    largest.save(HERE / "icon.png")
    (HERE / "icon_rgba.bin").write_bytes(images[64].tobytes())

    # A contact sheet for checking the sizes by eye; not used by the build.
    sheet = Image.new("RGB", (620, 300), (32, 32, 32))
    x = 10
    for size in SIZES[:-1]:
        sheet.paste(images[size], (x, 10), images[size])
        x += size + 10
    return sheet


if __name__ == "__main__":
    import sys

    sheet = main()
    if len(sys.argv) > 1:
        sheet.save(sys.argv[1])
