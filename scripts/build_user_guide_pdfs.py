"""Build the shareable user-guide PDFs with pinned dependencies.

Run with:
uv run --with-requirements scripts/user-guide-requirements.txt python scripts/build_user_guide_pdfs.py --kind full docs/user-guide/invoice-reimbursement-user-manual.md output/pdf/invoice-reimbursement-user-manual-zh-cn.pdf
uv run --with-requirements scripts/user-guide-requirements.txt python scripts/build_user_guide_pdfs.py --kind quick docs/user-guide/invoice-reimbursement-quick-start.md output/pdf/invoice-reimbursement-quick-start-zh-cn.pdf
"""

from __future__ import annotations

import argparse
import html
import json
import re
from pathlib import Path
from typing import Literal

from PIL import Image as PilImage
from reportlab.lib import colors
from reportlab.lib.enums import TA_CENTER, TA_LEFT
from reportlab.lib.pagesizes import A4
from reportlab.lib.styles import ParagraphStyle, getSampleStyleSheet
from reportlab.lib.units import mm
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.ttfonts import TTFont
from reportlab.platypus import (
    BaseDocTemplate,
    CondPageBreak,
    Frame,
    HRFlowable,
    Image,
    KeepTogether,
    ListFlowable,
    ListItem,
    PageBreak,
    PageTemplate,
    Paragraph,
    Preformatted,
    Spacer,
    Table,
    TableStyle,
)
from reportlab.platypus.tableofcontents import TableOfContents


Kind = Literal["full", "quick"]
REPO_ROOT = Path(__file__).resolve().parents[1]
APP_VERSION = json.loads(
    (REPO_ROOT / "package.json").read_text(encoding="utf-8")
)["version"]
DOCUMENT_DATE = "2026-07-23"
FONT_PATH = Path("/System/Library/Fonts/STHeiti Light.ttc")
FONT_NAME = "GuideHeiti"
PAPER = colors.HexColor("#FFFFFF")
TEXT = colors.HexColor("#171719")
MUTED = colors.HexColor("#68686D")
LIGHT = colors.HexColor("#F2F2F3")
BORDER = colors.HexColor("#D7D7DB")
PURPLE = colors.HexColor("#8D72DD")
ORANGE = colors.HexColor("#BC6849")
GREEN = colors.HexColor("#7EA58E")
PAGE_WIDTH, PAGE_HEIGHT = A4
LEFT_MARGIN = 18 * mm
RIGHT_MARGIN = 18 * mm
TOP_MARGIN = 17 * mm
BOTTOM_MARGIN = 16 * mm
CONTENT_WIDTH = PAGE_WIDTH - LEFT_MARGIN - RIGHT_MARGIN


def register_fonts() -> None:
    if FONT_NAME in pdfmetrics.getRegisteredFontNames():
        return
    if not FONT_PATH.is_file():
        raise FileNotFoundError(f"Chinese font not found: {FONT_PATH}")
    pdfmetrics.registerFont(TTFont(FONT_NAME, str(FONT_PATH), subfontIndex=0))


def styles() -> dict[str, ParagraphStyle]:
    base = getSampleStyleSheet()
    return {
        "body": ParagraphStyle(
            "GuideBody",
            parent=base["BodyText"],
            fontName=FONT_NAME,
            fontSize=10.2,
            leading=16.8,
            textColor=TEXT,
            spaceAfter=7,
            wordWrap="CJK",
            allowWidows=0,
            allowOrphans=0,
        ),
        "lead": ParagraphStyle(
            "GuideLead",
            parent=base["BodyText"],
            fontName=FONT_NAME,
            fontSize=12.2,
            leading=20,
            textColor=MUTED,
            spaceAfter=10,
            wordWrap="CJK",
        ),
        "h1": ParagraphStyle(
            "GuideH1",
            parent=base["Title"],
            fontName=FONT_NAME,
            fontSize=27,
            leading=36,
            textColor=TEXT,
            alignment=TA_LEFT,
            spaceAfter=12,
            wordWrap="CJK",
        ),
        "h2": ParagraphStyle(
            "GuideH2",
            parent=base["Heading1"],
            fontName=FONT_NAME,
            fontSize=20,
            leading=28,
            textColor=TEXT,
            spaceBefore=4,
            spaceAfter=12,
            keepWithNext=True,
            wordWrap="CJK",
        ),
        "h3": ParagraphStyle(
            "GuideH3",
            parent=base["Heading2"],
            fontName=FONT_NAME,
            fontSize=14,
            leading=21,
            textColor=PURPLE,
            spaceBefore=10,
            spaceAfter=7,
            keepWithNext=True,
            wordWrap="CJK",
        ),
        "h4": ParagraphStyle(
            "GuideH4",
            parent=base["Heading3"],
            fontName=FONT_NAME,
            fontSize=11.5,
            leading=18,
            textColor=TEXT,
            spaceBefore=7,
            spaceAfter=5,
            keepWithNext=True,
            wordWrap="CJK",
        ),
        "caption": ParagraphStyle(
            "GuideCaption",
            parent=base["BodyText"],
            fontName=FONT_NAME,
            fontSize=8.2,
            leading=12,
            alignment=TA_CENTER,
            textColor=MUTED,
            spaceBefore=5,
            spaceAfter=11,
            wordWrap="CJK",
        ),
        "quote": ParagraphStyle(
            "GuideQuote",
            parent=base["BodyText"],
            fontName=FONT_NAME,
            fontSize=9.5,
            leading=15.5,
            textColor=TEXT,
            leftIndent=8,
            rightIndent=8,
            spaceAfter=0,
            wordWrap="CJK",
        ),
        "table": ParagraphStyle(
            "GuideTable",
            parent=base["BodyText"],
            fontName=FONT_NAME,
            fontSize=8.4,
            leading=12.5,
            textColor=TEXT,
            wordWrap="CJK",
        ),
        "table_header": ParagraphStyle(
            "GuideTableHeader",
            parent=base["BodyText"],
            fontName=FONT_NAME,
            fontSize=8.6,
            leading=13,
            textColor=TEXT,
            wordWrap="CJK",
        ),
        "code": ParagraphStyle(
            "GuideCode",
            parent=base["Code"],
            fontName=FONT_NAME,
            fontSize=8.5,
            leading=13,
            textColor=TEXT,
            backColor=LIGHT,
            borderColor=BORDER,
            borderWidth=0.5,
            borderPadding=8,
            spaceBefore=4,
            spaceAfter=9,
        ),
        "cover_kicker": ParagraphStyle(
            "GuideCoverKicker",
            parent=base["BodyText"],
            fontName=FONT_NAME,
            fontSize=9,
            leading=14,
            textColor=PURPLE,
            spaceAfter=12,
        ),
        "cover_title": ParagraphStyle(
            "GuideCoverTitle",
            parent=base["Title"],
            fontName=FONT_NAME,
            fontSize=34,
            leading=44,
            alignment=TA_LEFT,
            textColor=TEXT,
            spaceAfter=14,
            wordWrap="CJK",
        ),
        "cover_subtitle": ParagraphStyle(
            "GuideCoverSubtitle",
            parent=base["BodyText"],
            fontName=FONT_NAME,
            fontSize=13,
            leading=22,
            textColor=MUTED,
            spaceAfter=18,
            wordWrap="CJK",
        ),
    }


def inline_markup(value: str) -> str:
    escaped = html.escape(value.strip())
    escaped = re.sub(
        r"\*\*(.+?)\*\*",
        r'<b>\1</b>',
        escaped,
    )
    escaped = re.sub(
        re.escape(chr(96)) + r"([^" + re.escape(chr(96)) + r"]+)" + re.escape(chr(96)),
        r'<font color="#6848BD">\1</font>',
        escaped,
    )
    escaped = re.sub(
        r"\[([^\]]+)\]\((https?://[^)]+)\)",
        r'<link href="\2" color="#6848BD">\1</link>',
        escaped,
    )
    return escaped


class GuideDocTemplate(BaseDocTemplate):
    def __init__(self, filename: str, *, kind: Kind, title: str):
        super().__init__(
            filename,
            pagesize=A4,
            leftMargin=LEFT_MARGIN,
            rightMargin=RIGHT_MARGIN,
            topMargin=TOP_MARGIN,
            bottomMargin=BOTTOM_MARGIN,
            title=title,
            author="发票报销",
            subject="发票报销桌面 App 使用手册",
            keywords="发票 报销 macOS Gmail QQ PDF Excel",
        )
        self.kind = kind
        self.running_title = title
        self._heading_sequence = 0
        frame = Frame(
            self.leftMargin,
            self.bottomMargin,
            self.width,
            self.height,
            id="content",
            leftPadding=0,
            rightPadding=0,
            topPadding=0,
            bottomPadding=0,
        )
        self.addPageTemplates(
            PageTemplate(id="guide", frames=[frame], onPage=self.draw_page),
        )

    def beforeDocument(self) -> None:
        self._heading_sequence = 0

    def draw_page(self, canvas, doc) -> None:
        canvas.saveState()
        canvas.setFillColor(PAPER)
        canvas.rect(0, 0, PAGE_WIDTH, PAGE_HEIGHT, fill=1, stroke=0)
        canvas.setFont(FONT_NAME, 7.5)
        canvas.setFillColor(MUTED)
        if not (self.kind == "full" and doc.page == 1):
            canvas.drawString(LEFT_MARGIN, PAGE_HEIGHT - 10 * mm, self.running_title)
            canvas.setStrokeColor(BORDER)
            canvas.setLineWidth(0.5)
            canvas.line(
                LEFT_MARGIN,
                PAGE_HEIGHT - 12.5 * mm,
                PAGE_WIDTH - RIGHT_MARGIN,
                PAGE_HEIGHT - 12.5 * mm,
            )
        canvas.drawString(LEFT_MARGIN, 8.5 * mm, f"发票报销 v{APP_VERSION}")
        canvas.drawRightString(
            PAGE_WIDTH - RIGHT_MARGIN,
            8.5 * mm,
            f"第 {doc.page} 页",
        )
        canvas.restoreState()

    def afterFlowable(self, flowable) -> None:
        level = getattr(flowable, "toc_level", None)
        if level is None:
            return
        text = flowable.getPlainText()
        key = f"heading-{self._heading_sequence}"
        self._heading_sequence += 1
        self.canv.bookmarkPage(key)
        self.canv.addOutlineEntry(text, key, level=level, closed=False)
        if level == 0:
            self.notify("TOCEntry", (level, text, self.page, key))


def quote_box(text: str, style_map: dict[str, ParagraphStyle]) -> Table:
    markup = "<br/>".join(inline_markup(part) for part in text.split("<br/>"))
    paragraph = Paragraph(markup, style_map["quote"])
    table = Table([[paragraph]], colWidths=[CONTENT_WIDTH])
    table.setStyle(
        TableStyle(
            [
                ("BACKGROUND", (0, 0), (-1, -1), colors.HexColor("#F7F4F2")),
                ("BOX", (0, 0), (-1, -1), 0.5, BORDER),
                ("LINEBEFORE", (0, 0), (0, -1), 4, ORANGE),
                ("LEFTPADDING", (0, 0), (-1, -1), 10),
                ("RIGHTPADDING", (0, 0), (-1, -1), 10),
                ("TOPPADDING", (0, 0), (-1, -1), 9),
                ("BOTTOMPADDING", (0, 0), (-1, -1), 9),
            ]
        )
    )
    return table


def image_flowables(
    markdown_path: Path,
    target: str,
    alt: str,
    style_map: dict[str, ParagraphStyle],
    kind: Kind,
) -> list:
    path = (markdown_path.parent / target).resolve()
    if not path.is_file():
        raise FileNotFoundError(f"Referenced guide image does not exist: {path}")
    with PilImage.open(path) as source:
        width_px, height_px = source.size
    aspect = width_px / height_px
    max_width = CONTENT_WIDTH
    if kind == "quick":
        if path.name == "workflow.png":
            max_height = 45 * mm
        elif path.name == "mailbox-setup.png":
            max_height = 42 * mm
        elif aspect < 1:
            max_height = 88 * mm
        else:
            max_height = 58 * mm
    else:
        if path.name == "mailbox-setup.png":
            max_height = 40 * mm
        elif path.name == "item-status-flow.png":
            max_height = 62 * mm
        else:
            max_height = 145 * mm if aspect < 1 else 100 * mm
    width = max_width
    height = width / aspect
    if height > max_height:
        height = max_height
        width = height * aspect
    image = Image(str(path), width=width, height=height)
    image.hAlign = "CENTER"
    caption = Paragraph(inline_markup(alt), style_map["caption"])
    return [KeepTogether([Spacer(1, 4), image, caption])]


def markdown_table(
    lines: list[str],
    style_map: dict[str, ParagraphStyle],
) -> Table:
    rows: list[list[str]] = []
    for line in lines:
        cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
        if all(re.fullmatch(r":?-{3,}:?", cell) for cell in cells):
            continue
        rows.append(cells)
    if not rows:
        raise ValueError("Markdown table has no rows")
    columns = max(len(row) for row in rows)
    for row in rows:
        row.extend([""] * (columns - len(row)))
    content = []
    for row_index, row in enumerate(rows):
        style = style_map["table_header"] if row_index == 0 else style_map["table"]
        content.append([Paragraph(inline_markup(cell), style) for cell in row])
    column_widths = [CONTENT_WIDTH / columns] * columns
    table = Table(content, colWidths=column_widths, repeatRows=1, hAlign="LEFT")
    table.setStyle(
        TableStyle(
            [
                ("BACKGROUND", (0, 0), (-1, 0), LIGHT),
                ("TEXTCOLOR", (0, 0), (-1, -1), TEXT),
                ("GRID", (0, 0), (-1, -1), 0.45, BORDER),
                ("VALIGN", (0, 0), (-1, -1), "TOP"),
                ("LEFTPADDING", (0, 0), (-1, -1), 7),
                ("RIGHTPADDING", (0, 0), (-1, -1), 7),
                ("TOPPADDING", (0, 0), (-1, -1), 6),
                ("BOTTOMPADDING", (0, 0), (-1, -1), 6),
            ]
        )
    )
    return table


def is_block_start(line: str) -> bool:
    stripped = line.strip()
    return bool(
        not stripped
        or stripped.startswith(("#", ">", "- ", "* ", "![", "~~~", "<!--"))
        or re.match(r"\d+\.\s+", stripped)
        or stripped == "---"
        or stripped.startswith("|")
    )


def parse_markdown(
    markdown_path: Path,
    *,
    kind: Kind,
    style_map: dict[str, ParagraphStyle],
) -> tuple[str, list]:
    lines = markdown_path.read_text(encoding="utf-8").splitlines()
    story: list = []
    title = "发票报销"
    index = 0
    first_h1_seen = False
    first_major_section = True

    while index < len(lines):
        raw = lines[index]
        stripped = raw.strip()
        if not stripped:
            index += 1
            continue
        if stripped == "<!-- pagebreak -->":
            story.append(PageBreak())
            index += 1
            continue
        if stripped.startswith("~~~"):
            language = stripped[3:].strip()
            code_lines: list[str] = []
            index += 1
            while index < len(lines) and not lines[index].strip().startswith("~~~"):
                code_lines.append(lines[index])
                index += 1
            index += 1
            del language
            story.append(Preformatted("\n".join(code_lines), style_map["code"]))
            continue
        heading = re.match(r"^(#{1,4})\s+(.+)$", stripped)
        if heading:
            depth = len(heading.group(1))
            text = heading.group(2).strip()
            if depth == 1:
                title = text
                if kind == "full" and not first_h1_seen:
                    first_h1_seen = True
                    index += 1
                    continue
                paragraph = Paragraph(inline_markup(text), style_map["h1"])
            elif depth == 2:
                if kind == "full":
                    if not first_major_section:
                        story.append(CondPageBreak(90 * mm))
                    first_major_section = False
                paragraph = Paragraph(inline_markup(text), style_map["h2"])
                paragraph.toc_level = 0
            elif depth == 3:
                paragraph = Paragraph(inline_markup(text), style_map["h3"])
                paragraph.toc_level = 1
            else:
                paragraph = Paragraph(inline_markup(text), style_map["h4"])
                paragraph.toc_level = 2
            story.append(paragraph)
            index += 1
            continue
        if stripped == "---":
            story.append(
                HRFlowable(
                    width="100%",
                    thickness=0.6,
                    color=BORDER,
                    spaceBefore=8,
                    spaceAfter=10,
                )
            )
            index += 1
            continue
        image_match = re.match(r"^!\[([^\]]*)\]\(([^)]+)\)$", stripped)
        if image_match:
            story.extend(
                image_flowables(
                    markdown_path,
                    image_match.group(2),
                    image_match.group(1),
                    style_map,
                    kind,
                )
            )
            index += 1
            continue
        if stripped.startswith(">"):
            quote_lines: list[str] = []
            while index < len(lines) and lines[index].strip().startswith(">"):
                quote_lines.append(lines[index].strip()[1:].strip())
                index += 1
            story.append(quote_box("<br/>".join(quote_lines), style_map))
            story.append(Spacer(1, 8))
            continue
        if stripped.startswith("|"):
            table_lines: list[str] = []
            while index < len(lines) and lines[index].strip().startswith("|"):
                table_lines.append(lines[index])
                index += 1
            story.append(markdown_table(table_lines, style_map))
            story.append(Spacer(1, 9))
            continue
        list_match = re.match(r"^(\d+)\.\s+(.+)$", stripped)
        if list_match or stripped.startswith(("- ", "* ")):
            ordered = bool(list_match)
            items: list[ListItem] = []
            while index < len(lines):
                current = lines[index].strip()
                numbered = re.match(r"^(\d+)\.\s+(.+)$", current)
                bullet = current.startswith(("- ", "* "))
                if ordered and not numbered:
                    break
                if not ordered and not bullet:
                    break
                value = numbered.group(2) if numbered else current[2:]
                items.append(
                    ListItem(
                        Paragraph(inline_markup(value), style_map["body"]),
                        leftIndent=12,
                    )
                )
                index += 1
            list_options = {
                "bulletType": "1" if ordered else "bullet",
                "leftIndent": 18,
                "bulletFontName": FONT_NAME,
                "bulletFontSize": 8.5,
                "bulletColor": PURPLE,
                "spaceAfter": 6,
            }
            if ordered:
                list_options["start"] = "1"
            story.append(ListFlowable(items, **list_options))
            continue
        paragraph_lines = [stripped]
        index += 1
        while index < len(lines) and not is_block_start(lines[index]):
            paragraph_lines.append(lines[index].strip())
            index += 1
        text = " ".join(paragraph_lines)
        paragraph_style = style_map["lead"] if not story else style_map["body"]
        story.append(Paragraph(inline_markup(text), paragraph_style))

    return title, story


def cover_story(
    markdown_path: Path,
    title: str,
    style_map: dict[str, ParagraphStyle],
) -> list:
    workflow = markdown_path.parent / "assets" / "workflow.png"
    story: list = [
        Spacer(1, 30 * mm),
        Paragraph("发票报销 · USER GUIDE", style_map["cover_kicker"]),
        Paragraph(inline_markup(title), style_map["cover_title"]),
        Paragraph(
            "从批次创建、范围扫描到安全归属和报销包导出",
            style_map["cover_subtitle"],
        ),
        HRFlowable(width=55 * mm, thickness=4, color=PURPLE, hAlign="LEFT"),
        Spacer(1, 18 * mm),
    ]
    if workflow.is_file():
        story.extend(
            image_flowables(
                markdown_path,
                "assets/workflow.png",
                "创建批次 -> 扫描/识别 -> 安全筛选 -> 归属/导出",
                style_map,
                "full",
            )
        )
    metadata = Table(
        [
            [Paragraph("适用版本", style_map["table_header"]), Paragraph(f"v{APP_VERSION}", style_map["table"])],
            [Paragraph("适用设备", style_map["table_header"]), Paragraph("Apple Silicon Mac", style_map["table"])],
            [Paragraph("最低系统", style_map["table_header"]), Paragraph("macOS 11", style_map["table"])],
            [Paragraph("文档日期", style_map["table_header"]), Paragraph(DOCUMENT_DATE, style_map["table"])],
        ],
        colWidths=[36 * mm, 72 * mm],
        hAlign="LEFT",
    )
    metadata.setStyle(
        TableStyle(
            [
                ("BACKGROUND", (0, 0), (0, -1), LIGHT),
                ("GRID", (0, 0), (-1, -1), 0.45, BORDER),
                ("VALIGN", (0, 0), (-1, -1), "MIDDLE"),
                ("LEFTPADDING", (0, 0), (-1, -1), 8),
                ("RIGHTPADDING", (0, 0), (-1, -1), 8),
                ("TOPPADDING", (0, 0), (-1, -1), 7),
                ("BOTTOMPADDING", (0, 0), (-1, -1), 7),
            ]
        )
    )
    story.extend([Spacer(1, 10 * mm), metadata, PageBreak()])
    return story


def toc_story(style_map: dict[str, ParagraphStyle]) -> list:
    toc = TableOfContents()
    toc.levelStyles = [
        ParagraphStyle(
            "GuideTOC0",
            fontName=FONT_NAME,
            fontSize=10.5,
            leading=17,
            leftIndent=0,
            firstLineIndent=0,
            textColor=TEXT,
            spaceBefore=5,
        ),
        ParagraphStyle(
            "GuideTOC1",
            fontName=FONT_NAME,
            fontSize=8.8,
            leading=14,
            leftIndent=12,
            firstLineIndent=0,
            textColor=MUTED,
            spaceBefore=2,
        ),
        ParagraphStyle(
            "GuideTOC2",
            fontName=FONT_NAME,
            fontSize=8,
            leading=12,
            leftIndent=24,
            firstLineIndent=0,
            textColor=MUTED,
        ),
    ]
    return [
        Paragraph("目录", style_map["h1"]),
        Paragraph(
            "按实际工作顺序查找安装、邮箱、同步、确认、批次和导出说明。",
            style_map["lead"],
        ),
        Spacer(1, 6),
        toc,
        PageBreak(),
    ]


def build_pdf(markdown_path: Path, output_path: Path, *, kind: Kind) -> None:
    register_fonts()
    style_map = styles()
    title, content = parse_markdown(markdown_path, kind=kind, style_map=style_map)
    if kind == "full":
        story = cover_story(markdown_path, title, style_map) + toc_story(style_map) + content
    else:
        story = content
    output_path.parent.mkdir(parents=True, exist_ok=True)
    temporary = output_path.with_suffix(".part.pdf")
    if temporary.exists():
        temporary.unlink()
    document = GuideDocTemplate(str(temporary), kind=kind, title=title)
    document.multiBuild(story)
    temporary.replace(output_path)


def main() -> int:
    parser = argparse.ArgumentParser(description="Build shareable user-guide PDFs")
    parser.add_argument("--kind", choices=("full", "quick"), required=True)
    parser.add_argument("markdown", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    build_pdf(args.markdown, args.output, kind=args.kind)
    print(args.output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
