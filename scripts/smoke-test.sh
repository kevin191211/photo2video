#!/usr/bin/env bash
# 煙霧測試：用 test_images 以 CLI 模式實際轉出影片，驗證核心轉檔沒有回歸。
# 發版前可跑一次確認輸出的時長／解析度／格式正確。
#
# 需求：已建置的 release exe（會自動 cargo build --release）、dist/ffmpeg.exe
# 用法：bash scripts/smoke-test.sh
set -u

cd "$(dirname "$0")/.." || exit 1

EXE="./target/release/photo2video.exe"
FF="./dist/ffmpeg.exe"
IMAGES="test_images"
TMP="${TEMP:-/tmp}/p2v_smoke"
FAIL=0

[ -f "$FF" ] || { echo "✗ 找不到 $FF（驗證需要）"; exit 1; }
[ -d "$IMAGES" ] || { echo "✗ 找不到 $IMAGES"; exit 1; }

echo "→ 建置 release…"
# 注意：不能寫成 `cargo build … | tail -1 || …`——管線的退出碼來自 tail
# （永遠 0），build 真的失敗也偵測不到，會拿舊 exe 給出假通過。用
# PIPESTATUS 取 cargo 自己的退出碼。
cargo build --release 2>&1 | tail -1
[ "${PIPESTATUS[0]}" = 0 ] || { echo "✗ 建置失敗"; exit 1; }

mkdir -p "$TMP"
N=$(ls "$IMAGES" | wc -l | tr -d ' ')

# 取影片時長（秒，一位小數）
duration() { "$FF" -i "$1" 2>&1 | sed -n 's/.*Duration: 00:00:0*\([0-9.]*\),.*/\1/p'; }

# 一個案例：格式、fps、期望時長
case_run() {
  local label="$1" ext="$2" fps="$3" want="$4"
  local out="$TMP/smoke.$ext"
  rm -f "$out"
  "$EXE" --cli "$IMAGES" "$fps" "$out" >/dev/null 2>&1
  if [ ! -f "$out" ]; then echo "✗ $label：沒有產出檔案"; FAIL=1; return; fi
  local got; got=$(duration "$out")
  local res; res=$("$FF" -i "$out" 2>&1 | sed -n 's/.*, \([0-9]*x[0-9]*\).*/\1/p' | head -1)
  if [ "$got" = "$want" ] && [ "$res" = "1920x1080" ]; then
    echo "✓ $label：時長 ${got}s、解析度 $res"
  else
    echo "✗ $label：時長 ${got}s（期望 ${want}s）、解析度 $res（期望 1920x1080）"; FAIL=1
  fi
  rm -f "$out"
}

# 錯誤案例：以指定引數執行，期望非 0 退出碼且錯誤訊息含某片段。
# 用法：error_case "標題" "訊息片段" <cli 引數…>
error_case() {
  local label="$1" want="$2"; shift 2
  local out; out=$("$EXE" --cli "$@" 2>&1); local code=$?
  if [ "$code" != 0 ] && printf '%s' "$out" | grep -q "$want"; then
    echo "✓ $label：正確拒絕（含「$want」）"
  else
    echo "✗ $label：退出碼 $code、輸出「$out」（期望非 0 且含「$want」）"; FAIL=1
  fi
}

# 各容器格式，N 張、fps → 時長 = N/fps
case_run "MP4 (H.264) fps=2" mp4  2 "$(awk "BEGIN{printf \"%.2f\", $N/2}")"
case_run "MKV (H.264) fps=2" mkv  2 "$(awk "BEGIN{printf \"%.2f\", $N/2}")"
case_run "MOV (H.264) fps=2" mov  2 "$(awk "BEGIN{printf \"%.2f\", $N/2}")"
case_run "AVI (H.264) fps=2" avi  2 "$(awk "BEGIN{printf \"%.2f\", $N/2}")"
case_run "WebM (VP9)  fps=3" webm 3 "$(awk "BEGIN{printf \"%.2f\", $N/3}")"
# 極端 fps 邊界
case_run "MP4 fps=1（慢）" mp4 1 "$(awk "BEGIN{printf \"%.2f\", $N/1}")"

# 副檔名處理：輸出無副檔名時應自動補成所選格式（預設 mp4）
ext_out="$TMP/smoke_noext"
rm -f "$ext_out" "$ext_out.mp4"
"$EXE" --cli "$IMAGES" 2 "$ext_out" >/dev/null 2>&1
if [ -f "$ext_out.mp4" ]; then
  echo "✓ 無副檔名輸出：自動補成 .mp4"
else
  echo "✗ 無副檔名輸出：預期補成 .mp4，未產出"; FAIL=1
fi
rm -f "$ext_out" "$ext_out.mp4"

# 錯誤路徑：確認各種不合法輸入都被明確拒絕（非 0 退出碼＋易懂訊息）
mkdir -p "$TMP/empty"
error_case "空資料夾"   "沒有圖片"        "$TMP/empty" 2 "$TMP/x.mp4"
error_case "資料夾不存在" "找不到資料夾"    "$TMP/nope_xyz" 2 "$TMP/x.mp4"
error_case "fps=0"      "介於 1 到 60"    "$IMAGES" 0 "$TMP/x.mp4"
error_case "fps 非數字"  "必須是正整數"    "$IMAGES" abc "$TMP/x.mp4"
rmdir "$TMP/empty" 2>/dev/null; rm -f "$TMP/x.mp4"

rmdir "$TMP" 2>/dev/null
if [ "$FAIL" = 0 ]; then echo "全部通過。"; else echo "有案例失敗。"; exit 1; fi
