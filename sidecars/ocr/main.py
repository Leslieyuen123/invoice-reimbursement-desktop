import contextlib
import json
import sys
from pathlib import Path
from typing import Any


PDF_RENDER_SCALE = 200 / 72
_engine: Any | None = None


class InputFileMissingError(Exception):
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


def recognize_pdf(path: Path) -> str:
    with contextlib.redirect_stdout(sys.stderr):
        import numpy as np
        import pypdfium2 as pdfium

    document = pdfium.PdfDocument(str(path))
    page_text: list[str] = []
    try:
        for page_number in range(len(document)):
            page = document[page_number]
            bitmap = None
            source_image = None
            pil_image = None
            try:
                bitmap = page.render(scale=PDF_RENDER_SCALE)
                source_image = bitmap.to_pil()
                pil_image = source_image.convert("RGB")
                pixels = np.array(pil_image, copy=True)
                page_text.append(recognize_image(pixels))
            finally:
                if pil_image is not None:
                    pil_image.close()
                if source_image is not None:
                    source_image.close()
                if bitmap is not None:
                    bitmap.close()
                page.close()
    finally:
        document.close()
    return "\n".join(text for text in page_text if text)


def recognize_path(path_value: str) -> str:
    path = Path(path_value)
    if not path.is_file():
        raise InputFileMissingError
    if path.suffix.lower() == ".pdf":
        return recognize_pdf(path)
    return recognize_image(str(path))


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

    try:
        text = recognize_path(path)
    except InputFileMissingError:
        return error_response("Input file does not exist.")
    except Exception:
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
