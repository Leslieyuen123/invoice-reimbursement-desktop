from __future__ import annotations

import argparse
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
    x, y = end
    draw.polygon(((x, y), (x - 18, y - 11), (x - 18, y + 11)), fill=color)


def build_workflow(path: Path) -> None:
    image = Image.new("RGB", (1800, 650), BACKGROUND)
    draw = ImageDraw.Draw(image)
    draw.text((90, 60), "从邮箱发票到报销材料", font=font(54), fill=TEXT)
    draw.text(
        (90, 130),
        "绑定邮箱只是开始。导出前仍需要人工检查和确认。",
        font=font(27),
        fill=MUTED,
    )

    boxes = [
        (80, 235, 375, 505),
        (420, 235, 715, 505),
        (760, 235, 1055, 505),
        (1100, 235, 1395, 505),
        (1440, 235, 1735, 505),
    ]
    stages = [
        ("同步或导入", "立即同步\n后台自动同步\n手动导入", PURPLE),
        ("自动识别", "读取日期、金额\n分类和公司", PURPLE),
        ("检查并确认", "修正字段\n保存并确认", ORANGE),
        ("归入批次", "创建月度或\n自定义批次", ORANGE),
        ("导出材料", "PDF + Excel\n原件 + 清单", GREEN),
    ]
    for box, (title, detail, accent) in zip(boxes, stages, strict=True):
        centered_text(draw, box, title, detail, accent)
    for left, right in zip(boxes, boxes[1:]):
        arrow(draw, (left[2] + 12, 370), (right[0] - 12, 370))

    draw.text(
        (90, 570),
        "推荐日常路径：打开控制台 -> 立即同步 -> 待处理池 -> 报销批次 -> 导出报销包",
        font=font(25),
        fill=TEXT,
    )
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
