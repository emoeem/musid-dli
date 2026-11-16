#!/usr/bin/env bash
# musid-dli full-provider workflow runner.
# Runs every provider smoke test in sequence; pass provider names to filter
# (e.g. ./run_all_smokes.sh netease kugou).
# QR auth modules poll up to 180s: have the phone ready for those runs.
set -u
cd "$(dirname "$0")/.."

declare -a STEPS=(
  "netease|netease_smoke|search track stream lyrics playlist chart album artist link auth user"
  "qqmusic|qqmusic_smoke|search track stream lyrics playlist chart album link auth wechat user"
  "kugou|kugou_smoke|search lyrics playlist chart artist stream auth user"
  "kuwo|kuwo_smoke|search lyrics stream"
  "migu|migu_smoke|search lyrics stream playlist"
  "bilibili|bilibili_smoke|search reference stream auth favorites"
  "jamendo|jamendo_smoke|search stream playlist album link"
  "apple|apple_smoke|search stream lyrics album playlist link"
  "fivesing|fivesing_smoke|search stream lyrics playlist link"
)

FAILED=()

for step in "${STEPS[@]}"; do
  IFS='|' read -r feature example modules <<< "$step"
  if [[ $# -gt 0 ]]; then
    match=false
    for wanted in "$@"; do
      [[ "$wanted" == "$feature" ]] && match=true && break
    done
    $match || continue
  fi
  echo "=============================================="
  echo "== $feature ($example)"
  echo "=============================================="
  if cargo run --features "source-$feature" --example "$example" $modules; then
    echo "== $feature: ALL REQUESTED MODULES PASSED"
  else
    echo "== $feature: FAILED (see above)"
    FAILED+=("$feature")
  fi
done

echo "=============================================="
if [[ ${#FAILED[@]} -eq 0 ]]; then
  echo "WORKFLOW RESULT: all requested providers passed"
else
  echo "WORKFLOW RESULT: failed providers: ${FAILED[*]}"
  echo "(Known gates: qqmusic stream code=10006, kugou stream login-required,"
  echo " bilibili keyword search v_voucher — see docs/IMPLEMENTATION_STATUS.md.)"
  exit 1
fi
