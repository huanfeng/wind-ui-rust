# -*- coding: utf-8 -*-
"""windui 品牌图标生成器：几何参数 → SVG / ICO / PNG / Rust 顶点常量。

图标是「天蓝底 + 白色对称 W」。W **不取自任何字体**——字体的字形轮廓受版权保护，
随 MIT/Apache 双许可的 crate 分发有风险。这里用几何构造：给一条 5 点中心折线，
按逐段厚度做偏移求交得到轮廓，再用两条水平半平面把顶端与底端切平。

产物（本脚本是它们的唯一真相源，改参数后重跑并把打印的顶点贴回 src/icon.rs）：
- assets/windui.svg      矢量源（README / 打包工具用）
- assets/windui.ico      Windows 资源图标（16/32/48 走 BMP 条目，64+ 走 PNG 条目）
- assets/windui-256.png  预览图
- 标准输出：src/icon.rs 里 W_OUTLINE 常量的 Rust 字面量

用法：python scripts/gen_icon.py
"""
import math
import os
import struct
import sys

# ---- 造型参数（定稿值；改这里就是改图标）----------------------------------
ASPECT = 1.35   # W 的宽高比（局部坐标宽 = ASPECT，高 = 1）
MID = 0.0       # 中峰中心线 y：0 = 中峰齐顶（上沿被顶边切平），正数 = 中峰下沉
T_OUT = 0.225   # 外侧两笔厚度（占 W 宽）
T_IN = 0.170    # 内侧两笔厚度（外粗内细，取自真实字体的做法）
SPREAD = 0.50   # 两个谷底的水平位置（0.5 = 四等分）
FLAT = 0.20     # 两谷压到底边以下的量：>0 则底部尖角被切成平台
BOX = (0.150, 0.305, 0.850, 0.765)  # W 在图标中的目标框；y 已下移 0.035 做光学对齐
CORNER = 0.22   # 底板圆角半径（占边长）
BG = (0x1E, 0x90, 0xFF)  # 天蓝 #1E90FF

# ---- 几何 -------------------------------------------------------------------


def _n(v):
    l = math.hypot(*v)
    return (v[0] / l, v[1] / l)


def _isect(p, d, q, e):
    """过 p 方向 d 的直线 与 过 q 方向 e 的直线的交点。"""
    den = d[0] * e[1] - d[1] * e[0]
    if abs(den) < 1e-12:
        return p
    t = ((q[0] - p[0]) * e[1] - (q[1] - p[1]) * e[0]) / den
    return (p[0] + d[0] * t, p[1] + d[1] * t)


def _offset_chain(pts, offs, extend):
    """把中心折线按逐段偏移量 offs 平移，相邻段的偏移直线求交得轮廓拐点。

    首尾各沿段方向外延 extend，好让顶边裁剪切出平口。末点必须从**段终点**再外延：
    从段起点算的话，走完 extend 还没走出这一段，末笔就伸不出顶边，裁剪后左右会不对称。
    """
    segs = []
    for i, (a, b) in enumerate(zip(pts, pts[1:])):
        d = _n((b[0] - a[0], b[1] - a[1]))
        nm = (d[1], -d[0])
        segs.append(((a[0] + nm[0] * offs[i], a[1] + nm[1] * offs[i]), d))
    p, d = segs[0]
    out = [(p[0] - d[0] * extend, p[1] - d[1] * extend)]
    for (p1, d1), (p2, d2) in zip(segs, segs[1:]):
        out.append(_isect(p1, d1, p2, d2))
    p, d = segs[-1]
    tail = math.dist(pts[-2], pts[-1]) + extend
    out.append((p[0] + d[0] * tail, p[1] + d[1] * tail))
    return out


def _clip(poly, y, keep_below):
    """Sutherland-Hodgman 半平面裁剪：keep_below 时保留 y' >= y，否则保留 y' <= y。"""
    out = []
    for i, cur in enumerate(poly):
        prv = poly[i - 1]
        cin = cur[1] >= y if keep_below else cur[1] <= y
        pin = prv[1] >= y if keep_below else prv[1] <= y
        if cin != pin:
            t = (y - prv[1]) / (cur[1] - prv[1])
            out.append((prv[0] + (cur[0] - prv[0]) * t, y))
        if cin:
            out.append(cur)
    return out


def _fit(poly, box):
    """等比缩放 + 居中到 box=(x0,y0,x1,y1)，不变形。"""
    xs = [p[0] for p in poly]
    ys = [p[1] for p in poly]
    w, h = max(xs) - min(xs), max(ys) - min(ys)
    x0, y0, x1, y1 = box
    s = min((x1 - x0) / w, (y1 - y0) / h)
    ox = x0 + ((x1 - x0) - w * s) / 2 - min(xs) * s
    oy = y0 + ((y1 - y0) - h * s) / 2 - min(ys) * s
    return [(p[0] * s + ox, p[1] * s + oy) for p in poly]


def w_outline():
    """对称 W 的闭合多边形，归一化到图标坐标（0..1，y 向下）。"""
    a = ASPECT
    v = a * SPREAD / 2.0
    yb = 1.0 + FLAT
    pts = [(0.0, 0.0), (v, yb), (a / 2, MID), (a - v, yb), (a, 0.0)]
    offs = [T_OUT * a / 2, T_IN * a / 2, T_IN * a / 2, T_OUT * a / 2]
    poly = _offset_chain(pts, offs, 1.0) + _offset_chain(pts, [-o for o in offs], 1.0)[::-1]
    poly = _clip(poly, 0.0, True)       # 顶边平切
    if FLAT > 0:
        poly = _clip(poly, 1.0, False)  # 底边平切
    return _fit(poly, BOX)


def assert_symmetric(poly):
    """左右对称断言：所有 x 关于 0.5 镜像后应与原集合逐一配对。

    这条断言不是形式主义——之前的末端外延 bug 正是只让右笔少切了一刀，
    渲染出来只是「看着有点怪」，肉眼在小尺寸下会放过。
    """
    orig = sorted(round(p[0], 6) for p in poly)
    mirr = sorted(round(1.0 - p[0], 6) for p in poly)
    d = max(abs(x - y) for x, y in zip(orig, mirr))
    assert d < 1e-6, f"W 左右不对称，最大偏差 {d}"


# ---- 光栅化（自实现，不依赖 PIL 的矢量能力）--------------------------------


def _rounded_rect_cover(size, r):
    """圆角矩形的逐像素覆盖率（0..1）。四角按到圆心的距离做 1px 软边。"""
    cov = [[1.0] * size for _ in range(size)]
    for y in range(size):
        for x in range(size):
            px, py = x + 0.5, y + 0.5
            cx = r if px < r else (size - r if px > size - r else px)
            cy = r if py < r else (size - r if py > size - r else py)
            d = math.hypot(px - cx, py - cy)
            if d > r - 0.5:
                cov[y][x] = max(0.0, min(1.0, r + 0.5 - d))
    return cov


def _poly_cover(size, poly, ss=4):
    """多边形逐像素覆盖率：ss×ss 超采样 + 扫描线奇偶填充。"""
    pts = [(x * size * ss, y * size * ss) for x, y in poly]
    n = len(pts)
    acc = [[0] * size for _ in range(size)]
    ymin = max(0, int(min(p[1] for p in pts)))
    ymax = min(size * ss - 1, int(max(p[1] for p in pts)) + 1)
    for sy in range(ymin, ymax + 1):
        yc = sy + 0.5
        xs = []
        for i in range(n):
            x1, y1 = pts[i]
            x2, y2 = pts[(i + 1) % n]
            if (y1 <= yc) == (y2 <= yc):
                continue
            xs.append(x1 + (yc - y1) * (x2 - x1) / (y2 - y1))
        xs.sort()
        row = acc[sy // ss]
        for i in range(0, len(xs) - 1, 2):
            a, b = xs[i], xs[i + 1]
            for sx in range(max(0, int(a)), min(size * ss, int(b) + 1)):
                if a <= sx + 0.5 <= b:
                    row[sx // ss] += 1
    inv = 1.0 / (ss * ss)
    return [[min(1.0, v * inv) for v in r] for r in acc]


def rgba(size):
    """渲染 size×size 的非预乘 RGBA8。"""
    base = _rounded_rect_cover(size, size * CORNER)
    ink = _poly_cover(size, w_outline())
    out = bytearray(size * size * 4)
    for y in range(size):
        for x in range(size):
            b, w = base[y][x], ink[y][x]
            w = min(w, b)  # W 不越出底板
            r = BG[0] * (1 - w) + 255 * w
            g = BG[1] * (1 - w) + 255 * w
            bl = BG[2] * (1 - w) + 255 * w
            i = (y * size + x) * 4
            out[i:i + 4] = bytes((round(r), round(g), round(bl), round(b * 255)))
    return bytes(out)


# ---- 导出 -------------------------------------------------------------------


def svg():
    d = " ".join(
        ("M" if i == 0 else "L") + f"{x * 256:.2f} {y * 256:.2f}"
        for i, (x, y) in enumerate(w_outline())
    ) + " Z"
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="0 0 256 256">\n'
        f'  <rect width="256" height="256" rx="{CORNER * 256:.1f}" ry="{CORNER * 256:.1f}" '
        f'fill="#{BG[0]:02X}{BG[1]:02X}{BG[2]:02X}"/>\n'
        f'  <path d="{d}" fill="#FFFFFF"/>\n'
        "</svg>\n"
    )


def _png(size, px):
    """最小 PNG 编码器（RGBA8，zlib 压缩）。"""
    import zlib

    raw = b"".join(b"\x00" + px[y * size * 4:(y + 1) * size * 4] for y in range(size))

    def chunk(tag, data):
        c = tag + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def _ico_bmp(size, px):
    """ICO 内的 BMP 条目：BITMAPINFOHEADER(高度记两倍) + BGRA 自下而上 + 全 0 AND 掩码。"""
    hdr = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    rows = []
    for y in range(size - 1, -1, -1):
        row = bytearray()
        for x in range(size):
            i = (y * size + x) * 4
            row += bytes((px[i + 2], px[i + 1], px[i], px[i + 3]))
        rows.append(bytes(row))
    mask_stride = ((size + 31) // 32) * 4
    return hdr + b"".join(rows) + b"\x00" * (mask_stride * size)


# ICO 里要放的档位 = Windows 各缩放比下实际索取的像素数。
# `GetSystemMetricsForDpi(SM_CXSMICON/SM_CXICON, dpi)` 的实测值：
#   100%→16/32  125%→20/40  150%→24/48  175%→28/56  200%→32/64  250%→40/80
# 少一档，那个缩放比下系统就得拉伸最近的一档——"任务栏图标发虚"正是这么来的。
# 128/256 供资源管理器的大图标视图。
ICO_SIZES = (16, 20, 24, 28, 32, 40, 48, 56, 64, 80, 128, 256)


def ico(sizes=ICO_SIZES):
    imgs = []
    for s in sizes:
        px = rgba(s)
        # ≤48 用 BMP 条目（最保守的兼容路径）；更大的用 PNG 条目压体积。
        imgs.append(_ico_bmp(s, px) if s <= 48 else _png(s, px))
    out = struct.pack("<HHH", 0, 1, len(imgs))
    off = 6 + 16 * len(imgs)
    for s, data in zip(sizes, imgs):
        b = 0 if s >= 256 else s
        out += struct.pack("<BBBBHHII", b, b, 0, 0, 1, 32, len(data), off)
        off += len(data)
    return out + b"".join(imgs)


def rust_const():
    poly = w_outline()
    lines = [
        "/// W 轮廓顶点（归一化到图标边长，y 向下，闭合多边形）。",
        "///",
        "/// 由 `scripts/gen_icon.py` 生成——那里是造型参数的唯一真相源。改造型要跑脚本重出，",
        "/// 不要手改这里的数值：顶点是折线偏移求交 + 两次半平面裁剪的结果，手改必然破坏对称。",
        f"pub(crate) const W_OUTLINE: [(f32, f32); {len(poly)}] = [",
    ]
    for x, y in poly:
        lines.append(f"    ({x:.6f}, {y:.6f}),")
    lines.append("];")
    return "\n".join(lines)


def main():
    poly = w_outline()
    assert_symmetric(poly)
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    ad = os.path.join(root, "assets")
    os.makedirs(ad, exist_ok=True)
    with open(os.path.join(ad, "windui.svg"), "w", encoding="utf-8") as f:
        f.write(svg())
    with open(os.path.join(ad, "windui.ico"), "wb") as f:
        f.write(ico())
    with open(os.path.join(ad, "windui-256.png"), "wb") as f:
        f.write(_png(256, rgba(256)))
    print(f"顶点数 {len(poly)}，对称性断言通过。已写出 assets/windui.svg / .ico / -256.png\n")
    print(rust_const())


if __name__ == "__main__":
    sys.exit(main())
