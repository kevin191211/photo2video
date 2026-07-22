#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
}

#[derive(Clone, Copy, PartialEq)]
struct Resolution {
    w: u32,
    h: u32,
}

impl Resolution {
    fn label(&self) -> String {
        match (self.w, self.h) {
            (1280, 720) => "HD 1280 × 720".into(),
            (1920, 1080) => "Full HD 1920 × 1080".into(),
            (2560, 1440) => "2K 2560 × 1440".into(),
            (3840, 2160) => "4K 3840 × 2160".into(),
            _ => format!("{} × {}", self.w, self.h),
        }
    }

    const ALL: [Resolution; 4] = [
        Resolution { w: 1280, h: 720 },
        Resolution { w: 1920, h: 1080 },
        Resolution { w: 2560, h: 1440 },
        Resolution { w: 3840, h: 2160 },
    ];
}

/// 調色參數，全部以 -100 ~ +100 表示，0 為不調整
#[derive(Clone, Copy, PartialEq, Default)]
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

    /// 轉成 ffmpeg 濾鏡串；全部為 0 時回傳 None
    fn filter_chain(&self) -> Option<String> {
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
            // 大半徑低強度的 unsharp ≈ 局部對比（清晰度）；負值則柔化
            let amount = self.clarity as f64 * 0.015;
            filters.push(format!(
                "unsharp=luma_msize_x=13:luma_msize_y=13:luma_amount={amount:.4}"
            ));
        }
        Some(filters.join(","))
    }
}

#[derive(Clone, Copy, PartialEq)]
enum SubPos {
    Top,
    Middle,
    Bottom,
}

impl SubPos {
    fn label(&self) -> &'static str {
        match self {
            SubPos::Top => "上方",
            SubPos::Middle => "置中",
            SubPos::Bottom => "下方",
        }
    }
    const ALL: [SubPos; 3] = [SubPos::Top, SubPos::Middle, SubPos::Bottom];
}

/// 字幕樣式（全域套用；大小以 1080p 高度為基準的像素值）
#[derive(Clone, PartialEq)]
struct SubtitleStyle {
    font_idx: usize,
    size: i32,
    color: egui::Color32,
    outline_w: i32,
    outline_color: egui::Color32,
    boxed: bool,
    pos: SubPos,
}

impl Default for SubtitleStyle {
    fn default() -> Self {
        Self {
            font_idx: 0,
            size: 48,
            color: egui::Color32::WHITE,
            outline_w: 2,
            outline_color: egui::Color32::BLACK,
            boxed: false,
            pos: SubPos::Bottom,
        }
    }
}

/// 一個字幕段落：從第 start 張到第 end 張（1-based、含端點）顯示同一段文字
#[derive(Clone, PartialEq)]
struct SubtitleEntry {
    start: usize,
    end: usize,
    text: String,
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

/// 設定檔路徑：%APPDATA%\photo2video\config.json
fn config_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("photo2video")
        .join("config.json")
}

/// 讀取上次儲存的每秒張數；沒有設定檔或值不合法時回傳 None
fn load_saved_fps() -> Option<u32> {
    let txt = std::fs::read_to_string(config_path()).ok()?;
    let json: serde_json::Value = serde_json::from_str(&txt).ok()?;
    let fps = json.get("fps")?.as_u64()? as u32;
    (1..=60).contains(&fps).then_some(fps)
}

/// 儲存每秒張數設定（失敗不影響使用，靜默忽略）
fn save_fps(fps: u32) {
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, serde_json::json!({ "fps": fps }).to_string());
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

/// 把 Windows 路徑轉成 filtergraph 內安全的形式（/ 分隔、跳脫冒號）
fn ff_path_escape(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/").replace(':', r"\:")
}

fn ff_color(c: egui::Color32) -> String {
    format!(
        "0x{:02X}{:02X}{:02X}@{:.3}",
        c.r(),
        c.g(),
        c.b(),
        c.a() as f32 / 255.0
    )
}

/// 產生一段 drawtext 濾鏡。fontsize 為實際像素；enable 為顯示時間區間
fn drawtext_filter(
    fontfile: &Path,
    textfile: &Path,
    style: &SubtitleStyle,
    fontsize: f64,
    frame_h: u32,
    enable: Option<(f64, f64)>,
) -> String {
    let margin = (frame_h as f64 * 0.04).round();
    let y = match style.pos {
        SubPos::Top => format!("{margin:.0}"),
        SubPos::Middle => "(h-text_h)/2".into(),
        SubPos::Bottom => format!("h-text_h-{margin:.0}"),
    };
    let mut f = format!(
        "drawtext=fontfile='{}':textfile='{}':fontsize={:.0}:fontcolor={}:borderw={}:bordercolor={}:x=(w-text_w)/2:y={}",
        ff_path_escape(fontfile),
        ff_path_escape(textfile),
        fontsize.max(1.0),
        ff_color(style.color),
        style.outline_w,
        ff_color(style.outline_color),
        y,
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
/// 預覽結果附帶「這是第幾張照片的渲染」，避免快速切換時舊結果蓋掉新照片
type PreviewMsg = (usize, PreviewResult);
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
    sub_entries: Vec<SubtitleEntry>,
    sub_style: SubtitleStyle,
    fonts: Vec<(String, PathBuf)>,
    preview_selected: Option<usize>,
    preview_dirty: bool,
    preview_rx: Option<Receiver<PreviewMsg>>,
    preview_tex: Option<egui::TextureHandle>,
    preview_error: Option<String>,
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
    about_open: bool,
    /// fps 最後一次變動的時間；拖動時不即時寫設定檔，停止變動後才寫
    fps_pending_save: Option<Instant>,
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
                let res = image::open(&job).ok().map(|img| {
                    // RGB 即可（縮圖不需要透明通道），省 1/4 記憶體與上傳頻寬
                    let t = img.thumbnail(320, 180).to_rgb8();
                    (t.width(), t.height(), t.into_raw())
                });
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
            sub_entries: Vec::new(),
            sub_style: SubtitleStyle::default(),
            fonts: detect_fonts(),
            preview_selected: None,
            preview_dirty: false,
            preview_rx: None,
            preview_tex: None,
            preview_error: None,
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
            about_open: false,
            fps_pending_save: None,
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
            app.add_photos(initial_files);
        }
        app
    }

    fn mark_preview_dirty(&mut self) {
        self.preview_dirty = true;
        self.preview_error = None;
    }

    /// 依選取的照片與目前調色參數，在背景執行 ffmpeg 產生預覽圖
    fn spawn_preview(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.preview_selected else { return };
        let Some(photo) = self.photos.get(idx).cloned() else { return };
        let adj = self.adj;
        let res = self.resolution;
        // 收集涵蓋這張照片的所有字幕段落（與輸出行為一致）
        let idx1 = idx + 1;
        let captions: Vec<String> = self
            .sub_entries
            .iter()
            .filter(|e| e.start <= idx1 && idx1 <= e.end && !e.text.trim().is_empty())
            .map(|e| e.text.clone())
            .collect();
        let style = self.sub_style.clone();
        let font = self.fonts.get(style.font_idx).map(|(_, p)| p.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        self.preview_rx = Some(rx);
        self.preview_dirty = false;
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send((idx, render_preview(&photo, &adj, res, &captions, &style, font.as_deref())));
            ctx.request_repaint();
        });
    }

    fn poll_preview(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.preview_rx {
            if let Ok((for_idx, res)) = rx.try_recv() {
                self.preview_rx = None;
                // 渲染期間使用者已切到別張照片 → 丟棄過期結果（dirty 仍在，會重新渲染）
                if self.preview_selected == Some(for_idx) {
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
        }
        // 連續回饋：參數有變且沒有渲染在跑就立刻渲染。
        // 同時間最多一個渲染、完成後才會再啟動下一個（以最新參數），
        // 更新頻率被渲染時間自然限流；拖動滑桿的過程即時看到調色變化
        if self.preview_dirty && self.preview_rx.is_none() {
            self.spawn_preview(ctx);
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
        // 用 HashSet 去重；逐一 contains 是 O(n²)，加入數千張照片要數百萬次路徑比對
        let mut seen: HashSet<PathBuf> = self.photos.iter().cloned().collect();
        for f in files {
            if seen.insert(f.clone()) {
                self.photos.push(f);
            }
        }
        natural_sort(&mut self.photos);
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
        // 請求可視範圍＋預取邊界內還沒有縮圖的
        let lo = first.saturating_sub(PREFETCH);
        let hi = (last + PREFETCH).min(n);
        let need: Vec<PathBuf> = self.photos[lo..hi]
            .iter()
            .filter(|p| !self.thumbs.contains_key(*p))
            .cloned()
            .collect();
        if !need.is_empty() {
            for p in &need {
                self.thumbs.insert(p.clone(), Thumb::Loading);
            }
            let (lock, cv) = &*self.thumb_jobs;
            lock.lock().unwrap().extend(need);
            cv.notify_all();
        }

        // 淘汰：貼圖數量明顯超過保留窗時才做一次 O(n) 掃描
        let keep_lo = first.saturating_sub(KEEP);
        let keep_hi = (last + KEEP).min(n);
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
        // 清掉還在排隊的解碼工作，工作池不再為已移除的照片做白工
        self.thumb_jobs.0.lock().unwrap().clear();
        self.preview_selected = None;
        self.preview_tex = None;
        self.preview_error = None;
    }

    fn remove_photo(&mut self, i: usize) {
        self.photos.remove(i);
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
            self.add_photos(files);
        }
    }

    fn pick_files(&mut self) {
        if let Some(files) = rfd::FileDialog::new()
            .set_title("選擇照片")
            .add_filter("圖片檔", IMAGE_EXTS)
            .pick_files()
        {
            self.add_photos(files);
        }
    }

    fn start_convert(&mut self, ctx: &egui::Context) {
        let ext = self.format.ext();
        let Some(output) = rfd::FileDialog::new()
            .set_title("選擇影片儲存位置")
            .add_filter(format!("{} 影片", ext.to_uppercase()), &[ext])
            .set_file_name(format!("output.{ext}"))
            .save_file()
        else {
            return;
        };

        let (tx, rx) = std::sync::mpsc::channel();
        self.rx = Some(rx);
        self.state = ConvertState::Working {
            progress: 0.0,
            status: "準備中…".into(),
        };

        let photos = self.photos.clone();
        let fps = self.fps;
        let format = self.format;
        let res = self.resolution;
        let adj = self.adj;
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
                    (s < end).then(|| (s, end - 1, e.text.clone()))
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
            match run_conversion(&photos, fps, format, res, &adj, &subs, &fx, &output, &send) {
                Ok(()) => send(WorkerMsg::Done(output.clone())),
                Err(e) => send(WorkerMsg::Error(e)),
            }
        });
    }

    /// 在背景執行緒檢查 GitHub 是否有新版本
    fn spawn_update_check(&mut self, ctx: &egui::Context) {
        if matches!(
            self.update_status,
            UpdateStatus::Checking | UpdateStatus::Downloading(_)
        ) {
            return;
        }
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
        let Some(rx) = &self.update_rx else { return };
        let mut finished = false;
        let mut restart = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                UpdateMsg::CheckResult(res) => {
                    finished = true;
                    self.update_status = match res {
                        Ok(Some(tag)) => UpdateStatus::Available(tag),
                        Ok(None) => UpdateStatus::UpToDate,
                        Err(e) => UpdateStatus::Failed(e),
                    };
                }
                UpdateMsg::Progress(p) => self.update_status = UpdateStatus::Downloading(p),
                UpdateMsg::Ready => {
                    finished = true;
                    restart = true;
                }
                UpdateMsg::Failed(e) => {
                    finished = true;
                    self.update_status = UpdateStatus::Failed(e);
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
                restart_app();
            }
        }
    }

    fn poll_worker(&mut self) {
        let Some(rx) = &self.rx else { return };
        let mut done = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                WorkerMsg::Status(s) => {
                    if let ConvertState::Working { status, .. } = &mut self.state {
                        *status = s;
                    }
                }
                WorkerMsg::Progress(p) => {
                    if let ConvertState::Working { progress, .. } = &mut self.state {
                        *progress = p;
                    }
                }
                WorkerMsg::Done(path) => {
                    self.state = ConvertState::Done(path);
                    done = true;
                }
                WorkerMsg::Error(e) => {
                    self.state = ConvertState::Error(e);
                    done = true;
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
                            restart_app();
                        }
                    });
                    ui.add_space(8.0);
                }

                // 狀態列
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
                            if ui.small_button("開啟資料夾").clicked() {
                                if let Some(dir) = path.parent() {
                                    let _ =
                                        std::process::Command::new("explorer").arg(dir).spawn();
                                }
                            }
                        });
                        ui.add_space(8.0);
                    }
                    ConvertState::Error(e) => {
                        let e = e.clone();
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!("✖ 轉換失敗：{e}"))
                                    .color(theme::ERROR),
                            )
                            .truncate(),
                        );
                        ui.add_space(8.0);
                    }
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
                        self.ui_subtitle_section(ui);
                        ui.add_space(14.0);
                        ui.separator();
                        ui.add_space(10.0);
                        self.ui_fx_section(ui);
                        ui.add_space(8.0);
                    });
            });
    }

    fn ui_adjust_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            section_toggle(ui, "調色", &mut self.sec_adjust_open);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !self.adj.is_neutral() && ui.small_button("↺ 重設").clicked() {
                    self.adj = Adjustments::default();
                    self.mark_preview_dirty();
                }
            });
        });
        if !self.sec_adjust_open {
            return;
        }
        let before = self.adj;

        ui.label(
            egui::RichText::new("套用到影片中的每一張照片，滑桿連點兩下可歸零")
                .size(11.0)
                .color(theme::TEXT_WEAK),
        );
        ui.add_space(10.0);

        group_label(ui, "白平衡");
        adj_slider(ui, &mut self.adj.temp, "色溫");
        adj_slider(ui, &mut self.adj.tint, "色調");
        ui.add_space(10.0);

        group_label(ui, "光線");
        adj_slider(ui, &mut self.adj.exposure, "曝光度");
        adj_slider(ui, &mut self.adj.contrast, "對比");
        adj_slider(ui, &mut self.adj.brightness, "亮度");
        adj_slider(ui, &mut self.adj.shadows, "陰影");
        adj_slider(ui, &mut self.adj.whites, "白色");
        adj_slider(ui, &mut self.adj.blacks, "黑色");
        ui.add_space(10.0);

        group_label(ui, "質感與色彩");
        adj_slider(ui, &mut self.adj.clarity, "清晰度");
        adj_slider(ui, &mut self.adj.vibrance, "鮮豔度");
        adj_slider(ui, &mut self.adj.saturation, "飽和度");

        if self.adj != before {
            self.mark_preview_dirty();
        }
    }

    fn ui_subtitle_section(&mut self, ui: &mut egui::Ui) {
        let style_before = self.sub_style.clone();
        let total = self.photos.len().max(1);
        let mut entries_changed = false;

        ui.horizontal(|ui| {
            section_toggle(ui, "字幕", &mut self.sec_sub_open);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("＋ 新增段落").clicked() {
                    let start = self.preview_selected.map(|i| i + 1).unwrap_or(1);
                    self.sub_entries.push(SubtitleEntry {
                        start,
                        end: total,
                        text: String::new(),
                    });
                    entries_changed = true;
                    self.sec_sub_open = true; // 收合時新增段落 → 自動展開
                }
            });
        });
        if !self.sec_sub_open {
            if entries_changed {
                self.mark_preview_dirty();
            }
            return;
        }
        ui.label(
            egui::RichText::new("一段連續的照片共用同一句字幕；樣式為全部共用")
                .size(11.0)
                .color(theme::TEXT_WEAK),
        );
        ui.add_space(8.0);

        let mut remove_entry: Option<usize> = None;
        for (k, entry) in self.sub_entries.iter_mut().enumerate() {
            egui::Frame::default()
                .fill(theme::CARD)
                .corner_radius(8)
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("段落 {}", k + 1))
                                .strong()
                                .size(12.5)
                                .color(theme::ACCENT),
                        );
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new("第").size(12.0).color(theme::TEXT_WEAK));
                        let r1 =
                            ui.add(egui::DragValue::new(&mut entry.start).range(1..=total));
                        ui.label(egui::RichText::new("到").size(12.0).color(theme::TEXT_WEAK));
                        let r2 = ui.add(egui::DragValue::new(&mut entry.end).range(1..=total));
                        ui.label(egui::RichText::new("張").size(12.0).color(theme::TEXT_WEAK));
                        if r1.changed() || r2.changed() {
                            if entry.end < entry.start {
                                entry.end = entry.start;
                            }
                            entries_changed = true;
                        }
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.small_button("🗑").on_hover_text("刪除這個段落").clicked()
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
                            .hint_text("這段照片要顯示的字幕（可多行）"),
                    );
                    if resp.changed() {
                        entries_changed = true;
                    }
                });
            ui.add_space(6.0);
        }
        if let Some(k) = remove_entry {
            self.sub_entries.remove(k);
            entries_changed = true;
        }
        if self.sub_entries.is_empty() {
            ui.label(
                egui::RichText::new("尚未加入字幕，點右上「＋ 新增段落」開始")
                    .size(11.5)
                    .color(theme::TEXT_WEAK),
            );
        }
        if entries_changed {
            self.mark_preview_dirty();
        }
        ui.add_space(10.0);

        if self.fonts.is_empty() {
            ui.colored_label(theme::ERROR, "找不到可用的系統字型，字幕功能無法使用");
        } else {
            group_label(ui, "字幕樣式");
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
                    slider_row(ui, &mut self.sub_style.size, 12, 200, "大小");
                    slider_row(ui, &mut self.sub_style.outline_w, 0, 8, "外框");
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("文字").color(theme::TEXT_WEAK));
                        ui.color_edit_button_srgba(&mut self.sub_style.color);
                        ui.add_space(10.0);
                        ui.label(egui::RichText::new("外框").color(theme::TEXT_WEAK));
                        ui.color_edit_button_srgba(&mut self.sub_style.outline_color);
                        ui.add_space(10.0);
                        ui.label(egui::RichText::new("位置").color(theme::TEXT_WEAK));
                        egui::ComboBox::from_id_salt("sub_pos")
                            .selected_text(self.sub_style.pos.label())
                            .width(70.0)
                            .show_ui(ui, |ui| {
                                for p in SubPos::ALL {
                                    ui.selectable_value(&mut self.sub_style.pos, p, p.label());
                                }
                            });
                    });
                    ui.checkbox(&mut self.sub_style.boxed, "半透明底框");
                });
        }

        if self.sub_style != style_before {
            self.mark_preview_dirty();
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
                        ui.checkbox(&mut self.music_fade, "結尾自動淡出（2 秒）");
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
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let secs = self.photos.len() as f32 / self.fps as f32;
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
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), preview_h),
            egui::Sense::hover(),
        );
        let p = ui.painter().clone();
        p.rect_filled(rect, 10, theme::PREVIEW_BG);
        p.rect_stroke(
            rect,
            10,
            egui::Stroke::new(1.0, theme::BORDER),
            egui::StrokeKind::Inside,
        );

        // 渲染完成前先用縮圖放大當佔位，切換照片時畫面不留白
        let placeholder = match (self.preview_tex.is_none(), self.preview_selected) {
            (true, Some(i)) => self.photos.get(i).and_then(|p| match self.thumbs.get(p) {
                Some(Thumb::Ready(t)) => Some(t.clone()),
                _ => None,
            }),
            _ => None,
        };
        if let Some(tex) = self.preview_tex.as_ref().or(placeholder.as_ref()) {
            let img_rect = fit_rect(tex.size_vec2(), rect.shrink(14.0));
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
                "點選下方縮圖即可預覽調色與字幕效果",
                egui::FontId::proportional(13.0),
                theme::TEXT_WEAK,
            );
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
                    let tex = match self.thumbs.get(photo) {
                        Some(Thumb::Ready(t)) => Some(t.clone()),
                        _ => None,
                    };
                    let selected = self.preview_selected == Some(i);
                    let has_caption = self.sub_entries.iter().any(|e| {
                        e.start <= i + 1 && i + 1 <= e.end && !e.text.trim().is_empty()
                    });
                    let resp = thumb_item(ui, tex.as_ref(), i, selected, has_caption);
                    if resp.clicked() {
                        click_idx = Some(i);
                    }
                    resp.context_menu(|ui| {
                        if ui.button("移除這張照片").clicked() {
                            remove_idx = Some(i);
                            ui.close_menu();
                        }
                        if ui.button("清空全部").clicked() {
                            clear_all = true;
                            ui.close_menu();
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
            self.select_photo(Some(i));
        }
        if !working {
            if clear_all {
                self.clear_photos();
            } else if let Some(i) = remove_idx {
                self.remove_photo(i);
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
                        UpdateStatus::Checking | UpdateStatus::Downloading(_)
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
            restart_app();
        }
    }

    /// 拖曳檔案進視窗時的全螢幕提示
    fn ui_drop_overlay(&self, ctx: &egui::Context) {
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if !hovering || self.is_working() {
            return;
        }
        let screen = ctx.screen_rect();
        let p = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("drop_overlay"),
        ));
        p.rect_filled(screen, 0, egui::Color32::from_black_alpha(150));
        let card = egui::Rect::from_center_size(screen.center(), egui::vec2(340.0, 116.0));
        p.rect_filled(card, 12, theme::CARD);
        p.rect_stroke(
            card,
            12,
            egui::Stroke::new(1.5, theme::ACCENT),
            egui::StrokeKind::Inside,
        );
        p.text(
            card.center() - egui::vec2(0.0, 16.0),
            egui::Align2::CENTER_CENTER,
            "⬇",
            egui::FontId::proportional(26.0),
            theme::ACCENT,
        );
        p.text(
            card.center() + egui::vec2(0.0, 20.0),
            egui::Align2::CENTER_CENTER,
            "放開滑鼠加入照片",
            egui::FontId::proportional(15.0),
            theme::TEXT,
        );
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // 關閉程式時若還有未寫入的 fps 設定，補寫一次
        if self.fps_pending_save.is_some() {
            save_fps(self.fps);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
            let mut files = Vec::new();
            for p in dropped {
                if p.is_dir() {
                    files.extend(collect_images_in_dir(&p));
                } else if is_audio(&p) {
                    // 拖入音訊檔＝設定為背景音樂
                    self.music_path = Some(p);
                } else {
                    files.push(p);
                }
            }
            self.add_photos(files);
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

/// 帶色條的區段標題
fn section_header(ui: &mut egui::Ui, title: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(3.0, 15.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 2, theme::ACCENT);
        ui.label(egui::RichText::new(title).strong().size(14.5).color(theme::TEXT));
    });
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

/// 調色滑桿：左標籤、右數值，連點兩下歸零
fn adj_slider(ui: &mut egui::Ui, value: &mut i32, label: &str) {
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
        let resp = ui.add(egui::Slider::new(value, -100..=100).show_value(false));
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
        ui.add(egui::Slider::new(value, min..=max).show_value(false));
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

/// 膠卷縮圖項目
fn thumb_item(
    ui: &mut egui::Ui,
    tex: Option<&egui::TextureHandle>,
    idx: usize,
    selected: bool,
    has_caption: bool,
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

    let stroke = if selected {
        egui::Stroke::new(2.0, theme::ACCENT)
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
    p.to_string_lossy().replace('\\', "/").replace('\'', r"'\''")
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

fn ensure_ffmpeg(on_download: impl Fn()) -> Result<(), String> {
    if FFMPEG_READY.load(Ordering::Relaxed) {
        return Ok(());
    }
    if !ffmpeg_sidecar::command::ffmpeg_is_installed() {
        on_download();
        ffmpeg_sidecar::download::auto_download().map_err(|e| format!("FFmpeg 下載失敗：{e}"))?;
    }
    FFMPEG_READY.store(true, Ordering::Relaxed);
    Ok(())
}

/// 內容相同就不重寫，避免每次預覽渲染都做磁碟寫入
/// （拖動調色滑桿時字幕內容通常沒變）
fn write_if_changed(path: &Path, content: &str) -> std::io::Result<()> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        if existing == content {
            return Ok(());
        }
    }
    std::fs::write(path, content)
}

/// 預覽底圖快取：拖動調色滑桿時會對同一張照片反覆重渲染，
/// 把「縮放後的底圖」存成暫存 BMP 後改以小圖當輸入，
/// 免去每次重新解碼原始大圖（大照片可省下大半渲染時間）
struct PreviewBase {
    /// (照片路徑, 檔案修改時間, 預覽高度)
    key: (PathBuf, Option<SystemTime>, u32),
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

/// 這次預覽渲染與底圖快取的關係
enum BaseRole {
    /// 命中：直接以底圖當輸入
    Cached(PathBuf),
    /// 未命中：本次渲染用 split 順便輸出底圖（原圖只解碼一次）
    Build { out: PathBuf, serial: u64 },
    /// 同一張照片先前建置失敗：維持完整流程、不再嘗試建置
    Skip,
}

/// 用 ffmpeg 渲染單張照片的預覽（含調色與字幕，縮小並依輸出比例補邊），回傳 RGB 像素
fn render_preview(
    photo: &Path,
    adj: &Adjustments,
    res: Resolution,
    captions: &[String],
    style: &SubtitleStyle,
    font: Option<&Path>,
) -> PreviewResult {
    ensure_ffmpeg(|| {})?;

    // 預覽畫布：寬 960、依輸出解析度等比例的高（取偶數）
    let pw: u32 = 960;
    let ph: u32 = ((pw as f64 * res.h as f64 / res.w as f64 / 2.0).round() as u32) * 2;

    // 查底圖快取：命中就以縮小後的底圖當輸入；未命中（第一次看這張照片）
    // 就在本次渲染順便輸出底圖，原圖只解碼一次
    let key = (
        photo.to_path_buf(),
        std::fs::metadata(photo).and_then(|m| m.modified()).ok(),
        ph,
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
            // 未命中：淘汰最舊的一筆後建立新槽
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
            let out = std::env::temp_dir().join(format!("photo2video_prev_base_{serial}.bmp"));
            BaseRole::Build { out, serial }
        }
    };

    // 與輸出相同順序：先縮放、再調色、後補邊，最後上字幕。
    // chain 為縮放之後的濾鏡（調色、補邊、字幕）
    let adjust_mid = adj
        .filter_chain()
        .map(|c| format!("{c},"))
        .unwrap_or_default();
    let mut chain = format!("{adjust_mid}pad={pw}:{ph}:(ow-iw)/2:(oh-ih)/2:color=black");
    if let Some(font) = font {
        let fontsize = style.size as f64 * ph as f64 / 1080.0;
        for (k, text) in captions.iter().enumerate() {
            let cap_path = std::env::temp_dir().join(format!("photo2video_cap_preview_{k}.txt"));
            write_if_changed(&cap_path, text.trim_end())
                .map_err(|e| format!("無法寫入字幕暫存檔：{e}"))?;
            chain.push(',');
            chain.push_str(&drawtext_filter(font, &cap_path, style, fontsize, ph, None));
        }
    }
    let scale = format!("scale={pw}:{ph}:force_original_aspect_ratio=decrease");

    // 直接以 rawvideo 從 stdout 取回 RGB 像素，省去圖檔編碼、寫檔與再解碼
    let mut cmd = FfmpegCommand::new();
    match &base {
        // 底圖已是縮放後的尺寸，直接跑後段濾鏡
        BaseRole::Cached(f) => {
            cmd.input(f.to_string_lossy())
                .args(["-vf", &chain])
                .args(["-frames:v", "1"])
                .rawvideo();
        }
        // split 一路出預覽、一路存底圖，下次重渲染就有快取可用
        BaseRole::Build { out, .. } => {
            let fc = format!("[0:v]{scale},split[pv][bs];[pv]{chain}[out]");
            cmd.arg("-y")
                .input(photo.to_string_lossy())
                .args(["-filter_complex", &fc])
                .args(["-map", "[out]", "-frames:v", "1"])
                .rawvideo()
                .args(["-map", "[bs]", "-frames:v", "1"])
                .output(out.to_string_lossy());
        }
        BaseRole::Skip => {
            cmd.input(photo.to_string_lossy())
                .args(["-vf", &format!("{scale},{chain}")])
                .args(["-frames:v", "1"])
                .rawvideo();
        }
    }

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
            FfmpegEvent::Log(level, msg)
                if matches!(level, LogLevel::Error | LogLevel::Fatal) =>
            {
                error_log.push(msg);
            }
            _ => {}
        }
    }
    let status = child.wait().map_err(|e| format!("FFmpeg 執行失敗：{e}"))?;
    let ok = status.success() && frame.is_some();

    // 底圖登記：渲染成功且快取槽仍在（未被 LRU 淘汰）才掛上，否則清掉孤兒檔
    if let BaseRole::Build { out, serial } = &base {
        let mut stored = false;
        if ok {
            let mut g = PREVIEW_BASE.lock().unwrap();
            if let Some(b) = g
                .iter_mut()
                .find(|b| b.serial == *serial && b.key == key)
            {
                b.file = Some(out.clone());
                stored = true;
            }
        }
        if !stored {
            let _ = std::fs::remove_file(out);
        }
    }

    if !ok {
        return Err(if error_log.is_empty() {
            "預覽渲染失敗".into()
        } else {
            error_log.join("\n")
        });
    }
    Ok(frame.unwrap())
}

/// 一次轉換的字幕資料：全域樣式 + 各段落（0-based 含端點的照片區間與文字）
struct SubtitleJob {
    style: SubtitleStyle,
    font: Option<PathBuf>,
    entries: Vec<(usize, usize, String)>,
}

fn run_conversion(
    photos: &[PathBuf],
    fps: u32,
    format: OutputFormat,
    res: Resolution,
    adj: &Adjustments,
    subs: &SubtitleJob,
    fx: &OutputFx,
    output: &Path,
    send: &dyn Fn(WorkerMsg),
) -> Result<(), String> {
    if photos.is_empty() {
        return Err("沒有照片可轉換".into());
    }

    // 第一次執行時自動下載 ffmpeg
    ensure_ffmpeg(|| {
        send(WorkerMsg::Status(
            "第一次使用，正在下載 FFmpeg（約 80MB，請稍候）…".into(),
        ));
    })?;

    send(WorkerMsg::Status("建立照片清單…".into()));

    let animated = fx.ken_burns || fx.transition != Transition::None;

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

    // 用 concat demuxer 列出每張照片與顯示時間
    let mut list = String::new();
    for p in photos {
        list.push_str(&format!("file '{}'\nduration {duration}\n", concat_escape(p)));
    }
    // concat demuxer 的慣例：最後一張要再列一次，最後一段 duration 才會生效
    if let Some(last) = photos.last() {
        list.push_str(&format!("file '{}'\n", concat_escape(last)));
    }

    let list_path = std::env::temp_dir().join("photo2video_list.txt");
    std::fs::write(&list_path, &list).map_err(|e| format!("無法寫入暫存清單：{e}"))?;

    let (w, h) = (res.w, res.h);
    // 先縮放到目標解析度再調色（大照片可省下數倍運算），最後補邊，
    // 黑邊仍不受亮度、曝光等調整影響
    let adjust_mid = adj
        .filter_chain()
        .map(|c| format!("{c},"))
        .unwrap_or_default();
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

    // 字幕：每個段落產生一段 drawtext，用時間區間涵蓋該段照片
    let mut caption_files: Vec<PathBuf> = Vec::new();
    if let Some(font) = &subs.font {
        let fontsize = subs.style.size as f64 * h as f64 / 1080.0;
        let d = eff_dur;
        for (k, (s, e, text)) in subs.entries.iter().enumerate() {
            let text = text.trim_end();
            if text.is_empty() {
                continue;
            }
            let cap_path = std::env::temp_dir().join(format!("photo2video_cap_{k}.txt"));
            std::fs::write(&cap_path, text).map_err(|e| format!("無法寫入字幕暫存檔：{e}"))?;
            // 以半格時間為緩衝，準確涵蓋第 s ~ e 張照片的所有格
            let enable = (*s as f64 * d - d * 0.25, (*e as f64 + 1.0) * d - d * 0.25);
            vf.push(',');
            vf.push_str(&drawtext_filter(
                font,
                &cap_path,
                &subs.style,
                fontsize,
                h,
                Some(enable),
            ));
            caption_files.push(cap_path);
        }
    }

    // 轉場（淡入淡出）：eq 以每格評估的週期函數在每個切點附近把亮度與飽和度壓到黑，
    // 首尾也各有一次淡入/淡出；不用串接大量 fade 濾鏡（fade 的 st 前後會整段變黑）
    if fx.transition == Transition::FadeBlack {
        let f = (eff_dur * 0.4).clamp(0.05, 0.5);
        let dip = format!(
            "max(0,1-min(mod(t,{d:.6}),{d:.6}-mod(t,{d:.6}))/{f:.6})",
            d = eff_dur,
            f = f
        );
        vf.push_str(&format!(
            ",eq=eval=frame:brightness='-{dip}':saturation='max(0,1-{dip})'"
        ));
    }
    vf.push_str(",setsar=1,format=yuv420p");

    let out_fps = if animated { OUT_FPS } else { fps };
    let mut cmd = FfmpegCommand::new();
    cmd.arg("-y")
        .args(["-filter_threads", &filter_threads()])
        .args(["-f", "concat", "-safe", "0"])
        .input(list_path.to_string_lossy());
    // 背景音樂：無限循環讀取，總長由 -t 截止（音樂短會循環、長會被裁切）
    if let Some(m) = &fx.music {
        cmd.args(["-stream_loop", "-1"]);
        cmd.input(m.path.to_string_lossy());
    }
    cmd.args(["-vf", &vf])
        .args(["-r", &out_fps.to_string()])
        // 精確限制總長度，避免 concat 清單重複最後一張造成多出一格
        .args(["-t", &format!("{total_secs}")]);

    if let Some(m) = &fx.music {
        cmd.args(["-map", "0:v", "-map", "1:a"]);
        let mut af = format!("volume={:.3}", m.volume.max(0) as f64 / 100.0);
        if m.fade_out && total_secs > 3.0 {
            af.push_str(&format!(",afade=t=out:st={:.3}:d=2", total_secs - 2.0));
        }
        cmd.args(["-af", &af]);
        if matches!(format, OutputFormat::Webm) {
            cmd.args(["-c:a", "libopus", "-b:a", "128k"]);
        } else {
            cmd.args(["-c:a", "aac", "-b:a", "192k"]);
        }
    }

    match format {
        OutputFormat::Webm => {
            send(WorkerMsg::Status("轉換中…（編碼器：VP9）".into()));
            cmd.args([
                "-c:v", "libvpx-vp9", "-b:v", "0", "-crf", "30", "-cpu-used", "5", "-row-mt",
                "1",
            ]);
        }
        _ => {
            send(WorkerMsg::Status("偵測硬體編碼器…".into()));
            let enc = detect_h264_encoder();
            send(WorkerMsg::Status(format!(
                "轉換中…（編碼器：{}）",
                enc.display_name()
            )));
            cmd.args(enc.codec_args());
        }
    }

    cmd.output(output.to_string_lossy());

    let total_frames = if animated {
        (total_secs * OUT_FPS as f64) as f32
    } else {
        photos.len() as f32
    };
    let mut error_log: Vec<String> = Vec::new();

    let mut child = cmd.spawn().map_err(|e| format!("FFmpeg 啟動失敗：{e}"))?;
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

    let status = child.wait().map_err(|e| format!("FFmpeg 執行失敗：{e}"))?;
    let _ = std::fs::remove_file(&list_path);
    for f in &caption_files {
        let _ = std::fs::remove_file(f);
    }

    if !status.success() {
        let detail = error_log.join("\n");
        return Err(if detail.is_empty() {
            "FFmpeg 轉換失敗".into()
        } else {
            format!("FFmpeg 轉換失敗：{detail}")
        });
    }

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
    let output = PathBuf::from(&args[2]);

    let mut photos = collect_images_in_dir(&dir);
    natural_sort(&mut photos);
    if photos.is_empty() {
        return Err(format!("資料夾內沒有圖片：{}", dir.display()));
    }
    println!("共 {} 張照片，fps={fps}", photos.len());

    let format = match output.extension().and_then(|e| e.to_str()) {
        Some("mkv") => OutputFormat::Mkv,
        Some("mov") => OutputFormat::Mov,
        Some("avi") => OutputFormat::Avi,
        Some("webm") => OutputFormat::Webm,
        _ => OutputFormat::Mp4,
    };

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
        &no_subs,
        &no_fx,
        &output,
        &send,
    )?;
    println!("完成：{}", output.display());
    Ok(())
}

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
    // 清掉上次一鍵更新留下的舊版檔案
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("exe.old"));
    }

    // 提早在背景讀取中文字型檔（20MB+），與視窗建立同時進行，縮短啟動時間
    let font_loader = thread::spawn(load_cjk_font_bytes);

    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "--cli" {
        if let Err(e) = run_cli(&args[2..]) {
            eprintln!("錯誤：{e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // 其餘參數視為要開啟的照片或資料夾（支援「開啟方式」與拖曳到執行檔上）
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
