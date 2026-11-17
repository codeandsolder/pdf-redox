#!/usr/bin/env bash
set -euo pipefail
export PYTHONDONTWRITEBYTECODE=1
if (( $# < 2 )); then
  printf 'Usage: %s SOURCE.pdf OUTPUT.pdf [--pages 1,8,10] [--min-confidence 0.97] [--max-side 1600] [--ocr-threads 6] [--raster-mode auto|all|off] [--ocr-only] [--keep-native-text]\n' "$0" >&2
  exit 2
fi
SOURCE=$(realpath -- "$1")
OUTPUT=$2
shift 2
PAGES=''
CONFIDENCE=0.97
SIDE=1600
THREADS=''
KEEP_NATIVE=0
OCR_ONLY=0
RASTER_MODE=auto
while (( $# )); do
  case "$1" in
    --pages) PAGES=$2; shift 2;;
    --min-confidence) CONFIDENCE=$2; shift 2;;
    --max-side) SIDE=$2; shift 2;;
    --ocr-threads) THREADS=$2; shift 2;;
    --raster-mode) RASTER_MODE=$2; shift 2;;
    --keep-native-text) KEEP_NATIVE=1; shift;;
    --ocr-only) OCR_ONLY=1; shift;;
    *) printf 'Unknown option: %s\n' "$1" >&2; exit 2;;
  esac
done
if [[ "$SOURCE" == "$(realpath -m -- "$OUTPUT")" ]]; then
  printf 'Output must not overwrite the source PDF\n' >&2; exit 2
fi
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
REPO_ROOT=$(dirname -- "$SCRIPT_DIR")
BASE="$REPO_ROOT/target/release"
ADAPTER="$SCRIPT_DIR/raster_hybrid.py"
SCRATCH=$(mktemp -d -p /home/jan/benchmarks pdf-redox-ocr-XXXXXXXX)
trap 'rm -rf -- "$SCRATCH"' EXIT
INPUT_PDF="$SCRATCH/optimized.pdf"
if [[ "$OCR_ONLY" == 1 ]]; then
  INPUT_PDF="$SOURCE"
else
  "$BASE/pdf-redox" "$SOURCE" --optimize-for processing -o "$INPUT_PDF"
fi
set -- --pdf "$INPUT_PDF" \
  --output "$SCRATCH/draft.json" --max-side "$SIDE" --work-dir "$SCRATCH" --raster-mode "$RASTER_MODE"
if [[ -n "$PAGES" ]]; then set -- "$@" --pages "$PAGES"; fi
if [[ -n "$THREADS" ]]; then set -- "$@" --ocr-threads "$THREADS"; fi
if [[ "$KEEP_NATIVE" == 1 ]]; then set -- "$@" --keep-native-text; fi
uv run --python 3.12 "$ADAPTER" "$@"
mkdir -p -- "$(dirname -- "$OUTPUT")"
"$BASE/pdf-redox-ocr-overlay" "$INPUT_PDF" \
  --ocr "$SCRATCH/draft.json" \
  --apply-unreviewed --min-confidence "$CONFIDENCE" -o "$OUTPUT"
