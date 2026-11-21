"""重新生成本目录的解码测试样例（需要 Pillow）：python3 gen.py

样例都很小（几十到几百字节），内容与各测试文件里的注释一一对应。
"""
from PIL import Image

RED, BLUE, GREEN = (255, 0, 0), (0, 0, 255), (0, 255, 0)


def left_red_right_blue(w, h):
    im = Image.new("RGB", (w, h), BLUE)
    im.paste(RED, (0, 0, w // 2, h))
    return im


# JPEG：16×8 左红右蓝；同一张图加 EXIF Orientation=6；8×8 灰度。
im = left_red_right_blue(16, 8)
im.save("red_blue_16x8.jpg", quality=95, subsampling=0)
exif = Image.Exif()
exif[0x0112] = 6
im.save("red_blue_16x8_orient6.jpg", quality=95, subsampling=0, exif=exif.tobytes())
Image.new("L", (8, 8), 128).save("gray_8x8.jpg", quality=95)

# GIF：4×2，索引 0 为透明色；像素 (0,0) 透明，上行其余红，下行蓝；第二帧全绿（应被忽略）。
pal = [0, 0, 0, *RED, *BLUE, *GREEN] + [0] * (256 - 4) * 3
f1 = Image.new("P", (4, 2), 1)
f1.putpalette(pal)
for x in range(4):
    f1.putpixel((x, 1), 2)
f1.putpixel((0, 0), 0)
f2 = Image.new("P", (4, 2), 3)
f2.putpalette(pal)
f1.save("red_blue_4x2_anim.gif", save_all=True, append_images=[f2], transparency=0,
        duration=100, loop=0, disposal=1, optimize=False)

# GIF：逻辑画布 4×4，唯一一帧 2×2 纯红位于 (2,2)。Pillow 不直接写局部帧，手工拼字节。
gif = bytearray(b"GIF89a")
gif += (4).to_bytes(2, "little") + (4).to_bytes(2, "little")
gif += bytes([0b1000_0000, 0, 0])  # 全局调色板 2 色
gif += bytes([*RED, 0, 0, 0])
gif += bytes([0x2C]) + (2).to_bytes(2, "little") * 2 + (2).to_bytes(2, "little") * 2 + bytes([0])
# LZW 最小码长 2；码流：清除码(4)、4 个索引 0、结束码(5)，按 3 位码打包。
codes = [4, 0, 0, 0, 0, 5]
bits, nbits, out = 0, 0, bytearray()
width = 3
for c in codes:
    bits |= c << nbits
    nbits += width
    while nbits >= 8:
        out.append(bits & 0xFF)
        bits >>= 8
        nbits -= 8
if nbits:
    out.append(bits & 0xFF)
gif += bytes([2, len(out)]) + out + bytes([0, 0x3B])
open("offset_frame_4x4.gif", "wb").write(gif)

# WebP：无损 4×2（左两列红不透明，右两列蓝 alpha 128）；有损 16×8 左红右蓝。
im = Image.new("RGBA", (4, 2), (0, 0, 255, 128))
im.paste((255, 0, 0, 255), (0, 0, 2, 2))
im.save("red_blue_4x2_lossless.webp", lossless=True, exact=True)
left_red_right_blue(16, 8).save("red_blue_16x8_lossy.webp", quality=95)
