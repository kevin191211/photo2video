#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::thread;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use eframe::egui;
use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::FfmpegEvent;

const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "bmp", "webp", "tif", "tiff"];

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
        format!("{} x {}", self.w, self.h)
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

type PreviewResult = Result<(u32, u32, Vec<u8>), String>;

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
    preview_last_change: Option<Instant>,
    preview_rx: Option<Receiver<PreviewResult>>,
    preview_tex: Option<egui::TextureHandle>,
    preview_error: Option<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_chinese_fonts(&cc.egui_ctx);
        Self {
            photos: Vec::new(),
            fps: 2,
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
            preview_last_change: None,
            preview_rx: None,
            preview_tex: None,
            preview_error: None,
        }
    }

    fn mark_preview_dirty(&mut self) {
        self.preview_dirty = true;
        self.preview_last_change = Some(Instant::now());
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
            let _ = tx.send(render_preview(&photo, &adj, res, &captions, &style, font.as_deref()));
            ctx.request_repaint();
        });
    }

    fn poll_preview(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.preview_rx {
            if let Ok(res) = rx.try_recv() {
                self.preview_rx = None;
                match res {
                    Ok((w, h, rgba)) => {
                        let img = egui::ColorImage::from_rgba_unmultiplied(
                            [w as usize, h as usize],
                            &rgba,
                        );
                        self.preview_tex =
                            Some(ctx.load_texture("preview", img, Default::default()));
                    }
                    Err(e) => self.preview_error = Some(e),
                }
            }
        }
        // 防抖：滑桿停止拖動 300ms 後才重新渲染
        if self.preview_dirty && self.preview_rx.is_none() {
            if let Some(t) = self.preview_last_change {
                let elapsed = t.elapsed();
                if elapsed >= Duration::from_millis(300) {
                    self.spawn_preview(ctx);
                } else {
                    ctx.request_repaint_after(Duration::from_millis(320) - elapsed);
                }
            }
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
        for f in files {
            if !self.photos.contains(&f) {
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
        let ctx = ctx.clone();

        thread::spawn(move || {
            let send = |msg: WorkerMsg| {
                let _ = tx.send(msg);
                ctx.request_repaint();
            };
            match run_conversion(&photos, fps, format, res, &adj, &subs, &output, &send) {
                Ok(()) => send(WorkerMsg::Done(output.clone())),
                Err(e) => send(WorkerMsg::Error(e)),
            }
        });
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
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_worker();
        self.poll_preview(ctx);

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
                } else {
                    files.push(p);
                }
            }
            self.add_photos(files);
        }

        egui::SidePanel::right("adjust_panel")
            .default_width(380.0)
            .min_width(320.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.heading("調色（套用至整部影片）");
                    });
                    ui.label(
                        egui::RichText::new("下方參數會套用到影片中的每一張照片")
                            .weak()
                            .small(),
                    );
                    ui.add_space(6.0);

                    // 預覽區
                    if let Some(tex) = &self.preview_tex {
                        let max_w = ui.available_width();
                        ui.add(
                            egui::Image::new((tex.id(), tex.size_vec2()))
                                .max_size(egui::vec2(max_w, 240.0)),
                        );
                    } else if self.preview_selected.is_some() {
                        ui.label("（預覽產生中…）");
                    } else {
                        ui.label(
                            egui::RichText::new("加入照片後，點選左側清單中的檔名即可預覽")
                                .weak(),
                        );
                    }
                    if self.preview_rx.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("更新預覽中…");
                        });
                    }
                    if let Some(e) = &self.preview_error {
                        ui.colored_label(egui::Color32::RED, format!("預覽失敗：{e}"));
                    }
                    ui.add_space(6.0);
                    ui.separator();

                    let before = self.adj;

                    ui.label(egui::RichText::new("白平衡").strong());
                    adj_slider(ui, &mut self.adj.temp, "色溫");
                    adj_slider(ui, &mut self.adj.tint, "色調");
                    ui.add_space(6.0);

                    ui.label(egui::RichText::new("光線").strong());
                    adj_slider(ui, &mut self.adj.exposure, "曝光度");
                    adj_slider(ui, &mut self.adj.contrast, "對比");
                    adj_slider(ui, &mut self.adj.brightness, "亮度");
                    adj_slider(ui, &mut self.adj.shadows, "陰影");
                    adj_slider(ui, &mut self.adj.whites, "白色");
                    adj_slider(ui, &mut self.adj.blacks, "黑色");
                    ui.add_space(6.0);

                    ui.label(egui::RichText::new("質感與色彩").strong());
                    adj_slider(ui, &mut self.adj.clarity, "清晰度");
                    adj_slider(ui, &mut self.adj.vibrance, "鮮豔度");
                    adj_slider(ui, &mut self.adj.saturation, "飽和度");
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("提示：滑桿連點兩下可回到 0")
                            .weak()
                            .small(),
                    );
                    ui.add_space(8.0);

                    if ui.button("↺ 全部重設").clicked() {
                        self.adj = Adjustments::default();
                    }

                    if self.adj != before {
                        self.mark_preview_dirty();
                    }

                    ui.add_space(8.0);
                    ui.separator();
                    ui.heading("字幕");
                    ui.label(
                        egui::RichText::new(
                            "以「段落」設定：一段連續的照片共用同一句字幕；樣式為全部共用",
                        )
                        .weak()
                        .small(),
                    );
                    ui.add_space(4.0);

                    let style_before = self.sub_style.clone();

                    // 字幕段落編輯器
                    let total = self.photos.len().max(1);
                    let mut remove_entry: Option<usize> = None;
                    let mut entries_changed = false;
                    for (k, entry) in self.sub_entries.iter_mut().enumerate() {
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(format!("段落 {}：從第", k + 1));
                                let r1 = ui.add(
                                    egui::DragValue::new(&mut entry.start).range(1..=total),
                                );
                                ui.label("張 到第");
                                let r2 = ui.add(
                                    egui::DragValue::new(&mut entry.end).range(1..=total),
                                );
                                ui.label("張");
                                if r1.changed() || r2.changed() {
                                    if entry.end < entry.start {
                                        entry.end = entry.start;
                                    }
                                    entries_changed = true;
                                }
                                if ui.small_button("🗑 刪除").clicked() {
                                    remove_entry = Some(k);
                                }
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
                        ui.add_space(4.0);
                    }
                    if let Some(k) = remove_entry {
                        self.sub_entries.remove(k);
                        entries_changed = true;
                    }
                    if ui.button("＋ 新增字幕段落").clicked() {
                        // 預設從目前選取的照片開始到最後一張
                        let start = self.preview_selected.map(|i| i + 1).unwrap_or(1);
                        self.sub_entries.push(SubtitleEntry {
                            start,
                            end: total,
                            text: String::new(),
                        });
                        entries_changed = true;
                    }
                    if entries_changed {
                        self.mark_preview_dirty();
                    }
                    ui.add_space(6.0);

                    if self.fonts.is_empty() {
                        ui.colored_label(
                            egui::Color32::RED,
                            "找不到可用的系統字型，字幕功能無法使用",
                        );
                    } else {
                        ui.horizontal(|ui| {
                            ui.label("字型：");
                            let cur = self
                                .fonts
                                .get(self.sub_style.font_idx)
                                .map(|(n, _)| n.as_str())
                                .unwrap_or("？");
                            egui::ComboBox::from_id_salt("sub_font")
                                .selected_text(cur)
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
                        ui.add(
                            egui::Slider::new(&mut self.sub_style.size, 12..=200)
                                .text("大小（1080p 基準）"),
                        );
                        ui.horizontal(|ui| {
                            ui.label("文字顏色：");
                            ui.color_edit_button_srgba(&mut self.sub_style.color);
                            ui.add_space(12.0);
                            ui.label("外框顏色：");
                            ui.color_edit_button_srgba(&mut self.sub_style.outline_color);
                        });
                        ui.add(
                            egui::Slider::new(&mut self.sub_style.outline_w, 0..=8)
                                .text("外框粗細"),
                        );
                        ui.horizontal(|ui| {
                            ui.label("位置：");
                            egui::ComboBox::from_id_salt("sub_pos")
                                .selected_text(self.sub_style.pos.label())
                                .show_ui(ui, |ui| {
                                    for p in SubPos::ALL {
                                        ui.selectable_value(
                                            &mut self.sub_style.pos,
                                            p,
                                            p.label(),
                                        );
                                    }
                                });
                            ui.add_space(12.0);
                            ui.checkbox(&mut self.sub_style.boxed, "半透明底框");
                        });
                    }

                    if self.sub_style != style_before {
                        self.mark_preview_dirty();
                    }
                    ui.add_space(8.0);
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("照片轉影片工具");
            ui.add_space(8.0);

            let working = self.is_working();

            ui.horizontal(|ui| {
                ui.add_enabled_ui(!working, |ui| {
                    if ui.button("📁 選擇資料夾").clicked() {
                        if let Some(dir) = rfd::FileDialog::new()
                            .set_title("選擇照片資料夾")
                            .pick_folder()
                        {
                            let files = collect_images_in_dir(&dir);
                            self.add_photos(files);
                        }
                    }
                    if ui.button("🖼 選擇照片（可多選）").clicked() {
                        if let Some(files) = rfd::FileDialog::new()
                            .set_title("選擇照片")
                            .add_filter("圖片檔", IMAGE_EXTS)
                            .pick_files()
                        {
                            self.add_photos(files);
                        }
                    }
                    if ui.button("🗑 清空清單").clicked() {
                        self.photos.clear();
                        self.preview_selected = None;
                        self.preview_tex = None;
                    }
                });
            });

            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("提示：也可以直接把照片或資料夾拖曳到這個視窗")
                    .weak()
                    .small(),
            );
            ui.add_space(8.0);

            ui.group(|ui| {
                ui.label(format!(
                    "已加入 {} 張照片（依檔名排序，點選檔名可在右側預覽調色效果）",
                    self.photos.len()
                ));
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let mut remove_idx: Option<usize> = None;
                        let mut click_idx: Option<usize> = None;
                        for (i, p) in self.photos.iter().enumerate() {
                            ui.horizontal(|ui| {
                                if ui.small_button("✕").clicked() {
                                    remove_idx = Some(i);
                                }
                                let selected = self.preview_selected == Some(i);
                                let has_caption = self.sub_entries.iter().any(|e| {
                                    e.start <= i + 1 && i + 1 <= e.end && !e.text.trim().is_empty()
                                });
                                let name = p.file_name().unwrap_or_default().to_string_lossy();
                                let label = if has_caption {
                                    format!("💬 {name}")
                                } else {
                                    name.into_owned()
                                };
                                if ui.selectable_label(selected, label).clicked() {
                                    click_idx = Some(i);
                                }
                            });
                        }
                        if let Some(i) = click_idx {
                            self.select_photo(Some(i));
                        }
                        if let Some(i) = remove_idx {
                            if !working {
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
                        }
                    });
            });

            ui.add_space(8.0);

            let res_before = self.resolution;
            ui.horizontal(|ui| {
                ui.label("每秒播放張數 (fps)：");
                ui.add_enabled(
                    !working,
                    egui::DragValue::new(&mut self.fps).range(1..=60).speed(0.1),
                );
                ui.add_space(16.0);
                ui.label("輸出格式：");
                ui.add_enabled_ui(!working, |ui| {
                    egui::ComboBox::from_id_salt("format")
                        .selected_text(self.format.label())
                        .show_ui(ui, |ui| {
                            for f in OutputFormat::ALL {
                                ui.selectable_value(&mut self.format, f, f.label());
                            }
                        });
                });
                ui.add_space(16.0);
                ui.label("解析度：");
                ui.add_enabled_ui(!working, |ui| {
                    egui::ComboBox::from_id_salt("resolution")
                        .selected_text(self.resolution.label())
                        .show_ui(ui, |ui| {
                            for r in Resolution::ALL {
                                ui.selectable_value(&mut self.resolution, r, r.label());
                            }
                        });
                });
            });

            if self.resolution != res_before {
                self.mark_preview_dirty();
            }

            if !self.photos.is_empty() {
                let secs = self.photos.len() as f32 / self.fps as f32;
                ui.label(egui::RichText::new(format!("預估影片長度：約 {secs:.1} 秒")).weak());
            }

            ui.add_space(12.0);

            let can_convert = !self.photos.is_empty() && !working;
            if ui
                .add_enabled(
                    can_convert,
                    egui::Button::new(egui::RichText::new("▶ 開始轉換").size(18.0))
                        .min_size(egui::vec2(160.0, 36.0)),
                )
                .clicked()
            {
                self.start_convert(ctx);
            }

            ui.add_space(8.0);

            match &self.state {
                ConvertState::Idle => {}
                ConvertState::Working { progress, status } => {
                    ui.label(status);
                    ui.add(egui::ProgressBar::new(*progress).show_percentage());
                }
                ConvertState::Done(path) => {
                    ui.colored_label(
                        egui::Color32::from_rgb(0, 160, 0),
                        format!("✔ 轉換完成：{}", path.display()),
                    );
                    if ui.button("開啟輸出資料夾").clicked() {
                        if let Some(dir) = path.parent() {
                            let _ = std::process::Command::new("explorer").arg(dir).spawn();
                        }
                    }
                }
                ConvertState::Error(e) => {
                    ui.colored_label(egui::Color32::RED, format!("✖ 轉換失敗：{e}"));
                }
            }
        });
    }
}

/// 調色滑桿：連點兩下重設回 0
fn adj_slider(ui: &mut egui::Ui, value: &mut i32, label: &str) {
    let resp = ui.add(egui::Slider::new(value, -100..=100).text(label));
    if resp.double_clicked() {
        *value = 0;
    }
}

fn is_image(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn collect_images_in_dir(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_file() && is_image(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// 自然排序：img2.jpg 會排在 img10.jpg 前面
fn natural_sort(paths: &mut [PathBuf]) {
    paths.sort_by(|a, b| {
        let ka = natural_key(&a.to_string_lossy().to_lowercase());
        let kb = natural_key(&b.to_string_lossy().to_lowercase());
        ka.cmp(&kb)
    });
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

fn setup_chinese_fonts(ctx: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\msjh.ttc",    // 微軟正黑體
        r"C:\Windows\Fonts\msjhbd.ttc",  // 微軟正黑體（粗體）
        r"C:\Windows\Fonts\mingliu.ttc", // 細明體
        r"C:\Windows\Fonts\msyh.ttc",    // 微軟雅黑（備援）
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
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
            return;
        }
    }
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
        let candidates = [
            ("h264_nvenc", H264Encoder::Nvenc),
            ("h264_qsv", H264Encoder::Qsv),
            ("h264_amf", H264Encoder::Amf),
        ];
        for (name, enc) in candidates {
            if test_encoder(name) {
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

fn ensure_ffmpeg(on_download: impl Fn()) -> Result<(), String> {
    if !ffmpeg_sidecar::command::ffmpeg_is_installed() {
        on_download();
        ffmpeg_sidecar::download::auto_download().map_err(|e| format!("FFmpeg 下載失敗：{e}"))?;
    }
    Ok(())
}

/// 用 ffmpeg 渲染單張照片的預覽（含調色與字幕，縮小並依輸出比例補邊），回傳 RGBA 像素
fn render_preview(
    photo: &Path,
    adj: &Adjustments,
    res: Resolution,
    captions: &[String],
    style: &SubtitleStyle,
    font: Option<&Path>,
) -> PreviewResult {
    ensure_ffmpeg(|| {})?;

    // 預覽畫布：寬 640、依輸出解析度等比例的高（取偶數）
    let pw: u32 = 640;
    let ph: u32 = ((pw as f64 * res.h as f64 / res.w as f64 / 2.0).round() as u32) * 2;

    // 與輸出相同順序：先縮放、再調色、後補邊，最後上字幕
    let adjust_mid = adj
        .filter_chain()
        .map(|c| format!("{c},"))
        .unwrap_or_default();
    let mut vf = format!(
        "scale={pw}:{ph}:force_original_aspect_ratio=decrease,{adjust_mid}\
         pad={pw}:{ph}:(ow-iw)/2:(oh-ih)/2:color=black"
    );
    if let Some(font) = font {
        let fontsize = style.size as f64 * ph as f64 / 1080.0;
        for (k, text) in captions.iter().enumerate() {
            let cap_path = std::env::temp_dir().join(format!("photo2video_cap_preview_{k}.txt"));
            std::fs::write(&cap_path, text.trim_end())
                .map_err(|e| format!("無法寫入字幕暫存檔：{e}"))?;
            vf.push(',');
            vf.push_str(&drawtext_filter(font, &cap_path, style, fontsize, ph, None));
        }
    }

    let out = std::env::temp_dir().join("photo2video_preview.png");
    let mut cmd = FfmpegCommand::new();
    cmd.arg("-y")
        .input(photo.to_string_lossy())
        .args(["-vf", &vf])
        .args(["-frames:v", "1"])
        .args(["-update", "1"])
        .output(out.to_string_lossy());

    let mut error_log: Vec<String> = Vec::new();
    let mut child = cmd.spawn().map_err(|e| format!("FFmpeg 啟動失敗：{e}"))?;
    let iter = child
        .iter()
        .map_err(|e| format!("FFmpeg 輸出讀取失敗：{e}"))?;
    for event in iter {
        if let FfmpegEvent::Log(level, msg) = event {
            use ffmpeg_sidecar::event::LogLevel;
            if matches!(level, LogLevel::Error | LogLevel::Fatal) {
                error_log.push(msg);
            }
        }
    }
    let status = child.wait().map_err(|e| format!("FFmpeg 執行失敗：{e}"))?;
    if !status.success() {
        return Err(if error_log.is_empty() {
            "預覽渲染失敗".into()
        } else {
            error_log.join("\n")
        });
    }

    let img = image::open(&out)
        .map_err(|e| format!("預覽圖讀取失敗：{e}"))?
        .to_rgba8();
    let (w, h) = img.dimensions();
    Ok((w, h, img.into_raw()))
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

    // 用 concat demuxer 列出每張照片與顯示時間
    let duration = 1.0 / fps as f64;
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

    // 字幕：每個段落產生一段 drawtext，用時間區間涵蓋該段照片
    let mut caption_files: Vec<PathBuf> = Vec::new();
    if let Some(font) = &subs.font {
        let fontsize = subs.style.size as f64 * h as f64 / 1080.0;
        let d = 1.0 / fps as f64;
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
    vf.push_str(",setsar=1,format=yuv420p");

    let mut cmd = FfmpegCommand::new();
    cmd.arg("-y")
        .args(["-filter_threads", &filter_threads()])
        .args(["-f", "concat", "-safe", "0"])
        .input(list_path.to_string_lossy())
        .args(["-vf", &vf])
        .args(["-r", &fps.to_string()])
        // 精確限制總長度，避免 concat 清單重複最後一張造成多出一格
        .args(["-t", &format!("{}", photos.len() as f64 / fps as f64)]);

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

    let total_frames = photos.len() as f32;
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
    run_conversion(
        &photos,
        fps,
        format,
        Resolution { w: 1920, h: 1080 },
        &Adjustments::default(),
        &no_subs,
        &output,
        &send,
    )?;
    println!("完成：{}", output.display());
    Ok(())
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "--cli" {
        if let Err(e) = run_cli(&args[2..]) {
            eprintln!("錯誤：{e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1080.0, 640.0])
            .with_min_inner_size([860.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "照片轉影片工具",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
