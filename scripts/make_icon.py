from pathlib import Path

from PIL import Image, ImageDraw


def cubic(p0, p1, p2, p3, steps=96):
    points = []
    for i in range(steps + 1):
        t = i / steps
        u = 1 - t
        x = u**3 * p0[0] + 3 * u**2 * t * p1[0] + 3 * u * t**2 * p2[0] + t**3 * p3[0]
        y = u**3 * p0[1] + 3 * u**2 * t * p1[1] + 3 * u * t**2 * p2[1] + t**3 * p3[1]
        points.append((x, y))
    return points


def sparkle_points(size, pad):
    # Official Gemma 4-point sparkle in a 64x64 viewBox, scaled into the padded square.
    inner = size - pad * 2
    scale = inner / 64.0

    def m(x, y):
        return (pad + x * scale, pad + y * scale)

    top = m(32, 3)
    right = m(61, 32)
    bottom = m(32, 61)
    left = m(3, 32)
    pts = []
    pts += cubic(top, m(34.7, 19.5), m(44.5, 29.3), right)
    pts += cubic(right, m(44.5, 34.7), m(34.7, 44.5), bottom)[1:]
    pts += cubic(bottom, m(29.3, 44.5), m(19.5, 34.7), left)[1:]
    pts += cubic(left, m(19.5, 29.3), m(29.3, 19.5), top)[1:]
    return pts


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))


def gradient_color(t):
    stops = [
        (0.0, (66, 133, 244)),
        (0.32, (142, 98, 219)),
        (0.62, (234, 67, 53)),
        (1.0, (251, 188, 4)),
    ]
    t = max(0.0, min(1.0, t))
    for i in range(len(stops) - 1):
        t0, c0 = stops[i]
        t1, c1 = stops[i + 1]
        if t <= t1:
            local = 0 if t1 == t0 else (t - t0) / (t1 - t0)
            return lerp(c0, c1, local)
    return stops[-1][1]


def render(size=1024, pad=140):
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).polygon(sparkle_points(size, pad), fill=255)
    gradient = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    px = gradient.load()
    m = mask.load()
    for y in range(size):
        for x in range(size):
            if m[x, y] == 0:
                continue
            t = ((x - pad) + (y - pad)) / (2 * (size - 2 * pad))
            r, g, b = gradient_color(t)
            px[x, y] = (r, g, b, m[x, y])
    return gradient


def main():
    root = Path(__file__).resolve().parents[1]
    out = root / "src-tauri" / "app-icon.png"
    out.parent.mkdir(parents=True, exist_ok=True)
    render().save(out)
    print(out)


if __name__ == "__main__":
    main()
