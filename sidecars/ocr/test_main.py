import json
import re
import subprocess
import sys
from pathlib import Path


WORKER = Path(__file__).with_name("main.py")
REPO_ROOT = Path(__file__).resolve().parents[2]
FIXTURES = REPO_ROOT / "src-tauri" / "tests" / "fixtures"
IMAGE_FIXTURE = FIXTURES / "image-invoice.png"
PDF_FIXTURE = FIXTURES / "text-invoice.pdf"


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
