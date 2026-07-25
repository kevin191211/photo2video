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

# 混合尺寸縮放補邊：橫向／直向／小圖／4K 都應等比例縮放置中、補黑邊成 1920x1080。
# （其餘案例都用同尺寸的 test_images，這條 scale/pad 路徑靠這裡覆蓋。）
MIX="$TMP/mixed"
rm -rf "$MIX"; mkdir -p "$MIX"
"$FF" -f lavfi -i color=red:s=1920x1080:d=1    -frames:v 1 -y "$MIX/1_landscape.png" >/dev/null 2>&1
"$FF" -f lavfi -i color=blue:s=1080x1920:d=1   -frames:v 1 -y "$MIX/2_portrait.png"  >/dev/null 2>&1
"$FF" -f lavfi -i color=green:s=400x300:d=1    -frames:v 1 -y "$MIX/3_small.png"      >/dev/null 2>&1
"$FF" -f lavfi -i color=yellow:s=3840x2160:d=1 -frames:v 1 -y "$MIX/4_4k.png"         >/dev/null 2>&1
mix_out="$TMP/mixed.mp4"; rm -f "$mix_out"
"$EXE" --cli "$MIX" 2 "$mix_out" >/dev/null 2>&1
mix_res=$("$FF" -i "$mix_out" 2>&1 | sed -n 's/.*, \([0-9]*x[0-9]*\).*/\1/p' | head -1)
mix_dur=$(duration "$mix_out")
if [ "$mix_res" = "1920x1080" ] && [ "$mix_dur" = "2.00" ]; then
  echo "✓ 混合尺寸（橫/直/小/4K）：都輸出 1920x1080、時長 ${mix_dur}s"
else
  echo "✗ 混合尺寸：解析度 $mix_res（期望 1920x1080）、時長 ${mix_dur}s（期望 2.00）"; FAIL=1
fi
rm -rf "$MIX" "$mix_out"

# 特殊字元路徑：資料夾與檔名含單引號、空格、中文（都是 Windows 合法檔名）。
# concat 清單以單引號包住每個路徑，escape 沒處理好單引號會讓整條清單解析失敗、
# 轉檔中止。這條路徑（concat_escape）先前無自動化覆蓋。
SPDIR="$TMP/引號 don't test"
rm -rf "$SPDIR"; mkdir -p "$SPDIR"
"$FF" -f lavfi -i color=red:s=640x480:d=1   -frames:v 1 -y "$SPDIR/don't 1.png"       >/dev/null 2>&1
"$FF" -f lavfi -i color=blue:s=640x480:d=1  -frames:v 1 -y "$SPDIR/我的 2.png"         >/dev/null 2>&1
"$FF" -f lavfi -i color=green:s=640x480:d=1 -frames:v 1 -y "$SPDIR/it's a test 10.png" >/dev/null 2>&1
sp_out="$TMP/引號 out.mp4"; rm -f "$sp_out"
"$EXE" --cli "$SPDIR" 2 "$sp_out" >/dev/null 2>&1
sp_res=$("$FF" -i "$sp_out" 2>&1 | sed -n 's/.*, \([0-9]*x[0-9]*\).*/\1/p' | head -1)
sp_dur=$(duration "$sp_out")
if [ "$sp_res" = "1920x1080" ] && [ "$sp_dur" = "1.50" ]; then
  echo "✓ 特殊字元路徑（單引號/空格/中文）：時長 ${sp_dur}s、解析度 $sp_res"
else
  echo "✗ 特殊字元路徑：時長 ${sp_dur}s（期望 1.50）、解析度 $sp_res（期望 1920x1080）"; FAIL=1
fi
rm -rf "$SPDIR" "$sp_out"

# EXIF 方向：手機直拍照片常以「未旋轉像素＋方向標記」儲存，轉檔必須依 EXIF
# 自動轉正（與縮圖、預覽、原始像素解析度的判斷一致），否則直拍照片會躺著輸出。
# 需 python3 + Pillow 產生帶 EXIF orientation 的測試圖；沒有就略過這個案例。
if python -c "import PIL" >/dev/null 2>&1; then
  EXIFDIR="$TMP/exif"; rm -rf "$EXIFDIR"; mkdir -p "$EXIFDIR"
  python - "$EXIFDIR" <<'PY'
from PIL import Image
import sys
d = sys.argv[1]
# 800x200 明顯橫向、亮灰（好與黑邊區分）；orientation=6 表示顯示時要轉成 200x800 直向
img = Image.new("RGB", (800, 200), (200, 200, 200))
ex = img.getexif(); ex[0x0112] = 6
img.save(f"{d}/wide.jpg", exif=ex, quality=95)
PY
  exif_out="$TMP/exif.mp4"; rm -f "$exif_out"
  "$EXE" --cli "$EXIFDIR" 2 "$exif_out" >/dev/null 2>&1
  exif_frame="$TMP/exif_frame.png"; rm -f "$exif_frame"
  "$FF" -i "$exif_out" -frames:v 1 -y "$exif_frame" >/dev/null 2>&1
  # 量出畫面中非黑內容的邊界框：已轉正→直向（高>寬），沒轉正→橫向（寬>高）
  verdict=$(python - "$exif_frame" <<'PY'
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert("RGB"); W, H = im.size; px = im.load()
xs = []; ys = []
for y in range(0, H, 4):
    for x in range(0, W, 4):
        r, g, b = px[x, y]
        if r + g + b > 150:
            xs.append(x); ys.append(y)
print("portrait" if xs and (max(ys) - min(ys)) > (max(xs) - min(xs)) else "landscape")
PY
)
  if [ "$verdict" = portrait ]; then
    echo "✓ EXIF 方向：orientation=6 直拍照片自動轉正為直向輸出"
  else
    echo "✗ EXIF 方向：未依 EXIF 轉正（輸出為 $verdict），直拍照片會躺著"; FAIL=1
  fi
  rm -rf "$EXIFDIR" "$exif_out" "$exif_frame"
else
  echo "↷ 略過 EXIF 方向測試（環境未安裝 python3 + Pillow）"
fi

# 錯誤路徑：確認各種不合法輸入都被明確拒絕（非 0 退出碼＋易懂訊息）
mkdir -p "$TMP/empty"
error_case "空資料夾"   "沒有圖片"        "$TMP/empty" 2 "$TMP/x.mp4"
error_case "資料夾不存在" "找不到資料夾"    "$TMP/nope_xyz" 2 "$TMP/x.mp4"
error_case "fps=0"      "介於 1 到 60"    "$IMAGES" 0 "$TMP/x.mp4"
error_case "fps 非數字"  "必須是正整數"    "$IMAGES" abc "$TMP/x.mp4"
rmdir "$TMP/empty" 2>/dev/null; rm -f "$TMP/x.mp4"

rmdir "$TMP" 2>/dev/null
if [ "$FAIL" = 0 ]; then echo "全部通過。"; else echo "有案例失敗。"; exit 1; fi
