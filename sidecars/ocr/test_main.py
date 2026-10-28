import importlib.util
import json
import os
import re
import struct
import subprocess
import sys
import types
import zlib
from pathlib import Path

import pytest
from PIL import Image, ImageDraw


WORKER = Path(__file__).with_name("main.py")
REPO_ROOT = Path(__file__).resolve().parents[2]
FIXTURES = REPO_ROOT / "src-tauri" / "tests" / "fixtures"
IMAGE_FIXTURE = FIXTURES / "image-invoice.png"
PDF_FIXTURE = FIXTURES / "text-invoice.pdf"
BUILD_SCRIPT = REPO_ROOT / "scripts" / "build-ocr-sidecar.sh"

WORKER_SPEC = importlib.util.spec_from_file_location("invoice_ocr_worker", WORKER)
assert WORKER_SPEC is not None and WORKER_SPEC.loader is not None
worker = importlib.util.module_from_spec(WORKER_SPEC)
WORKER_SPEC.loader.exec_module(worker)


def run_worker(requests: list[str], timeout: int = 120) -> tuple[list[dict], str]:
    completed = subprocess.run(
        [sys.executable, str(WORKER)],
        input="\n".join(requests) + "\n",
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )
    assert completed.returncode == 0, completed.stderr
    output_lines = [line for line in completed.stdout.splitlines() if line.strip()]
    assert len(output_lines) == len([line for line in requests if line.strip()]), (
        completed.stdout,
        completed.stderr,
    )
    return [json.loads(line) for line in output_lines], completed.stderr


def normalized(text: str) -> str:
    return re.sub(r"[\s,，:：。￥¥]", "", text)


def assert_fixture_text(text: str) -> None:
    compact = normalized(text)
    assert "北京" in compact, text
    assert "128.50" in compact, text


def test_recognizes_two_images_in_one_worker_process() -> None:
    request = json.dumps({"path": str(IMAGE_FIXTURE)}, ensure_ascii=False)

    responses, _ = run_worker([request, request])

    assert len(responses) == 2
    for response in responses:
        assert response["ok"] is True
        assert response["warnings"] == []
        assert_fixture_text(response["text"])


def test_malformed_request_does_not_stop_next_valid_request() -> None:
    valid = json.dumps({"path": str(IMAGE_FIXTURE)}, ensure_ascii=False)

    responses, stderr = run_worker(["{not-json", valid])

    assert responses[0]["ok"] is False
    assert "traceback" not in responses[0]["error"].lower()
    assert responses[1]["ok"] is True
    assert_fixture_text(responses[1]["text"])
    assert "Traceback" not in stderr


def test_request_errors_are_sanitized_and_worker_continues(tmp_path: Path) -> None:
    secret_directory = tmp_path / "customer-private-secret"
    secret_directory.mkdir()
    corrupt = secret_directory / "broken-invoice.png"
    corrupt.write_bytes(b"not an image")
    missing = secret_directory / "missing-invoice.png"
    valid = json.dumps({"path": str(IMAGE_FIXTURE)}, ensure_ascii=False)

    responses, stderr = run_worker(
        [
            json.dumps({}),
            json.dumps({"path": str(missing)}),
            json.dumps({"path": str(corrupt)}),
            valid,
        ]
    )

    assert [response["ok"] for response in responses] == [False, False, False, True]
    serialized_errors = json.dumps(responses[:3], ensure_ascii=False)
    assert "customer-private-secret" not in serialized_errors
    assert str(tmp_path) not in serialized_errors
    assert "Traceback" not in stderr
    assert_fixture_text(responses[-1]["text"])


def test_pdf_is_rendered_and_recognized() -> None:
    request = json.dumps({"path": str(PDF_FIXTURE)}, ensure_ascii=False)

    responses, _ = run_worker([request])

    assert responses[0]["ok"] is True
    assert responses[0]["warnings"] == []
    assert responses[0]["text"].strip()


def test_pdf_text_operation_extracts_embedded_text() -> None:
    request = json.dumps(
        {"operation": "extract_pdf_text", "path": str(PDF_FIXTURE)},
        ensure_ascii=False,
    )

    responses, _ = run_worker([request])

    assert responses[0]["ok"] is True
    assert responses[0]["warnings"] == []
    assert "开票日期：2026年06月18日" in responses[0]["text"]
    assert "价税合计（小写）¥128.50" in responses[0]["text"]


def test_pdf_text_budget_is_checked_before_allocating_text(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    text_page = FakePdfTextPage(worker.MAX_PDF_TEXT_CHARACTERS + 1)
    page = FakePdfPage((10.0, 10.0), text_page=text_page)
    document = FakePdfDocument([page])
    install_fake_pdfium(monkeypatch, document)

    with pytest.raises(worker.ResourceLimitError):
        worker.extract_pdf_text(Path("compressed-text-bomb.pdf"))

    assert not text_page.range_requested
    assert text_page.closed
    assert page.closed
    assert document.closed


def test_pdf_text_page_count_is_rejected_before_any_page_is_opened(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    pages = [FakePdfPage((10.0, 10.0)) for _ in range(101)]
    document = FakePdfDocument(pages)
    install_fake_pdfium(monkeypatch, document)

    with pytest.raises(worker.ResourceLimitError):
        worker.extract_pdf_text(Path("too-many-pages.pdf"))

    assert not any(page.opened for page in pages)
    assert document.closed


def test_pdf_text_resource_error_does_not_stop_next_request(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    first = tmp_path / "too-large.pdf"
    second = tmp_path / "valid.pdf"
    first.write_bytes(b"placeholder")
    second.write_bytes(b"placeholder")

    def fake_extract(path: Path) -> str:
        if path == first:
            raise worker.ResourceLimitError
        return "开票日期：2026年06月18日"

    monkeypatch.setattr(worker, "extract_pdf_text", fake_extract, raising=False)
    responses = [
        worker.handle_line(
            json.dumps({"operation": "extract_pdf_text", "path": str(path)})
        )
        for path in [first, second]
    ]

    assert responses[0] == {
        "ok": False,
        "error": "document exceeds OCR resource limits",
    }
    assert responses[1] == {
        "ok": True,
        "text": "开票日期：2026年06月18日",
        "warnings": [],
    }


def test_unknown_operation_is_rejected_without_touching_the_file(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        worker,
        "recognize_path",
        lambda _path: pytest.fail("unknown operation must not read the file"),
    )

    response = worker.handle_line(
        json.dumps({"operation": "delete_everything", "path": "/private/invoice.pdf"})
    )

    assert response == {"ok": False, "error": "Unsupported operation."}


def test_build_script_runs_locked_offline_and_copies_the_host_artifact(
    tmp_path: Path,
) -> None:
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    uv_arguments = tmp_path / "uv-arguments"
    host_triple = "aarch64-test-darwin"
    destination = (
        REPO_ROOT / "src-tauri" / "binaries" / f"invoice-ocr-{host_triple}"
    )
    write_executable(
        fake_bin / "rustc",
        f'#!/bin/sh\nprintf "host: {host_triple}\\n"\n',
    )
    write_executable(
        fake_bin / "uv",
        "#!/bin/sh\n"
        'printf "%s\\n" "$@" > "$FAKE_UV_ARGUMENTS"\n'
        "mkdir -p dist\n"
        "printf packaged-sidecar > dist/invoice-ocr\n",
    )
    environment = os.environ.copy()
    environment.update(
        {
            "FAKE_UV_ARGUMENTS": str(uv_arguments),
            "OFFLINE": "1",
            "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
        }
    )

    try:
        completed = subprocess.run(
            ["bash", str(BUILD_SCRIPT)],
            cwd=REPO_ROOT,
            env=environment,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )

        assert completed.returncode == 0, completed.stderr
        assert uv_arguments.read_text().splitlines() == [
            "run",
            "--locked",
            "--offline",
            "--project",
            ".",
            "pyinstaller",
            "--clean",
            "--noconfirm",
            "invoice-ocr.spec",
        ]
        assert destination.read_bytes() == b"packaged-sidecar"
        assert destination.stat().st_mode & 0o111
    finally:
        destination.unlink(missing_ok=True)


def test_oversized_image_headers_are_rejected_before_load_and_worker_continues(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    too_wide = tmp_path / "too-wide.png"
    too_wide.write_bytes(png_header(20_001, 1))
    too_many_pixels = tmp_path / "too-many-pixels.png"
    too_many_pixels.write_bytes(png_header(10_000, 4_001))
    valid = tmp_path / "valid.png"
    Image.new("RGB", (2, 2), "white").save(valid)
    calls = []
    monkeypatch.setattr(
        worker,
        "recognize_image",
        lambda image: calls.append(image) or "价税合计 ¥128.50",
    )

    responses = [
        worker.handle_line(json.dumps({"path": str(path)}))
        for path in [too_wide, too_many_pixels, valid]
    ]

    assert responses[0] == {
        "ok": False,
        "error": "document exceeds OCR resource limits",
    }
    assert responses[1] == responses[0]
    assert responses[2]["ok"] is True
    assert len(calls) == 1


def test_image_ocr_receives_the_prepared_grayscale_array(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    path = tmp_path / "small.png"
    Image.new("RGBA", (3, 2), (10, 20, 30, 40)).save(path)
    received = []
    monkeypatch.setattr(
        worker,
        "recognize_image",
        lambda image: received.append(image) or "价税合计 ¥128.50",
    )

    text = worker.recognize_path(str(path))

    assert text == "价税合计 ¥128.50"
    assert len(received) == 1
    assert received[0].ndim == 2
    assert received[0].dtype.name == "uint8"
    # A 3x2 page is below the upscale threshold and is capped at 2x.
    assert received[0].shape == (4, 6)


def test_pdf_page_count_is_rejected_before_any_page_is_opened(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    document = FakePdfDocument([FakePdfPage((10.0, 10.0)) for _ in range(101)])
    install_fake_pdfium(monkeypatch, document)

    with pytest.raises(worker.ResourceLimitError):
        worker.recognize_pdf(Path("too-many-pages.pdf"))

    assert not any(page.opened for page in document.pages)
    assert document.closed


@pytest.mark.parametrize(
    "page_size",
    [
        (7_201.0, 10.0),
        (2_520.0, 2_520.0),
    ],
)
def test_pdf_page_render_budget_is_checked_before_render(
    monkeypatch: pytest.MonkeyPatch, page_size: tuple[float, float]
) -> None:
    page = FakePdfPage(page_size)
    document = FakePdfDocument([page])
    install_fake_pdfium(monkeypatch, document)

    with pytest.raises(worker.ResourceLimitError):
        worker.recognize_pdf(Path("oversized-page.pdf"))

    assert not page.rendered
    assert page.closed
    assert document.closed


def test_pdf_total_rendered_pixels_are_checked_before_any_render(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    six_thousand_pixels_in_points = 6_000 / worker.PDF_RENDER_SCALE
    pages = [
        FakePdfPage((six_thousand_pixels_in_points, six_thousand_pixels_in_points))
        for _ in range(3)
    ]
    document = FakePdfDocument(pages)
    install_fake_pdfium(monkeypatch, document)

    with pytest.raises(worker.ResourceLimitError):
        worker.recognize_pdf(Path("too-many-total-pixels.pdf"))

    assert not any(page.rendered for page in pages)
    assert all(page.closed for page in pages)
    assert document.closed


def test_preprocessing_upscales_small_pages_with_lanczos_grayscale(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    prepared = worker.prepare_image(Image.new("RGB", (900, 600), "white"))

    assert prepared.ndim == 2
    assert prepared.dtype.name == "uint8"
    # Short side 600 is below 1200 and the 2x cap keeps the page at 1200.
    assert prepared.shape == (1200, 1800)


def test_preprocessing_keeps_large_pages_at_their_own_resolution() -> None:
    prepared = worker.prepare_image(Image.new("RGB", (2_000, 1_400), "white"))

    assert prepared.shape == (1_400, 2_000)


def test_upscale_factor_stays_inside_the_document_budget() -> None:
    assert worker.upscale_factor(1_000, 1_000) == pytest.approx(1.6)
    assert worker.upscale_factor(2_000, 1_400) == 1.0
    # A page that is already taller than the dimension budget must not grow.
    assert worker.upscale_factor(700, 30_000) == 1.0
    factor = worker.upscale_factor(1_100, 19_900)
    assert 1_100 * factor <= worker.MAX_IMAGE_DIMENSION
    assert 19_900 * factor <= worker.MAX_IMAGE_DIMENSION
    assert (1_100 * factor) * (19_900 * factor) <= worker.MAX_IMAGE_PIXELS


def test_adaptive_binarization_applies_to_clean_documents_only() -> None:
    import numpy as np

    clean = Image.new("L", (400, 200), 255)
    draw = ImageDraw.Draw(clean)
    for top in range(20, 180, 20):
        draw.rectangle((20, top, 380, top + 6), fill=0)
    gradient = Image.frombytes(
        "L", (400, 200), bytes(x % 256 for _ in range(200) for x in range(400))
    )

    clean_page = worker.prepare_image(clean.convert("RGB"))
    photo_page = worker.prepare_image(gradient.convert("RGB"))

    assert set(np.unique(clean_page).tolist()) <= {0, 255}
    assert len(np.unique(photo_page)) > 16


def test_dark_screenshots_are_inverted_before_recognition() -> None:
    dark = Image.new("L", (400, 200), 20)
    draw = ImageDraw.Draw(dark)
    draw.rectangle((40, 60, 360, 90), fill=235)

    assert worker.invert_dark_background(worker._grayscale_array(dark)).mean() > 200
    assert worker.prepare_image(dark.convert("RGB")).mean() > 127


def test_light_screenshots_are_not_inverted() -> None:
    light = Image.new("L", (400, 200), 240)

    gray = worker._grayscale_array(light)
    assert worker.invert_dark_background(gray) is gray


def test_raster_falls_back_to_raw_pixels_when_no_amount_was_read(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    path = tmp_path / "invoice.png"
    Image.new("RGB", (900, 600), "white").save(path)
    calls = []

    def fake_recognize(image):
        calls.append(image.ndim)
        if image.ndim == 2:
            return "北京 发票号码 202607140001"
        return "价税合计（小写）￥648.00"

    monkeypatch.setattr(worker, "recognize_image", fake_recognize)

    assert worker.recognize_path(str(path)) == "价税合计（小写）￥648.00"
    assert calls == [2, 3]


def test_raster_skips_the_raw_fallback_when_the_prepared_text_has_an_amount(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    path = tmp_path / "invoice.png"
    Image.new("RGB", (900, 600), "white").save(path)
    calls = []

    def fake_recognize(image):
        calls.append(image.ndim)
        return "北京 价税合计 648.00"

    monkeypatch.setattr(worker, "recognize_image", fake_recognize)

    assert worker.recognize_path(str(path)) == "北京 价税合计 648.00"
    assert calls == [2]


def test_better_recognition_text_keeps_the_richer_page() -> None:
    assert (
        worker.better_recognition_text(
            "发票号码 202607140001\n北京", "价税合计（小写）￥648.00"
        )
        == "价税合计（小写）￥648.00"
    )
    assert (
        worker.better_recognition_text(
            "价税合计（小写）￥648.00", "发票号码 202607140001"
        )
        == "价税合计（小写）￥648.00"
    )
    assert (
        worker.better_recognition_text("北京 合计 1.00", "北京 发票号码")
        == "北京 合计 1.00"
    )


def test_probable_amount_rejects_rates_and_counts() -> None:
    assert worker.has_probable_amount("价税合计（小写） 648.00")
    assert worker.has_probable_amount("￥ 1,648.00")
    assert worker.has_probable_amount("RMB 648.00")
    assert not worker.has_probable_amount("税率 1% 数量 18")


def test_pdf_pages_use_only_the_prepared_pass(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    page = RenderingFakePdfPage(Image.new("RGB", (900, 600), "white"))
    document = FakePdfDocument([page])
    install_fake_pdfium(monkeypatch, document)
    calls = []
    monkeypatch.setattr(
        worker,
        "recognize_image",
        lambda image: calls.append(image.ndim) or "北京 发票号码",
    )

    assert worker.recognize_pdf(Path("one-page.pdf")) == "北京 发票号码"
    assert calls == [2]
    assert page.closed


def png_header(width: int, height: int) -> bytes:
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + png_chunk(b"IHDR", ihdr) + png_chunk(
        b"IDAT", zlib.compress(b"")
    ) + png_chunk(b"IEND", b"")


def write_executable(path: Path, contents: str) -> None:
    path.write_text(contents)
    path.chmod(0o755)


def png_chunk(kind: bytes, data: bytes) -> bytes:
    return (
        struct.pack(">I", len(data))
        + kind
        + data
        + struct.pack(">I", zlib.crc32(kind + data))
    )


class FakePdfTextPage:
    def __init__(self, character_count: int, text: str = "") -> None:
        self.character_count = character_count
        self.text = text
        self.range_requested = False
        self.closed = False

    def count_chars(self) -> int:
        return self.character_count

    def get_text_range(self, **_kwargs) -> str:
        self.range_requested = True
        return self.text

    def close(self) -> None:
        self.closed = True


class FakePdfPage:
    def __init__(
        self,
        size: tuple[float, float],
        text_page: FakePdfTextPage | None = None,
    ) -> None:
        self.size = size
        self.text_page = text_page
        self.opened = False
        self.rendered = False
        self.closed = False

    def get_size(self) -> tuple[float, float]:
        self.opened = True
        return self.size

    def render(self, **_kwargs):
        self.rendered = True
        raise AssertionError("render must not run for a rejected PDF")

    def get_textpage(self) -> FakePdfTextPage:
        self.opened = True
        assert self.text_page is not None
        return self.text_page

    def close(self) -> None:
        self.closed = True


class RenderingFakePdfPage:
    def __init__(self, image: Image.Image) -> None:
        self.image = image
        self.closed = False

    def get_size(self) -> tuple[float, float]:
        return (
            self.image.width / worker.PDF_RENDER_SCALE,
            self.image.height / worker.PDF_RENDER_SCALE,
        )

    def render(self, **_kwargs) -> "FakeBitmap":
        return FakeBitmap(self.image)

    def close(self) -> None:
        self.closed = True


class FakeBitmap:
    def __init__(self, image: Image.Image) -> None:
        self.image = image
        self.closed = False

    def to_pil(self) -> Image.Image:
        return self.image.copy()

    def close(self) -> None:
        self.closed = True


class FakePdfDocument:
    def __init__(self, pages: list[FakePdfPage]) -> None:
        self.pages = pages
        self.closed = False

    def __len__(self) -> int:
        return len(self.pages)

    def __getitem__(self, index: int) -> FakePdfPage:
        return self.pages[index]

    def close(self) -> None:
        self.closed = True


def install_fake_pdfium(
    monkeypatch: pytest.MonkeyPatch, document: FakePdfDocument
) -> None:
    module = types.SimpleNamespace(PdfDocument=lambda _path: document)
    monkeypatch.setitem(sys.modules, "pypdfium2", module)
