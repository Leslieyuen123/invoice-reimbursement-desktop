from __future__ import annotations

import argparse
import math
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont


FONT_PATH = "/System/Library/Fonts/STHeiti Medium.ttc"
BACKGROUND = "#F4F4F5"
PAPER = "#FFFFFF"
TEXT = "#171719"
MUTED = "#68686D"
BORDER = "#D7D7DB"
PURPLE = "#8D72DD"
ORANGE = "#BC6849"
GREEN = "#7EA58E"
RED = "#C77979"


def font(size: int) -> ImageFont.FreeTypeFont:
    return ImageFont.truetype(FONT_PATH, size=size)


def centered_text(
    draw: ImageDraw.ImageDraw,
    box: tuple[int, int, int, int],
    title: str,
    detail: str,
    accent: str,
) -> None:
    left, top, right, bottom = box
    draw.rounded_rectangle(box, radius=8, fill=PAPER, outline=BORDER, width=2)
    draw.rectangle((left, top, left + 10, bottom), fill=accent)
    title_font = font(36)
    detail_font = font(23)
    title_box = draw.textbbox((0, 0), title, font=title_font)
    detail_box = draw.multiline_textbbox((0, 0), detail, font=detail_font, spacing=8, align="center")
    total_height = (title_box[3] - title_box[1]) + 20 + (detail_box[3] - detail_box[1])
    y = top + (bottom - top - total_height) // 2
    draw.text(((left + right) // 2, y), title, font=title_font, fill=TEXT, anchor="ma")
    draw.multiline_text(
        ((left + right) // 2, y + 62),
        detail,
        font=detail_font,
        fill=MUTED,
        anchor="ma",
        spacing=8,
        align="center",
    )


def arrow(
    draw: ImageDraw.ImageDraw,
    start: tuple[int, int],
    end: tuple[int, int],
    color: str = MUTED,
) -> None:
    draw.line((start, end), fill=color, width=5)
    angle = math.atan2(end[1] - start[1], end[0] - start[0])
    base_x = end[0] - 18 * math.cos(angle)
    base_y = end[1] - 18 * math.sin(angle)
    perpendicular_x = 11 * math.sin(angle)
    perpendicular_y = -11 * math.cos(angle)
    draw.polygon(
        (
            end,
            (base_x + perpendicular_x, base_y + perpendicular_y),
            (base_x - perpendicular_x, base_y - perpendicular_y),
        ),
        fill=color,
    )


def build_workflow(path: Path) -> None:
    image = Image.new("RGB", (1800, 650), BACKGROUND)
    draw = ImageDraw.Draw(image)
    draw.text((80, 30), "从创建批次到报销材料", font=font(48), fill=TEXT)
    draw.text(
        (80, 88),
        "安全项自动归属并导出；只有异常项进入待处理池。",
        font=font(25),
        fill=MUTED,
    )

    boxes = [
        (80, 145, 430, 335),
        (500, 145, 850, 335),
        (920, 145, 1270, 335),
        (1340, 145, 1690, 335),
    ]
    stages = [
        ("创建自动批次", "按月或自定义范围\n保留自动处理", PURPLE),
        ("范围扫描/识别", "同步已启用邮箱\n提取日期、金额和分类", PURPLE),
        ("安全筛选", "排除重复、缺失\n损坏和归属冲突", ORANGE),
        ("归属/导出", "安全项加入批次\n生成完整报销包", GREEN),
    ]
    for box, (title, detail, accent) in zip(boxes, stages, strict=True):
        centered_text(draw, box, title, detail, accent)
    for left, right in zip(boxes, boxes[1:]):
        arrow(draw, (left[2] + 12, 240), (right[0] - 12, 240))

    pending = (610, 430, 890, 585)
    correction = (990, 430, 1270, 585)
    rerun = (1370, 430, 1650, 585)
    centered_text(draw, pending, "待处理池", "仅保留不确定项", RED)
    centered_text(draw, correction, "人工修正", "处理重复或补全字段", ORANGE)
    centered_text(draw, rerun, "再次运行", "回到原批次重试", PURPLE)
    arrow(draw, (1095, 335), (750, 415), RED)
    arrow(draw, (pending[2] + 12, 508), (correction[0] - 12, 508), ORANGE)
    arrow(draw, (correction[2] + 12, 508), (rerun[0] - 12, 508), PURPLE)
    draw.text((770, 368), "发现异常", font=font(22), fill=RED)
    draw.text((1400, 603), "重新扫描与安全筛选", font=font(20), fill=MUTED)
    image.save(path, optimize=True)


def status_box(
    draw: ImageDraw.ImageDraw,
    box: tuple[int, int, int, int],
    title: str,
    detail: str,
    accent: str,
) -> None:
    centered_text(draw, box, title, detail, accent)


def build_status_flow(path: Path) -> None:
    image = Image.new("RGB", (1800, 900), BACKGROUND)
    draw = ImageDraw.Draw(image)
    draw.text((90, 55), "待处理池状态怎么理解", font=font(54), fill=TEXT)
    draw.text(
        (90, 125),
        "只有完成确认且没有重复风险的票据，才能进入报销批次。",
        font=font(27),
        fill=MUTED,
    )

    pending = (90, 255, 440, 455)
    confirmation = (725, 255, 1075, 455)
    ready = (1360, 255, 1710, 455)
    failed = (90, 610, 440, 800)
    duplicate = (725, 610, 1075, 800)
    resolution = (1360, 610, 1710, 800)

    status_box(draw, pending, "待识别", "系统正在读取\n发票内容", PURPLE)
    status_box(draw, confirmation, "待确认", "检查识别结果\n保存并确认", ORANGE)
    status_box(draw, ready, "可纳入批次", "可以归入批次\n并准备导出", GREEN)
    status_box(draw, failed, "识别失败", "检查文件后\n点击重新识别", RED)
    status_box(draw, duplicate, "疑似重复", "系统发现相似票据\n需要人工判断", RED)
    status_box(draw, resolution, "处理重复", "确认保留\n或删除重复", ORANGE)

    arrow(draw, (pending[2] + 15, 355), (confirmation[0] - 15, 355))
    arrow(draw, (confirmation[2] + 15, 355), (ready[0] - 15, 355), GREEN)
    draw.line(((265, 455), (265, 585)), fill=RED, width=5)
    draw.polygon(((265, 610), (254, 590), (276, 590)), fill=RED)
    draw.line(((900, 455), (900, 585)), fill=RED, width=5)
    draw.polygon(((900, 610), (889, 590), (911, 590)), fill=RED)
    arrow(draw, (duplicate[2] + 15, 705), (resolution[0] - 15, 705), ORANGE)

    draw.text((465, 330), "识别完成", font=font(24), fill=MUTED)
    draw.text((1110, 330), "确认完成", font=font(24), fill=GREEN)
    draw.text((295, 515), "读取失败", font=font(24), fill=RED)
    draw.text((930, 515), "发现相似项", font=font(24), fill=RED)

    image.save(path, optimize=True)


def build_all(output_dir: Path) -> list[Path]:
    output_dir.mkdir(parents=True, exist_ok=True)
    workflow = output_dir / "workflow.png"
    status_flow = output_dir / "item-status-flow.png"
    build_workflow(workflow)
    build_status_flow(status_flow)
    return [workflow, status_flow]


def main() -> int:
    parser = argparse.ArgumentParser(description="Build user-guide diagrams")
    parser.add_argument("output_dir", type=Path)
    args = parser.parse_args()
    for output in build_all(args.output_dir):
        print(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
