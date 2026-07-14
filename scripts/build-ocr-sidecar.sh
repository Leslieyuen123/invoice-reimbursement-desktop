#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
PROJECT_DIR="${REPO_ROOT}/sidecars/ocr"
BINARY_DIR="${REPO_ROOT}/src-tauri/binaries"
HOST_TRIPLE="$(rustc -Vv | sed -n 's/^host: //p')"

if [[ -z "${HOST_TRIPLE}" ]]; then
  echo "Unable to determine the Rust host triple." >&2
  exit 1
fi

case "${HOST_TRIPLE}" in
  *-windows-*)
    SOURCE_NAME="invoice-ocr.exe"
    DESTINATION_NAME="invoice-ocr-${HOST_TRIPLE}.exe"
    ;;
  *)
    SOURCE_NAME="invoice-ocr"
    DESTINATION_NAME="invoice-ocr-${HOST_TRIPLE}"
    ;;
esac

rm -rf "${PROJECT_DIR}/build" "${PROJECT_DIR}/dist"
(
  cd "${PROJECT_DIR}"
  uv run --project . pyinstaller --clean --noconfirm invoice-ocr.spec
)

mkdir -p "${BINARY_DIR}"
cp "${PROJECT_DIR}/dist/${SOURCE_NAME}" "${BINARY_DIR}/${DESTINATION_NAME}"
chmod +x "${BINARY_DIR}/${DESTINATION_NAME}"
echo "Built ${BINARY_DIR}/${DESTINATION_NAME}"
