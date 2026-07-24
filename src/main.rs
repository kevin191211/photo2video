#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;
use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::FfmpegEvent;

const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "bmp", "webp", "tif", "tiff"];
const AUDIO_EXTS: &[&str] = &["mp3", "wav", "m4a", "aac", "flac", "ogg", "opus", "wma"];

/// 轉場/動態效果啟用時的輸出影格率（純幻燈片模式則維持照片張數 = 影格數）
const OUT_FPS: u32 = 30;

/// GitHub 儲存庫（檢查更新與下載頁面用）
const GITHUB_REPO: &str = "kevin191211/photo2video";

/// 全域配色：深色剪輯工具風格
mod theme {
    use eframe::egui::Color32;

    pub const BG: Color32 = Color32::from_rgb(0x13, 0x14, 0x17); // 中央工作區
    pub const PANEL: Color32 = Color32::from_rgb(0x1B, 0x1C, 0x21); // 側欄與上下欄
    pub const CARD: Color32 = Color32::from_rgb(0x25, 0x27, 0x2D); // 卡片、按鈕
    pub const CARD_HOVER: Color32 = Color32::from_rgb(0x2E, 0x30, 0x38);
    pub const BORDER: Color32 = Color32::from_rgb(0x32, 0x34, 0x3C);
    pub const TEXT: Color32 = Color32::from_rgb(0xE9, 0xEA, 0xEE);
    pub const TEXT_WEAK: Color32 = Color32::from_rgb(0x9A, 0x9C, 0xA8);
    pub const ACCENT: Color32 = Color32::from_rgb(0x5B, 0x8C, 0xFF);
    pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x74, 0x9D, 0xFF);
    pub const ACCENT_ACTIVE: Color32 = Color32::from_rgb(0x48, 0x76, 0xE6);
    pub const SUCCESS: Color32 = Color32::from_rgb(0x3D, 0xBE, 0x7B);
    pub const ERROR: Color32 = Color32::from_rgb(0xE5, 0x60, 0x5A);
    pub const PREVIEW_BG: Color32 = Color32::from_rgb(0x0D, 0x0E, 0x10);
}

#[derive(Clone, Copy, PartialEq)]
enum OutputFormat {
    Mp4,
    Mkv,
    Mov,
    Avi,
    Webm,
}

impl OutputFormat {
    fn label(&self) -> &'static str {
        match self {
            OutputFormat::Mp4 => "MP4 (H.264)",
            OutputFormat::Mkv => "MKV (H.264)",
            OutputFormat::Mov => "MOV (H.264)",
            OutputFormat::Avi => "AVI (H.264)",
            OutputFormat::Webm => "WebM (VP9)",
        }
    }

    fn ext(&self) -> &'static str {
        match self {
            OutputFormat::Mp4 => "mp4",
            OutputFormat::Mkv => "mkv",
            OutputFormat::Mov => "mov",
            OutputFormat::Avi => "avi",
            OutputFormat::Webm => "webm",
        }
    }

    const ALL: [OutputFormat; 5] = [
        OutputFormat::Mp4,
        OutputFormat::Mkv,
        OutputFormat::Mov,
        OutputFormat::Avi,
        OutputFormat::Webm,
    ];

    /// 由副檔名找回格式（專案檔用）；不認得時回傳 None
    fn from_ext(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|f| f.ext() == s)
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Resolution {
    w: u32,
    h: u32,
}

impl Resolution {
    /// w=h=0 為「原始像素」哨兵值：實際輸出尺寸依照片中最大寬高決定
    /// （見 App::resolved_resolution），使用前必須先解析成具體尺寸
    fn is_native(&self) -> bool {
        self.w == 0
    }

    fn label(&self) -> String {
        match (self.w, self.h) {
            (0, 0) => "原始像素（依最大照片）".into(),
            (1280, 720) => "HD 1280 × 720".into(),
            (1920, 1080) => "Full HD 1920 × 1080".into(),
            (2560, 1440) => "2K 2560 × 1440".into(),
            (3840, 2160) => "4K 3840 × 2160".into(),
            _ => format!("{} × {}", self.w, self.h),
        }
    }

    const ALL: [Resolution; 5] = [
        Resolution { w: 1280, h: 720 },
        Resolution { w: 1920, h: 1080 },
        Resolution { w: 2560, h: 1440 },
        Resolution { w: 3840, h: 2160 },
        Resolution { w: 0, h: 0 },
    ];
}

/// 調色參數，全部以 -100 ~ +100 表示，0 為不調整
#[derive(Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Adjustments {
    temp: i32,       // 色溫（+ 偏暖 / − 偏冷）
    tint: i32,       // 色調（+ 偏洋紅 / − 偏綠）
    exposure: i32,   // 曝光度
    contrast: i32,   // 對比
    brightness: i32, // 亮度
    shadows: i32,    // 陰影
    whites: i32,     // 白色
    blacks: i32,     // 黑色
    clarity: i32,    // 清晰度
    vibrance: i32,   // 鮮豔度
    saturation: i32, // 飽和度
}

impl Adjustments {
    fn is_neutral(&self) -> bool {
        *self == Self::default()
    }

    /// 所有參數的值（固定順序，與 values_mut 對應）
    fn values(&self) -> [i32; 11] {
        [
            self.temp,
            self.tint,
            self.exposure,
            self.contrast,
            self.brightness,
            self.shadows,
            self.whites,
            self.blacks,
            self.clarity,
            self.vibrance,
            self.saturation,
        ]
    }

    fn values_mut(&mut self) -> [&mut i32; 11] {
        [
            &mut self.temp,
            &mut self.tint,
            &mut self.exposure,
            &mut self.contrast,
            &mut self.brightness,
            &mut self.shadows,
            &mut self.whites,
            &mut self.blacks,
            &mut self.clarity,
            &mut self.vibrance,
            &mut self.saturation,
        ]
    }

    /// 把所有參數夾回合法範圍；專案檔可能被手改出超界值，
    /// 直接餵給 filter_chain 會產生不合法的 ffmpeg 濾鏡參數
    fn clamped(mut self) -> Self {
        for v in self.values_mut() {
            *v = (*v).clamp(-100, 100);
        }
        self
    }

    /// 轉成 ffmpeg 濾鏡串；全部為 0 時回傳 None
    /// unsharp_scale：清晰度的 unsharp 是以「像素」為半徑，需依實際輸出寬度縮放，
    /// 預覽（960px）與輸出（完整解析度）才會呈現相同強度的局部對比。
    /// 輸出傳 1.0（維持 13px），預覽傳 預覽寬/輸出寬
    fn filter_chain(&self, unsharp_scale: f64) -> Option<String> {
        if self.is_neutral() {
            return None;
        }
        let mut filters: Vec<String> = Vec::new();

        if self.temp != 0 {
            // 6500K 為中性；滑桿 +100 → 約 3000K（暖）、-100 → 約 10000K（冷）
            let kelvin = (6500.0 - self.temp as f64 * 35.0).clamp(1000.0, 40000.0);
            filters.push(format!("colortemperature=temperature={kelvin:.0}"));
        }
        if self.tint != 0 {
            // 綠—洋紅軸：gm 為中間調綠色平衡，+ 偏綠，故取負號對應「+ 偏洋紅」
            let gm = -self.tint as f64 / 100.0 * 0.3;
            filters.push(format!("colorbalance=gm={gm:.4}"));
        }
        if self.exposure != 0 {
            let ev = self.exposure as f64 / 100.0 * 3.0;
            filters.push(format!("exposure=exposure={ev:.3}"));
        }
        if self.contrast != 0 || self.brightness != 0 || self.saturation != 0 {
            let c = 1.0 + self.contrast as f64 * 0.008;
            let b = self.brightness as f64 * 0.004;
            let s = (1.0 + self.saturation as f64 * 0.01).max(0.0);
            filters.push(format!("eq=contrast={c:.4}:brightness={b:.4}:saturation={s:.4}"));
        }
        if self.shadows != 0 || self.whites != 0 || self.blacks != 0 {
            let mut pts: Vec<(f64, f64)> = Vec::new();
            if self.blacks < 0 {
                // 壓黑：把輸入黑點往右移
                pts.push((0.0, 0.0));
                pts.push((-self.blacks as f64 / 100.0 * 0.12, 0.0));
            } else {
                // 提黑：抬高輸出黑點
                pts.push((0.0, self.blacks as f64 / 100.0 * 0.15));
            }
            if self.shadows != 0 {
                let y = (0.25 + self.shadows as f64 / 100.0 * 0.15).clamp(0.0, 1.0);
                pts.push((0.25, y));
            }
            if self.whites > 0 {
                // 提白：把輸入白點往左移
                pts.push((1.0 - self.whites as f64 / 100.0 * 0.15, 1.0));
                pts.push((1.0, 1.0));
            } else {
                pts.push((1.0, 1.0 + self.whites as f64 / 100.0 * 0.15));
            }
            let pts_str = pts
                .iter()
                .map(|(x, y)| format!("{x:.4}/{y:.4}"))
                .collect::<Vec<_>>()
                .join(" ");
            filters.push(format!("curves=all='{pts_str}'"));
        }
        if self.vibrance != 0 {
            let v = self.vibrance as f64 / 100.0 * 2.0;
            filters.push(format!("vibrance=intensity={v:.4}"));
        }
        if self.clarity != 0 {
            // 大半徑低強度的 unsharp ≈ 局部對比（清晰度）；負值則柔化。
            // 核心尺寸須為 3~23 的奇數，依輸出寬度縮放後仍取最近的合法奇數
            let amount = self.clarity as f64 * 0.015;
            let msize = ((13.0 * unsharp_scale).round() as i32).clamp(3, 23) | 1;
            filters.push(format!(
                "unsharp=luma_msize_x={msize}:luma_msize_y={msize}:luma_amount={amount:.4}"
            ));
        }
        Some(filters.join(","))
    }
}

/// 文字樣式（字型與顏色全域共用；大小與位置為每段文字獨立設定）
#[derive(Clone, PartialEq)]
struct SubtitleStyle {
    font_idx: usize,
    color: egui::Color32,
    outline_w: i32,
    outline_color: egui::Color32,
    boxed: bool,
}

impl Default for SubtitleStyle {
    fn default() -> Self {
        Self {
            font_idx: 0,
            color: egui::Color32::WHITE,
            outline_w: 2,
            outline_color: egui::Color32::BLACK,
            boxed: false,
        }
    }
}

/// 一段文字：從第 start 張到第 end 張（1-based、含端點）顯示；
/// 位置為文字中心點在畫面上的比例（0~1），大小以 1080p 高度為基準，旋轉單位為度（順時針）
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct SubtitleEntry {
    start: usize,
    end: usize,
    text: String,
    x: f32,
    y: f32,
    size: i32,
    rot: f32,
}

impl SubtitleEntry {
    fn new(start: usize, end: usize) -> Self {
        Self {
            start,
            end,
            text: String::new(),
            x: 0.5,
            y: 0.85,
            size: 48,
            rot: 0.0,
        }
    }
}

// 專案檔的相容性保證（缺欄位不會整份開不起來）也要涵蓋巢狀的文字段落：
// SubtitleEntry 沒掛 serde(default) 的話，未來版本加欄位後，舊版程式
// 讀新版專案檔會因段落缺欄位而整份反序列化失敗
impl Default for SubtitleEntry {
    fn default() -> Self {
        Self::new(1, 1)
    }
}

/// 照片之間的轉場效果
#[derive(Clone, Copy, PartialEq)]
enum Transition {
    None,
    FadeBlack,
}

impl Transition {
    fn label(&self) -> &'static str {
        match self {
            Transition::None => "無（直接切換）",
            Transition::FadeBlack => "淡入淡出",
        }
    }
    const ALL: [Transition; 2] = [Transition::None, Transition::FadeBlack];

    /// 專案檔內的穩定識別字串（label 是給人看的中文，不適合存檔）
    fn id(&self) -> &'static str {
        match self {
            Transition::None => "none",
            Transition::FadeBlack => "fade_black",
        }
    }

    fn from_id(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.id() == s)
    }
}

/// 背景音樂設定
#[derive(Clone)]
struct MusicJob {
    path: PathBuf,
    volume: i32, // 百分比 0~200
    fade_out: bool,
}

/// 輸出效果（轉場、動態縮放、音樂）
#[derive(Clone)]
struct OutputFx {
    transition: Transition,
    ken_burns: bool,
    music: Option<MusicJob>,
}

/// 專案檔副檔名（內容為 JSON）
const PROJECT_EXT: &str = "p2v";

/// 專案檔內容：照片清單與所有編輯設定。
/// 與 App 狀態分離的獨立結構，欄位盡量用穩定的表示法
/// （字型存名稱而非索引、格式/轉場存字串），並全部給預設值，
/// 舊版程式開新版專案檔（或反之）時缺欄位不會整個開不起來
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct ProjectFile {
    /// 專案檔格式版本，目前為 1
    version: u32,
    /// 存檔當下的程式版本（僅供除錯參考）
    app_version: String,
    photos: Vec<PathBuf>,
    fps: u32,
    /// 輸出格式副檔名（mp4/mkv/mov/avi/webm）
    format: String,
    /// 輸出解析度；(0, 0) 為「原始像素」
    resolution: (u32, u32),
    adj: Adjustments,
    /// 個別照片的調色覆寫（HashMap 的 PathBuf 鍵存 JSON 不穩定，改用陣列）
    adj_overrides: Vec<(PathBuf, Adjustments)>,
    sub_entries: Vec<SubtitleEntry>,
    /// 字型名稱；開檔時依名稱找回索引，找不到就用第一個字型
    sub_font: String,
    sub_color: [u8; 4],
    sub_outline_w: i32,
    sub_outline_color: [u8; 4],
    sub_boxed: bool,
    /// 轉場（none/fade_black）
    transition: String,
    ken_burns: bool,
    music_path: Option<PathBuf>,
    music_volume: i32,
    music_fade: bool,
}

impl Default for ProjectFile {
    fn default() -> Self {
        let style = SubtitleStyle::default();
        Self {
            version: 1,
            app_version: String::new(),
            photos: Vec::new(),
            fps: 10,
            format: "mp4".into(),
            resolution: (1920, 1080),
            adj: Adjustments::default(),
            adj_overrides: Vec::new(),
            sub_entries: Vec::new(),
            sub_font: String::new(),
            sub_color: style.color.to_array(),
            sub_outline_w: style.outline_w,
            sub_outline_color: style.outline_color.to_array(),
            sub_boxed: style.boxed,
            transition: "none".into(),
            ken_burns: false,
            music_path: None,
            music_volume: 100,
            music_fade: true,
        }
    }
}

/// 解析 "v1.2.3" 或 "1.2.3" 為可比較的版本號
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let mut it = s.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// 查詢 GitHub 最新 release：Ok(Some(tag)) 表示有新版、Ok(None) 表示已是最新
fn check_latest_release() -> Result<Option<String>, String> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
    let resp = match ureq::get(&url)
        .set("User-Agent", concat!("photo2video/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(10))
        .call()
    {
        Ok(r) => r,
        // 還沒有任何 release 時 GitHub 回 404，視為沒有新版
        Err(ureq::Error::Status(404, _)) => return Ok(None),
        Err(e) => return Err(format!("連線失敗：{e}")),
    };
    let json: serde_json::Value = resp
        .into_json()
        .map_err(|e| format!("回應解析失敗：{e}"))?;
    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or("回應中沒有版本資訊")?
        .to_string();
    let latest = parse_version(&tag).ok_or("無法解析最新版本號")?;
    let current = parse_version(env!("CARGO_PKG_VERSION")).ok_or("無法解析目前版本號")?;
    Ok((latest > current).then_some(tag))
}

/// 暫存檔路徑加上行程 ID：同時開兩個程式實例（各轉各的照片）時，
/// 不會互踩對方的照片清單、字幕檔與預覽底圖
fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("photo2video_{}_{name}", std::process::id()))
}

/// 設定檔路徑：%APPDATA%\photo2video\config.json
fn config_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("photo2video")
        .join("config.json")
}

/// 閃退紀錄路徑：%APPDATA%\photo2video\crash.log
fn crash_log_path() -> PathBuf {
    config_path().with_file_name("crash.log")
}

/// 安裝 panic hook：任何執行緒 panic 時把訊息、位置與 backtrace 寫入 crash.log，
/// 下次啟動偵測到就顯示回報介面。release 版已 strip 符號，backtrace 只有位址，
/// 但 panic 訊息與原始碼位置（file:line）仍是編譯期字串，足以定位問題
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "（無法取得錯誤內容）".into());
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "未知位置".into());
        let bt = std::backtrace::Backtrace::force_capture();
        let report = format!(
            "Photo2Video v{}\npanic：{msg}\n位置：{loc}\n\nbacktrace：\n{bt}",
            env!("CARGO_PKG_VERSION")
        );
        let path = crash_log_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // 只保留最後一次：多次 panic 時最新的最接近使用者實際遇到的問題
        let _ = std::fs::write(&path, &report);
        default_hook(info);
    }));
}

/// 讀出上次留下的閃退紀錄並刪除檔案（讀過就清掉，避免每次啟動重複提示）
fn take_crash_report() -> Option<String> {
    let path = crash_log_path();
    let report = std::fs::read_to_string(&path)
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let _ = std::fs::remove_file(&path);
    Some(report)
}

/// 讀取上次儲存的每秒張數；沒有設定檔或值不合法時回傳 None
fn load_saved_fps() -> Option<u32> {
    let txt = std::fs::read_to_string(config_path()).ok()?;
    let json: serde_json::Value = serde_json::from_str(&txt).ok()?;
    let fps = json.get("fps")?.as_u64()? as u32;
    (1..=60).contains(&fps).then_some(fps)
}

/// 讀取整份設定檔 JSON；不存在或壞掉時回傳空物件
fn load_config() -> serde_json::Value {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}))
}

/// 更新設定檔中的單一欄位並保留其他欄位（失敗不影響使用，靜默忽略）
fn update_config(key: &str, value: serde_json::Value) {
    let mut cfg = load_config();
    cfg[key] = value;
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // 原子寫入：先寫臨時檔再 rename 覆蓋。直接 write 若寫到一半崩潰/斷電，
    // 會留下損壞的 config.json，下次啟動解析失敗就丟失 fps 慣用值與整份
    // 最近專案清單。臨時檔壞了也不影響既有的 config.json
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, cfg.to_string()).is_ok() {
        // rename 失敗（如防毒鎖檔）就清掉臨時檔，不留垃圾
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// 儲存每秒張數設定
fn save_fps(fps: u32) {
    update_config("fps", serde_json::json!(fps));
}

/// 最近開啟/儲存的專案檔路徑（新的在前）
fn load_recent_projects() -> Vec<PathBuf> {
    load_config()
        .get("recent_projects")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(PathBuf::from))
                .collect()
        })
        .unwrap_or_default()
}

fn save_recent_projects(list: &[PathBuf]) {
    let arr: Vec<String> = list.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    update_config("recent_projects", serde_json::json!(arr));
}

/// 下載指定版本的 photo2video.exe 並原地替換目前的執行檔
fn download_update(tag: &str, progress: &dyn Fn(f32)) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("無法取得程式路徑：{e}"))?;
    let url = format!("https://github.com/{GITHUB_REPO}/releases/download/{tag}/photo2video.exe");
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .build();
    let resp = agent
        .get(&url)
        .set("User-Agent", concat!("photo2video/", env!("CARGO_PKG_VERSION")))
        .call()
        .map_err(|e| format!("下載失敗：{e}"))?;
    let total: f64 = resp
        .header("Content-Length")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);

    // 下載到同資料夾的暫存檔（同一磁碟才能直接改名替換）
    let tmp = exe.with_extension("exe.new");
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(&tmp).map_err(|e| format!("無法建立暫存檔：{e}"))?;
    let mut buf = [0u8; 64 * 1024];
    let mut done: f64 = 0.0;
    let mut last_pct: i32 = -1;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("下載中斷：{e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("寫入暫存檔失敗：{e}"))?;
        done += n as f64;
        if total > 0.0 {
            let pct = (done / total * 100.0) as i32;
            if pct != last_pct {
                last_pct = pct;
                progress((done / total) as f32);
            }
        }
    }
    drop(file);

    // 簡單驗證是 Windows 執行檔（MZ 開頭），避免把錯誤頁面存成 exe
    let mut magic = [0u8; 2];
    std::fs::File::open(&tmp)
        .and_then(|mut f| f.read_exact(&mut magic))
        .map_err(|e| format!("暫存檔讀取失敗：{e}"))?;
    if &magic != b"MZ" {
        let _ = std::fs::remove_file(&tmp);
        return Err("下載的檔案不是有效的執行檔".into());
    }

    // 執行中的 exe 不能覆寫，但可以改名：舊檔改 .old、新檔補上原位
    let old = exe.with_extension("exe.old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("無法替換舊版程式：{e}"))?;
    if let Err(e) = std::fs::rename(&tmp, &exe) {
        let _ = std::fs::rename(&old, &exe); // 失敗時還原
        return Err(format!("無法安裝新版程式：{e}"));
    }
    Ok(())
}

/// 重新啟動程式（新版 exe 已放在原本的路徑上）
fn restart_app() {
    // process::exit 不執行解構子，App::drop 的暫存清理不會跑；
    // 暫存檔名帶行程 PID，重啟後的新行程也不會清到這些檔案，
    // 不在這裡先清掉的話，每次自動更新都會留下孤兒暫存檔持續累積
    clean_own_temp_files();
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    std::process::exit(0);
}

/// 掃描 Windows 字型資料夾中常見且支援中文的字型
fn detect_fonts() -> Vec<(String, PathBuf)> {
    let candidates: [(&str, &str); 12] = [
        ("微軟正黑體", "msjh.ttc"),
        ("微軟正黑體（粗體）", "msjhbd.ttc"),
        ("標楷體", "kaiu.ttf"),
        ("新細明體", "mingliu.ttc"),
        ("微軟雅黑", "msyh.ttc"),
        ("Arial", "arial.ttf"),
        ("Arial（粗體）", "arialbd.ttf"),
        ("Times New Roman", "times.ttf"),
        ("Impact", "impact.ttf"),
        ("Comic Sans MS", "comic.ttf"),
        ("Consolas", "consola.ttf"),
        ("Segoe UI", "segoeui.ttf"),
    ];
    let fonts_dir = Path::new(r"C:\Windows\Fonts");
    candidates
        .iter()
        .filter_map(|(name, file)| {
            let p = fonts_dir.join(file);
            p.exists().then(|| (name.to_string(), p))
        })
        .collect()
}

/// 百分比編碼（供組 GitHub 回報網址用）：非「未保留字元」的位元組一律以 %XX 表示，
/// 中文等 UTF-8 多位元組也逐位元組編碼，瀏覽器與 GitHub 都能正確還原
fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 依「百分比編碼後」的長度預算截斷文字（每字元以最壞 3 倍估算）。
/// 系統開啟網址的長度上限約 2048：以「字元數」截斷時，中文經 UTF-8
/// ＋百分比編碼可膨脹 9 倍，URL 超限瀏覽器根本開不起來、按鈕看似沒反應
fn truncate_for_url(s: &str, budget: usize) -> String {
    let mut out = String::new();
    let mut enc_len = 0usize;
    for c in s.chars() {
        let l = c.len_utf8() * 3;
        if enc_len + l > budget {
            out.push_str("\n…（內容過長已截斷）");
            break;
        }
        enc_len += l;
        out.push(c);
    }
    out
}

/// 解碼縮圖並套用 EXIF 方向。手機照片常以「未旋轉的原始像素＋方向標記」儲存，
/// image::open 不會自動套用方向，但輸出／預覽走的 ffmpeg 會自動旋轉——兩者不一致
/// 會讓縮圖列與預覽佔位圖側躺，實際影片卻是正的。這裡讀取方向並套用後再縮圖
fn decode_thumbnail(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    use image::ImageDecoder;
    let mut decoder = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    // 方向須在 from_decoder 消耗掉 decoder 之前先取出
    let orientation = decoder.orientation().ok()?;
    let mut img = image::DynamicImage::from_decoder(decoder).ok()?;
    img.apply_orientation(orientation);
    // RGB 即可（縮圖不需要透明通道），省 1/4 記憶體與上傳頻寬
    let t = img.thumbnail(320, 180).to_rgb8();
    Some((t.width(), t.height(), t.into_raw()))
}

/// 讀取照片的顯示尺寸（已套用 EXIF 方向，只讀檔頭不解碼像素）。
/// image_dimensions 回傳的是未旋轉的原始像素，手機直拍照片會拿到寬高顛倒的值；
/// 輸出走的 ffmpeg 會自動旋轉，這裡須以旋轉後的寬高計算，
/// 「原始像素」解析度才會與實際輸出一致，直拍照片不會被塞進橫向畫布
fn oriented_dimensions(path: &Path) -> Option<(u32, u32)> {
    use image::metadata::Orientation;
    use image::ImageDecoder;
    let mut decoder = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    let orientation = decoder.orientation().ok()?;
    let (w, h) = decoder.dimensions();
    let swapped = matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    );
    Some(if swapped { (h, w) } else { (w, h) })
}

/// 把 Windows 路徑轉成 filtergraph 內安全的形式（/ 分隔、跳脫冒號與單引號）。
/// 呼叫端以 fontfile='…' 單引號包住路徑：路徑本身含 ' 時（如使用者名稱
/// O'Brien，字幕暫存檔就在 %TEMP% 使用者目錄下）會提前終止引號、
/// 整條濾鏡解析失敗；比照 concat_escape 以 '\'' 跳脫
fn ff_path_escape(p: &Path) -> String {
    p.to_string_lossy()
        .replace('\\', "/")
        .replace(':', r"\:")
        .replace('\'', r"'\''")
}

fn ff_color(c: egui::Color32) -> String {
    // egui Color32 內部為預乘 alpha，直接取 r/g/b 會把半透明色送成偏暗的預乘值；
    // ffmpeg 要的是直通（straight）RGB，故取未預乘值，半透明文字/外框才不會變暗
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    format!("0x{r:02X}{g:02X}{b:02X}@{:.3}", a as f32 / 255.0)
}

/// 產生一段 drawtext 濾鏡。fontsize 為實際像素；(x_frac, y_frac) 為文字中心點
/// 在畫面上的比例位置；enable 為顯示時間區間
fn drawtext_filter(
    fontfile: &Path,
    textfile: &Path,
    style: &SubtitleStyle,
    fontsize: f64,
    x_frac: f32,
    y_frac: f32,
    enable: Option<(f64, f64)>,
) -> String {
    // expansion=none：文字完全照字面輸出。預設的 normal 展開會把內容裡的
    // %{...} 當內建變數、% 觸發 strftime，使用者打「特價 50% off」「{重點}」
    // 之類的文字會掉字甚至讓整個轉檔失敗；檔案中的真實換行仍正常斷行
    let mut f = format!(
        "drawtext=expansion=none:fontfile='{}':textfile='{}':fontsize={:.0}:fontcolor={}:borderw={}:bordercolor={}:x=(w*{:.4}-text_w/2):y=(h*{:.4}-text_h/2)",
        ff_path_escape(fontfile),
        ff_path_escape(textfile),
        fontsize.max(1.0),
        ff_color(style.color),
        style.outline_w,
        ff_color(style.outline_color),
        x_frac,
        y_frac,
    );
    if style.boxed {
        let pad = (fontsize * 0.25).round().max(4.0);
        f.push_str(&format!(":box=1:boxcolor=black@0.4:boxborderw={pad:.0}"));
    }
    if let Some((a, b)) = enable {
        f.push_str(&format!(":enable='between(t,{a:.3},{b:.3})'"));
    }
    f
}

enum WorkerMsg {
    Status(String),
    Progress(f32),
    Done(PathBuf),
    Error(String),
}

enum ConvertState {
    Idle,
    Working { progress: f32, status: String },
    Done(PathBuf),
    Error(String),
}

/// 檢查更新的狀態
enum UpdateStatus {
    Idle,
    Checking,
    UpToDate,
    Available(String),
    Downloading(f32),
    ReadyToRestart,
    Failed(String),
}

/// 更新背景執行緒回傳的訊息
enum UpdateMsg {
    CheckResult(Result<Option<String>, String>),
    Progress(f32),
    Ready,
    Failed(String),
}

/// 預覽像素為 RGB24（ffmpeg rawvideo 直接輸出，不經 PNG 編解碼）
type PreviewResult = Result<(u32, u32, Vec<u8>), String>;
/// 預覽結果附帶「這是哪一張照片的渲染」（以路徑辨識，非索引）：移除照片會讓
/// 索引指向不同照片，用索引比對會把舊照片的渲染顯示到新照片上、或漏掉有效結果
type PreviewMsg = (PathBuf, PreviewResult);
/// 縮圖解碼結果：(寬, 高, RGB 像素)；None 表示解碼失敗
type ThumbMsg = (PathBuf, Option<(u32, u32, Vec<u8>)>);

enum Thumb {
    Loading,
    Ready(egui::TextureHandle),
    Failed,
}

struct App {
    photos: Vec<PathBuf>,
    fps: u32,
    format: OutputFormat,
    resolution: Resolution,
    state: ConvertState,
    rx: Option<Receiver<WorkerMsg>>,
    adj: Adjustments,
    /// 個別照片的調色覆寫；沒有覆寫的照片沿用全域 adj
    adj_overrides: HashMap<PathBuf, Adjustments>,
    /// 縮圖列 Ctrl/Shift+點選 的多選照片（個別調色的目標）
    multi_sel: HashSet<PathBuf>,
    /// 多選調色時滑桿顯示的工作值；改動時寫入所有選取照片的覆寫
    sel_adj: Adjustments,
    /// 「原始像素」解析度的快取（照片中最大寬高）；照片增減時清除重算
    native_res_cache: Option<Resolution>,
    /// 每張照片的尺寸快取（已套用 EXIF 方向；None＝讀取失敗不重試），
    /// 讓 native 解析度重算只讀「新照片」的檔頭
    dims_cache: HashMap<PathBuf, Option<(u32, u32)>>,
    sub_entries: Vec<SubtitleEntry>,
    sub_style: SubtitleStyle,
    /// 預覽區目前選取的文字（sub_entries 索引），用於顯示縮放/旋轉控制框
    sel_text: Option<usize>,
    fonts: Vec<(String, PathBuf)>,
    preview_selected: Option<usize>,
    preview_dirty: bool,
    preview_rx: Option<Receiver<PreviewMsg>>,
    preview_tex: Option<egui::TextureHandle>,
    preview_error: Option<String>,
    /// 已為哪一張照片的鄰居做過底圖預取（避免每幀重複觸發）
    prefetch_for: Option<usize>,
    thumbs: HashMap<PathBuf, Thumb>,
    /// 縮圖解碼佇列：常駐工作執行緒從這裡領工作（含喚醒用 Condvar）
    thumb_jobs: Arc<(Mutex<VecDeque<PathBuf>>, Condvar)>,
    thumb_rx: Receiver<ThumbMsg>,
    wheel_accum: f32,
    scroll_to_selected: bool,
    transition: Transition,
    ken_burns: bool,
    music_path: Option<PathBuf>,
    music_volume: i32,
    music_fade: bool,
    sec_adjust_open: bool,
    sec_sub_open: bool,
    sec_fx_open: bool,
    update_rx: Option<Receiver<UpdateMsg>>,
    update_status: UpdateStatus,
    update_banner_dismissed: bool,
    /// 下載更新失敗的訊息：使用者主動點「立即更新」後失敗才設，供底欄
    /// 顯示告知（啟動背景檢查失敗屬 Failed 但不設此，維持靜默不打擾）
    update_download_error: Option<String>,
    about_open: bool,
    /// fps 最後一次變動的時間；拖動時不即時寫設定檔，停止變動後才寫
    fps_pending_save: Option<Instant>,
    /// 上次執行留下的閃退紀錄（crash.log 內容）；有值時在底欄顯示回報橫幅
    crash_report: Option<String>,
    /// 剛選的資料夾/檔案沒有找到任何照片；在空狀態顯示提示，避免使用者以為沒反應
    import_found_nothing: bool,
    /// 最近開啟/儲存的專案檔（新的在前），顯示在空狀態畫面供一鍵開啟
    recent_projects: Vec<PathBuf>,
    /// 轉檔中的輸出檔路徑；轉檔中關窗或 worker 異常中斷時據此清掉
    /// 半成品檔案（與 run_conversion 失敗時的清理一致），完成後清為 None
    convert_output: Option<PathBuf>,
    /// 目前專案檔路徑（最近一次開啟或儲存的 .p2v）：Ctrl+S 直接覆寫
    /// 存檔，不再每次都跳「另存新檔」對話框；新專案時清除
    current_project: Option<PathBuf>,
    /// 專案最近一次儲存成功的時間：Ctrl+S 直接覆寫沒有對話框，
    /// 底欄短暫顯示「已儲存」讓使用者確認有存進去
    project_saved_at: Option<Instant>,
    /// 目前已套用的視窗標題（避免每幀重送 ViewportCommand）
    applied_title: String,
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        initial_files: Vec<PathBuf>,
        cjk_font: Option<Vec<u8>>,
    ) -> Self {
        setup_chinese_fonts(&cc.egui_ctx, cjk_font);
        apply_theme(&cc.egui_ctx);
        let (thumb_tx, thumb_rx) = std::sync::mpsc::channel::<ThumbMsg>();
        // 常駐縮圖解碼工作池：閒置時停在 Condvar 上，有工作就喚醒，
        // 不再每批照片都重新建立執行緒
        let thumb_jobs: Arc<(Mutex<VecDeque<PathBuf>>, Condvar)> =
            Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        let workers = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(8);
        for _ in 0..workers {
            let jobs = Arc::clone(&thumb_jobs);
            let tx = thumb_tx.clone();
            let ctx = cc.egui_ctx.clone();
            thread::spawn(move || loop {
                let job = {
                    let (lock, cv) = &*jobs;
                    let mut q = lock.lock().unwrap();
                    loop {
                        if let Some(p) = q.pop_front() {
                            break p;
                        }
                        q = cv.wait(q).unwrap();
                    }
                };
                let res = decode_thumbnail(&job);
                let _ = tx.send((job, res));
                ctx.request_repaint();
            });
        }
        let mut app = Self {
            photos: Vec::new(),
            fps: load_saved_fps().unwrap_or(10),
            format: OutputFormat::Mp4,
            resolution: Resolution { w: 1920, h: 1080 },
            state: ConvertState::Idle,
            rx: None,
            adj: Adjustments::default(),
            adj_overrides: HashMap::new(),
            multi_sel: HashSet::new(),
            sel_adj: Adjustments::default(),
            native_res_cache: None,
            dims_cache: HashMap::new(),
            sub_entries: Vec::new(),
            sub_style: SubtitleStyle::default(),
            sel_text: None,
            fonts: detect_fonts(),
            preview_selected: None,
            preview_dirty: false,
            preview_rx: None,
            preview_tex: None,
            preview_error: None,
            prefetch_for: None,
            thumbs: HashMap::new(),
            thumb_jobs,
            thumb_rx,
            wheel_accum: 0.0,
            scroll_to_selected: false,
            transition: Transition::None,
            ken_burns: false,
            music_path: None,
            music_volume: 100,
            music_fade: true,
            sec_adjust_open: true,
            sec_sub_open: true,
            sec_fx_open: true,
            update_rx: None,
            update_status: UpdateStatus::Idle,
            update_banner_dismissed: false,
            update_download_error: None,
            about_open: false,
            fps_pending_save: None,
            crash_report: take_crash_report(),
            import_found_nothing: false,
            recent_projects: load_recent_projects(),
            convert_output: None,
            current_project: None,
            project_saved_at: None,
            applied_title: String::new(),
        };
        // 啟動時在背景檢查是否有新版本（失敗不影響使用）
        app.spawn_update_check(&cc.egui_ctx);
        // 背景預熱：先確認 ffmpeg 可用並偵測硬體編碼器（最多要跑 3 個測試編碼、
        // 費時 1~3 秒），第一次按「開始轉換」或看預覽時就不用等。
        // 尚未安裝 ffmpeg 時不在這裡下載，留給轉換流程顯示下載進度
        thread::spawn(|| {
            if ffmpeg_sidecar::command::ffmpeg_is_installed() {
                FFMPEG_READY.store(true, Ordering::Relaxed);
                detect_h264_encoder();
            }
        });
        if !initial_files.is_empty() {
            // 「開啟方式」或拖到執行檔圖示上：含 .p2v 就只開專案（與拖入
            // 視窗的處理一致）。開專案是「取代整個工作狀態」的操作，同批
            // 夾帶的照片語意不明，加上去只會弄髒剛開的專案。
            // 專案檔也不能丟給 add_photos——會被 is_image 過濾而靜默沒反應
            if let Some(proj) = initial_files.iter().find(|p| is_project_file(p)) {
                app.load_project(proj);
            } else {
                app.add_photos(initial_files);
            }
        }
        app
    }

    fn mark_preview_dirty(&mut self) {
        self.preview_dirty = true;
        self.preview_error = None;
        // 參數（如解析度）可能變了，閒置時重新評估鄰居預取
        self.prefetch_for = None;
    }

    /// 依選取的照片與目前調色參數，在背景執行 ffmpeg 產生預覽圖。
    /// 文字不在這裡燒錄——預覽的文字由 egui 即時繪製，拖曳/縮放/旋轉不需重新渲染
    fn spawn_preview(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.preview_selected else { return };
        let Some(photo) = self.photos.get(idx).cloned() else { return };
        let adj = self.effective_adj(&photo);
        let res = self.resolved_resolution();
        let (tx, rx) = std::sync::mpsc::channel();
        self.preview_rx = Some(rx);
        self.preview_dirty = false;
        let ctx = ctx.clone();
        thread::spawn(move || {
            let result = render_preview(&photo, &adj, res);
            let _ = tx.send((photo, result));
            ctx.request_repaint();
        });
    }

    fn poll_preview(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.preview_rx {
            match rx.try_recv() {
                Ok((for_photo, res)) => {
                    self.preview_rx = None;
                    // 以路徑辨識：渲染期間使用者切到別張、或移除照片使索引重排時，
                    // 只有目前選取的正是這張照片才採用結果，否則丟棄（dirty 仍在會重render）
                    let is_current = self
                        .preview_selected
                        .and_then(|i| self.photos.get(i))
                        .map(|p| p == &for_photo)
                        .unwrap_or(false);
                    if is_current {
                        match res {
                            Ok((w, h, rgb)) => {
                                let img = egui::ColorImage::from_rgb(
                                    [w as usize, h as usize],
                                    &rgb,
                                );
                                self.preview_tex =
                                    Some(ctx.load_texture("preview", img, Default::default()));
                            }
                            Err(e) => self.preview_error = Some(e),
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // 渲染執行緒異常結束沒回傳結果：不清掉等待狀態的話，
                    // 之後永遠不會再啟動渲染，預覽卡在「產生中…」
                    self.preview_rx = None;
                    self.preview_error = Some("預覽渲染異常中斷".into());
                }
            }
        }
        // 連續回饋：參數有變且沒有渲染在跑就立刻渲染。
        // 同時間最多一個渲染、完成後才會再啟動下一個（以最新參數），
        // 更新頻率被渲染時間自然限流；拖動滑桿的過程即時看到調色變化
        if self.preview_dirty && self.preview_rx.is_none() {
            self.spawn_preview(ctx);
        }

        // 閒置（目前照片已渲染完、沒在轉檔）時，背景預取前後張照片的底圖，
        // 逐張瀏覽時切換到下一張直接命中快取
        if !self.preview_dirty && self.preview_rx.is_none() && !self.is_working() {
            if let Some(i) = self.preview_selected {
                if self.prefetch_for != Some(i) {
                    self.prefetch_for = Some(i);
                    let (pw, ph) = preview_canvas(self.resolved_resolution());
                    for j in [i.checked_sub(1), Some(i + 1)].into_iter().flatten() {
                        if let Some(p) = self.photos.get(j) {
                            let p = p.clone();
                            thread::spawn(move || prefetch_preview_base(p, pw, ph));
                        }
                    }
                }
            }
        }
    }

    /// 接收背景執行緒解碼完成的縮圖並上傳為貼圖。
    /// 每幀最多上傳 8 張：批次載入時解碼常同時到貨，
    /// 一次全部上傳會造成單幀突波卡頓，分幀攤平
    fn poll_thumbs(&mut self, ctx: &egui::Context) {
        const MAX_UPLOADS_PER_FRAME: usize = 8;
        let mut uploaded = 0;
        while uploaded < MAX_UPLOADS_PER_FRAME {
            let Ok((path, res)) = self.thumb_rx.try_recv() else { break };
            // 只收還在等待中（Loading）的結果：照片已被清空/移除時，
            // 解碼中的工作仍會遲到送回，若照收會留下淘汰掃描
            // （只走訪目前照片清單）永遠釋放不到的貼圖
            if !matches!(self.thumbs.get(&path), Some(Thumb::Loading)) {
                continue;
            }
            let state = match res {
                Some((w, h, rgb)) => {
                    uploaded += 1;
                    let img = egui::ColorImage::from_rgb([w as usize, h as usize], &rgb);
                    Thumb::Ready(ctx.load_texture(
                        format!("thumb:{}", path.display()),
                        img,
                        Default::default(),
                    ))
                }
                None => Thumb::Failed,
            };
            self.thumbs.insert(path, state);
        }
        if uploaded == MAX_UPLOADS_PER_FRAME {
            // 佇列可能還有縮圖，下一幀繼續上傳
            ctx.request_repaint();
        }
    }

    /// 這張照片實際生效的調色：有個別覆寫用覆寫，否則用全域
    fn effective_adj(&self, photo: &Path) -> Adjustments {
        self.adj_overrides.get(photo).copied().unwrap_or(self.adj)
    }

    /// 多選集合變動後，讓調色滑桿顯示第一張選取照片目前生效的調色
    fn sync_sel_adj(&mut self) {
        if let Some(p) = self.photos.iter().find(|p| self.multi_sel.contains(*p)) {
            self.sel_adj = self.effective_adj(p);
        }
    }

    /// 解析度選「原始像素」時，以照片中最大的寬與高為輸出解析度；
    /// 讀取所有照片檔頭有成本，結果快取到照片增減時再重算。
    /// 尺寸另有一層「每張照片」的持久快取：照片增減只需讀新照片的
    /// 檔頭，其餘用快取值重算最大值——否則每加/刪一張都在 UI 執行緒
    /// 重讀全部照片的檔頭，上千張時每次增減凍結一兩秒
    fn resolved_resolution(&mut self) -> Resolution {
        if !self.resolution.is_native() {
            return self.resolution;
        }
        if let Some(r) = self.native_res_cache {
            return r;
        }
        let (mut w, mut h) = (0u32, 0u32);
        for p in &self.photos {
            // None 也快取：讀不到的檔案不重試，避免壞檔每次重算都拖慢
            let dims = self
                .dims_cache
                .entry(p.clone())
                .or_insert_with(|| oriented_dimensions(p));
            if let Some((pw, ph)) = *dims {
                w = w.max(pw);
                h = h.max(ph);
            }
        }
        // 沒照片或全讀不到就退回 Full HD；編碼要求偶數尺寸
        let r = if w == 0 || h == 0 {
            Resolution { w: 1920, h: 1080 }
        } else {
            Resolution {
                w: w.max(2) & !1,
                h: h.max(2) & !1,
            }
        };
        self.native_res_cache = Some(r);
        r
    }

    fn select_photo(&mut self, idx: Option<usize>) {
        if self.preview_selected != idx {
            self.preview_selected = idx;
            self.preview_tex = None;
            if idx.is_some() {
                self.mark_preview_dirty();
            }
        }
    }

    fn is_working(&self) -> bool {
        matches!(self.state, ConvertState::Working { .. })
    }

    fn add_photos(&mut self, mut files: Vec<PathBuf>) {
        files.retain(|p| is_image(p));
        // 統一轉絕對路徑：以相對路徑啟動（CLI 參數、「開啟方式」）加入的
        // 照片，字面路徑存進專案檔後會隨工作目錄不同而「遺失」；
        // 去重也不受同檔案相對/絕對兩種寫法影響（轉檔的 concat_escape
        // 已為同因做絕對化，這裡讓清單本身就是絕對路徑）
        for f in &mut files {
            if let Ok(abs) = std::path::absolute(&*f) {
                *f = abs;
            }
        }
        // 這批有真的加入照片就清掉「找不到照片」提示，並讓上次的轉換
        // 結果橫幅失效（照片變了，上次輸出已過時）
        if !files.is_empty() {
            self.import_found_nothing = false;
            self.clear_result_banner();
        }
        self.native_res_cache = None;
        // 加入前的照片順序：供下方把文字段落的編號對回排序後的新位置
        let old_photos: Vec<PathBuf> = self.photos.clone();
        // 用 HashSet 去重；逐一 contains 是 O(n²)，加入數千張照片要數百萬次路徑比對。
        // 去重 key 用小寫路徑：Windows 檔案系統不分大小寫，但 PathBuf 比較區分，
        // 同一張照片以不同大小寫路徑加入（命令列/開啟方式 vs 對話框）會被當成
        // 兩張重複（比照最近專案清單的 same_path_ci）
        let mut seen: HashSet<String> = self
            .photos
            .iter()
            .map(|p| p.to_string_lossy().to_lowercase())
            .collect();
        for f in files {
            if seen.insert(f.to_string_lossy().to_lowercase()) {
                self.photos.push(f);
            }
        }
        // 排序會重新編號，選取的照片索引會失效：新加入的照片若排在
        // 選取照片之前，同一個索引就指向別張了。記住選取照片的路徑，
        // 排序後找回它的新索引，讓選取跟著照片走而非固定在數字上
        let selected_path = self
            .preview_selected
            .and_then(|i| self.photos.get(i).cloned());
        // 文字段落同理：以 1-based 照片編號綁定，排序前先記下起訖照片的路徑，
        // 排序後找回新編號，段落才會跟著照片走、不會靜默套到別張照片上
        let entry_paths: Vec<(Option<PathBuf>, Option<PathBuf>)> = self
            .sub_entries
            .iter()
            .map(|e| {
                let at = |n: usize| n.checked_sub(1).and_then(|i| old_photos.get(i).cloned());
                (at(e.start), at(e.end))
            })
            .collect();
        natural_sort(&mut self.photos);
        if let Some(p) = selected_path {
            self.preview_selected = self.photos.iter().position(|q| *q == p);
        }
        if !entry_paths.is_empty() {
            let new_pos: HashMap<&PathBuf, usize> =
                self.photos.iter().enumerate().map(|(i, p)| (p, i)).collect();
            for (e, (sp, ep)) in self.sub_entries.iter_mut().zip(entry_paths) {
                if let Some(i) = sp.as_ref().and_then(|p| new_pos.get(p)) {
                    e.start = i + 1;
                }
                if let Some(i) = ep.as_ref().and_then(|p| new_pos.get(p)) {
                    e.end = i + 1;
                }
            }
        }
        if self.preview_selected.is_none() && !self.photos.is_empty() {
            self.preview_selected = Some(0);
        }
        if self.preview_selected.is_some() {
            self.mark_preview_dirty();
        }

        // 縮圖不在這裡整批解碼：縮圖列會依可視範圍按需請求（見 manage_thumbs），
        // 初始載入不再隨照片數變慢
    }

    /// 依縮圖列可視範圍按需載入縮圖，並淘汰遠離範圍的貼圖，
    /// 記憶體用量不隨照片總數成長
    fn manage_thumbs(&mut self, first: usize, last: usize) {
        /// 可視範圍外先預先解碼的張數（單側）
        const PREFETCH: usize = 64;
        /// 可視範圍外保留貼圖的張數（單側），之外的淘汰
        const KEEP: usize = 256;
        /// 超出保留數多少才啟動淘汰掃描（避免每幀掃描）
        const SLACK: usize = 192;

        let n = self.photos.len();
        // 請求可視範圍＋預取邊界內還沒有縮圖的。
        // 順序即解碼優先序：可視範圍最先，接著右側預取、再左側預取——
        // 由左到右一路排的話，用捲軸大幅跳轉時眼前的縮圖
        // 得等左側 64 張幕後預取解碼完才輪到，畫面停在一排佔位符
        let lo = first.saturating_sub(PREFETCH);
        let hi = (last + PREFETCH).min(n);
        let need: Vec<PathBuf> = (first..last)
            .chain(last..hi)
            .chain(lo..first)
            .map(|i| &self.photos[i])
            .filter(|p| !self.thumbs.contains_key(*p))
            .cloned()
            .collect();
        let keep_lo = first.saturating_sub(KEEP);
        let keep_hi = (last + KEEP).min(n);
        if !need.is_empty() {
            for p in &need {
                self.thumbs.insert(p.clone(), Thumb::Loading);
            }
            let (lock, cv) = &*self.thumb_jobs;
            let mut q = lock.lock().unwrap();
            // 新請求插到最前面：眼前看得到的優先解碼，
            // 快速捲動時不用排在已滾走區域的舊工作後面
            for p in need.into_iter().rev() {
                q.push_front(p);
            }
            // 佇列偏長時，清掉已遠離目前範圍的過時工作
            // （同時移除 Loading 標記，之後捲回來會重新請求）
            if q.len() > 64 {
                let keep: HashSet<&PathBuf> = self.photos[keep_lo..keep_hi].iter().collect();
                let thumbs = &mut self.thumbs;
                q.retain(|p| {
                    if keep.contains(p) {
                        true
                    } else {
                        thumbs.remove(p);
                        false
                    }
                });
            }
            cv.notify_all();
        }

        // 淘汰：貼圖數量明顯超過保留窗時才做一次 O(n) 掃描
        if self.thumbs.len() > (keep_hi - keep_lo) + SLACK {
            for (i, p) in self.photos.iter().enumerate() {
                if (i < keep_lo || i >= keep_hi)
                    && matches!(self.thumbs.get(p), Some(Thumb::Ready(_)))
                {
                    self.thumbs.remove(p);
                }
            }
        }
    }

    fn clear_photos(&mut self) {
        self.photos.clear();
        self.thumbs.clear();
        self.adj_overrides.clear();
        self.multi_sel.clear();
        // 文字段落以照片編號綁定，照片全沒了段落就沒有依附對象；
        // 與 remove_photo 一致（段落綁定的照片全移除時段落一併刪除），
        // 否則清空後加入另一批照片，舊文字會靜默套在不相干的新照片上
        self.sub_entries.clear();
        self.sel_text = None;
        self.native_res_cache = None;
        self.dims_cache.clear();
        // 清掉還在排隊的解碼工作，工作池不再為已移除的照片做白工
        self.thumb_jobs.0.lock().unwrap().clear();
        self.preview_selected = None;
        self.preview_tex = None;
        self.preview_error = None;
        // 主動清空不是「找不到照片」，別讓上次的提示殘留到空狀態
        self.import_found_nothing = false;
        // 清掉上一次的轉換結果橫幅（完成/失敗）：否則「清空」後底欄仍
        // 顯示「✔ 轉換完成＋舊輸出路徑」，「開啟資料夾」也指向舊輸出，
        // 與空白畫面不符。clear_photos 只在非轉檔時被呼叫，重置為 Idle
        // 安全（new_project／load_project 呼叫本函式後不必再自行重置）
        self.state = ConvertState::Idle;
    }

    /// 讓上次的轉換結果橫幅（完成/失敗）失效：照片或設定變更後上次的
    /// 輸出已過時，殘留的「✔ 轉換完成＋舊路徑」會誤導使用者、「開啟
    /// 資料夾」也指向無關的舊輸出。只清結果橫幅，不動轉檔中/閒置狀態
    fn clear_result_banner(&mut self) {
        if matches!(self.state, ConvertState::Done(_) | ConvertState::Error(_)) {
            self.state = ConvertState::Idle;
        }
    }

    fn remove_photo(&mut self, i: usize) {
        let removed = self.photos.remove(i);
        self.thumbs.remove(&removed);
        self.adj_overrides.remove(&removed);
        let was_multi = self.multi_sel.remove(&removed);
        self.clear_result_banner();
        self.dims_cache.remove(&removed);
        self.native_res_cache = None;
        // 移除的若是多選中的照片，重新同步調色滑桿顯示值到剩餘的第一張，
        // 否則滑桿仍顯示已移除照片的舊值，誤導使用者拖動時套錯絕對值
        if was_multi {
            self.sync_sel_adj();
        }

        // 文字段落以 1-based 照片編號綁定，移除照片後其後編號整體前移一位，
        // 段落須跟著調整，否則文字會靜默套到錯誤的照片上。
        // rn 為被移除照片的編號；只涵蓋被刪那張的段落（end' < start'）整段刪除
        let rn = i + 1;
        let mut removed_entries: Vec<usize> = Vec::new();
        for (k, e) in self.sub_entries.iter_mut().enumerate() {
            if e.start > rn {
                e.start -= 1;
            }
            if e.end >= rn {
                e.end -= 1;
            }
            if e.end < e.start {
                removed_entries.push(k);
            }
        }
        for k in removed_entries.into_iter().rev() {
            self.sub_entries.remove(k);
            self.sel_text = match self.sel_text {
                Some(s) if s == k => None,
                Some(s) if s > k => Some(s - 1),
                other => other,
            };
        }

        match self.preview_selected {
            Some(s) if s == i => {
                let next = if self.photos.is_empty() {
                    None
                } else {
                    Some(s.min(self.photos.len() - 1))
                };
                self.preview_selected = None;
                self.select_photo(next);
            }
            Some(s) if s > i => {
                self.preview_selected = Some(s - 1);
            }
            _ => {}
        }
    }

    fn pick_folder(&mut self) {
        if let Some(dir) = rfd::FileDialog::new()
            .set_title("選擇照片資料夾")
            .pick_folder()
        {
            let files = collect_images_in_dir(&dir);
            // 掃不到照片（空資料夾，或照片都在子資料夾）就標記，於空狀態提示
            self.import_found_nothing = files.is_empty();
            self.add_photos(files);
        }
    }

    fn pick_files(&mut self) {
        if let Some(files) = rfd::FileDialog::new()
            .set_title("選擇照片")
            .add_filter("圖片檔", IMAGE_EXTS)
            .pick_files()
        {
            // 使用者可切「所有檔案」選到不支援格式（如 HEIC）：全部不支援時
            // add_photos 會靜默過濾掉，與拖放一致地提示（見 update 的拖放處理）
            if !files.is_empty() && !files.iter().any(|p| is_image(p)) {
                self.import_found_nothing = true;
            }
            self.add_photos(files);
        }
    }

    /// 把目前的編輯狀態打包成專案檔內容
    fn project_data(&self) -> ProjectFile {
        // 覆寫依照片順序輸出，同一份專案每次存檔的內容才穩定
        let adj_overrides: Vec<(PathBuf, Adjustments)> = self
            .photos
            .iter()
            .filter_map(|p| self.adj_overrides.get(p).map(|a| (p.clone(), *a)))
            .collect();
        ProjectFile {
            version: 1,
            app_version: env!("CARGO_PKG_VERSION").into(),
            photos: self.photos.clone(),
            fps: self.fps,
            format: self.format.ext().into(),
            resolution: (self.resolution.w, self.resolution.h),
            adj: self.adj,
            adj_overrides,
            sub_entries: self.sub_entries.clone(),
            sub_font: self
                .fonts
                .get(self.sub_style.font_idx)
                .map(|(n, _)| n.clone())
                .unwrap_or_default(),
            sub_color: self.sub_style.color.to_array(),
            sub_outline_w: self.sub_style.outline_w,
            sub_outline_color: self.sub_style.outline_color.to_array(),
            sub_boxed: self.sub_style.boxed,
            transition: self.transition.id().into(),
            ken_burns: self.ken_burns,
            music_path: self.music_path.clone(),
            music_volume: self.music_volume,
            music_fade: self.music_fade,
        }
    }

    fn save_project_dialog(&mut self) {
        // 預帶目前專案的檔名，另存時不用重打
        let default_name = self
            .current_project
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("我的專案.{PROJECT_EXT}"));
        let Some(mut path) = rfd::FileDialog::new()
            .set_title("儲存專案")
            .add_filter("Photo2Video 專案", &[PROJECT_EXT])
            .set_file_name(default_name)
            .save_file()
        else {
            return;
        };
        // 使用者改掉或拿掉副檔名時補回來，之後開啟對話框的過濾器才找得到
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_none_or(|e| !e.eq_ignore_ascii_case(PROJECT_EXT))
        {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            path.set_file_name(format!("{name}.{PROJECT_EXT}"));
        }
        self.save_project_to(&path);
    }

    /// 寫入專案檔到指定路徑；成功時更新最近清單與「目前專案」
    fn save_project_to(&mut self, path: &Path) {
        let json = match serde_json::to_string_pretty(&self.project_data()) {
            Ok(j) => j,
            Err(e) => {
                rfd::MessageDialog::new()
                    .set_level(rfd::MessageLevel::Error)
                    .set_title("儲存專案失敗")
                    .set_description(format!("無法產生專案內容：\n{e}"))
                    .show();
                return;
            }
        };
        // 原子寫入：先寫臨時檔再 rename 覆蓋。直接覆寫既有專案檔時若寫到
        // 一半崩潰/斷電，會損壞使用者辛苦設定的整個專案（照片、調色、文字、
        // 音樂全丟）。先寫 .tmp 成功才置換，既有專案檔在意外時仍完好
        let tmp = path.with_extension(format!("{PROJECT_EXT}.tmp"));
        let result = std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, path));
        match result {
            Ok(()) => {
                self.remember_recent_project(path);
                self.current_project = Some(path.to_path_buf());
                self.project_saved_at = Some(Instant::now());
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp); // 失敗時不留臨時檔
                rfd::MessageDialog::new()
                    .set_level(rfd::MessageLevel::Error)
                    .set_title("儲存專案失敗")
                    .set_description(format!("無法寫入檔案：\n{e}"))
                    .show();
            }
        }
    }

    /// Ctrl+S：已知目前專案就直接覆寫存檔（快速儲存的通用慣例），
    /// 還沒存過才開「另存新檔」對話框
    fn quick_save_project(&mut self) {
        match self.current_project.clone() {
            Some(p) => self.save_project_to(&p),
            None => self.save_project_dialog(),
        }
    }

    /// 開新專案：回到空狀態，所有編輯設定回復預設
    /// （fps 保留使用者慣用值，與程式啟動時一致）
    fn new_project(&mut self) {
        self.clear_photos(); // 內含轉換結果橫幅（state）的重置
        self.adj = Adjustments::default();
        self.sel_adj = Adjustments::default();
        self.sub_entries.clear();
        self.sub_style = SubtitleStyle::default();
        self.sel_text = None;
        self.transition = Transition::None;
        self.ken_burns = false;
        self.music_path = None;
        self.music_volume = 100;
        self.music_fade = true;
        self.format = OutputFormat::Mp4;
        self.resolution = Resolution { w: 1920, h: 1080 };
        // 新專案不再屬於任何 .p2v：避免 Ctrl+S 把空白狀態覆寫進舊專案檔
        self.current_project = None;
    }

    /// 記到最近專案清單最前面（去重、最多保留 8 筆）並寫入設定檔
    fn remember_recent_project(&mut self, path: &Path) {
        // Windows 路徑不分大小寫：精確比對會讓同一專案檔因大小寫寫法
        // 不同（D:\a.p2v 與 d:\a.p2v）在清單重複出現
        self.recent_projects.retain(|p| !same_path_ci(p, path));
        self.recent_projects.insert(0, path.to_path_buf());
        self.recent_projects.truncate(8);
        save_recent_projects(&self.recent_projects);
    }

    fn open_project_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("開啟專案")
            .add_filter("Photo2Video 專案", &[PROJECT_EXT])
            .pick_file()
        {
            self.load_project(&path);
        }
    }

    /// 讀入專案檔並還原所有編輯狀態。照片檔已不在原路徑時略過該張並提醒；
    /// 文字段落的照片編號跟著平移，維持綁在同一張照片上
    fn load_project(&mut self, path: &Path) {
        // 與照片清單同因（見 add_photos）：以相對路徑開啟（CLI 參數、
        // 「開啟方式」）時，字面路徑存進最近清單會隨工作目錄失效
        let path = &std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        // 與「新專案」的確認一致：目前已有照片（可能有未存檔的編輯）時
        // 先確認再取代，否則開啟/拖入其他專案會默默清掉現有工作。
        // 啟動參數與空狀態的最近清單此時照片為空，維持一鍵直開不多問
        if !self.photos.is_empty() {
            let r = rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Warning)
                .set_title("開啟專案")
                .set_description(
                    "將以開啟的專案取代目前的照片與所有設定，尚未儲存的變更會遺失。",
                )
                .set_buttons(rfd::MessageButtons::OkCancel)
                .show();
            if r != rfd::MessageDialogResult::Ok {
                return;
            }
        }
        let pf: ProjectFile = match std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|txt| serde_json::from_str(&txt).map_err(|e| e.to_string()))
        {
            Ok(p) => p,
            Err(e) => {
                // 開不起來的檔案從最近清單移除，之後不再顯示
                self.recent_projects.retain(|p| !same_path_ci(p, path));
                save_recent_projects(&self.recent_projects);
                rfd::MessageDialog::new()
                    .set_level(rfd::MessageLevel::Error)
                    .set_title("開啟專案失敗")
                    .set_description(format!("無法讀取專案檔：\n{e}"))
                    .show();
                return;
            }
        };
        self.remember_recent_project(path);
        // 之後 Ctrl+S 直接覆寫回這個檔案
        self.current_project = Some(path.to_path_buf());

        // kept_before[i] = 原始清單前 i 張中仍存在的張數；
        // 文字段落編號用它平移，結果與逐張執行 remove_photo 一致。
        // 除了檔案存在，也要求是支援的圖片格式：專案檔可能被手改塞入
        // 非圖片路徑，不先擋下會直到轉檔才由 ffmpeg 失敗
        let orig_n = pf.photos.len();
        let exists: Vec<bool> = pf
            .photos
            .iter()
            .map(|p| p.is_file() && is_image(p))
            .collect();
        let missing = exists.iter().filter(|e| !**e).count();
        let mut kept_before = vec![0usize; orig_n + 1];
        for (i, e) in exists.iter().enumerate() {
            kept_before[i + 1] = kept_before[i] + usize::from(*e);
        }

        self.clear_photos(); // 內含轉換結果橫幅（state）的重置
        self.photos = pf
            .photos
            .iter()
            .zip(&exists)
            .filter(|(_, e)| **e)
            .map(|(p, _)| p.clone())
            .collect();
        self.fps = pf.fps.clamp(1, 60);
        // 取消尚未落盤的 fps 延遲寫入：設定檔的 fps 是「使用者慣用值」，
        // 若剛拖完滑桿（計時器還掛著）就載入專案，時間一到會把「專案的
        // fps」誤存成慣用值（關閉程式時 App::drop 的補寫同理）
        self.fps_pending_save = None;
        self.format = OutputFormat::from_ext(&pf.format).unwrap_or(OutputFormat::Mp4);
        // 只接受選單裡有的解析度，避免手改出怪尺寸讓下拉選單對不上
        self.resolution = Resolution::ALL
            .iter()
            .copied()
            .find(|r| (r.w, r.h) == pf.resolution)
            .unwrap_or(Resolution { w: 1920, h: 1080 });
        self.adj = pf.adj.clamped();
        let photo_set: HashSet<&PathBuf> = self.photos.iter().collect();
        self.adj_overrides = pf
            .adj_overrides
            .into_iter()
            .filter(|(p, _)| photo_set.contains(p))
            .map(|(p, a)| (p, a.clamped()))
            .collect();
        self.sub_entries = pf
            .sub_entries
            .into_iter()
            .filter_map(|mut e| {
                if orig_n == 0 {
                    return None;
                }
                let start = e.start.clamp(1, orig_n);
                let end = e.end.clamp(start, orig_n);
                e.start = kept_before[start - 1] + 1;
                e.end = kept_before[end];
                if e.end < e.start {
                    return None; // 整段綁定的照片都遺失了
                }
                e.x = e.x.clamp(0.0, 1.0);
                e.y = e.y.clamp(0.0, 1.0);
                e.size = e.size.clamp(8, 300);
                // 旋轉角與 UI 滑桿同範圍；手改出超界值會讓滑桿與顯示值對不上
                e.rot = e.rot.clamp(-180.0, 180.0);
                Some(e)
            })
            .collect();
        let from_rgba =
            |c: [u8; 4]| egui::Color32::from_rgba_premultiplied(c[0], c[1], c[2], c[3]);
        self.sub_style = SubtitleStyle {
            font_idx: self
                .fonts
                .iter()
                .position(|(n, _)| *n == pf.sub_font)
                .unwrap_or(0),
            color: from_rgba(pf.sub_color),
            outline_w: pf.sub_outline_w.clamp(0, 8),
            outline_color: from_rgba(pf.sub_outline_color),
            boxed: pf.sub_boxed,
        };
        self.sel_text = None;
        self.transition = Transition::from_id(&pf.transition).unwrap_or(Transition::None);
        self.ken_burns = pf.ken_burns;
        // 音樂檔遺失就整組略過（照片少幾張還能用，音樂缺檔設定就沒意義）
        let music_missing = matches!(&pf.music_path, Some(p) if !p.is_file());
        self.music_path = pf.music_path.filter(|p| p.is_file());
        self.music_volume = pf.music_volume.clamp(0, 200);
        self.music_fade = pf.music_fade;

        if !self.photos.is_empty() {
            self.preview_selected = Some(0);
            self.mark_preview_dirty();
        }
        if missing > 0 || music_missing {
            let mut lines = Vec::new();
            if missing > 0 {
                lines.push(format!(
                    "有 {missing} 張照片已不在原路徑（或不是支援的圖片檔），已從清單移除。"
                ));
            }
            if music_missing {
                lines.push("背景音樂檔已不在原路徑，已清除音樂設定。".into());
            }
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Warning)
                .set_title("專案已開啟，但部分檔案遺失")
                .set_description(lines.join("\n"))
                .show();
        }
    }

    fn start_convert(&mut self, ctx: &egui::Context) {
        let ext = self.format.ext();
        // 有開啟專案就用專案名當輸出預設檔名（如「日本旅遊.p2v」→
        // 「日本旅遊.mp4」），比固定的 output 更貼合使用者、不用每次改名
        let stem = self
            .current_project
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".into());
        let Some(mut output) = rfd::FileDialog::new()
            .set_title("選擇影片儲存位置")
            .add_filter(format!("{} 影片", ext.to_uppercase()), &[ext])
            .set_file_name(format!("{stem}.{ext}"))
            .save_file()
        else {
            return;
        };

        // 確保輸出副檔名與所選格式一致：對話框允許使用者改掉或拿掉副檔名，
        // 但無副檔名時 ffmpeg 無法判斷容器會直接失敗，副檔名與格式不符則會
        // 產生不相容檔案（如 VP9 塞進 mp4）。已正確則不動；是其他影片副檔名
        // 就替換；無副檔名或非影片副檔名則附加，保留使用者輸入的檔名
        let known = ["mp4", "mkv", "mov", "avi", "webm"];
        match output
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
        {
            Some(e) if e == ext => {}
            Some(e) if known.contains(&e.as_str()) => {
                output.set_extension(ext);
            }
            _ => {
                let name = output
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                output.set_file_name(format!("{name}.{ext}"));
            }
        }

        let (tx, rx) = std::sync::mpsc::channel();
        self.rx = Some(rx);
        self.convert_output = Some(output.clone());
        // 開新轉檔前清掉上次的取消旗標
        CONVERT_CANCEL.store(false, Ordering::Relaxed);
        self.state = ConvertState::Working {
            progress: 0.0,
            status: "準備中…".into(),
        };
        let photos = self.photos.clone();
        let fps = self.fps;
        let format = self.format;
        let res = self.resolved_resolution();
        let adj = self.adj;
        let adj_overrides = self.adj_overrides.clone();
        let total = self.photos.len();
        let subs = SubtitleJob {
            style: self.sub_style.clone(),
            font: self
                .fonts
                .get(self.sub_style.font_idx)
                .map(|(_, p)| p.clone()),
            entries: self
                .sub_entries
                .iter()
                .filter(|e| !e.text.trim().is_empty())
                .filter_map(|e| {
                    let s = e.start.max(1) - 1;
                    let end = e.end.min(total);
                    (s < end).then(|| TextJob {
                        s,
                        e: end - 1,
                        text: e.text.clone(),
                        x: e.x,
                        y: e.y,
                        size: e.size,
                        rot: e.rot,
                    })
                })
                .collect(),
        };
        let fx = OutputFx {
            transition: self.transition,
            ken_burns: self.ken_burns,
            music: self.music_path.clone().map(|path| MusicJob {
                path,
                volume: self.music_volume,
                fade_out: self.music_fade,
            }),
        };
        let ctx = ctx.clone();

        thread::spawn(move || {
            let send = |msg: WorkerMsg| {
                let _ = tx.send(msg);
                ctx.request_repaint();
            };
            match run_conversion(
                &photos,
                fps,
                format,
                res,
                &adj,
                &adj_overrides,
                &subs,
                &fx,
                &output,
                &send,
            ) {
                Ok(()) => send(WorkerMsg::Done(output.clone())),
                Err(e) => send(WorkerMsg::Error(e)),
            }
        });
    }

    /// 使用者按「取消」：設旗標並終止 ffmpeg。worker 的 ffmpeg 被 kill 後
    /// run_conversion 回錯誤，退回軟體編碼的邏輯因旗標而不重跑；
    /// poll_worker 收到錯誤時據旗標把狀態設回 Idle（取消不算轉換失敗），
    /// 半成品輸出檔由 run_conversion 的失敗清理刪除
    fn cancel_convert(&mut self) {
        if !self.is_working() {
            return;
        }
        CONVERT_CANCEL.store(true, Ordering::Relaxed);
        let pid = CONVERT_FFMPEG_PID.swap(0, Ordering::Relaxed);
        if pid != 0 {
            kill_pid(pid);
        }
        if let ConvertState::Working { status, .. } = &mut self.state {
            *status = "正在取消…".into();
        }
    }

    /// 在背景執行緒檢查 GitHub 是否有新版本
    fn spawn_update_check(&mut self, ctx: &egui::Context) {
        // ReadyToRestart 也要擋：新版 exe 已替換到磁碟上，重新檢查會把
        // 「待重啟」狀態洗掉，又回報有新版，導致使用者重複下載整包更新
        if matches!(
            self.update_status,
            UpdateStatus::Checking | UpdateStatus::Downloading(_) | UpdateStatus::ReadyToRestart
        ) {
            return;
        }
        // 清掉上次的下載失敗訊息：重新檢查代表要重走更新流程，否則檢查完
        // 底欄會同時顯示「有新版本」與殘留的「更新下載失敗」，互相矛盾
        self.update_download_error = None;
        self.update_status = UpdateStatus::Checking;
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_rx = Some(rx);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(UpdateMsg::CheckResult(check_latest_release()));
            ctx.request_repaint();
        });
    }

    /// 在背景下載新版並替換執行檔，完成後自動重新啟動
    fn spawn_self_update(&mut self, ctx: &egui::Context, tag: String) {
        if matches!(self.update_status, UpdateStatus::Downloading(_)) {
            return;
        }
        self.update_status = UpdateStatus::Downloading(0.0);
        // 重試時清掉上次的下載失敗訊息
        self.update_download_error = None;
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_rx = Some(rx);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let progress = |p: f32| {
                let _ = tx.send(UpdateMsg::Progress(p));
                ctx.request_repaint();
            };
            let msg = match download_update(&tag, &progress) {
                Ok(()) => UpdateMsg::Ready,
                Err(e) => UpdateMsg::Failed(e),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    fn poll_update(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let Some(rx) = &self.update_rx else { return };
        let mut finished = false;
        let mut restart = false;
        loop {
            match rx.try_recv() {
                Ok(UpdateMsg::CheckResult(res)) => {
                    finished = true;
                    self.update_status = match res {
                        Ok(Some(tag)) => UpdateStatus::Available(tag),
                        Ok(None) => UpdateStatus::UpToDate,
                        Err(e) => UpdateStatus::Failed(e),
                    };
                }
                Ok(UpdateMsg::Progress(p)) => self.update_status = UpdateStatus::Downloading(p),
                Ok(UpdateMsg::Ready) => {
                    finished = true;
                    restart = true;
                }
                Ok(UpdateMsg::Failed(e)) => {
                    finished = true;
                    // 只有下載才會送 UpdateMsg::Failed（使用者主動觸發），
                    // 記下供底欄告知；檢查失敗走 CheckResult(Err) 不設此
                    self.update_download_error = Some(e.clone());
                    self.update_status = UpdateStatus::Failed(e);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // 更新執行緒異常結束沒回報：不處理的話狀態卡在
                    // 「檢查中/下載中」，busy 旗標讓「檢查更新」永遠按不下去
                    if !finished
                        && matches!(
                            self.update_status,
                            UpdateStatus::Checking | UpdateStatus::Downloading(_)
                        )
                    {
                        self.update_status = UpdateStatus::Failed("更新程序異常中斷".into());
                    }
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            self.update_rx = None;
        }
        if restart {
            if self.is_working() {
                // 轉檔進行中不強制重啟，等使用者按「重新啟動完成更新」
                self.update_status = UpdateStatus::ReadyToRestart;
            } else {
                self.restart_for_update();
            }
        }
    }

    /// 更新完成後的重新啟動。restart_app 以 process::exit 結束、不會執行
    /// App::drop：尚未寫入的 fps 設定（變動後延遲 800ms 才寫檔）要在這裡
    /// 先補寫，否則重啟瞬間剛調過的 fps 會靜默遺失
    fn restart_for_update(&mut self) {
        if self.fps_pending_save.take().is_some() {
            save_fps(self.fps);
        }
        restart_app();
    }

    fn poll_worker(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let Some(rx) = &self.rx else { return };
        let mut done = false;
        loop {
            match rx.try_recv() {
                Ok(WorkerMsg::Status(s)) => {
                    if let ConvertState::Working { status, .. } = &mut self.state {
                        *status = s;
                    }
                }
                Ok(WorkerMsg::Progress(p)) => {
                    if let ConvertState::Working { progress, .. } = &mut self.state {
                        *progress = p;
                    }
                }
                Ok(WorkerMsg::Done(path)) => {
                    self.state = ConvertState::Done(path);
                    self.convert_output = None;
                    done = true;
                }
                Ok(WorkerMsg::Error(e)) => {
                    // 半成品輸出檔已由 run_conversion 的清理刪除。
                    // 使用者取消造成的中斷回到 Idle（不顯示為轉換失敗）
                    self.state = if CONVERT_CANCEL.swap(false, Ordering::Relaxed) {
                        ConvertState::Idle
                    } else {
                        ConvertState::Error(e)
                    };
                    self.convert_output = None;
                    done = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // 轉檔執行緒 panic 等異常結束而沒回報結果：不處理的話
                    // 狀態永遠停在「轉換中」、按鈕全部鎖死，只能重開程式。
                    // 已收到 Done/Error（is_working 為否）則屬正常收尾，不覆蓋
                    if self.is_working() {
                        // run_conversion 的收尾（終止 ffmpeg、刪半成品）沒機會
                        // 執行，在這裡補做，否則 ffmpeg 繼續在背景寫損毀的檔案
                        let pid = CONVERT_FFMPEG_PID.swap(0, Ordering::Relaxed);
                        if pid != 0 {
                            kill_pid(pid);
                        }
                        if let Some(out) = self.convert_output.take() {
                            let _ = std::fs::remove_file(&out);
                        }
                        self.state = ConvertState::Error(
                            "轉換程序異常中斷（可能為程式錯誤），請重試或回報問題".into(),
                        );
                    }
                    done = true;
                    break;
                }
            }
        }
        if done {
            self.rx = None;
        }
    }

    // ---------- UI ----------

    fn ui_bottom_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("footer")
            .frame(
                egui::Frame::default()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(16, 12)),
            )
            .show(ctx, |ui| {
                // 上次執行閃退：顯示回報橫幅（開 GitHub 回報頁／關閉）。
                // 不在這裡 clone 內容：報告含 backtrace 可達數十 KB，
                // 橫幅顯示期間每幀複製一次是純浪費；只在點擊時借用組網址
                if self.crash_report.is_some() {
                    let mut open_report = false;
                    let mut dismiss = false;
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("⚠ 程式上次異常關閉")
                                .size(12.0)
                                .strong()
                                .color(theme::ERROR),
                        );
                        if ui
                            .small_button("🔗 回報問題")
                            .on_hover_text("開啟回報頁面（已帶入錯誤與版本）")
                            .clicked()
                        {
                            open_report = true;
                        }
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.small_button("✕").on_hover_text("隱藏通知").clicked() {
                                    dismiss = true;
                                }
                            },
                        );
                    });
                    ui.add_space(8.0);
                    if open_report {
                        if let Some(report) = &self.crash_report {
                            // panic 訊息與位置在最前面一定保得住，
                            // 截掉的只有 backtrace 尾段（strip 後僅剩位址，價值不高）
                            let excerpt = truncate_for_url(report, 1500);
                            let body = format!(
                                "版本：v{}\n作業系統：Windows\n\n閃退紀錄：\n{excerpt}\n\n（發生了什麼、當時在做哪個操作，可補充於此）",
                                env!("CARGO_PKG_VERSION")
                            );
                            let url = format!(
                                "https://github.com/{GITHUB_REPO}/issues/new?title={}&body={}",
                                urlencode("程式閃退回報"),
                                urlencode(&body)
                            );
                            ctx.open_url(egui::OpenUrl::new_tab(url));
                        }
                    }
                    if dismiss {
                        self.crash_report = None;
                    }
                }

                // 新版本通知：點「立即更新」直接下載並自動重啟完成更新
                let new_tag = match &self.update_status {
                    UpdateStatus::Available(tag) if !self.update_banner_dismissed => {
                        Some(tag.clone())
                    }
                    _ => None,
                };
                if let Some(tag) = new_tag {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("⬆ 有新版本 {tag} 可以下載"))
                                .size(12.0)
                                .strong()
                                .color(theme::SUCCESS),
                        );
                        if ui.small_button("立即更新").clicked() {
                            self.spawn_self_update(ctx, tag.clone());
                        }
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.small_button("✕").on_hover_text("隱藏通知").clicked() {
                                    self.update_banner_dismissed = true;
                                }
                            },
                        );
                    });
                    ui.add_space(8.0);
                }
                if let UpdateStatus::Downloading(p) = &self.update_status {
                    let p = *p;
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(13.0).color(theme::ACCENT));
                        ui.label(
                            egui::RichText::new(format!("正在下載更新… {:.0}%", p * 100.0))
                                .size(12.0)
                                .color(theme::TEXT_WEAK),
                        );
                    });
                    ui.add(
                        egui::ProgressBar::new(p)
                            .fill(theme::ACCENT)
                            .desired_height(6.0),
                    );
                    ui.add_space(8.0);
                }
                if matches!(self.update_status, UpdateStatus::ReadyToRestart) {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("✔ 新版已下載完成")
                                .size(12.0)
                                .strong()
                                .color(theme::SUCCESS),
                        );
                        let working = self.is_working();
                        let mut btn =
                            ui.add_enabled(!working, egui::Button::new("重新啟動完成更新").small());
                        if working {
                            btn = btn.on_disabled_hover_text("轉換完成後即可重新啟動");
                        }
                        if btn.clicked() {
                            self.restart_for_update();
                        }
                    });
                    ui.add_space(8.0);
                }
                // 下載更新失敗：使用者從「立即更新」觸發後失敗，若不顯示，
                // banner 的下載中訊息消失後就毫無回饋，使用者不知已失敗
                if let Some(err) = self.update_download_error.clone() {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("✖ 更新下載失敗")
                                .size(12.0)
                                .strong()
                                .color(theme::ERROR),
                        )
                        .on_hover_text(&err);
                        ui.hyperlink_to(
                            egui::RichText::new("改用瀏覽器下載").size(12.0),
                            format!("https://github.com/{GITHUB_REPO}/releases/latest"),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.small_button("✕").on_hover_text("隱藏通知").clicked() {
                                    self.update_download_error = None;
                                }
                            },
                        );
                    });
                    ui.add_space(8.0);
                }

                // Ctrl+S 直接覆寫不開對話框，短暫顯示已儲存供使用者確認
                if let Some(t) = self.project_saved_at {
                    let elapsed = t.elapsed();
                    if elapsed < Duration::from_millis(2500) {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("✔ 專案已儲存")
                                    .size(12.0)
                                    .strong()
                                    .color(theme::SUCCESS),
                            );
                            if let Some(p) = &self.current_project {
                                ui.label(
                                    egui::RichText::new(
                                        p.file_name().unwrap_or_default().to_string_lossy(),
                                    )
                                    .size(11.5)
                                    .color(theme::TEXT_WEAK),
                                );
                            }
                        });
                        ui.add_space(8.0);
                        ctx.request_repaint_after(
                            Duration::from_millis(2550) - elapsed,
                        );
                    } else {
                        self.project_saved_at = None;
                    }
                }

                // 狀態列
                let mut cancel_convert = false;
                match &self.state {
                    ConvertState::Idle => {}
                    ConvertState::Working { progress, status } => {
                        let (progress, status) = (*progress, status.clone());
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(13.0).color(theme::ACCENT));
                            ui.label(
                                egui::RichText::new(status)
                                    .size(12.0)
                                    .color(theme::TEXT_WEAK),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .small_button("✕ 取消")
                                        .on_hover_text("停止轉換並刪除未完成的影片")
                                        .clicked()
                                    {
                                        cancel_convert = true;
                                    }
                                    ui.add_space(8.0);
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{:.0}%",
                                            progress * 100.0
                                        ))
                                        .size(12.0)
                                        .strong()
                                        .color(theme::ACCENT),
                                    );
                                },
                            );
                        });
                        ui.add(
                            egui::ProgressBar::new(progress)
                                .fill(theme::ACCENT)
                                .desired_height(6.0),
                        );
                        ui.add_space(8.0);
                    }
                    ConvertState::Done(path) => {
                        let path = path.clone();
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("✔ 轉換完成")
                                    .strong()
                                    .color(theme::SUCCESS),
                            );
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(path.display().to_string())
                                        .size(11.5)
                                        .color(theme::TEXT_WEAK),
                                )
                                .truncate(),
                            );
                            if ui
                                .small_button("▶ 播放")
                                .on_hover_text("用預設播放器開啟這支影片")
                                .clicked()
                            {
                                open_file(&path);
                            }
                            if ui
                                .small_button("開啟資料夾")
                                .on_hover_text("在檔案總管中開啟並選取這支影片")
                                .clicked()
                            {
                                open_in_explorer(&path);
                            }
                        });
                        ui.add_space(8.0);
                    }
                    ConvertState::Error(e) => {
                        let e = e.clone();
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(format!("✖ 轉換失敗：{e}"))
                                        .color(theme::ERROR),
                                )
                                .truncate(),
                            );
                            // 快速回報：開 GitHub 回報頁（已帶入錯誤與版本）
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .small_button("🔗 回報問題")
                                        .on_hover_text("開啟回報頁面（已帶入錯誤與版本）")
                                        .clicked()
                                    {
                                        // 以編碼後長度截短帶入 URL：400「字元」的中文經
                                        // 百分比編碼可達 3600 字元，一樣超過系統上限
                                        let short_e = truncate_for_url(&e, 1500);
                                        let body = format!(
                                            "版本：v{}\n作業系統：Windows\n\n錯誤訊息：\n{short_e}\n\n（發生了什麼、用了哪些設定，可補充於此）",
                                            env!("CARGO_PKG_VERSION")
                                        );
                                        let url = format!(
                                            "https://github.com/{GITHUB_REPO}/issues/new?title={}&body={}",
                                            urlencode("轉換失敗回報"),
                                            urlencode(&body)
                                        );
                                        // 用 egui 的 open_url（走系統預設瀏覽器）而非 explorer：
                                        // explorer 對帶 query string（含 &）的網址解析不可靠，
                                        // 可能開不了或只帶到 & 之前
                                        ctx.open_url(egui::OpenUrl::new_tab(url));
                                    }
                                },
                            );
                        });
                        ui.add_space(8.0);
                    }
                }
                if cancel_convert {
                    self.cancel_convert();
                }

                let working = self.is_working();
                let res_before = self.resolution;
                let fps_before = self.fps;

                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!working, |ui| {
                        ui.label(egui::RichText::new("每秒張數").color(theme::TEXT_WEAK));
                        ui.add(
                            egui::DragValue::new(&mut self.fps)
                                .range(1..=60)
                                .speed(0.1)
                                .suffix(" fps"),
                        );
                        ui.add_space(12.0);
                        ui.label(egui::RichText::new("格式").color(theme::TEXT_WEAK));
                        egui::ComboBox::from_id_salt("format")
                            .selected_text(self.format.label())
                            .width(130.0)
                            .show_ui(ui, |ui| {
                                for f in OutputFormat::ALL {
                                    ui.selectable_value(&mut self.format, f, f.label());
                                }
                            });
                        ui.add_space(12.0);
                        ui.label(egui::RichText::new("解析度").color(theme::TEXT_WEAK));
                        egui::ComboBox::from_id_salt("resolution")
                            .selected_text(self.resolution.label())
                            .width(170.0)
                            .show_ui(ui, |ui| {
                                for r in Resolution::ALL {
                                    ui.selectable_value(&mut self.resolution, r, r.label());
                                }
                            });
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let can_convert = !self.photos.is_empty() && !working;
                        if primary_button(ui, "▶  開始轉換", can_convert).clicked() {
                            self.start_convert(ctx);
                        }
                        ui.add_space(6.0);
                        if ui
                            .button(
                                egui::RichText::new(concat!("ℹ v", env!("CARGO_PKG_VERSION")))
                                    .size(12.0)
                                    .color(theme::TEXT_WEAK),
                            )
                            .on_hover_text("關於與檢查更新")
                            .clicked()
                        {
                            self.about_open = !self.about_open;
                        }
                    });
                });

                if self.resolution != res_before {
                    self.mark_preview_dirty();
                }
                if self.fps != fps_before {
                    // 拖動中每格變動都會進來，延後到停止變動再寫檔（見 update）
                    self.fps_pending_save = Some(Instant::now());
                }
            });
    }

    fn ui_side_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("adjust_panel")
            .frame(
                egui::Frame::default()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(14, 12)),
            )
            .default_width(330.0)
            .min_width(300.0)
            .max_width(430.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.ui_adjust_section(ui);
                        ui.add_space(14.0);
                        ui.separator();
                        ui.add_space(10.0);
                        self.ui_text_section(ui);
                        ui.add_space(14.0);
                        ui.separator();
                        ui.add_space(10.0);
                        self.ui_fx_section(ui);
                        ui.add_space(8.0);
                    });
            });
    }

    fn ui_adjust_section(&mut self, ui: &mut egui::Ui) {
        // 多選模式：滑桿改編輯 sel_adj，寫入所有選取照片的覆寫；否則編輯全域 adj
        let scoped = !self.multi_sel.is_empty();
        let orig = if scoped { self.sel_adj } else { self.adj };
        let mut work = orig;
        let mut changed = false;
        let mut unpin = false;
        let mut reset_all = false;

        let title = if scoped {
            format!("調色（選取 {} 張）", self.multi_sel.len())
        } else {
            "調色".to_string()
        };
        ui.horizontal(|ui| {
            section_toggle(ui, &title, &mut self.sec_adjust_open);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !work.is_neutral() && ui.small_button("↺ 重設").clicked() {
                    work = Adjustments::default();
                    changed = true;
                    reset_all = true;
                }
                if scoped && ui.small_button("取消個別調色").clicked() {
                    unpin = true;
                }
            });
        });

        if self.sec_adjust_open {
            let hint = if scoped {
                "只套用到選取的照片；Ctrl+點縮圖增減選取，直接點縮圖離開多選"
            } else {
                "套用到每一張照片，滑桿連點兩下可歸零；Ctrl+點縮圖可只調特定照片"
            };
            ui.label(
                egui::RichText::new(hint)
                    .size(11.0)
                    .color(theme::TEXT_WEAK),
            );
            ui.add_space(10.0);

            let before = work;
            group_label(ui, "白平衡");
            // 滑桿方向與濾鏡一致：色溫 + 偏暖（黃）、色調 + 偏洋紅
            adj_slider_rail(
                ui,
                &mut work.temp,
                "色溫",
                Some((
                    egui::Color32::from_rgb(0x50, 0x78, 0xE0),
                    egui::Color32::from_rgb(0xE0, 0xC8, 0x46),
                )),
            );
            adj_slider_rail(
                ui,
                &mut work.tint,
                "色調",
                Some((
                    egui::Color32::from_rgb(0x55, 0xC0, 0x50),
                    egui::Color32::from_rgb(0xD8, 0x5C, 0xC8),
                )),
            );
            ui.add_space(10.0);

            group_label(ui, "光線");
            adj_slider(ui, &mut work.exposure, "曝光度");
            adj_slider(ui, &mut work.contrast, "對比");
            adj_slider(ui, &mut work.brightness, "亮度");
            adj_slider(ui, &mut work.shadows, "陰影");
            adj_slider(ui, &mut work.whites, "白色");
            adj_slider(ui, &mut work.blacks, "黑色");
            ui.add_space(10.0);

            group_label(ui, "質感與色彩");
            adj_slider(ui, &mut work.clarity, "清晰度");
            adj_slider(ui, &mut work.vibrance, "鮮豔度");
            adj_slider(ui, &mut work.saturation, "飽和度");

            if work != before {
                changed = true;
            }
        }

        if unpin {
            // 移除選取照片的覆寫，回到跟隨全域調色
            for p in &self.multi_sel {
                self.adj_overrides.remove(p);
            }
            self.sync_sel_adj();
            self.mark_preview_dirty();
        } else if changed {
            if scoped {
                // 滑桿顯示的是第一張選取照片的值：整組覆蓋會把其他選取
                // 照片在「別的參數」上的個別設定靜默蓋成第一張的值。
                // 只把這次實際改動的欄位套進每張照片自己的調色；
                // 「重設」才是明確要求整組歸零，維持全部覆蓋
                self.sel_adj = work;
                for p in &self.multi_sel {
                    let mut a = if reset_all {
                        Adjustments::default()
                    } else {
                        self.effective_adj(p)
                    };
                    if !reset_all {
                        for ((dst, o), n) in a
                            .values_mut()
                            .into_iter()
                            .zip(orig.values())
                            .zip(work.values())
                        {
                            if n != o {
                                *dst = n;
                            }
                        }
                    }
                    self.adj_overrides.insert(p.clone(), a);
                }
            } else {
                self.adj = work;
            }
            self.mark_preview_dirty();
        }
    }

    fn ui_text_section(&mut self, ui: &mut egui::Ui) {
        let total = self.photos.len().max(1);

        ui.horizontal(|ui| {
            section_toggle(ui, "文字", &mut self.sec_sub_open);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("＋ 新增文字").clicked() {
                    let start = self.preview_selected.map(|i| i + 1).unwrap_or(1);
                    self.sub_entries.push(SubtitleEntry::new(start, total));
                    self.sel_text = Some(self.sub_entries.len() - 1);
                    self.sec_sub_open = true; // 收合時新增 → 自動展開
                }
            });
        });
        if !self.sec_sub_open {
            return;
        }
        ui.label(
            egui::RichText::new("在中央預覽直接拖曳文字調整位置；點選文字可縮放與旋轉")
                .size(11.0)
                .color(theme::TEXT_WEAK),
        );
        ui.add_space(8.0);

        let mut remove_entry: Option<usize> = None;
        let mut select_entry: Option<usize> = None;
        for (k, entry) in self.sub_entries.iter_mut().enumerate() {
            let selected = self.sel_text == Some(k);
            egui::Frame::default()
                .fill(theme::CARD)
                .corner_radius(8)
                .stroke(if selected {
                    egui::Stroke::new(1.5, theme::ACCENT)
                } else {
                    egui::Stroke::NONE
                })
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Label::new(
                                    egui::RichText::new(format!("文字 {}", k + 1))
                                        .strong()
                                        .size(12.5)
                                        .color(theme::ACCENT),
                                )
                                .sense(egui::Sense::click()),
                            )
                            .on_hover_text("點擊可在預覽中選取這段文字")
                            .clicked()
                        {
                            select_entry = Some(k);
                        }
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new("第").size(12.0).color(theme::TEXT_WEAK));
                        let r1 =
                            ui.add(egui::DragValue::new(&mut entry.start).range(1..=total));
                        ui.label(egui::RichText::new("到").size(12.0).color(theme::TEXT_WEAK));
                        let r2 = ui.add(egui::DragValue::new(&mut entry.end).range(1..=total));
                        ui.label(egui::RichText::new("張").size(12.0).color(theme::TEXT_WEAK));
                        if (r1.changed() || r2.changed()) && entry.end < entry.start {
                            entry.end = entry.start;
                        }
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.small_button("🗑").on_hover_text("刪除這段文字").clicked()
                                {
                                    remove_entry = Some(k);
                                }
                            },
                        );
                    });
                    let resp = ui.add(
                        egui::TextEdit::multiline(&mut entry.text)
                            .desired_rows(2)
                            .desired_width(f32::INFINITY)
                            .hint_text("這段照片要顯示的文字（可多行）"),
                    );
                    if resp.gained_focus() {
                        select_entry = Some(k);
                    }
                    slider_row(ui, &mut entry.size, 8, 300, "大小");
                    // 旋轉（f32 度數）：連點兩下歸零
                    ui.horizontal(|ui| {
                        let (rect, _) = ui
                            .allocate_exact_size(egui::vec2(30.0, 18.0), egui::Sense::hover());
                        ui.painter().text(
                            rect.left_center(),
                            egui::Align2::LEFT_CENTER,
                            "旋轉",
                            egui::FontId::proportional(12.5),
                            theme::TEXT_WEAK,
                        );
                        ui.spacing_mut().slider_width =
                            (ui.available_width() - 44.0).max(60.0);
                        let resp = drop_slider(ui, &mut entry.rot, -180.0, 180.0);
                        if resp.hovered()
                            && ui.input(|i| {
                                i.pointer
                                    .button_double_clicked(egui::PointerButton::Primary)
                            })
                        {
                            entry.rot = 0.0;
                        }
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.add_space(8.0);
                                let color = if entry.rot.abs() > 0.01 {
                                    theme::ACCENT
                                } else {
                                    theme::TEXT_WEAK
                                };
                                ui.label(
                                    egui::RichText::new(format!("{:.0}°", entry.rot))
                                        .size(11.5)
                                        .color(color),
                                );
                            },
                        );
                    });
                });
            ui.add_space(6.0);
        }
        if let Some(k) = select_entry {
            self.sel_text = Some(k);
        }
        if let Some(k) = remove_entry {
            self.sub_entries.remove(k);
            self.sel_text = match self.sel_text {
                Some(s) if s == k => None,
                Some(s) if s > k => Some(s - 1),
                other => other,
            };
        }
        if self.sub_entries.is_empty() {
            ui.label(
                egui::RichText::new("尚未加入文字，點右上「＋ 新增文字」開始")
                    .size(11.5)
                    .color(theme::TEXT_WEAK),
            );
        }
        ui.add_space(10.0);

        if self.fonts.is_empty() {
            ui.colored_label(theme::ERROR, "找不到可用的系統字型，文字功能無法使用");
        } else {
            group_label(ui, "文字樣式（全部共用）");
            ui.add_space(2.0);
            egui::Frame::default()
                .fill(theme::CARD)
                .corner_radius(8)
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("字型").color(theme::TEXT_WEAK));
                        let cur = self
                            .fonts
                            .get(self.sub_style.font_idx)
                            .map(|(n, _)| n.as_str())
                            .unwrap_or("？");
                        egui::ComboBox::from_id_salt("sub_font")
                            .selected_text(cur)
                            .width(ui.available_width() - 8.0)
                            .show_ui(ui, |ui| {
                                for (i, (name, _)) in self.fonts.iter().enumerate() {
                                    ui.selectable_value(
                                        &mut self.sub_style.font_idx,
                                        i,
                                        name,
                                    );
                                }
                            });
                    });
                    slider_row(ui, &mut self.sub_style.outline_w, 0, 8, "外框");
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("文字").color(theme::TEXT_WEAK));
                        ui.color_edit_button_srgba(&mut self.sub_style.color);
                        ui.add_space(10.0);
                        ui.label(egui::RichText::new("外框").color(theme::TEXT_WEAK));
                        ui.color_edit_button_srgba(&mut self.sub_style.outline_color);
                        ui.add_space(10.0);
                        ui.checkbox(&mut self.sub_style.boxed, "半透明底框");
                    });
                });
        }
    }

    fn ui_fx_section(&mut self, ui: &mut egui::Ui) {
        section_toggle(ui, "轉場與音樂", &mut self.sec_fx_open);
        if !self.sec_fx_open {
            return;
        }
        ui.label(
            egui::RichText::new("啟用轉場或動態縮放時，輸出會提升為 30fps（轉檔時間較長）")
                .size(11.0)
                .color(theme::TEXT_WEAK),
        );
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("轉場").color(theme::TEXT_WEAK));
            egui::ComboBox::from_id_salt("transition")
                .selected_text(self.transition.label())
                .width(160.0)
                .show_ui(ui, |ui| {
                    for t in Transition::ALL {
                        ui.selectable_value(&mut self.transition, t, t.label());
                    }
                });
        });
        ui.checkbox(&mut self.ken_burns, "動態縮放（Ken Burns 緩慢推近）");
        ui.add_space(10.0);

        group_label(ui, "背景音樂");
        ui.add_space(2.0);
        let mut remove_music = false;
        egui::Frame::default()
            .fill(theme::CARD)
            .corner_radius(8)
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                match &self.music_path {
                    None => {
                        if ui.button("🎵  選擇音樂檔").clicked() {
                            if let Some(f) = rfd::FileDialog::new()
                                .set_title("選擇背景音樂")
                                .add_filter("音訊檔", AUDIO_EXTS)
                                .pick_file()
                            {
                                self.music_path = Some(f);
                            }
                        }
                        ui.label(
                            egui::RichText::new(
                                "支援 MP3、WAV、M4A、FLAC、OGG，也可以直接拖進視窗",
                            )
                            .size(11.0)
                            .color(theme::TEXT_WEAK),
                        );
                    }
                    Some(p) => {
                        let name = p.file_name().unwrap_or_default().to_string_lossy();
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("🎵").size(13.0));
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(name.as_ref())
                                        .size(12.5)
                                        .color(theme::TEXT),
                                )
                                .truncate(),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.small_button("✕").on_hover_text("移除音樂").clicked()
                                    {
                                        remove_music = true;
                                    }
                                },
                            );
                        });
                        slider_row(ui, &mut self.music_volume, 0, 200, "音量");
                        ui.checkbox(&mut self.music_fade, "結尾自動淡出（最多 2 秒）");
                        ui.label(
                            egui::RichText::new("音樂比影片短會自動循環，比影片長會自動裁切")
                                .size(11.0)
                                .color(theme::TEXT_WEAK),
                        );
                    }
                }
            });
        if remove_music {
            self.music_path = None;
        }
    }

    fn ui_central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ctx, |ui| {
                if self.photos.is_empty() {
                    self.ui_empty_state(ui);
                } else {
                    self.ui_workspace(ui, ctx);
                }
            });
    }

    fn ui_empty_state(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        let r = rect.shrink(4.0);
        ui.painter()
            .rect_filled(r, 14, egui::Color32::from_rgb(0x17, 0x18, 0x1C));
        // 虛線外框
        let dash = egui::Stroke::new(1.2, egui::Color32::from_rgb(0x3A, 0x3D, 0x46));
        let rr = r.shrink(1.5);
        for (a, b) in [
            (rr.left_top(), rr.right_top()),
            (rr.right_top(), rr.right_bottom()),
            (rr.right_bottom(), rr.left_bottom()),
            (rr.left_bottom(), rr.left_top()),
        ] {
            ui.painter().extend(egui::Shape::dashed_line(&[a, b], dash, 7.0, 6.0));
        }

        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(r)
                .layout(egui::Layout::top_down(egui::Align::Center)),
        );
        let content_h = 210.0;
        child.add_space(((r.height() - content_h) / 2.0).max(24.0));
        child.label(egui::RichText::new("🎬").size(46.0));
        child.add_space(8.0);
        child.label(
            egui::RichText::new("將照片拖曳到這裡")
                .size(17.0)
                .strong()
                .color(theme::TEXT),
        );
        child.add_space(2.0);
        child.label(
            egui::RichText::new("支援 JPG、PNG、BMP、WebP、TIFF，會依檔名自動排序")
                .size(12.0)
                .color(theme::TEXT_WEAK),
        );
        child.add_space(16.0);
        child.allocate_ui_with_layout(
            egui::vec2(330.0, 40.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                if primary_button(ui, "📁  選擇資料夾", true).clicked() {
                    self.pick_folder();
                }
                ui.add_space(4.0);
                if ui
                    .add(egui::Button::new("🖼  選擇照片").min_size(egui::vec2(120.0, 38.0)))
                    .clicked()
                {
                    self.pick_files();
                }
            },
        );
        child.add_space(10.0);
        if child
            .add(egui::Button::new(
                egui::RichText::new("📂  開啟之前存的專案…")
                    .size(12.5)
                    .color(theme::TEXT_WEAK),
            ))
            .clicked()
        {
            self.open_project_dialog();
        }
        // 最近的專案：點檔名直接開啟，不用再走檔案對話框。
        // 不在這裡檢查檔案是否存在（每幀摸磁碟太浪費），
        // 開啟失敗時 load_project 會提示並將它從清單移除
        if !self.recent_projects.is_empty() {
            child.add_space(18.0);
            child.label(
                egui::RichText::new("最近的專案")
                    .size(12.0)
                    .strong()
                    .color(theme::TEXT_WEAK),
            );
            child.add_space(2.0);
            let recent: Vec<PathBuf> = self.recent_projects.iter().take(5).cloned().collect();
            for p in recent {
                let name = p
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.to_string_lossy().into_owned());
                let btn = egui::Button::new(
                    egui::RichText::new(format!("🕘  {name}"))
                        .size(13.5)
                        .color(theme::ACCENT),
                )
                .frame(false);
                if child
                    .add(btn)
                    .on_hover_text(p.to_string_lossy())
                    .clicked()
                {
                    self.load_project(&p);
                }
            }
        }
        // 剛選的資料夾掃不到照片時明講原因，避免使用者以為程式沒反應
        if self.import_found_nothing {
            child.add_space(14.0);
            child.label(
                egui::RichText::new("⚠ 沒有可以加入的照片")
                    .size(12.5)
                    .strong()
                    .color(theme::ERROR),
            );
            child.label(
                egui::RichText::new(
                    "支援 JPG／PNG／BMP／WebP／TIFF；其他格式（如 iPhone 的 HEIC）與子資料夾內的照片不會被加入",
                )
                .size(11.5)
                .color(theme::TEXT_WEAK),
            );
        }
    }

    fn ui_workspace(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let working = self.is_working();

        // 工具列
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!working, |ui| {
                if ui.button("📁  加入資料夾").clicked() {
                    self.pick_folder();
                }
                if ui.button("🖼  加入照片").clicked() {
                    self.pick_files();
                }
                if ui.button("🗑  清空").clicked() {
                    self.clear_photos();
                }
                ui.separator();
                if ui.button("🆕  新專案").clicked() {
                    // 誤點會失去所有未存檔的編輯，先確認
                    let r = rfd::MessageDialog::new()
                        .set_level(rfd::MessageLevel::Warning)
                        .set_title("開新專案")
                        .set_description("將清空目前的照片與所有設定，尚未儲存的變更會遺失。")
                        .set_buttons(rfd::MessageButtons::OkCancel)
                        .show();
                    if r == rfd::MessageDialogResult::Ok {
                        self.new_project();
                    }
                }
                if ui.button("📂  開啟專案").clicked() {
                    self.open_project_dialog();
                }
                if ui
                    .button("💾  儲存專案")
                    .on_hover_text("選擇位置儲存（另存新檔）；Ctrl+S 可直接覆寫目前專案")
                    .clicked()
                {
                    self.save_project_dialog();
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // 與實際輸出時長一致：Ken Burns 會把每張時長對齊到 30fps 的整數格
                // （見 run_conversion 的 eff_dur），fps 無法整除 30 時直接用 張數/fps
                // 會與成品有落差
                let eff_dur = if self.ken_burns {
                    let d = ((OUT_FPS as f64 / self.fps as f64).round() as i64).max(1);
                    d as f64 / OUT_FPS as f64
                } else {
                    1.0 / self.fps as f64
                };
                let secs = self.photos.len() as f64 * eff_dur;
                egui::Frame::default()
                    .fill(theme::CARD)
                    .corner_radius(12)
                    .inner_margin(egui::Margin::symmetric(10, 4))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} 張照片 · 約 {:.1} 秒",
                                self.photos.len(),
                                secs
                            ))
                            .size(11.5)
                            .color(theme::TEXT_WEAK),
                        );
                    });
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("拖曳即可加入照片 · 滾輪或 ← → 切換預覽 · 右鍵縮圖可移除")
                        .size(11.0)
                        .color(theme::TEXT_WEAK),
                );
            });
        });
        ui.add_space(10.0);

        if self.photos.is_empty() {
            return;
        }

        // 預覽卡片
        let film_h = 96.0;
        let preview_h = (ui.available_height() - film_h - 14.0).max(160.0);
        // Sense::click：點擊預覽空白處可取消文字選取
        let (rect, bg_resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), preview_h),
            egui::Sense::click(),
        );
        let p = ui.painter().clone();
        p.rect_filled(rect, 10, theme::PREVIEW_BG);
        p.rect_stroke(
            rect,
            10,
            egui::Stroke::new(1.0, theme::BORDER),
            egui::StrokeKind::Inside,
        );

        // 先解析輸出解析度（resolved_resolution 需要 &mut self 更新快取），
        // 再借用預覽貼圖
        let out_res = self.resolved_resolution();
        // 渲染完成前先用縮圖放大當佔位，切換照片時畫面不留白
        let placeholder = match (self.preview_tex.is_none(), self.preview_selected) {
            (true, Some(i)) => self.photos.get(i).and_then(|p| match self.thumbs.get(p) {
                Some(Thumb::Ready(t)) => Some(t.clone()),
                _ => None,
            }),
            _ => None,
        };
        let mut img_rect_opt: Option<egui::Rect> = None;
        if let Some(tex) = self.preview_tex.as_ref().or(placeholder.as_ref()) {
            // 文字座標與版面一律以「輸出畫布」（含補邊黑邊）為基準：
            // 佔位縮圖是未補邊的原始比例，若直接 fit 進卡片，照片位置
            // 和文字座標都會與渲染完成後不同，每切一張照片就閃跳一次。
            // 先取畫布矩形，再把貼圖置中其內（正式預覽圖本身就是畫布
            // 尺寸，fit 結果等於畫布矩形，行為不變）
            let (cw, ch) = preview_canvas(out_res);
            let canvas_rect =
                fit_rect(egui::vec2(cw as f32, ch as f32), rect.shrink(14.0));
            let img_rect = fit_rect(tex.size_vec2(), canvas_rect);
            img_rect_opt = Some(canvas_rect);
            p.image(
                tex.id(),
                img_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else if self.preview_selected.is_some() {
            p.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "預覽產生中…",
                egui::FontId::proportional(13.0),
                theme::TEXT_WEAK,
            );
        } else {
            p.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "點選下方縮圖即可預覽調色與文字效果",
                egui::FontId::proportional(13.0),
                theme::TEXT_WEAK,
            );
        }

        // 首次使用下載 ffmpeg（約 80MB）沒有進度條，這裡覆蓋一條說明橫幅
        // （即使佔位縮圖已顯示也蓋在上面），慢速網路下才不會被誤認為當機
        if FFMPEG_DOWNLOADING.load(Ordering::Relaxed) {
            let band = egui::Rect::from_center_size(rect.center(), egui::vec2(rect.width(), 34.0));
            p.rect_filled(band, 0, egui::Color32::from_black_alpha(180));
            p.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "首次使用，正在下載影片處理元件（約 80MB），請稍候…",
                egui::FontId::proportional(13.0),
                theme::TEXT,
            );
            ui.ctx().request_repaint_after(Duration::from_millis(300));
        }

        // 左上角：檔名資訊
        if let Some(i) = self.preview_selected {
            if let Some(photo) = self.photos.get(i) {
                let name = photo.file_name().unwrap_or_default().to_string_lossy();
                let text = format!("{} / {}　{}", i + 1, self.photos.len(), name);
                let galley = p.layout_no_wrap(
                    text,
                    egui::FontId::proportional(11.5),
                    theme::TEXT,
                );
                let chip = egui::Rect::from_min_size(
                    rect.min + egui::vec2(10.0, 10.0),
                    galley.size() + egui::vec2(16.0, 9.0),
                );
                p.rect_filled(chip, 6, egui::Color32::from_black_alpha(160));
                p.galley(chip.min + egui::vec2(8.0, 4.5), galley, theme::TEXT);
            }
        }

        // 右上角：預覽更新中的轉圈。
        // 注意：不能用 ui.put()——它會把版面游標拉回這個位置，
        // 造成後面的縮圖列被畫到視窗上方（跑版）
        if self.preview_rx.is_some() && (self.preview_tex.is_some() || placeholder.is_some()) {
            let t = ui.input(|i| i.time) as f32;
            let center = rect.right_top() + egui::vec2(-22.0, 22.0);
            let radius = 8.0;
            let start = t * 5.0;
            let points: Vec<egui::Pos2> = (0..=18)
                .map(|k| {
                    let a = start + k as f32 * (std::f32::consts::TAU * 0.75 / 18.0);
                    center + egui::vec2(a.cos(), a.sin()) * radius
                })
                .collect();
            p.add(egui::Shape::line(points, egui::Stroke::new(2.5, theme::ACCENT)));
            ui.ctx().request_repaint();
        }

        // 預覽錯誤
        if let Some(e) = &self.preview_error {
            p.text(
                rect.center_bottom() - egui::vec2(0.0, 18.0),
                egui::Align2::CENTER_CENTER,
                format!("預覽失敗：{e}"),
                egui::FontId::proportional(12.0),
                theme::ERROR,
            );
        }

        // 文字即時繪製與互動編輯（拖曳移動、角落縮放、旋轉把手）
        if let Some(ir) = img_rect_opt {
            self.ui_text_overlay(ui, rect, ir);
        }
        if bg_resp.clicked() {
            self.sel_text = None;
        }

        ui.add_space(10.0);

        // 滾輪在預覽區或縮圖列上：切換上一張／下一張
        let strip_rect = ui.available_rect_before_wrap();
        if ui.rect_contains_pointer(rect) || ui.rect_contains_pointer(strip_rect) {
            self.wheel_accum += ctx.input(|i| i.raw_scroll_delta.y + i.raw_scroll_delta.x);
            const STEP: f32 = 40.0;
            while self.wheel_accum >= STEP {
                self.wheel_accum -= STEP;
                let cur = self.preview_selected.unwrap_or(0);
                if cur > 0 {
                    self.select_photo(Some(cur - 1));
                    self.scroll_to_selected = true;
                }
            }
            while self.wheel_accum <= -STEP {
                self.wheel_accum += STEP;
                let cur = self.preview_selected.unwrap_or(0);
                if cur + 1 < self.photos.len() {
                    self.select_photo(Some(cur + 1));
                    self.scroll_to_selected = true;
                }
            }
        } else {
            self.wheel_accum = 0.0;
        }

        // 縮圖膠卷
        let mut click_idx: Option<usize> = None;
        let mut toggle_idx: Option<usize> = None; // Ctrl+點：加入/移出多選
        let mut range_idx: Option<usize> = None; // Shift+點：從目前選取連選到這張
        let mut remove_idx: Option<usize> = None;
        let mut clear_all = false;
        let mut vis_range: Option<(usize, usize)> = None;
        // 縮圖尺寸固定，可虛擬化：只渲染捲動範圍內的縮圖、前後以空白撐出總寬，
        // 照片數量再多每一幀的繪製成本也不變
        let thumb_size = egui::vec2(132.0, 84.0);
        let n = self.photos.len();
        egui::ScrollArea::horizontal().show_viewport(ui, |ui, viewport| {
            ui.set_min_height(thumb_size.y);
            ui.horizontal(|ui| {
                let spacing = ui.spacing().item_spacing.x;
                let stride = thumb_size.x + spacing;
                let origin = ui.next_widget_position();
                // 對虛擬位置捲動，不需要選取的縮圖真的被渲染出來
                if self.scroll_to_selected {
                    if let Some(sel) = self.preview_selected {
                        let r = egui::Rect::from_min_size(
                            egui::pos2(origin.x + sel as f32 * stride, origin.y),
                            thumb_size,
                        );
                        ui.scroll_to_rect(r, Some(egui::Align::Center));
                    }
                }
                let first = (((viewport.min.x / stride).floor() as isize) - 1).max(0) as usize;
                let last = ((((viewport.max.x / stride).ceil() as isize) + 1).max(0) as usize).min(n);
                let first = first.min(last);
                vis_range = Some((first, last));
                if first > 0 {
                    ui.add_space(first as f32 * stride);
                }
                // 讓每張縮圖的自動 ID 與索引繫結，捲動時 hover／右鍵選單狀態才不會錯位
                ui.skip_ahead_auto_ids(first);
                for i in first..last {
                    let photo = &self.photos[i];
                    let (tex, failed) = match self.thumbs.get(photo) {
                        Some(Thumb::Ready(t)) => (Some(t.clone()), false),
                        Some(Thumb::Failed) => (None, true),
                        _ => (None, false),
                    };
                    let selected = self.preview_selected == Some(i);
                    let has_caption = self.sub_entries.iter().any(|e| {
                        (e.start..=e.end).contains(&(i + 1)) && !e.text.trim().is_empty()
                    });
                    let multi = self.multi_sel.contains(photo);
                    let has_adj = self.adj_overrides.contains_key(photo);
                    let mut resp = thumb_item(
                        ui, tex.as_ref(), i, selected, has_caption, multi, has_adj, failed,
                    );
                    if failed {
                        resp = resp.on_hover_text("這張照片無法讀取（檔案損毀或格式不支援）");
                    }
                    if resp.clicked() {
                        let mods = ui.input(|inp| inp.modifiers);
                        if mods.ctrl {
                            toggle_idx = Some(i);
                        } else if mods.shift {
                            range_idx = Some(i);
                        } else {
                            click_idx = Some(i);
                        }
                    }
                    resp.context_menu(|ui| {
                        // 轉檔中要與工具列一致地「明確禁用」：照常可點但被
                        // 後面的 !working 守門擋掉的話，點了毫無反應也沒有
                        // 回饋，使用者會以為程式壞了
                        ui.add_enabled_ui(!working, |ui| {
                            if ui.button("移除這張照片").clicked() {
                                remove_idx = Some(i);
                                ui.close_menu();
                            }
                            if ui.button("清空全部").clicked() {
                                clear_all = true;
                                ui.close_menu();
                            }
                        });
                        if working {
                            ui.label(
                                egui::RichText::new("轉換中無法修改照片")
                                    .size(11.0)
                                    .color(theme::TEXT_WEAK),
                            );
                        }
                    });
                }
                if last < n {
                    ui.add_space((n - last) as f32 * stride - spacing);
                }
            });
        });

        self.scroll_to_selected = false;

        // 依這一幀的可視範圍按需載入／淘汰縮圖
        if let Some((first, last)) = vis_range {
            self.manage_thumbs(first, last);
        }

        if let Some(i) = click_idx {
            // 一般點選：離開多選模式，回到單張預覽
            self.multi_sel.clear();
            self.select_photo(Some(i));
        }
        if let Some(i) = toggle_idx {
            let p = self.photos[i].clone();
            if !self.multi_sel.remove(&p) {
                self.multi_sel.insert(p);
            }
            self.select_photo(Some(i));
            self.sync_sel_adj();
        }
        if let Some(i) = range_idx {
            let anchor = self.preview_selected.unwrap_or(i);
            for k in anchor.min(i)..=anchor.max(i) {
                self.multi_sel.insert(self.photos[k].clone());
            }
            self.select_photo(Some(i));
            self.sync_sel_adj();
        }
        if !working {
            if clear_all {
                self.clear_photos();
            } else if let Some(i) = remove_idx {
                self.remove_photo(i);
            }
        }
    }

    /// 預覽區的文字即時繪製與互動編輯。
    /// 文字由 egui 直接畫在預覽圖上（非 ffmpeg 燒錄），
    /// 拖曳＝移動、角落把手＝縮放、頂部圓形把手＝旋轉，全部零延遲
    fn ui_text_overlay(&mut self, ui: &mut egui::Ui, card: egui::Rect, img: egui::Rect) {
        let Some(cur_idx) = self.preview_selected else { return };
        let cur = cur_idx + 1;
        let style = self.sub_style.clone();
        let res_h = self.resolved_resolution().h as f32;
        // 文字內容裁切到畫布（與輸出一致：輸出只到畫面邊界，溢出補邊黑邊的部分
        // 會被切掉，預覽若畫到黑邊區就會所見非所得）；選取框與把手則裁切到整張
        // 卡片，文字靠邊時把手才不會一起被切掉、仍可操作
        let p = ui.painter().with_clip_rect(img);
        let chrome = ui.painter().with_clip_rect(card);
        let scale = img.height() / 1080.0;
        let ow_screen = style.outline_w as f32 * img.height() / res_h;

        for k in 0..self.sub_entries.len() {
            let e = &self.sub_entries[k];
            if !(e.start..=e.end).contains(&cur) || e.text.trim().is_empty() {
                continue;
            }
            let (text, ex, ey, esize, erot) =
                (e.text.trim_end().to_string(), e.x, e.y, e.size, e.rot);

            let font_px = (esize as f32 * scale).max(2.0);
            let galley = p.layout(
                text,
                egui::FontId::proportional(font_px),
                style.color,
                f32::INFINITY,
            );
            let half = galley.size() / 2.0;
            let center = img.min + egui::vec2(ex * img.width(), ey * img.height());
            let angle = erot.to_radians();

            // 半透明底框（近似輸出的 box=1）
            if style.boxed {
                let pad = (font_px * 0.25).max(2.0);
                let ext = half + egui::vec2(pad, pad);
                let corners: Vec<egui::Pos2> = [
                    egui::vec2(-ext.x, -ext.y),
                    egui::vec2(ext.x, -ext.y),
                    egui::vec2(ext.x, ext.y),
                    egui::vec2(-ext.x, ext.y),
                ]
                .into_iter()
                .map(|v| center + rot_vec(v, angle))
                .collect();
                p.add(egui::Shape::convex_polygon(
                    corners,
                    egui::Color32::from_black_alpha(102),
                    egui::Stroke::NONE,
                ));
            }

            // 外框（近似 ffmpeg 的 borderw：以 8 個方向偏移重繪）＋本體
            let mk = |off: egui::Vec2, override_color: Option<egui::Color32>| {
                let pos = center + rot_vec(-half + off, angle);
                let mut ts =
                    egui::epaint::TextShape::new(pos.round(), galley.clone(), style.color);
                ts.angle = angle;
                ts.override_text_color = override_color;
                egui::Shape::Text(ts)
            };
            if ow_screen > 0.05 {
                for (dx, dy) in [
                    (-1.0, 0.0),
                    (1.0, 0.0),
                    (0.0, -1.0),
                    (0.0, 1.0),
                    (-0.7, -0.7),
                    (0.7, -0.7),
                    (-0.7, 0.7),
                    (0.7, 0.7),
                ] {
                    p.add(mk(
                        egui::vec2(dx, dy) * ow_screen,
                        Some(style.outline_color),
                    ));
                }
            }
            p.add(mk(egui::Vec2::ZERO, None));

            // 主體互動：以旋轉後的外接矩形當點擊/拖曳範圍
            let bb_ext = egui::vec2(
                half.x * angle.cos().abs() + half.y * angle.sin().abs(),
                half.x * angle.sin().abs() + half.y * angle.cos().abs(),
            ) + egui::vec2(6.0, 6.0);
            let bbox = egui::Rect::from_center_size(center, bb_ext * 2.0);
            let id = ui.id().with(("free_text", k));
            let resp = ui.interact(bbox, id, egui::Sense::click_and_drag());
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Move);
            }
            if resp.clicked() || resp.drag_started() {
                self.sel_text = Some(k);
            }
            if resp.dragged() {
                let d = resp.drag_delta();
                let en = &mut self.sub_entries[k];
                en.x = (en.x + d.x / img.width()).clamp(0.0, 1.0);
                en.y = (en.y + d.y / img.height()).clamp(0.0, 1.0);
            }

            // 選取框與把手
            if self.sel_text == Some(k) {
                let ext = half + egui::vec2(8.0, 8.0);
                let corners_local = [
                    egui::vec2(-ext.x, -ext.y),
                    egui::vec2(ext.x, -ext.y),
                    egui::vec2(ext.x, ext.y),
                    egui::vec2(-ext.x, ext.y),
                ];
                let corners: Vec<egui::Pos2> = corners_local
                    .iter()
                    .map(|v| center + rot_vec(*v, angle))
                    .collect();
                for i in 0..4 {
                    chrome.line_segment(
                        [corners[i], corners[(i + 1) % 4]],
                        egui::Stroke::new(1.5, theme::ACCENT),
                    );
                }

                // 角落縮放把手
                for (ci, c) in corners.iter().enumerate() {
                    let vis = egui::Rect::from_center_size(*c, egui::vec2(9.0, 9.0));
                    chrome.rect_filled(vis, 2, egui::Color32::WHITE);
                    chrome.rect_stroke(
                        vis,
                        2,
                        egui::Stroke::new(1.5, theme::ACCENT),
                        egui::StrokeKind::Inside,
                    );
                    let hresp = ui.interact(
                        egui::Rect::from_center_size(*c, egui::vec2(14.0, 14.0)),
                        id.with(("corner", ci)),
                        egui::Sense::drag(),
                    );
                    if hresp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeNwSe);
                    }
                    if hresp.dragged() {
                        if let Some(ptr) = hresp.interact_pointer_pos() {
                            let prev = ptr - hresp.drag_delta();
                            let d0 = (prev - center).length();
                            let d1 = (ptr - center).length();
                            if d0 > 4.0 {
                                let en = &mut self.sub_entries[k];
                                en.size = (en.size as f32 * d1 / d0)
                                    .round()
                                    .clamp(8.0, 300.0)
                                    as i32;
                            }
                        }
                    }
                }

                // 旋轉把手（頂邊中點向外延伸的圓形）
                let top_mid = center + rot_vec(egui::vec2(0.0, -ext.y), angle);
                let handle = center + rot_vec(egui::vec2(0.0, -ext.y - 22.0), angle);
                chrome.line_segment([top_mid, handle], egui::Stroke::new(1.5, theme::ACCENT));
                chrome.circle_filled(handle, 5.5, theme::ACCENT);
                chrome.circle_stroke(handle, 5.5, egui::Stroke::new(1.5, egui::Color32::WHITE));
                let rresp = ui.interact(
                    egui::Rect::from_center_size(handle, egui::vec2(16.0, 16.0)),
                    id.with("rotate"),
                    egui::Sense::drag(),
                );
                if rresp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                }
                if rresp.dragged() {
                    if let Some(ptr) = rresp.interact_pointer_pos() {
                        let v = ptr - center;
                        if v.length() > 4.0 {
                            let mut deg = v.y.atan2(v.x).to_degrees() + 90.0;
                            if deg > 180.0 {
                                deg -= 360.0;
                            }
                            if deg < -180.0 {
                                deg += 360.0;
                            }
                            // 靠近 45° 倍數時吸附
                            for s in
                                [-180.0f32, -135.0, -90.0, -45.0, 0.0, 45.0, 90.0, 135.0, 180.0]
                            {
                                if (deg - s).abs() < 4.0 {
                                    deg = s;
                                    break;
                                }
                            }
                            self.sub_entries[k].rot = deg;
                        }
                    }
                }
            }
        }
    }

    /// 「關於」視窗：版本資訊、專案連結與手動檢查更新
    fn ui_about_window(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let mut open = self.about_open;
        let mut check_now = false;
        let mut start_update: Option<String> = None;
        let mut restart_now = false;
        egui::Window::new("關於")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(320.0);
                ui.vertical_centered(|ui| {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("🎬").size(34.0));
                    ui.label(
                        egui::RichText::new("Photo2Video — 照片轉影片")
                            .size(15.0)
                            .strong()
                            .color(theme::TEXT),
                    );
                    ui.label(
                        egui::RichText::new(concat!("版本 v", env!("CARGO_PKG_VERSION")))
                            .size(12.5)
                            .color(theme::TEXT_WEAK),
                    );
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);

                    match &self.update_status {
                        UpdateStatus::Idle => {}
                        UpdateStatus::Checking => {
                            ui.label(
                                egui::RichText::new("檢查更新中…")
                                    .size(12.5)
                                    .color(theme::TEXT_WEAK),
                            );
                        }
                        UpdateStatus::UpToDate => {
                            ui.label(
                                egui::RichText::new("✔ 目前已是最新版本")
                                    .size(12.5)
                                    .color(theme::SUCCESS),
                            );
                        }
                        UpdateStatus::Available(tag) => {
                            ui.label(
                                egui::RichText::new(format!("⬆ 有新版本 {tag} 可以下載"))
                                    .size(12.5)
                                    .strong()
                                    .color(theme::SUCCESS),
                            );
                            ui.add_space(2.0);
                            if ui.button("⬇ 立即更新").clicked() {
                                start_update = Some(tag.clone());
                            }
                        }
                        UpdateStatus::Downloading(p) => {
                            ui.label(
                                egui::RichText::new(format!("正在下載更新… {:.0}%", p * 100.0))
                                    .size(12.5)
                                    .color(theme::TEXT_WEAK),
                            );
                            ui.add(
                                egui::ProgressBar::new(*p)
                                    .fill(theme::ACCENT)
                                    .desired_height(6.0),
                            );
                        }
                        UpdateStatus::ReadyToRestart => {
                            ui.label(
                                egui::RichText::new("✔ 新版已下載完成")
                                    .size(12.5)
                                    .strong()
                                    .color(theme::SUCCESS),
                            );
                            ui.add_space(2.0);
                            if ui
                                .add_enabled(
                                    !self.is_working(),
                                    egui::Button::new("重新啟動完成更新"),
                                )
                                .on_disabled_hover_text("轉換完成後即可重新啟動")
                                .clicked()
                            {
                                restart_now = true;
                            }
                        }
                        UpdateStatus::Failed(e) => {
                            ui.label(
                                egui::RichText::new("✖ 更新失敗")
                                    .size(12.5)
                                    .color(theme::ERROR),
                            )
                            .on_hover_text(e);
                            ui.hyperlink_to(
                                egui::RichText::new("改用瀏覽器下載").size(12.0),
                                format!("https://github.com/{GITHUB_REPO}/releases/latest"),
                            );
                        }
                    }
                    ui.add_space(8.0);

                    let busy = matches!(
                        self.update_status,
                        UpdateStatus::Checking
                            | UpdateStatus::Downloading(_)
                            | UpdateStatus::ReadyToRestart
                    );
                    if ui
                        .add_enabled(!busy, egui::Button::new("🔄 檢查更新"))
                        .clicked()
                    {
                        check_now = true;
                    }
                    ui.add_space(4.0);
                });
            });
        self.about_open = open;
        if check_now {
            // 手動重新檢查時，讓新版通知條可以再次出現
            self.update_banner_dismissed = false;
            self.spawn_update_check(ctx);
        }
        if let Some(tag) = start_update {
            self.spawn_self_update(ctx, tag);
        }
        if restart_now {
            self.restart_for_update();
        }
    }

    /// 拖曳檔案進視窗時的全螢幕提示。
    /// 轉檔中放開的檔案會被忽略（update 有 !is_working 守門）：這裡不能
    /// 直接不畫——拖著檔案毫無反應、放開又默默消失，看起來像程式壞掉；
    /// 改顯示「轉換中無法加入」讓使用者放開前就知道現在不能加
    fn ui_drop_overlay(&self, ctx: &egui::Context) {
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if !hovering {
            return;
        }
        let working = self.is_working();
        let screen = ctx.screen_rect();
        let p = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("drop_overlay"),
        ));
        p.rect_filled(screen, 0, egui::Color32::from_black_alpha(150));
        let card = egui::Rect::from_center_size(screen.center(), egui::vec2(340.0, 116.0));
        p.rect_filled(card, 12, theme::CARD);
        let (accent, icon, msg) = if working {
            (theme::TEXT_WEAK, "⏳", "轉換中，暫時無法加入檔案")
        } else {
            (theme::ACCENT, "⬇", "放開滑鼠加入照片")
        };
        p.rect_stroke(
            card,
            12,
            egui::Stroke::new(1.5, accent),
            egui::StrokeKind::Inside,
        );
        p.text(
            card.center() - egui::vec2(0.0, 16.0),
            egui::Align2::CENTER_CENTER,
            icon,
            egui::FontId::proportional(26.0),
            accent,
        );
        p.text(
            card.center() + egui::vec2(0.0, 20.0),
            egui::Align2::CENTER_CENTER,
            msg,
            egui::FontId::proportional(15.0),
            theme::TEXT,
        );
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // 轉檔進行中關閉視窗：worker 執行緒會隨行程結束，但 ffmpeg 是
        // 獨立子行程，不主動終止會留在背景繼續編碼佔用 CPU，
        // 還寫出使用者以為已取消的輸出檔
        let pid = CONVERT_FFMPEG_PID.swap(0, Ordering::Relaxed);
        if pid != 0 {
            kill_pid(pid);
        }
        // 清掉寫到一半的輸出檔：留著看起來像轉好的影片，播放才發現損毀
        // （與 run_conversion 失敗時的清理一致）。ffmpeg 剛被終止，
        // 檔案控制代碼可能要一瞬間才釋放，失敗就稍候重試幾次
        if self.is_working() {
            if let Some(out) = self.convert_output.take() {
                for _ in 0..5 {
                    if std::fs::remove_file(&out).is_ok() || !out.exists() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
        // 關閉程式時若還有未寫入的 fps 設定，補寫一次
        if self.fps_pending_save.is_some() {
            save_fps(self.fps);
        }
        clean_own_temp_files();
    }
}

/// 清掉本行程建立的暫存檔（預覽底圖 BMP 等）。執行期間預覽底圖由 4 槽 LRU
/// 汰換刪除，但關閉時當下留著的最多 4 個不會清；加上檔名帶行程 ID 做多實例
/// 隔離，每次啟動都是新 pid，舊檔會永遠成為孤兒累積在暫存資料夾。
/// 只刪「本行程 pid 前綴」的檔案，不會動到其他正在執行的實例
fn clean_own_temp_files() {
    let prefix = format!("photo2video_{}_", std::process::id());
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// 清掉先前執行留下的孤兒暫存檔。閃退、強制結束或斷電時 App::drop
/// 不會執行，該次 pid 前綴的暫存檔（預覽底圖每張約 1.5MB）沒人清、
/// 之後的啟動都是新 pid 也永遠不會清到，會無限累積。
/// 以「修改時間超過 48 小時」認定孤兒：其他執行中實例的暫存檔極少
/// 存活這麼久，即使誤刪，預覽底圖讀不到時也會自動退回完整流程重建
fn clean_stale_temp_files() {
    const STALE_AGE: Duration = Duration::from_secs(48 * 60 * 60);
    let own_prefix = format!("photo2video_{}_", std::process::id());
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("photo2video_") || name.starts_with(&own_prefix) {
            continue;
        }
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE_AGE);
        if stale {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 視窗標題顯示目前專案名稱（看得出正在編輯哪個 .p2v）；
        // 只在變動時送指令，不每幀重送
        let desired_title = match &self.current_project {
            Some(p) => format!(
                "{} — Photo2Video",
                p.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
            None => "Photo2Video — 照片轉影片".to_string(),
        };
        if self.applied_title != desired_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(desired_title.clone()));
            self.applied_title = desired_title;
        }

        // fps 停止變動 800ms 後才寫設定檔，拖動過程不做磁碟 I/O
        if let Some(t) = self.fps_pending_save {
            let elapsed = t.elapsed();
            if elapsed >= Duration::from_millis(800) {
                save_fps(self.fps);
                self.fps_pending_save = None;
            } else {
                ctx.request_repaint_after(Duration::from_millis(820) - elapsed);
            }
        }
        self.poll_update();
        self.poll_worker();
        self.poll_preview(ctx);
        self.poll_thumbs(ctx);

        // 支援直接拖曳檔案/資料夾進視窗
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() && !self.is_working() {
            // 這批含專案檔就只開專案：開專案是「取代整個工作狀態」的操作，
            // 同批夾帶的照片/音訊語意不明。若照原順序逐一處理，load_project
            // 會先清空並載入專案，接著 add_photos 又把同批照片加上去弄髒
            // 專案；且 load_project 的取代確認被按「取消」時，照片仍會被加入。
            // 多個專案檔也只取第一個，不連續跳出多個確認框
            if let Some(proj) = dropped.iter().find(|p| is_project_file(p)) {
                self.load_project(proj);
            } else {
                let mut files = Vec::new();
                let mut any_dir = false;
                let mut had_unsupported = false;
                for p in dropped {
                    if p.is_dir() {
                        any_dir = true;
                        files.extend(collect_images_in_dir(&p));
                    } else if is_audio(&p) {
                        // 拖入音訊檔＝設定為背景音樂，並展開「轉場與音樂」區塊
                        // （比照新增文字自動展開）：收合時拖入音樂否則毫無回饋，
                        // 使用者會以為沒設定成功
                        self.music_path = Some(p);
                        self.sec_fx_open = true;
                    } else if is_image(&p) {
                        files.push(p);
                    } else {
                        // 非圖片非音訊非專案（如 iPhone 的 HEIC、GIF）：記下以便
                        // 提示，否則拖入後 add_photos 靜默過濾掉、使用者毫無回饋
                        had_unsupported = true;
                    }
                }
                // 掃不到照片時提示（空資料夾、照片都在子資料夾、或格式不支援）
                if files.is_empty() && (any_dir || had_unsupported) {
                    self.import_found_nothing = true;
                }
                self.add_photos(files);
            }
        }

        // Ctrl+S 儲存專案、Ctrl+O 開啟專案（轉換中不動作）
        if !self.is_working() {
            if !self.photos.is_empty()
                && ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S))
            {
                self.quick_save_project();
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::O)) {
                self.open_project_dialog();
            }
        }

        // 左右方向鍵切換預覽（輸入框有焦點時不動作）
        if !self.photos.is_empty() && ctx.memory(|m| m.focused().is_none()) {
            let cur = self.preview_selected.unwrap_or(0);
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowRight)) && cur + 1 < self.photos.len()
            {
                self.select_photo(Some(cur + 1));
                self.scroll_to_selected = true;
            }
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft)) && cur > 0 {
                self.select_photo(Some(cur - 1));
                self.scroll_to_selected = true;
            }
        }

        self.ui_bottom_bar(ctx);
        self.ui_side_panel(ctx);
        self.ui_central(ctx);
        self.ui_about_window(ctx);
        self.ui_drop_overlay(ctx);
    }
}

// ---------- UI 元件與主題 ----------

/// 套用整體深色主題
fn apply_theme(ctx: &egui::Context) {
    use egui::{
        Color32, CornerRadius, FontFamily, FontId, Shadow, Stroke, TextStyle, Vec2, Visuals,
    };

    // 固定使用深色主題，不跟隨系統（本 App 的配色只設計了深色）
    ctx.set_theme(egui::ThemePreference::Dark);

    let mut style = (*ctx.style()).clone();
    style.text_styles = [
        (TextStyle::Heading, FontId::new(17.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(13.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
    ]
    .into();
    style.spacing.item_spacing = Vec2::new(8.0, 7.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    style.spacing.interact_size = Vec2::new(40.0, 24.0);

    let mut v = Visuals::dark();
    v.override_text_color = Some(theme::TEXT);
    v.panel_fill = theme::PANEL;
    v.window_fill = theme::PANEL;
    v.extreme_bg_color = Color32::from_rgb(0x0F, 0x10, 0x13);
    v.faint_bg_color = theme::CARD;
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_shadow = Shadow {
        offset: [0, 6],
        blur: 20,
        spread: 0,
        color: Color32::from_black_alpha(110),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 14,
        spread: 0,
        color: Color32::from_black_alpha(90),
    };
    v.selection.bg_fill = theme::ACCENT;
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    v.slider_trailing_fill = false;
    v.hyperlink_color = theme::ACCENT;

    v.widgets.noninteractive.bg_fill = theme::PANEL;
    v.widgets.noninteractive.weak_bg_fill = theme::PANEL;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, theme::BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, theme::TEXT);
    v.widgets.noninteractive.corner_radius = CornerRadius::same(6);

    v.widgets.inactive.bg_fill = theme::CARD;
    v.widgets.inactive.weak_bg_fill = theme::CARD;
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, theme::TEXT);
    v.widgets.inactive.corner_radius = CornerRadius::same(6);

    v.widgets.hovered.bg_fill = theme::CARD_HOVER;
    v.widgets.hovered.weak_bg_fill = theme::CARD_HOVER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x45, 0x48, 0x52));
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    v.widgets.hovered.corner_radius = CornerRadius::same(6);

    v.widgets.active.bg_fill = theme::CARD_HOVER;
    v.widgets.active.weak_bg_fill = theme::CARD_HOVER;
    v.widgets.active.bg_stroke = Stroke::new(1.0, theme::ACCENT);
    v.widgets.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    v.widgets.active.corner_radius = CornerRadius::same(6);

    v.widgets.open.bg_fill = theme::CARD_HOVER;
    v.widgets.open.weak_bg_fill = theme::CARD_HOVER;
    v.widgets.open.bg_stroke = Stroke::new(1.0, theme::ACCENT);
    v.widgets.open.corner_radius = CornerRadius::same(6);

    style.visuals = v;
    ctx.all_styles_mut(|s| *s = style.clone());
}

/// 可收合的區段標題：色條 + 標題 + 展開/收合三角形，點擊切換
fn section_toggle(ui: &mut egui::Ui, title: &str, open: &mut bool) {
    let font = egui::FontId::proportional(14.5);
    let galley = ui
        .painter()
        .layout_no_wrap(title.to_string(), font, theme::TEXT);
    let w = 10.0 + galley.size().x + 8.0 + 14.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 20.0), egui::Sense::click());
    let p = ui.painter();
    let bar = egui::Rect::from_center_size(
        egui::pos2(rect.min.x + 1.5, rect.center().y),
        egui::vec2(3.0, 15.0),
    );
    p.rect_filled(bar, 2, theme::ACCENT);
    p.galley(
        egui::pos2(rect.min.x + 10.0, rect.center().y - galley.size().y / 2.0),
        galley,
        theme::TEXT,
    );
    // 三角形箭頭（開＝朝下、合＝朝右）
    let cx = rect.max.x - 7.0;
    let cy = rect.center().y;
    let color = if resp.hovered() { theme::TEXT } else { theme::TEXT_WEAK };
    let pts = if *open {
        vec![
            egui::pos2(cx - 4.5, cy - 2.5),
            egui::pos2(cx + 4.5, cy - 2.5),
            egui::pos2(cx, cy + 3.5),
        ]
    } else {
        vec![
            egui::pos2(cx - 2.5, cy - 4.5),
            egui::pos2(cx - 2.5, cy + 4.5),
            egui::pos2(cx + 3.5, cy),
        ]
    };
    p.add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        *open = !*open;
    }
}

/// 小型分組標籤
fn group_label(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(11.5)
            .strong()
            .color(theme::TEXT_WEAK),
    );
}

/// 主要動作按鈕（強調色）
fn primary_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    ui.scope(|ui| {
        let w = &mut ui.style_mut().visuals.widgets;
        for state in [&mut w.inactive, &mut w.hovered, &mut w.active] {
            state.fg_stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);
            state.corner_radius = egui::CornerRadius::same(8);
        }
        w.inactive.weak_bg_fill = theme::ACCENT;
        w.inactive.bg_fill = theme::ACCENT;
        w.hovered.weak_bg_fill = theme::ACCENT_HOVER;
        w.hovered.bg_fill = theme::ACCENT_HOVER;
        w.hovered.bg_stroke = egui::Stroke::NONE;
        w.active.weak_bg_fill = theme::ACCENT_ACTIVE;
        w.active.bg_fill = theme::ACCENT_ACTIVE;
        w.active.bg_stroke = egui::Stroke::NONE;
        ui.add_enabled(
            enabled,
            egui::Button::new(egui::RichText::new(text).size(14.5).strong())
                .min_size(egui::vec2(150.0, 38.0)),
        )
    })
    .inner
}

/// 自訂滑桿：細軌道 + 水滴形把手（尖端朝上、圓弧在下），點擊或拖曳皆可調整
fn drop_slider(ui: &mut egui::Ui, value: &mut f32, min: f32, max: f32) -> egui::Response {
    drop_slider_rail(ui, value, min, max, None)
}

/// rail 給 Some((左色, 右色)) 時，軌道畫成水平漸層（如白平衡的藍→黃）
fn drop_slider_rail(
    ui: &mut egui::Ui,
    value: &mut f32,
    min: f32,
    max: f32,
    rail: Option<(egui::Color32, egui::Color32)>,
) -> egui::Response {
    let width = ui.spacing().slider_width;
    let (rect, mut resp) =
        ui.allocate_exact_size(egui::vec2(width, 18.0), egui::Sense::click_and_drag());
    if resp.dragged() || resp.clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let t = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            let new = min + t * (max - min);
            if new != *value {
                *value = new;
                resp.mark_changed();
            }
        }
    }
    let t = ((*value - min) / (max - min)).clamp(0.0, 1.0);
    let cx = rect.left() + t * rect.width();
    let cy = rect.center().y;
    let p = ui.painter();
    match rail {
        Some((c0, c1)) => {
            let r = egui::Rect::from_min_max(
                egui::pos2(rect.left(), cy - 2.0),
                egui::pos2(rect.right(), cy + 2.0),
            );
            let mut mesh = egui::Mesh::default();
            mesh.colored_vertex(r.left_top(), c0);
            mesh.colored_vertex(r.right_top(), c1);
            mesh.colored_vertex(r.right_bottom(), c1);
            mesh.colored_vertex(r.left_bottom(), c0);
            mesh.add_triangle(0, 1, 2);
            mesh.add_triangle(0, 2, 3);
            p.add(egui::Shape::mesh(mesh));
        }
        None => {
            p.hline(rect.x_range(), cy, egui::Stroke::new(2.0, theme::CARD_HOVER));
        }
    }
    let fill = if resp.dragged() {
        theme::ACCENT
    } else if resp.hovered() {
        egui::Color32::WHITE
    } else {
        egui::Color32::from_rgb(0xC8, 0xCA, 0xD2)
    };
    let r = 4.0;
    let bulb = egui::pos2(cx, cy + 2.5);
    let apex = egui::pos2(cx, cy - 6.5);
    p.circle_filled(bulb, r, fill);
    p.add(egui::Shape::convex_polygon(
        vec![apex, egui::pos2(cx - r, bulb.y), egui::pos2(cx + r, bulb.y)],
        fill,
        egui::Stroke::NONE,
    ));
    resp
}

/// drop_slider 的整數版本
fn drop_slider_i32(ui: &mut egui::Ui, value: &mut i32, min: i32, max: i32) -> egui::Response {
    let mut f = *value as f32;
    let resp = drop_slider(ui, &mut f, min as f32, max as f32);
    *value = f.round() as i32;
    resp
}

/// 調色滑桿：左標籤、右數值，連點兩下歸零
fn adj_slider(ui: &mut egui::Ui, value: &mut i32, label: &str) {
    adj_slider_rail(ui, value, label, None)
}

/// 帶漸層軌道的調色滑桿（白平衡用：色溫 藍→黃、色調 綠→洋紅）
fn adj_slider_rail(
    ui: &mut egui::Ui,
    value: &mut i32,
    label: &str,
    rail: Option<(egui::Color32, egui::Color32)>,
) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(52.0, 18.0), egui::Sense::hover());
        ui.painter().text(
            rect.left_center(),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(12.5),
            theme::TEXT_WEAK,
        );
        ui.spacing_mut().slider_width = (ui.available_width() - 44.0).max(60.0);
        let resp = {
            let mut f = *value as f32;
            let r = drop_slider_rail(ui, &mut f, -100.0, 100.0, rail);
            *value = f.round() as i32;
            r
        };
        // 滑桿是拖曳型元件，double_clicked() 不會觸發，須自行偵測雙擊
        let double_clicked = resp.hovered()
            && ui.input(|i| i.pointer.button_double_clicked(egui::PointerButton::Primary));
        if double_clicked {
            *value = 0;
        }
        let (txt, color) = if *value == 0 {
            ("0".to_string(), theme::TEXT_WEAK)
        } else {
            (format!("{value:+}"), theme::ACCENT)
        };
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            ui.label(egui::RichText::new(txt).size(11.5).color(color));
        });
    });
}

/// 一般滑桿列：左標籤、右數值
fn slider_row(ui: &mut egui::Ui, value: &mut i32, min: i32, max: i32, label: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(30.0, 18.0), egui::Sense::hover());
        ui.painter().text(
            rect.left_center(),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(12.5),
            theme::TEXT_WEAK,
        );
        ui.spacing_mut().slider_width = (ui.available_width() - 40.0).max(60.0);
        drop_slider_i32(ui, value, min, max);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(value.to_string())
                    .size(11.5)
                    .color(theme::TEXT),
            );
        });
    });
}

/// 把 tex_size 等比縮放置中放進 bounds
fn fit_rect(tex_size: egui::Vec2, bounds: egui::Rect) -> egui::Rect {
    let s = (bounds.width() / tex_size.x).min(bounds.height() / tex_size.y);
    egui::Rect::from_center_size(bounds.center(), tex_size * s)
}

/// 以角度 a（弧度，順時針）旋轉向量
fn rot_vec(v: egui::Vec2, a: f32) -> egui::Vec2 {
    let (s, c) = a.sin_cos();
    egui::vec2(v.x * c - v.y * s, v.x * s + v.y * c)
}

/// 膠卷縮圖項目。multi＝在個別調色的多選集合中；has_adj＝這張照片有個別調色；
/// failed＝縮圖解碼失敗（檔案損毀或格式不支援），顯示警告而非載入中
#[allow(clippy::too_many_arguments)]
fn thumb_item(
    ui: &mut egui::Ui,
    tex: Option<&egui::TextureHandle>,
    idx: usize,
    selected: bool,
    has_caption: bool,
    multi: bool,
    has_adj: bool,
    failed: bool,
) -> egui::Response {
    let size = egui::vec2(132.0, 84.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let p = ui.painter();
    let hovered = resp.hovered();

    p.rect_filled(rect, 8, if hovered { theme::CARD_HOVER } else { theme::CARD });

    if let Some(tex) = tex {
        let img_rect = fit_rect(tex.size_vec2(), rect.shrink(3.0));
        p.image(
            tex.id(),
            img_rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    } else if failed {
        // 解碼失敗與載入中要能區分：一直顯示「…」會被當成程式卡住，
        // 使用者也不知道這張照片有問題（直到轉檔才爆錯）
        p.text(
            rect.center() - egui::vec2(0.0, 8.0),
            egui::Align2::CENTER_CENTER,
            "⚠",
            egui::FontId::proportional(18.0),
            theme::ERROR,
        );
        p.text(
            rect.center() + egui::vec2(0.0, 14.0),
            egui::Align2::CENTER_CENTER,
            "無法讀取",
            egui::FontId::proportional(10.5),
            theme::TEXT_WEAK,
        );
    } else {
        p.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "…",
            egui::FontId::proportional(16.0),
            theme::TEXT_WEAK,
        );
    }

    // 左下角序號
    let num = (idx + 1).to_string();
    let galley = p.layout_no_wrap(num, egui::FontId::proportional(10.5), theme::TEXT);
    let chip = egui::Rect::from_min_size(
        egui::pos2(rect.min.x + 6.0, rect.max.y - galley.size().y - 11.0),
        galley.size() + egui::vec2(10.0, 5.0),
    );
    p.rect_filled(chip, 4, egui::Color32::from_black_alpha(170));
    p.galley(chip.min + egui::vec2(5.0, 2.5), galley, theme::TEXT);

    // 右上角字幕標記
    if has_caption {
        p.text(
            egui::pos2(rect.max.x - 12.0, rect.min.y + 12.0),
            egui::Align2::CENTER_CENTER,
            "💬",
            egui::FontId::proportional(11.0),
            theme::TEXT,
        );
    }

    // 左上角多選勾選徽章（個別調色的目標）
    if multi {
        let c = egui::pos2(rect.min.x + 12.0, rect.min.y + 12.0);
        p.circle_filled(c, 8.0, theme::ACCENT);
        p.text(
            c,
            egui::Align2::CENTER_CENTER,
            "✓",
            egui::FontId::proportional(11.0),
            egui::Color32::WHITE,
        );
    }
    // 右下角標記：這張照片有自己的調色設定
    if has_adj {
        p.text(
            egui::pos2(rect.max.x - 12.0, rect.max.y - 12.0),
            egui::Align2::CENTER_CENTER,
            "🎨",
            egui::FontId::proportional(11.0),
            theme::TEXT,
        );
    }

    let stroke = if selected {
        egui::Stroke::new(2.0, theme::ACCENT)
    } else if multi {
        egui::Stroke::new(2.0, egui::Color32::from_rgb(0x8F, 0xAF, 0xFF))
    } else if hovered {
        egui::Stroke::new(1.0, egui::Color32::from_rgb(0x4A, 0x4D, 0x58))
    } else {
        egui::Stroke::new(1.0, theme::BORDER)
    };
    p.rect_stroke(rect, 8, stroke, egui::StrokeKind::Inside);

    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

// ---------- 檔案與排序 ----------

/// 副檔名是否在清單內（不分大小寫、不配置字串）
fn ext_in(p: &Path, exts: &[&str]) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
        .unwrap_or(false)
}

fn is_image(p: &Path) -> bool {
    ext_in(p, IMAGE_EXTS)
}

fn is_audio(p: &Path) -> bool {
    ext_in(p, AUDIO_EXTS)
}

fn is_project_file(p: &Path) -> bool {
    ext_in(p, &[PROJECT_EXT])
}

/// Windows 路徑不分大小寫的比較（最近專案清單去重用）
fn same_path_ci(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

fn collect_images_in_dir(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            // file_type 來自目錄列舉本身的資料，不像 path.is_file() 要對
            // 每個檔案再查一次檔案屬性，大資料夾可省數千次系統呼叫
            if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                let p = e.path();
                if is_image(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// 自然排序：img2.jpg 會排在 img10.jpg 前面
fn natural_sort(paths: &mut [PathBuf]) {
    // 每個路徑只建一次排序鍵；用 sort_by 逐次比較時每次都要重新轉小寫、
    // 解析數字並配置字串，大量照片時排序會慢上一個數量級
    paths.sort_by_cached_key(|p| natural_key(&p.to_string_lossy().to_lowercase()));
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum NatPart {
    Num(u128),
    Text(String),
}

fn natural_key(s: &str) -> Vec<NatPart> {
    let mut parts = Vec::new();
    let mut buf = String::new();
    let mut is_num = false;
    for c in s.chars() {
        let d = c.is_ascii_digit();
        if !buf.is_empty() && d != is_num {
            parts.push(flush_part(&buf, is_num));
            buf.clear();
        }
        is_num = d;
        buf.push(c);
    }
    if !buf.is_empty() {
        parts.push(flush_part(&buf, is_num));
    }
    parts
}

fn flush_part(buf: &str, is_num: bool) -> NatPart {
    if is_num {
        NatPart::Num(buf.parse().unwrap_or(0))
    } else {
        NatPart::Text(buf.to_string())
    }
}

/// 讀取系統中文字型檔（約 20MB+）；在 main 一開始的背景執行緒呼叫，
/// 與視窗建立同時進行，啟動時不用再等這段磁碟讀取
fn load_cjk_font_bytes() -> Option<Vec<u8>> {
    let candidates = [
        r"C:\Windows\Fonts\msjh.ttc",    // 微軟正黑體
        r"C:\Windows\Fonts\msjhbd.ttc",  // 微軟正黑體（粗體）
        r"C:\Windows\Fonts\mingliu.ttc", // 細明體
        r"C:\Windows\Fonts\msyh.ttc",    // 微軟雅黑（備援）
    ];
    candidates
        .iter()
        .find_map(|path| std::fs::read(path).ok())
}

fn setup_chinese_fonts(ctx: &egui::Context, bytes: Option<Vec<u8>>) {
    let Some(bytes) = bytes else { return };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("cjk".into(), egui::FontData::from_owned(bytes).into());
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .push("cjk".into());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push("cjk".into());
    ctx.set_fonts(fonts);
}

fn concat_escape(p: &Path) -> String {
    // concat demuxer 會把相對路徑解析成「相對於清單檔所在目錄」（暫存資料夾），
    // 以相對路徑啟動程式或走 CLI 模式時就會找不到照片。先轉成絕對路徑再寫入清單
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    abs.to_string_lossy().replace('\\', "/").replace('\'', r"'\''")
}

#[derive(Clone, Copy, PartialEq)]
enum H264Encoder {
    Nvenc,
    Qsv,
    Amf,
    Software,
}

impl H264Encoder {
    fn display_name(&self) -> &'static str {
        match self {
            H264Encoder::Nvenc => "NVIDIA NVENC（硬體加速）",
            H264Encoder::Qsv => "Intel Quick Sync（硬體加速）",
            H264Encoder::Amf => "AMD AMF（硬體加速）",
            H264Encoder::Software => "libx264（軟體）",
        }
    }

    fn codec_args(&self) -> Vec<&'static str> {
        match self {
            H264Encoder::Nvenc => vec![
                "-c:v", "h264_nvenc", "-preset", "p4", "-rc", "vbr", "-cq", "23", "-b:v", "0",
            ],
            H264Encoder::Qsv => vec!["-c:v", "h264_qsv", "-global_quality", "23"],
            H264Encoder::Amf => vec![
                "-c:v", "h264_amf", "-quality", "balanced", "-rc", "cqp", "-qp_i", "22",
                "-qp_p", "22",
            ],
            H264Encoder::Software => vec!["-c:v", "libx264", "-preset", "veryfast", "-crf", "18"],
        }
    }
}

/// 用 0.2 秒的測試片段確認編碼器真的能用（清單有列不代表有對應的 GPU）
fn test_encoder(name: &str) -> bool {
    let mut cmd = FfmpegCommand::new();
    cmd.args([
        "-hide_banner",
        "-f", "lavfi",
        "-i", "color=black:s=640x360:d=0.2",
        "-pix_fmt", "yuv420p",
        "-c:v", name,
        "-f", "null", "-",
    ]);
    let Ok(mut child) = cmd.spawn() else { return false };
    if let Ok(iter) = child.iter() {
        for _ in iter {}
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

/// 偵測可用的 H.264 硬體編碼器（只偵測一次並快取）
fn detect_h264_encoder() -> H264Encoder {
    static DETECTED: OnceLock<H264Encoder> = OnceLock::new();
    *DETECTED.get_or_init(|| {
        // 三種硬體編碼器平行測試；逐一測試時沒有對應硬體的機器
        // 每個候選都要等它失敗才輪到下一個，最壞要白等三段測試時間
        let candidates = [
            ("h264_nvenc", H264Encoder::Nvenc),
            ("h264_qsv", H264Encoder::Qsv),
            ("h264_amf", H264Encoder::Amf),
        ];
        let handles = candidates
            .map(|(name, enc)| thread::spawn(move || test_encoder(name).then_some(enc)));
        // 依候選順序取結果，優先序維持 NVENC > QSV > AMF
        for h in handles {
            if let Ok(Some(enc)) = h.join() {
                return enc;
            }
        }
        H264Encoder::Software
    })
}

fn filter_threads() -> String {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .to_string()
}

/// ffmpeg 是否已確認可用；ffmpeg_is_installed 每次都會啟動外部程序檢查，
/// 確認一次後快取，預覽渲染就不用每張都多付一次程序啟動的成本
static FFMPEG_READY: AtomicBool = AtomicBool::new(false);

/// 轉檔中的 ffmpeg 行程 ID（0 表示沒有）。ffmpeg 是獨立子行程，
/// Windows 上父行程結束不會連帶終止它：轉檔中關閉視窗，ffmpeg 會留在
/// 背景繼續編碼佔用 CPU，還寫出使用者以為已取消的輸出檔。
/// 關閉程式時（App::drop）據此主動終止
static CONVERT_FFMPEG_PID: AtomicU32 = AtomicU32::new(0);

/// 使用者按下「取消轉換」：終止 ffmpeg 並讓退回軟體編碼的邏輯不再重跑；
/// worker 據此把中斷回報為「已取消」而非轉換失敗
static CONVERT_CANCEL: AtomicBool = AtomicBool::new(false);

/// 強制終止指定行程（含其子行程）；行程已結束時安靜失敗
#[cfg(windows)]
fn kill_pid(pid: u32) {
    use std::os::windows::process::CommandExt;
    // 不建主控台視窗：本程式為 windows 子系統，taskkill 是主控台程式，
    // 不加旗標會在關閉瞬間閃出黑窗
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}
#[cfg(not(windows))]
fn kill_pid(_pid: u32) {}

/// 在檔案總管開啟並選取指定檔案（比只開父目錄好找）。
/// 檔案已不在（被刪/移動）時退回開啟父目錄
#[cfg(windows)]
fn open_in_explorer(path: &Path) {
    use std::os::windows::process::CommandExt;
    if path.exists() {
        // /select 的引數格式特殊（逗號後直接接路徑），用 raw_arg 原樣傳遞；
        // 路徑以引號包住以容忍空白
        let _ = std::process::Command::new("explorer")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn();
    } else if let Some(dir) = path.parent() {
        let _ = std::process::Command::new("explorer").arg(dir).spawn();
    }
}
#[cfg(not(windows))]
fn open_in_explorer(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}

/// 用系統預設程式開啟檔案（轉檔完成後直接播放影片用）
#[cfg(windows)]
fn open_file(path: &Path) {
    // explorer <檔案> 會以副檔名關聯的預設程式開啟（影片即播放器）
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}
#[cfg(not(windows))]
fn open_file(path: &Path) {
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

/// 序列化「檢查＋下載」：首次使用時預覽與轉檔可能同時走到這裡，
/// 兩個 auto_download 並發會互踩同一個安裝目錄，留下損毀的 ffmpeg
static FFMPEG_INIT: Mutex<()> = Mutex::new(());

/// 正在下載 ffmpeg。預覽走的下載沒有進度提示，UI 靠這個旗標在預覽區
/// 顯示「首次使用，正在下載…」而非只有一個轉圈，避免使用者以為當機
static FFMPEG_DOWNLOADING: AtomicBool = AtomicBool::new(false);

fn ensure_ffmpeg(on_download: impl Fn()) -> Result<(), String> {
    if FFMPEG_READY.load(Ordering::Relaxed) {
        return Ok(());
    }
    let _guard = FFMPEG_INIT.lock().unwrap();
    // 拿到鎖後再確認一次：先到的執行緒可能已完成下載
    if FFMPEG_READY.load(Ordering::Relaxed) {
        return Ok(());
    }
    if !ffmpeg_sidecar::command::ffmpeg_is_installed() {
        on_download();
        FFMPEG_DOWNLOADING.store(true, Ordering::Relaxed);
        let result = ffmpeg_sidecar::download::auto_download();
        FFMPEG_DOWNLOADING.store(false, Ordering::Relaxed);
        result.map_err(|e| format!("FFmpeg 下載失敗：{e}"))?;
    }
    FFMPEG_READY.store(true, Ordering::Relaxed);
    Ok(())
}

/// 預覽底圖快取：拖動調色滑桿時會對同一張照片反覆重渲染，
/// 把「縮放後的底圖」存成暫存 BMP 後改以小圖當輸入，
/// 免去每次重新解碼原始大圖（大照片可省下大半渲染時間）
struct PreviewBase {
    /// (照片路徑, 檔案修改時間, 預覽畫布寬高)。
    /// 畫布寬不再固定 960（直向輸出以高度為限），寬高都要進 key，
    /// 否則同高不同寬的兩種解析度會誤用彼此的底圖
    key: (PathBuf, Option<SystemTime>, (u32, u32)),
    /// None 表示建置中或建置失敗（失敗就維持完整流程，不重試）
    file: Option<PathBuf>,
    /// 底圖檔名流水號：新舊底圖用不同檔名，替換時不互踩
    serial: u64,
}

/// LRU 快取（尾端為最近使用）：保留多張，來回切換比較照片時不用重複解碼
static PREVIEW_BASE: Mutex<Vec<PreviewBase>> = Mutex::new(Vec::new());
/// 底圖快取張數上限（每張約 1.5MB 暫存檔）
const PREVIEW_BASE_CAP: usize = 4;
/// 底圖檔名全域流水號
static PREVIEW_BASE_SERIAL: AtomicU64 = AtomicU64::new(0);

/// 預覽畫布尺寸：輸出解析度等比縮放至 960×960 邊界內（取偶數、至少 2）。
/// 橫向輸出與舊版「寬固定 960」結果相同；直向輸出以高度為限（如 1080×1920
/// → 540×960），不再產生比顯示區大數倍的像素緩衝；極端長寬比也不會把
/// 另一邊四捨五入成 0 讓 ffmpeg 的 scale 直接失敗
fn preview_canvas(res: Resolution) -> (u32, u32) {
    let s = (960.0 / res.w as f64).min(960.0 / res.h as f64);
    let pw = ((res.w as f64 * s / 2.0).round() as u32 * 2).max(2);
    let ph = ((res.h as f64 * s / 2.0).round() as u32 * 2).max(2);
    (pw, ph)
}

/// 在背景為指定照片預先建置預覽底圖（已存在、建置中或 ffmpeg 未就緒則跳過）。
/// 停留在一張照片時先把鄰近照片準備好，切換過去直接命中快取
fn prefetch_preview_base(photo: PathBuf, pw: u32, ph: u32) {
    if !FFMPEG_READY.load(Ordering::Relaxed) {
        return;
    }
    let key = (
        photo.clone(),
        std::fs::metadata(&photo).and_then(|m| m.modified()).ok(),
        (pw, ph),
    );
    let (out, serial) = {
        let mut g = PREVIEW_BASE.lock().unwrap();
        if g.iter().any(|b| b.key == key) {
            return;
        }
        if g.len() >= PREVIEW_BASE_CAP {
            let old = g.remove(0);
            if let Some(f) = old.file {
                let _ = std::fs::remove_file(f);
            }
        }
        let serial = PREVIEW_BASE_SERIAL.fetch_add(1, Ordering::Relaxed);
        g.push(PreviewBase {
            key: key.clone(),
            file: None,
            serial,
        });
        (temp_path(&format!("prev_base_{serial}.bmp")), serial)
    };
    let ok = (|| {
        let mut cmd = FfmpegCommand::new();
        cmd.arg("-y")
            .input(photo.to_string_lossy())
            .args([
                "-vf",
                &format!("scale={pw}:{ph}:force_original_aspect_ratio=decrease"),
            ])
            .args(["-frames:v", "1"])
            .output(out.to_string_lossy());
        let mut child = cmd.spawn().ok()?;
        if let Ok(iter) = child.iter() {
            for _ in iter {}
        }
        child.wait().ok().filter(|s| s.success())
    })()
    .is_some();
    let mut stored = false;
    if ok {
        let mut g = PREVIEW_BASE.lock().unwrap();
        if let Some(b) = g.iter_mut().find(|b| b.serial == serial && b.key == key) {
            b.file = Some(out.clone());
            stored = true;
        }
    }
    if !stored {
        let _ = std::fs::remove_file(&out);
    }
}

/// 這次預覽渲染與底圖快取的關係
enum BaseRole {
    /// 命中：直接以底圖當輸入
    Cached(PathBuf),
    /// 未命中或建置中：本次以完整流程渲染
    Skip,
}

/// 用 ffmpeg 渲染單張照片的預覽（含調色，縮小並依輸出比例補邊），回傳 RGB 像素
fn render_preview(photo: &Path, adj: &Adjustments, res: Resolution) -> PreviewResult {
    ensure_ffmpeg(|| {})?;

    let (pw, ph) = preview_canvas(res);

    // 查底圖快取：命中就以縮小後的底圖當輸入；未命中（第一次看這張照片）
    // 就在本次渲染順便輸出底圖，原圖只解碼一次
    let key = (
        photo.to_path_buf(),
        std::fs::metadata(photo).and_then(|m| m.modified()).ok(),
        (pw, ph),
    );
    let base = {
        let mut g = PREVIEW_BASE.lock().unwrap();
        if let Some(pos) = g.iter().position(|b| b.key == key) {
            // 命中：移到尾端（最近使用）
            let b = g.remove(pos);
            let role = match &b.file {
                Some(f) => BaseRole::Cached(f.clone()),
                None => BaseRole::Skip,
            };
            g.push(b);
            role
        } else {
            // 未命中：本次以完整流程渲染，底圖交給背景執行緒另行建置。
            // 不能在同一次 ffmpeg 用 split 雙輸出（rawvideo 管線＋BMP 檔案）：
            // sidecar 讀取 stdout 影格時遇到第二個輸出會失敗，
            // ffmpeg 寫入被關閉的管線便回報 Invalid argument
            let photo2 = photo.to_path_buf();
            thread::spawn(move || prefetch_preview_base(photo2, pw, ph));
            BaseRole::Skip
        }
    };

    // 與輸出相同順序：先縮放、再調色、後補邊。
    // chain 為縮放之後的濾鏡（調色、補邊）；清晰度半徑依預覽寬與輸出寬的比例縮放
    let adjust_mid = adj
        .filter_chain(pw as f64 / res.w as f64)
        .map(|c| format!("{c},"))
        .unwrap_or_default();
    let chain = format!("{adjust_mid}pad={pw}:{ph}:(ow-iw)/2:(oh-ih)/2:color=black");
    let scale = format!("scale={pw}:{ph}:force_original_aspect_ratio=decrease");

    // 直接以 rawvideo 從 stdout 取回 RGB 像素，省去圖檔編碼、寫檔與再解碼
    let render_once = |input: &Path, vf: &str| -> PreviewResult {
        let mut cmd = FfmpegCommand::new();
        cmd.input(input.to_string_lossy())
            .args(["-vf", vf])
            .args(["-frames:v", "1"])
            .rawvideo();
        let mut error_log: Vec<String> = Vec::new();
        let mut frame: Option<(u32, u32, Vec<u8>)> = None;
        let mut child = cmd.spawn().map_err(|e| format!("FFmpeg 啟動失敗：{e}"))?;
        let iter = child
            .iter()
            .map_err(|e| format!("FFmpeg 輸出讀取失敗：{e}"))?;
        for event in iter {
            use ffmpeg_sidecar::event::LogLevel;
            match event {
                FfmpegEvent::OutputFrame(f) => frame = Some((f.width, f.height, f.data)),
                FfmpegEvent::Log(LogLevel::Error | LogLevel::Fatal, msg) => {
                    error_log.push(msg);
                }
                _ => {}
            }
        }
        let status = child.wait().map_err(|e| format!("FFmpeg 執行失敗：{e}"))?;
        if status.success() {
            if let Some(f) = frame {
                return Ok(f);
            }
        }
        Err(if error_log.is_empty() {
            "預覽渲染失敗".into()
        } else {
            error_log.join("\n")
        })
    };

    let full_vf = format!("{scale},{chain}");
    match &base {
        // 底圖已是縮放後的尺寸，直接跑後段濾鏡
        BaseRole::Cached(f) => render_once(f, &chain).or_else(|_| {
            // 取得路徑到 ffmpeg 開檔之間，底圖可能剛被 LRU 汰換刪除
            // （或被清暫存/防毒移走）：直接失敗會卡在「預覽失敗」且
            // dirty 已清、不會自動重試。移除失效的快取項目後
            // 退回完整流程重渲染
            {
                let mut g = PREVIEW_BASE.lock().unwrap();
                if let Some(pos) = g.iter().position(|b| b.key == key) {
                    let old = g.remove(pos);
                    if let Some(f) = old.file {
                        let _ = std::fs::remove_file(f);
                    }
                }
            }
            render_once(photo, &full_vf)
        }),
        BaseRole::Skip => render_once(photo, &full_vf),
    }
}

/// 把單張照片套用調色後輸出成暫存 PNG（已縮放至目標解析度內），
/// 供個別照片調色的輸出流程使用；主管線對這些暫存圖不再調色
fn pre_adjust_photo(
    photo: &Path,
    adj: &Adjustments,
    res: Resolution,
    out: &Path,
) -> Result<(), String> {
    let scale = format!(
        "scale={}:{}:force_original_aspect_ratio=decrease",
        res.w, res.h
    );
    let vf = match adj.filter_chain(1.0) {
        Some(c) => format!("{scale},{c}"),
        None => scale,
    };
    let mut cmd = FfmpegCommand::new();
    cmd.arg("-y")
        .input(photo.to_string_lossy())
        .args(["-vf", &vf])
        .args(["-frames:v", "1"])
        .output(out.to_string_lossy());
    let mut child = cmd.spawn().map_err(|e| format!("FFmpeg 啟動失敗：{e}"))?;
    let mut errs: Vec<String> = Vec::new();
    if let Ok(iter) = child.iter() {
        use ffmpeg_sidecar::event::LogLevel;
        for ev in iter {
            if let FfmpegEvent::Log(LogLevel::Error | LogLevel::Fatal, m) = ev {
                errs.push(m);
            }
        }
    }
    let st = child.wait().map_err(|e| format!("FFmpeg 執行失敗：{e}"))?;
    if !st.success() || !out.exists() {
        let name = photo
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| photo.display().to_string());
        return Err(if errs.is_empty() {
            format!("照片「{name}」調色失敗")
        } else {
            format!("照片「{name}」調色失敗：{}", errs.join("\n"))
        });
    }
    Ok(())
}

/// 輸出用的一段文字（照片區間為 0-based 含端點）
struct TextJob {
    s: usize,
    e: usize,
    text: String,
    x: f32,
    y: f32,
    size: i32,
    rot: f32,
}

/// 一次轉換的文字資料：全域樣式 + 各段文字
struct SubtitleJob {
    style: SubtitleStyle,
    font: Option<PathBuf>,
    entries: Vec<TextJob>,
}

// 轉換任務的完整參數組，拆包裝反而失去可讀性
#[allow(clippy::too_many_arguments)]
fn run_conversion(
    photos: &[PathBuf],
    fps: u32,
    format: OutputFormat,
    res: Resolution,
    adj: &Adjustments,
    adj_overrides: &HashMap<PathBuf, Adjustments>,
    subs: &SubtitleJob,
    fx: &OutputFx,
    output: &Path,
    send: &dyn Fn(WorkerMsg),
) -> Result<(), String> {
    if photos.is_empty() {
        return Err("沒有照片可轉換".into());
    }

    // 轉檔前先確認所有輸入檔仍在（照片或音樂可能在加入後被移除、磁碟斷線）。
    // 不先擋下的話，ffmpeg 會編碼到讀取該檔那一步才失敗，白費前面的時間；
    // 加上硬體編碼失敗會再退回軟體重跑，缺檔更會浪費雙倍時間
    if let Some(missing) = photos.iter().find(|p| !p.exists()) {
        let name = missing
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| missing.display().to_string());
        return Err(format!(
            "找不到照片檔案「{name}」，可能已被移除或所在磁碟未連接"
        ));
    }
    if let Some(m) = &fx.music {
        if !m.path.exists() {
            let name = m
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| m.path.display().to_string());
            return Err(format!(
                "找不到背景音樂檔案「{name}」，可能已被移除或所在磁碟未連接"
            ));
        }
    }

    // 第一次執行時自動下載 ffmpeg
    ensure_ffmpeg(|| {
        send(WorkerMsg::Status(
            "第一次使用，正在下載 FFmpeg（約 80MB，請稍候）…".into(),
        ));
    })?;

    // 個別照片調色：任一張的生效調色與全域不同時，先把「生效調色非中性」的照片
    // 各自套用調色輸出成暫存圖（已縮放至目標解析度內），主管線的濾鏡鏈就不再帶
    // 全域調色，避免對已調色的照片疊套兩次
    let has_per_photo = photos
        .iter()
        .any(|p| adj_overrides.get(p).is_some_and(|o| o != adj));
    let mut adj_temp: Vec<PathBuf> = Vec::new();
    let mut src_photos: Vec<PathBuf> = photos.to_vec();
    if has_per_photo {
        let need: Vec<usize> = (0..photos.len())
            .filter(|&i| {
                !adj_overrides
                    .get(&photos[i])
                    .copied()
                    .unwrap_or(*adj)
                    .is_neutral()
            })
            .collect();
        for (k, &i) in need.iter().enumerate() {
            // 主編碼還沒開始（CONVERT_FFMPEG_PID 為 0，取消無法 kill），
            // 在這裡檢查取消旗標，讓套用調色階段的取消也能提早中止
            if CONVERT_CANCEL.load(Ordering::Relaxed) {
                for f in &adj_temp {
                    let _ = std::fs::remove_file(f);
                }
                return Err("已取消".into());
            }
            send(WorkerMsg::Status(format!(
                "套用照片調色…（{}/{}）",
                k + 1,
                need.len()
            )));
            let eff = adj_overrides.get(&photos[i]).copied().unwrap_or(*adj);
            let out = temp_path(&format!("adj_{i}.png"));
            if let Err(e) = pre_adjust_photo(&photos[i], &eff, res, &out) {
                for f in &adj_temp {
                    let _ = std::fs::remove_file(f);
                }
                return Err(e);
            }
            src_photos[i] = out.clone();
            adj_temp.push(out);
        }
    }

    send(WorkerMsg::Status("建立照片清單…".into()));

    // AVI 容器在低幀率＋音訊下，muxer 會把最後一格多撐一兩個影格週期，使影片時長
    // 比預期長（fps 越低越明顯，2fps 可差近 1 秒）。升頻到 30fps 讓誤差縮到一個影格
    // 以內；重複的靜態影格在 H264 幾乎不增加檔案大小
    let avi_with_audio = matches!(format, OutputFormat::Avi) && fx.music.is_some();
    let animated = fx.ken_burns || fx.transition != Transition::None || avi_with_audio;

    // 每張照片的顯示秒數。Ken Burns 用 zoompan 以「每張輸出 kb_frames 格」計時，
    // 需把實際秒數對齊到 30fps 的格數，字幕與轉場時間點才不會累積漂移
    let duration = 1.0 / fps as f64;
    let (eff_dur, kb_frames) = if fx.ken_burns {
        let d = ((duration * OUT_FPS as f64).round() as i64).max(1);
        (d as f64 / OUT_FPS as f64, d)
    } else {
        (duration, 0)
    };
    let total_secs = photos.len() as f64 * eff_dur;

    // 用 concat demuxer 列出每張照片與顯示時間（個別調色的照片指向暫存圖）
    let mut list = String::new();
    for p in &src_photos {
        list.push_str(&format!("file '{}'\nduration {duration}\n", concat_escape(p)));
    }
    // concat demuxer 的慣例：最後一張要再列一次，最後一段 duration 才會生效
    if let Some(last) = src_photos.last() {
        list.push_str(&format!("file '{}'\n", concat_escape(last)));
    }

    let list_path = temp_path("list.txt");
    std::fs::write(&list_path, &list).map_err(|e| format!("無法寫入暫存清單：{e}"))?;

    let (w, h) = (res.w, res.h);
    // 先縮放到目標解析度再調色（大照片可省下數倍運算），最後補邊，
    // 黑邊仍不受亮度、曝光等調整影響。輸出即目標解析度，清晰度半徑用原始 13px
    let adjust_mid = if has_per_photo {
        // 個別調色模式：每張照片的調色已烙進暫存圖，主鏈不再調色
        String::new()
    } else {
        adj.filter_chain(1.0)
            .map(|c| format!("{c},"))
            .unwrap_or_default()
    };
    let mut vf = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease,{adjust_mid}\
         pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black"
    );

    // 動態縮放（Ken Burns）：每張照片產生 kb_frames 格緩慢推近/拉遠（奇偶張交替）；
    // 沒有 Ken Burns 但有轉場時，用 fps 濾鏡升頻，讓淡入淡出有足夠格數呈現
    if fx.ken_burns {
        let d = kb_frames;
        let dm = (d - 1).max(1);
        vf.push_str(&format!(
            ",zoompan=z='if(mod(floor(on/{d}),2),1.081-0.08*min(mod(on,{d})/{dm},1),1.001+0.08*min(mod(on,{d})/{dm},1))'\
             :x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':d={d}:s={w}x{h}:fps={OUT_FPS}"
        ));
    } else if animated {
        vf.push_str(&format!(",fps={OUT_FPS}"));
    }

    // 文字：先全部依清單順序收集（不分旋轉與否）。輸出的圖層順序必須與預覽
    // 一致——預覽以清單順序繪製，故此處也依清單順序套用（見下方 use_complex）
    let out_fps = if animated { OUT_FPS } else { fps };
    let mut caption_files: Vec<PathBuf> = Vec::new();
    // (該段文字, 字幕檔, 顯示區間, 字級像素)
    let mut ops: Vec<(&TextJob, PathBuf, (f64, f64), f64)> = Vec::new();
    if subs.font.is_some() {
        let d = eff_dur;
        // 以半個「輸出影格」為緩衝，準確涵蓋第 s ~ e 張照片的所有格；
        // 動態模式輸出為 30fps，若以照片時長的比例當緩衝，
        // fps 低時字幕會提早出現/消失（如每張 1 秒會早 0.25 秒）
        let buf = 0.5 / out_fps as f64;
        for (k, en) in subs.entries.iter().enumerate() {
            let text = en.text.trim_end();
            if text.is_empty() {
                continue;
            }
            let cap_path = temp_path(&format!("cap_{k}.txt"));
            std::fs::write(&cap_path, text).map_err(|e| format!("無法寫入字幕暫存檔：{e}"))?;
            let enable = (en.s as f64 * d - buf, (en.e as f64 + 1.0) * d - buf);
            let fontsize = en.size as f64 * h as f64 / 1080.0;
            ops.push((en, cap_path.clone(), enable, fontsize));
            caption_files.push(cap_path);
        }
    }

    // 有任何旋轉文字才需要 filter_complex（旋轉要走「透明畫布→rotate→overlay」）；
    // 全部無旋轉時維持單純的 -vf 路徑。無旋轉文字在此直接依序串進 vf
    let use_complex = ops.iter().any(|(en, ..)| en.rot.abs() >= 0.5);
    if !use_complex {
        if let Some(font) = &subs.font {
            for (en, cap, enable, fontsize) in &ops {
                vf.push(',');
                vf.push_str(&drawtext_filter(
                    font, cap, &subs.style, *fontsize, en.x, en.y, Some(*enable),
                ));
            }
        }
    }

    // 轉場（淡入淡出）：eq 以每格評估的週期函數在每個切點附近把亮度與飽和度壓到黑，
    // 首尾也各有一次淡入/淡出；不用串接大量 fade 濾鏡（fade 的 st 前後會整段變黑）
    let mut tail = String::new();
    if fx.transition == Transition::FadeBlack {
        // 淡出時間不能超過半張照片時長，否則畫面中點也回不到全亮：
        // fps 高於 10 時 0.05 秒的下限會超過 d/2，整支影片會恆定變暗
        let f = (eff_dur * 0.4).clamp(0.05, 0.5).min(eff_dur * 0.5);
        let dip = format!(
            "max(0,1-min(mod(t,{d:.6}),{d:.6}-mod(t,{d:.6}))/{f:.6})",
            d = eff_dur,
            f = f
        );
        tail.push_str(&format!(
            "eq=eval=frame:brightness='-{dip}':saturation='max(0,1-{dip})',"
        ));
    }
    // 編碼前強制輸出為偶數尺寸：某些濾鏡組合（如 zoompan、rotate、overlay）
    // 可能算出奇數高/寬（如 1920x1081），H.264（尤其 libx264）要求 yuv420p
    // 的長寬必須為偶數，否則會以「height not divisible by 2」失敗。
    // crop 至最接近的偶數（最多裁掉 1 像素，肉眼無感）
    tail.push_str("crop=trunc(iw/2)*2:trunc(ih/2)*2,setsar=1,format=yuv420p");

    // 先把視訊濾鏡定案：有旋轉文字走 filter_complex，否則走 -vf
    let filter_complex = if use_complex {
        // 依清單順序逐段套用：無旋轉直接在鏈上 drawtext；旋轉則畫在全幅透明畫布
        // 中央 → 以畫布中心旋轉 → overlay 到目標位置。順序與預覽一致，重疊時
        // 後加入的文字才會正確蓋在先加入的之上（不再一律讓旋轉文字浮在最上層）
        let font = subs.font.as_ref().unwrap();
        let mut fc = format!("[0:v]{vf}[v0];");
        let mut cur = "v0".to_string();
        for (i, (en, cap, (ea, eb), fontsize)) in ops.iter().enumerate() {
            let next = format!("v{}", i + 1);
            if en.rot.abs() < 0.5 {
                let dt =
                    drawtext_filter(font, cap, &subs.style, *fontsize, en.x, en.y, Some((*ea, *eb)));
                fc.push_str(&format!("[{cur}]{dt}[{next}];"));
            } else {
                let dt = drawtext_filter(font, cap, &subs.style, *fontsize, 0.5, 0.5, None);
                let rad = (en.rot as f64).to_radians();
                fc.push_str(&format!(
                    "color=c=black@0.0:s={w}x{h}:r={out_fps}:d={total_secs:.3},format=rgba,{dt},rotate={rad:.6}:ow=iw:oh=ih:c=none[t{i}];"
                ));
                let ox = ((en.x - 0.5) * w as f32).round();
                let oy = ((en.y - 0.5) * h as f32).round();
                fc.push_str(&format!(
                    "[{cur}][t{i}]overlay=x={ox:.0}:y={oy:.0}:enable='between(t,{ea:.3},{eb:.3})'[{next}];"
                ));
            }
            cur = next;
        }
        fc.push_str(&format!("[{cur}]{tail}[vout]"));
        Some(fc)
    } else {
        vf.push(',');
        vf.push_str(&tail);
        None
    };

    // 音訊濾鏡（有背景音樂時）
    let audio_filter = fx.music.as_ref().map(|m| {
        let mut af = format!("volume={:.3}", m.volume.max(0) as f64 / 100.0);
        // 結尾淡出：最多 2 秒；短片按總長一半縮短，才不會比影片還長
        if m.fade_out && total_secs > 1.0 {
            let fd = (total_secs * 0.5).min(2.0);
            af.push_str(&format!(",afade=t=out:st={:.3}:d={fd:.3}", total_secs - fd));
        }
        af
    });

    let total_frames = if animated {
        (total_secs * OUT_FPS as f64) as f32
    } else {
        photos.len() as f32
    };
    let out_fps_str = out_fps.to_string();
    let total_secs_str = format!("{total_secs}");
    let ft = filter_threads();

    // 以指定的視訊編碼器參數組裝並執行一次 ffmpeg。抽成閉包，硬體編碼器失敗時
    // 可用軟體編碼器重跑同一組濾鏡設定
    let run_once = |video_codec: &[&str]| -> Result<(), String> {
        // 取消可能發生在 ffmpeg 尚未啟動的空檔（首次下載 ffmpeg、準備階段）：
        // 此時 kill 的 PID 為 0 無效，若不在此攔下，主編碼一啟動就會跑到底
        if CONVERT_CANCEL.load(Ordering::Relaxed) {
            return Err("已取消".into());
        }
        let mut cmd = FfmpegCommand::new();
        cmd.arg("-y")
            .args(["-filter_threads", &ft])
            .args(["-f", "concat", "-safe", "0"])
            .input(list_path.to_string_lossy());
        // 背景音樂：無限循環讀取，總長由 -t 截止（音樂短會循環、長會被裁切）
        if let Some(m) = &fx.music {
            cmd.args(["-stream_loop", "-1"]);
            cmd.input(m.path.to_string_lossy());
        }
        if let Some(fc) = &filter_complex {
            cmd.args(["-filter_complex", fc]).args(["-map", "[vout]"]);
        } else {
            cmd.args(["-vf", &vf]);
        }
        cmd.args(["-r", &out_fps_str]).args(["-t", &total_secs_str]);
        if let Some(af) = &audio_filter {
            if filter_complex.is_some() {
                cmd.args(["-map", "1:a"]);
            } else {
                cmd.args(["-map", "0:v", "-map", "1:a"]);
            }
            cmd.args(["-af", af]);
            if matches!(format, OutputFormat::Webm) {
                cmd.args(["-c:a", "libopus", "-b:a", "128k"]);
            } else {
                cmd.args(["-c:a", "aac", "-b:a", "192k"]);
            }
        }
        cmd.args(video_codec);
        cmd.output(output.to_string_lossy());

        let mut error_log: Vec<String> = Vec::new();
        let mut child = cmd.spawn().map_err(|e| format!("FFmpeg 啟動失敗：{e}"))?;
        // 記下行程 ID：轉檔中關閉視窗時，App::drop 據此終止 ffmpeg
        CONVERT_FFMPEG_PID.store(child.as_inner().id(), Ordering::Relaxed);
        let iter = child
            .iter()
            .map_err(|e| format!("FFmpeg 輸出讀取失敗：{e}"))?;
        for event in iter {
            match event {
                FfmpegEvent::Progress(p) => {
                    let frac = (p.frame as f32 / total_frames).clamp(0.0, 1.0);
                    send(WorkerMsg::Progress(frac));
                }
                FfmpegEvent::Log(level, msg) => {
                    use ffmpeg_sidecar::event::LogLevel;
                    if matches!(level, LogLevel::Error | LogLevel::Fatal) {
                        error_log.push(msg);
                    }
                }
                _ => {}
            }
        }
        let status = child.wait();
        CONVERT_FFMPEG_PID.store(0, Ordering::Relaxed);
        let status = status.map_err(|e| format!("FFmpeg 執行失敗：{e}"))?;
        if !status.success() {
            let detail = error_log.join("\n");
            return Err(if detail.is_empty() {
                "FFmpeg 轉換失敗".into()
            } else {
                format!("FFmpeg 轉換失敗：{detail}")
            });
        }
        Ok(())
    };

    let result = match format {
        OutputFormat::Webm => {
            send(WorkerMsg::Status("轉換中…（編碼器：VP9）".into()));
            run_once(&[
                "-c:v", "libvpx-vp9", "-b:v", "0", "-crf", "30", "-cpu-used", "5", "-row-mt", "1",
            ])
        }
        _ => {
            send(WorkerMsg::Status("偵測硬體編碼器…".into()));
            let enc = detect_h264_encoder();
            send(WorkerMsg::Status(format!(
                "轉換中…（編碼器：{}）",
                enc.display_name()
            )));
            let first = run_once(&enc.codec_args());
            // 硬體編碼器可能因驅動/負載不穩而中途失敗（如 AMD AMF 的
            // SubmitInput 錯誤）；此時自動退回軟體 libx264 重跑，確保能轉出。
            // 但使用者按取消而 kill ffmpeg 導致的失敗不該退回軟體重跑
            if first.is_err()
                && enc != H264Encoder::Software
                && !CONVERT_CANCEL.load(Ordering::Relaxed)
            {
                send(WorkerMsg::Status(
                    "硬體編碼器失敗，改用軟體編碼重試…".into(),
                ));
                send(WorkerMsg::Progress(0.0));
                run_once(&H264Encoder::Software.codec_args())
            } else {
                first
            }
        }
    };

    let _ = std::fs::remove_file(&list_path);
    for f in &caption_files {
        let _ = std::fs::remove_file(f);
    }
    for f in &adj_temp {
        let _ = std::fs::remove_file(f);
    }
    // 轉檔失敗時清掉半成品輸出檔：ffmpeg 失敗常留下不完整的檔案，硬體編碼
    // 嘗試失敗更會先寫入一部分，留著會讓使用者誤以為成功、播到損毀的影片
    if result.is_err() {
        let _ = std::fs::remove_file(output);
    }
    result?;

    send(WorkerMsg::Progress(1.0));
    Ok(())
}

/// 命令列模式：photo2video --cli <照片資料夾> <fps> <輸出檔>
/// 輸出格式由輸出檔的副檔名決定（mp4/mkv/mov/avi/webm）
fn run_cli(args: &[String]) -> Result<(), String> {
    if args.len() != 3 {
        return Err("用法：photo2video --cli <照片資料夾> <fps> <輸出檔>".into());
    }
    let dir = PathBuf::from(&args[0]);
    let fps: u32 = args[1].parse().map_err(|_| "fps 必須是正整數".to_string())?;
    // fps=0 會讓每張顯示秒數變成 1/0=inf，寫進 concat 清單使 ffmpeg 直接失敗；
    // 與 GUI 一致限制在 1~60
    if !(1..=60).contains(&fps) {
        return Err("fps 必須介於 1 到 60".into());
    }
    let mut output = PathBuf::from(&args[2]);

    // 先分辨「找不到資料夾」與「資料夾內沒有圖片」：collect_images_in_dir 對
    // 不存在或非資料夾的路徑會靜默回傳空，直接沿用會誤報成「沒有圖片」
    if !dir.is_dir() {
        return Err(format!("找不到資料夾（或指定的不是資料夾）：{}", dir.display()));
    }
    let mut photos = collect_images_in_dir(&dir);
    natural_sort(&mut photos);
    if photos.is_empty() {
        return Err(format!("資料夾內沒有圖片：{}", dir.display()));
    }
    println!("共 {} 張照片，fps={fps}", photos.len());

    // 副檔名比對不分大小寫：out.WEBM 也要選 VP9，否則會落到預設 mp4(H264)、
    // 被 ffmpeg 依 .WEBM 寫成 H264-in-WebM 這種不相容檔案
    let known = ["mp4", "mkv", "mov", "avi", "webm"];
    let ext_lc = output
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let format = match ext_lc.as_deref() {
        Some("mkv") => OutputFormat::Mkv,
        Some("mov") => OutputFormat::Mov,
        Some("avi") => OutputFormat::Avi,
        Some("webm") => OutputFormat::Webm,
        _ => OutputFormat::Mp4,
    };
    // 確保輸出有正確副檔名：無副檔名時 ffmpeg 無法判斷容器會失敗；非影片副檔名
    // 則補上，保留使用者輸入的檔名（與 GUI 存檔行為一致）
    let ext = format.ext();
    match ext_lc.as_deref() {
        Some(e) if e == ext => {}
        Some(e) if known.contains(&e) => {
            output.set_extension(ext);
        }
        _ => {
            let name = output
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            output.set_file_name(format!("{name}.{ext}"));
        }
    }

    let send = |msg: WorkerMsg| match msg {
        WorkerMsg::Status(s) => println!("{s}"),
        WorkerMsg::Progress(p) => println!("進度 {:.0}%", p * 100.0),
        _ => {}
    };
    let no_subs = SubtitleJob {
        style: SubtitleStyle::default(),
        font: None,
        entries: Vec::new(),
    };
    let no_fx = OutputFx {
        transition: Transition::None,
        ken_burns: false,
        music: None,
    };
    run_conversion(
        &photos,
        fps,
        format,
        Resolution { w: 1920, h: 1080 },
        &Adjustments::default(),
        &HashMap::new(),
        &no_subs,
        &no_fx,
        &output,
        &send,
    )?;
    println!("完成：{}", output.display());
    Ok(())
}

/// release 版是 windows 子系統（無主控台），CLI 模式在終端機互動執行、且輸出
/// 未被重導向時，println! 會無處可去，使用者看不到進度與結果。附加到父行程
/// （cmd/powershell）的主控台讓輸出可見；若沒有父主控台（如雙擊執行），
/// AttachConsole 會失敗、屬無害
#[cfg(windows)]
fn attach_parent_console() {
    extern "system" {
        fn AttachConsole(dw_process_id: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
#[cfg(not(windows))]
fn attach_parent_console() {}

/// 載入內嵌的視窗圖示（標題列與工作列用）
fn load_app_icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../assets/icon_256.png"))
        .expect("內建圖示載入失敗")
        .to_rgba8();
    let (width, height) = img.dimensions();
    egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    }
}

fn main() -> eframe::Result {
    // 越早裝越好：連啟動階段（字型載入、視窗建立）的 panic 都要留下紀錄
    install_panic_hook();

    // 清掉上次一鍵更新留下的舊版檔案
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("exe.old"));
    }

    // 在背景清掉先前閃退/強制結束留下的孤兒暫存檔（不佔啟動時間）
    thread::spawn(clean_stale_temp_files);

    // 提早在背景讀取中文字型檔（20MB+），與視窗建立同時進行，縮短啟動時間
    let font_loader = thread::spawn(load_cjk_font_bytes);

    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "--cli" {
        // 需在任何 println! 之前附加主控台，否則 release 版的 CLI 輸出會看不到
        attach_parent_console();
        if let Err(e) = run_cli(&args[2..]) {
            eprintln!("錯誤：{e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // 其餘參數視為要開啟的照片、資料夾或 .p2v 專案檔
    // （支援「開啟方式」與拖曳到執行檔上）
    let mut initial_files: Vec<PathBuf> = Vec::new();
    for a in &args[1..] {
        let p = PathBuf::from(a);
        if p.is_dir() {
            initial_files.extend(collect_images_in_dir(&p));
        } else if p.is_file() {
            initial_files.push(p);
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Photo2Video — 照片轉影片")
            .with_icon(load_app_icon())
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([1024.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Photo2Video — 照片轉影片",
        options,
        Box::new(move |cc| {
            let cjk_font = font_loader.join().ok().flatten();
            Ok(Box::new(App::new(cc, initial_files, cjk_font)))
        }),
    )
}
