import contextlib
import json
import math
import re
import sys
from pathlib import Path
from typing import Any


PDF_RENDER_SCALE = 200 / 72
# Keep these document budgets aligned with src-tauri/src/infra/extraction.rs.
MAX_PDF_PAGES = 100
MAX_IMAGE_DIMENSION = 20_000
MAX_IMAGE_PIXELS = 40_000_000
MAX_TOTAL_PDF_RENDERED_PIXELS = 100_000_000
MAX_PDF_TEXT_CHARACTERS = 200_000
RESOURCE_LIMIT_MESSAGE = "document exceeds OCR resource limits"

# Preprocessing: imported images are phone screenshots or photos, so the
# glyphs handed to the engine are often small, faint or unevenly lit. The
# parameters below are deliberately conservative; the raw image is still
# recognised as a fallback when the prepared page yields no amount at all.
GRAYSCALE_UPSCALE_SHORT_SIDE = 1_200
UPSCALE_TARGET_SHORT_SIDE = 1_600
# Upscaling further than this cannot create detail, it only enlarges JPEG
# blocking and sensor noise, which measured worse on small blurry samples.
MAX_UPSCALE_FACTOR = 2.0
CONTRAST_CLIP_LIMIT = 2.0
CONTRAST_TILE_GRID = 8
BILEVEL_SAMPLE_SIDE = 256
BILEVEL_MIDTONE_MARGIN = 24
BILEVEL_MIDTONE_MAX_RATIO = 0.08
ADAPTIVE_BLOCK_SIZE = 31
ADAPTIVE_THRESHOLD_C = 10
DARK_BACKGROUND_MEAN = 110
AMOUNT_MARKER_PATTERN = re.compile(r"[¥￥]\s*\d|RMB\s*\d", re.IGNORECASE)
DECIMAL_AMOUNT_PATTERN = re.compile(r"\d{1,3}(?:,\d{3})*[.,]\d{2}(?!\d)")
INVOICE_SIGNAL_TOKENS = (
    "价税合计",
    "合计",
    "小写",
    "开票日期",
    "发票号码",
    "¥",
    "￥",
    "RMB",
    "金额",
    "税额",
    "名称",
)
_engine: Any | None = None


class InputFileMissingError(Exception):
    pass


class ResourceLimitError(Exception):
    pass


def get_engine() -> Any:
    global _engine
    if _engine is None:
        with contextlib.redirect_stdout(sys.stderr):
            from rapidocr_onnxruntime import RapidOCR

            _engine = RapidOCR()
    return _engine


def recognize_image(image: str | Any) -> str:
    with contextlib.redirect_stdout(sys.stderr):
        result, _ = get_engine()(image)
    if not result:
        return ""
    return "\n".join(
        str(item[1]).strip()
        for item in result
        if len(item) > 1 and str(item[1]).strip()
    )


def _imaging_modules() -> tuple[Any, Any]:
    with contextlib.redirect_stdout(sys.stderr):
        import cv2
        import numpy as np

    return np, cv2


def _grayscale_array(pil_image: Any) -> Any:
    np, _cv2 = _imaging_modules()
    return np.array(pil_image.convert("L"), copy=True)


def _rgb_array(pil_image: Any) -> Any:
    np, _cv2 = _imaging_modules()
    return np.array(pil_image.convert("RGB"), copy=True)


def upscale_factor(width: int, height: int) -> float:
    """Scale factor that brings a small page to an OCR friendly resolution.

    Returns ``1.0`` when the short side is already large enough, when growing
    the page would break the documented resource budget, or when the growth
    would exceed :data:`MAX_UPSCALE_FACTOR`.
    """
    short_side = min(width, height)
    if short_side >= GRAYSCALE_UPSCALE_SHORT_SIDE:
        return 1.0
    factor = min(UPSCALE_TARGET_SHORT_SIDE / short_side, MAX_UPSCALE_FACTOR)
    factor = min(factor, MAX_IMAGE_DIMENSION / max(width, height))
    factor = min(factor, math.sqrt(MAX_IMAGE_PIXELS / (width * height)))
    return factor if factor > 1.0 else 1.0


def upscale_for_ocr(gray: Any) -> Any:
    _np, cv2 = _imaging_modules()
    height, width = gray.shape[:2]
    factor = upscale_factor(width, height)
    if factor <= 1.0:
        return gray
    target = (max(1, round(width * factor)), max(1, round(height * factor)))
    return cv2.resize(gray, target, interpolation=cv2.INTER_LANCZOS4)


def invert_dark_background(gray: Any) -> Any:
    """Turn a dark mode screenshot into dark ink on a light page.

    The median of a downscaled copy decides instead of the mean, so a photo of
    a light invoice lying on a dark desk is not inverted by its background.
    """
    np, cv2 = _imaging_modules()
    height, width = gray.shape[:2]
    scale = min(BILEVEL_SAMPLE_SIDE / min(height, width), 1.0)
    if scale >= 1.0:
        sample = gray
    else:
        sample = cv2.resize(
            gray,
            (max(1, round(width * scale)), max(1, round(height * scale))),
            interpolation=cv2.INTER_AREA,
        )
    if float(np.median(sample)) >= DARK_BACKGROUND_MEAN:
        return gray
    return cv2.bitwise_not(gray)


def is_bilevel_document(gray: Any) -> bool:
    """Whether the page is already an almost two tone document.

    Adaptive binarisation helps screenshots and rendered PDF pages, but it
    flattens the lighter half of a photo or a seal into the background, so the
    decision is made on the mid tone fraction of the page.
    """
    _np, cv2 = _imaging_modules()
    height, width = gray.shape[:2]
    scale = min(BILEVEL_SAMPLE_SIDE / min(height, width), 1.0)
    if scale >= 1.0:
        sample = gray
    else:
        sample = cv2.resize(
            gray,
            (max(1, round(width * scale)), max(1, round(height * scale))),
            interpolation=cv2.INTER_AREA,
        )
    threshold, _ = cv2.threshold(sample, 0, 255, cv2.THRESH_BINARY + cv2.THRESH_OTSU)
    lower = max(0.0, float(threshold) - BILEVEL_MIDTONE_MARGIN)
    upper = min(255.0, float(threshold) + BILEVEL_MIDTONE_MARGIN)
    midtones = int(((sample > lower) & (sample <= upper)).sum())
    return midtones / sample.size <= BILEVEL_MIDTONE_MAX_RATIO


def binarize_document(gray: Any) -> Any:
    _np, cv2 = _imaging_modules()
    return cv2.adaptiveThreshold(
        gray,
        255,
        cv2.ADAPTIVE_THRESH_GAUSSIAN_C,
        cv2.THRESH_BINARY,
        ADAPTIVE_BLOCK_SIZE,
        ADAPTIVE_THRESHOLD_C,
    )


def enhance_contrast(gray: Any) -> Any:
    _np, cv2 = _imaging_modules()
    clahe = cv2.createCLAHE(
        clipLimit=CONTRAST_CLIP_LIMIT,
        tileGridSize=(CONTRAST_TILE_GRID, CONTRAST_TILE_GRID),
    )
    return clahe.apply(gray)


def prepare_image(pil_image: Any) -> Any:
    """Grayscale, small page upscale and local contrast for the OCR engine.

    Returns a two dimensional ``uint8`` array. RapidOCR converts a grayscale
    array to three channels itself, so no colour reconstruction is needed.
    """
    gray = _grayscale_array(pil_image)
    gray = invert_dark_background(gray)
    gray = upscale_for_ocr(gray)
    if is_bilevel_document(gray):
        return binarize_document(gray)
    return enhance_contrast(gray)


def has_probable_amount(text: str) -> bool:
    compact = "".join(text.split())
    return bool(
        AMOUNT_MARKER_PATTERN.search(compact) or DECIMAL_AMOUNT_PATTERN.search(compact)
    )


def recognition_text_score(text: str) -> tuple[int, int, int]:
    compact = "".join(text.split())
    signals = sum(1 for token in INVOICE_SIGNAL_TOKENS if token in compact)
    useful_characters = sum(1 for character in text if not character.isspace())
    return (1 if has_probable_amount(text) else 0, signals, useful_characters)


def better_recognition_text(primary: str, fallback: str) -> str:
    """Pick the richer of the preprocessed and the raw recognition result."""
    fallback_score = recognition_text_score(fallback)
    primary_score = recognition_text_score(primary)
    return fallback if fallback_score > primary_score else primary


def recognize_document_image(pil_image: Any, allow_raw_fallback: bool) -> str:
    """Recognise one page, retrying the raw pixels when no amount was read.

    The preprocessed page is the primary input. When it does not contain any
    amount at all the untouched page is recognised as well and the richer text
    wins, so a page whose small or noisy glyphs the preprocessing hurt can
    never end up with less text than before this change. PDF pages skip the
    fallback to keep multi page documents inside the caller's time budget.
    """
    primary = recognize_image(prepare_image(pil_image))
    if not allow_raw_fallback or has_probable_amount(primary):
        return primary
    fallback = recognize_image(_rgb_array(pil_image))
    return better_recognition_text(primary, fallback)


def recognize_pdf(path: Path) -> str:
    with contextlib.redirect_stdout(sys.stderr):
        import pypdfium2 as pdfium

    document = pdfium.PdfDocument(str(path))
    page_text: list[str] = []
    try:
        if len(document) > MAX_PDF_PAGES:
            raise ResourceLimitError

        total_pixels = 0
        for page_number in range(len(document)):
            page = document[page_number]
            try:
                width_points, height_points = page.get_size()
                width = math.ceil(width_points * PDF_RENDER_SCALE)
                height = math.ceil(height_points * PDF_RENDER_SCALE)
                validate_dimensions(width, height)
                total_pixels += width * height
                if total_pixels > MAX_TOTAL_PDF_RENDERED_PIXELS:
                    raise ResourceLimitError
            finally:
                page.close()
                page = None

        for page_number in range(len(document)):
            page = document[page_number]
            bitmap = None
            source_image = None
            pil_image = None
            try:
                bitmap = page.render(scale=PDF_RENDER_SCALE)
                source_image = bitmap.to_pil()
                pil_image = source_image.convert("RGB")
                page_text.append(
                    recognize_document_image(pil_image, allow_raw_fallback=False)
                )
            finally:
                if pil_image is not None:
                    pil_image.close()
                if source_image is not None:
                    source_image.close()
                if bitmap is not None:
                    bitmap.close()
                page.close()
                pil_image = None
                source_image = None
                bitmap = None
                page = None
    finally:
        document.close()
    return "\n".join(text for text in page_text if text)


def extract_pdf_text(path: Path) -> str:
    with contextlib.redirect_stdout(sys.stderr):
        import pypdfium2 as pdfium

    document = pdfium.PdfDocument(str(path))
    page_text: list[str] = []
    total_characters = 0
    try:
        if len(document) > MAX_PDF_PAGES:
            raise ResourceLimitError

        for page_number in range(len(document)):
            page = document[page_number]
            text_page = None
            try:
                text_page = page.get_textpage()
                character_count = text_page.count_chars()
                total_characters += character_count
                if total_characters > MAX_PDF_TEXT_CHARACTERS:
                    raise ResourceLimitError
                text = text_page.get_text_range(force_this=True)
                page_text.append(text)
            finally:
                if text_page is not None:
                    text_page.close()
                page.close()
                text_page = None
                page = None
    finally:
        document.close()
    return "\n".join(text for text in page_text if text)


def recognize_raster_image(path: Path) -> str:
    with contextlib.redirect_stdout(sys.stderr):
        from PIL import Image

    with Image.open(path) as source_image:
        validate_dimensions(*source_image.size)
        source_image.load()
        pil_image = source_image.convert("RGB")
        try:
            return recognize_document_image(pil_image, allow_raw_fallback=True)
        finally:
            pil_image.close()


def validate_dimensions(width: int, height: int) -> None:
    pixels = width * height
    if (
        width <= 0
        or height <= 0
        or width > MAX_IMAGE_DIMENSION
        or height > MAX_IMAGE_DIMENSION
        or pixels > MAX_IMAGE_PIXELS
    ):
        raise ResourceLimitError


def recognize_path(path_value: str) -> str:
    path = Path(path_value)
    if not path.is_file():
        raise InputFileMissingError
    if path.suffix.lower() == ".pdf":
        return recognize_pdf(path)
    return recognize_raster_image(path)


def error_response(message: str) -> dict[str, object]:
    return {"ok": False, "error": message}


def handle_line(line: str) -> dict[str, object]:
    try:
        request = json.loads(line)
    except (json.JSONDecodeError, TypeError):
        return error_response("Invalid request JSON.")

    if not isinstance(request, dict):
        return error_response("Request must be a JSON object.")
    path = request.get("path")
    if not isinstance(path, str) or not path.strip():
        return error_response("Request path must be a non-empty string.")
    operation = request.get("operation", "ocr")
    if operation not in ("ocr", "extract_pdf_text"):
        return error_response("Unsupported operation.")

    try:
        if operation == "extract_pdf_text":
            pdf_path = Path(path)
            if not pdf_path.is_file():
                raise InputFileMissingError
            text = extract_pdf_text(pdf_path)
        else:
            text = recognize_path(path)
    except InputFileMissingError:
        return error_response("Input file does not exist.")
    except ResourceLimitError:
        return error_response(RESOURCE_LIMIT_MESSAGE)
    except Exception:
        if operation == "extract_pdf_text":
            return error_response("Unable to extract PDF text.")
        return error_response("Unable to recognize input file.")
    return {"ok": True, "text": text, "warnings": []}


def main() -> None:
    protocol_stdout = sys.stdout
    for raw_line in sys.stdin:
        if not raw_line.strip():
            continue
        response = handle_line(raw_line)
        protocol_stdout.write(json.dumps(response, ensure_ascii=False) + "\n")
        protocol_stdout.flush()


if __name__ == "__main__":
    main()
