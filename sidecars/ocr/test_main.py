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
from PIL import Image


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
    monkeypatch.setattr(worker, "recognize_image", lambda image: calls.append(image) or "ok")

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


def test_image_ocr_receives_a_checked_numpy_array(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import numpy as np

    path = tmp_path / "small.png"
    Image.new("RGBA", (3, 2), (10, 20, 30, 40)).save(path)
    received = []
    monkeypatch.setattr(worker, "recognize_image", lambda image: received.append(image) or "ok")

    text = worker.recognize_path(str(path))

    assert text == "ok"
    assert len(received) == 1
    assert isinstance(received[0], np.ndarray)
    assert received[0].shape == (2, 3, 3)


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
