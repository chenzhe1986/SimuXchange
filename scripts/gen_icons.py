# 生成 SimuXchange 应用图标（临时脚本）
import os
from PIL import Image, ImageDraw

BASE = os.path.join(os.path.dirname(__file__), 'src-tauri', 'icons')
os.makedirs(BASE, exist_ok=True)

S = 512
img = Image.new('RGBA', (S, S), (0, 0, 0, 0))
d = ImageDraw.Draw(img)

# 圆角矩形背景：靛蓝渐变
bg = Image.new('RGBA', (S, S), (0, 0, 0, 0))
bd = ImageDraw.Draw(bg)
for y in range(S):
    t = y / S
    r = int(0x4F + (0x7C - 0x4F) * t)
    g = int(0x46 + (0x3A - 0x46) * t)
    b = int(0xE5 + (0xED - 0xE5) * t)
    bd.line([(0, y), (S, y)], fill=(r, g, b, 255))
mask = Image.new('L', (S, S), 0)
md = ImageDraw.Draw(mask)
md.rounded_rectangle([16, 16, S - 16, S - 16], radius=110, fill=255)
img.paste(bg, (0, 0), mask)

# K 线（蜡烛图）图案
d = ImageDraw.Draw(img)
white = (255, 255, 255, 235)
semi = (255, 255, 255, 150)
candles = [
    # (x中心, 影线上, 实体上, 实体下, 影线下, 颜色)
    (150, 150, 200, 310, 360, semi),
    (256, 110, 170, 290, 340, white),
    (362, 190, 240, 350, 400, semi),
]
bw = 56  # 实体宽度
for cx, wick_top, body_top, body_bot, wick_bot, color in candles:
    d.line([(cx, wick_top), (cx, wick_bot)], fill=color, width=14)
    d.rounded_rectangle([cx - bw // 2, body_top, cx + bw // 2, body_bot], radius=12, fill=color)

img.save(os.path.join(BASE, 'icon.png'))
for size in (32, 128):
    img.resize((size, size), Image.LANCZOS).save(os.path.join(BASE, f'{size}x{size}.png'))
img.resize((256, 256), Image.LANCZOS).save(
    os.path.join(BASE, 'icon.ico'),
    sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
)
print('icons generated:', os.listdir(BASE))
