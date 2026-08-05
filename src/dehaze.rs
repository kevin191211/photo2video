//! 煙霧移除：估出煙霧的散射光層後直接扣掉，保留煙火細節。
//!
//! 針對夜間煙火照片：火藥煙被煙火照亮後形成大面積的低頻輝光，
//! 蓋掉夜空也讓煙火線條發灰。
//!
//! 這裡不用經典的暗通道先驗去霧（I = J·t + A·(1−t)）——它假設霧是明亮的
//! 日景背景，而夜間煙霧的絕對亮度遠低於大氣光，暗通道估出的透射率幾乎恆為 1，
//! 等於什麼都沒去掉。夜空是黑的，煙霧是「疊加」在上面的散射光，
//! 所以改用加性模型 I = J + S，估出 S 再相減。
//!
//! 估 S 的關鍵在於煙霧是連續的「面」、煙火軌跡是離散的「細線」：
//! 最小值池化與形態學開運算會吃掉比結構元素細的亮物件，
//! 留下的就是煙霧層，煙火的線條因此不在 S 裡、扣完仍完整保留。

use image::{Rgb, RgbImage};

/// 估計煙霧層時的工作解析度（長邊）。煙霧是低頻訊號，
/// 縮圖估計不影響品質，卻讓大圖處理維持在一秒內。
const WORK_LONG_EDGE: u32 = 640;

/// 只在畫面某個區塊去煙時的作用範圍。
/// 用相對座標（0~1）而非像素，預覽縮圖與原尺寸才會框到同一塊。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Region {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Region {
    /// 夾回 0~1 並確保 x0 < x1、y0 < y1（框選可能從右下往左上拉）
    fn normalized(self) -> Self {
        let (x0, x1) = (self.x0.min(self.x1), self.x0.max(self.x1));
        let (y0, y1) = (self.y0.min(self.y1), self.y0.max(self.y1));
        Self {
            x0: x0.clamp(0.0, 1.0),
            y0: y0.clamp(0.0, 1.0),
            x1: x1.clamp(0.0, 1.0),
            y1: y1.clamp(0.0, 1.0),
        }
    }

    /// 框太細會退化成一條線，視為沒有框選
    fn is_usable(&self) -> bool {
        self.x1 - self.x0 > 0.01 && self.y1 - self.y0 > 0.01
    }
}

/// 去煙參數
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SmokeParams {
    /// 去除強度 0~100：煙霧層要扣掉多少，100 時幾乎移除全部散射光
    pub strength: i32,
    /// 細節保留 0~100：數值越高，煙霧層越貼合原圖邊緣（煙火線條越不被削）
    pub detail: i32,
    /// 殘霧壓制 0~100：對扣完的暗部再壓一次黑點，讓夜空回到純黑
    pub black: i32,
    /// 只去除這個區塊裡的煙霧；None＝整張照片
    pub region: Option<Region>,
    /// 區塊邊緣的羽化寬度 0~100（相對於區塊短邊的比例）。
    /// 邊界硬切會在畫面上留下一條看得出來的接縫
    pub feather: i32,
    /// 保護色（sRGB）：與其中任一色相近的像素都不去煙，用來留住不想被扣掉的顏色。
    /// 固定長度是為了讓參數維持 Copy；全為 None＝不做顏色保護
    pub protect: [Option<[u8; 3]>; MAX_PROTECT],
    /// 保護色的容許範圍 0~100：越大則越多相近的顏色一起被保護
    pub tolerance: i32,
}

/// 最多可以指定幾個保護色
pub const MAX_PROTECT: usize = 6;

impl Default for SmokeParams {
    fn default() -> Self {
        Self {
            strength: 80,
            detail: 60,
            black: 30,
            region: None,
            feather: 25,
            protect: [None; MAX_PROTECT],
            tolerance: 30,
        }
    }
}

impl SmokeParams {
    pub fn clamped(mut self) -> Self {
        self.strength = self.strength.clamp(0, 100);
        self.detail = self.detail.clamp(0, 100);
        self.black = self.black.clamp(0, 100);
        self.feather = self.feather.clamp(0, 100);
        self.tolerance = self.tolerance.clamp(0, 100);
        self.region = self
            .region
            .map(Region::normalized)
            .filter(Region::is_usable);
        self
    }

    pub fn is_neutral(&self) -> bool {
        self.strength <= 0
    }

    // 以下幾個是給 GUI 用的，smoke_cli 只會用到 add_protect
    /// 目前設定的保護色（依加入順序）
    #[allow(dead_code)]
    pub fn protect_colors(&self) -> impl Iterator<Item = (usize, [u8; 3])> + '_ {
        self.protect
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.map(|c| (i, c)))
    }

    #[allow(dead_code)]
    pub fn has_protect(&self) -> bool {
        self.protect.iter().any(Option::is_some)
    }

    /// 加一個保護色；已經滿了或已存在同色則回傳 false
    pub fn add_protect(&mut self, c: [u8; 3]) -> bool {
        if self.protect.contains(&Some(c)) {
            return false;
        }
        match self.protect.iter_mut().find(|s| s.is_none()) {
            Some(slot) => {
                *slot = Some(c);
                true
            }
            None => false,
        }
    }

    #[allow(dead_code)]
    pub fn remove_protect(&mut self, i: usize) {
        if let Some(slot) = self.protect.get_mut(i) {
            *slot = None;
        }
    }

    #[allow(dead_code)]
    pub fn clear_protect(&mut self) {
        self.protect = [None; MAX_PROTECT];
    }
}

/// 單通道浮點影像平面
#[derive(Clone)]
struct Plane {
    w: usize,
    h: usize,
    d: Vec<f32>,
}

impl Plane {
    fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            d: vec![0.0; w * h],
        }
    }
}

/// sRGB → 線性光。散射光在線性空間才是加性的，去霧公式必須在這裡算。
fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// 建 8-bit sRGB → 線性的查表，避免每像素做 powf
fn srgb_lut() -> [f32; 256] {
    let mut lut = [0.0f32; 256];
    for (i, v) in lut.iter_mut().enumerate() {
        *v = srgb_to_linear(i as f32 / 255.0);
    }
    lut
}

/// 半徑 r 的方框均值（積分圖，O(n)）。邊界以實際覆蓋面積正規化，
/// 不做 padding，避免邊緣被拉暗。
fn box_mean(p: &Plane, r: usize) -> Plane {
    let (w, h) = (p.w, p.h);
    // 積分圖多一行一列，索引 (y+1)*(w+1)+(x+1) 為左上角矩形和
    let mut sat = vec![0.0f64; (w + 1) * (h + 1)];
    for y in 0..h {
        let mut row = 0.0f64;
        for x in 0..w {
            row += p.d[y * w + x] as f64;
            sat[(y + 1) * (w + 1) + (x + 1)] = sat[y * (w + 1) + (x + 1)] + row;
        }
    }
    let mut out = Plane::new(w, h);
    for y in 0..h {
        let y0 = y.saturating_sub(r);
        let y1 = (y + r + 1).min(h);
        for x in 0..w {
            let x0 = x.saturating_sub(r);
            let x1 = (x + r + 1).min(w);
            let s = sat[y1 * (w + 1) + x1] - sat[y0 * (w + 1) + x1] - sat[y1 * (w + 1) + x0]
                + sat[y0 * (w + 1) + x0];
            let n = ((x1 - x0) * (y1 - y0)) as f64;
            out.d[y * w + x] = (s / n) as f32;
        }
    }
    out
}

/// 半徑 r 的可分離最小值濾波（形態學腐蝕）。
/// 先橫後直，各用單調佇列做 O(n) 滑動極值。
fn min_filter(p: &Plane, r: usize) -> Plane {
    let tmp = extreme_1d_rows(p, r, false);
    let t = transpose(&tmp);
    let t = extreme_1d_rows(&t, r, false);
    transpose(&t)
}

/// 半徑 r 的可分離最大值濾波（形態學膨脹）
fn max_filter(p: &Plane, r: usize) -> Plane {
    let tmp = extreme_1d_rows(p, r, true);
    let t = transpose(&tmp);
    let t = extreme_1d_rows(&t, r, true);
    transpose(&t)
}

fn transpose(p: &Plane) -> Plane {
    let mut out = Plane::new(p.h, p.w);
    for y in 0..p.h {
        for x in 0..p.w {
            out.d[x * p.h + y] = p.d[y * p.w + x];
        }
    }
    out
}

/// 對每一列做視窗 2r+1 的滑動極值（單調佇列）。max=true 取最大值，否則取最小值。
fn extreme_1d_rows(p: &Plane, r: usize, max: bool) -> Plane {
    let (w, h) = (p.w, p.h);
    let mut out = Plane::new(w, h);
    // 佇列存索引，對應值單調；隊首即為當前視窗的極值
    let mut dq: std::collections::VecDeque<usize> = std::collections::VecDeque::with_capacity(w);
    for y in 0..h {
        let row = &p.d[y * w..y * w + w];
        dq.clear();
        for x in 0..w {
            let hi = (x + r).min(w - 1);
            let lo = x.saturating_sub(r);
            // 納入尚未進過視窗的元素；先彈掉尾端不可能再成為極值者
            let start = if x == 0 { 0 } else { (x + r).min(w) };
            for i in start..=hi {
                while let Some(&last) = dq.back() {
                    let worse = if max {
                        row[last] <= row[i]
                    } else {
                        row[last] >= row[i]
                    };
                    if worse {
                        dq.pop_back();
                    } else {
                        break;
                    }
                }
                dq.push_back(i);
            }
            // 移除滑出左界者
            while let Some(&first) = dq.front() {
                if first < lo {
                    dq.pop_front();
                } else {
                    break;
                }
            }
            out.d[y * w + x] = row[*dq.front().expect("視窗非空")];
        }
    }
    out
}

/// 導引濾波：以 guide 的邊緣結構重建 src，讓透射率貼齊煙火線條，
/// 消除單純模糊會產生的光暈。
fn guided_filter(guide: &Plane, src: &Plane, r: usize, eps: f32) -> Plane {
    let n = guide.d.len();
    let mut ii = Plane::new(guide.w, guide.h);
    let mut ip = Plane::new(guide.w, guide.h);
    for i in 0..n {
        ii.d[i] = guide.d[i] * guide.d[i];
        ip.d[i] = guide.d[i] * src.d[i];
    }
    let mean_i = box_mean(guide, r);
    let mean_p = box_mean(src, r);
    let mean_ii = box_mean(&ii, r);
    let mean_ip = box_mean(&ip, r);

    let mut a = Plane::new(guide.w, guide.h);
    let mut b = Plane::new(guide.w, guide.h);
    for i in 0..n {
        let var = mean_ii.d[i] - mean_i.d[i] * mean_i.d[i];
        let cov = mean_ip.d[i] - mean_i.d[i] * mean_p.d[i];
        a.d[i] = cov / (var + eps);
        b.d[i] = mean_p.d[i] - a.d[i] * mean_i.d[i];
    }
    let mean_a = box_mean(&a, r);
    let mean_b = box_mean(&b, r);
    let mut out = Plane::new(guide.w, guide.h);
    for i in 0..n {
        out.d[i] = mean_a.d[i] * guide.d[i] + mean_b.d[i];
    }
    out
}

/// 雙線性放大單通道平面到指定尺寸
fn upscale(p: &Plane, w: usize, h: usize) -> Plane {
    if p.w == w && p.h == h {
        return p.clone();
    }
    let mut out = Plane::new(w, h);
    let sx = p.w as f32 / w as f32;
    let sy = p.h as f32 / h as f32;
    for y in 0..h {
        let fy = ((y as f32 + 0.5) * sy - 0.5).max(0.0);
        let y0 = (fy as usize).min(p.h - 1);
        let y1 = (y0 + 1).min(p.h - 1);
        let wy = fy - y0 as f32;
        for x in 0..w {
            let fx = ((x as f32 + 0.5) * sx - 0.5).max(0.0);
            let x0 = (fx as usize).min(p.w - 1);
            let x1 = (x0 + 1).min(p.w - 1);
            let wx = fx - x0 as f32;
            let v00 = p.d[y0 * p.w + x0];
            let v01 = p.d[y0 * p.w + x1];
            let v10 = p.d[y1 * p.w + x0];
            let v11 = p.d[y1 * p.w + x1];
            let top = v00 + (v01 - v00) * wx;
            let bot = v10 + (v11 - v10) * wx;
            out.d[y * w + x] = top + (bot - top) * wy;
        }
    }
    out
}

/// 最小值池化下採樣：輸出每個像素取原圖對應區塊各通道的最小值（線性光）
fn min_downsample(img: &RgbImage, lut: &[f32; 256], ww: usize, wh: usize) -> Vec<[f32; 3]> {
    let (fw, fh) = (img.width() as usize, img.height() as usize);
    let mut out = vec![[1.0f32; 3]; ww * wh];
    for y in 0..fh {
        // 區塊索引直接由座標比例算，可容忍 fw/ww 非整數倍
        let by = (y * wh / fh).min(wh - 1);
        for x in 0..fw {
            let bx = (x * ww / fw).min(ww - 1);
            let px = img.get_pixel(x as u32, y as u32);
            let o = &mut out[by * ww + bx];
            for c in 0..3 {
                let v = lut[px[c] as usize];
                if v < o[c] {
                    o[c] = v;
                }
            }
        }
    }
    out
}

/// 估計煙霧輝光層（三通道線性光，已放大回原尺寸）
fn estimate_smoke(img: &RgbImage, p: SmokeParams, lut: &[f32; 256]) -> Vec<Plane> {
    let (fw, fh) = (img.width() as usize, img.height() as usize);

    // --- 1. 縮到工作尺寸估煙霧層 ---
    let long = fw.max(fh) as u32;
    let scale = if long > WORK_LONG_EDGE {
        WORK_LONG_EDGE as f32 / long as f32
    } else {
        1.0
    };
    let ww = ((fw as f32 * scale).round() as usize).max(1);
    let wh = ((fh as f32 * scale).round() as usize).max(1);
    // 用「最小值池化」而非平均縮圖：平均會把煙火線條的亮度抹進背景，
    // 讓煙霧層被高估、煙火簇整團被當成煙霧削掉。取區塊最小值則等同先做一次
    // 腐蝕，細線在這一步就消失，只有連續的煙霧面留下來。
    let lin = min_downsample(img, lut, ww, wh);

    // --- 2. 以形態學開運算取出煙霧輝光層 ---
    // 開運算（先腐蝕再膨脹）會移除比結構元素細的亮物件（煙火線條、星點），
    // 只留下大尺度的連續亮面 —— 正是煙霧。單用腐蝕會把整層壓低，
    // 膨脹再把煙霧的原始高度還原回來。
    // 最小值池化已清掉細線，這裡只需小半徑掃掉殘餘的線條交叉點與亮星點。
    // 半徑放大反而會把煙火簇整團當成煙霧，連線條一起削暗。
    let r_open = ((ww.max(wh) as f32 * 0.006).round() as usize).clamp(2, 8);
    let mut guide = Plane::new(ww, wh);
    for (i, c) in lin.iter().enumerate() {
        guide.d[i] = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    }

    // --- 3. 導引濾波把開運算的方塊邊緣抹平，並貼回原圖結構 ---
    // detail 越高 → 半徑越小、eps 越小 → 煙霧層越貼合原圖，煙火線條保留越完整
    let dt = p.detail as f32 / 100.0;
    let r_guide = ((ww.max(wh) as f32 * (0.06 - 0.045 * dt)).round() as usize).clamp(3, 60);
    let eps = 10f32.powf(-3.0 - 2.0 * dt);

    let mut smoke = Vec::with_capacity(3);
    for c in 0..3 {
        let mut ch = Plane::new(ww, wh);
        for (i, px) in lin.iter().enumerate() {
            ch.d[i] = px[c];
        }
        let opened = max_filter(&min_filter(&ch, r_open), r_open);
        let mut s = guided_filter(&guide, &opened, r_guide, eps);
        // 煙霧層不可能亮過原圖，否則相減後會出現大片死黑
        for i in 0..ww * wh {
            s.d[i] = s.d[i].clamp(0.0, ch.d[i]);
        }
        smoke.push(upscale(&s, fw, fh));
    }

    smoke
}

/// 診斷用（smoke_cli）：把估出的煙霧層輸出成影像，並回傳其最濃處的顏色
#[allow(dead_code)]
pub fn debug_smoke_layer(img: &RgbImage, params: SmokeParams) -> (RgbImage, [f32; 3]) {
    let lut = srgb_lut();
    let smoke = estimate_smoke(img, params.clamped(), &lut);
    let mut a = [0.0f32; 3];
    for (c, s) in smoke.iter().enumerate() {
        a[c] = s.d.iter().copied().fold(0.0f32, f32::max);
    }
    let mut out = RgbImage::new(img.width(), img.height());
    for (i, px) in out.pixels_mut().enumerate() {
        *px = Rgb([
            (linear_to_srgb(smoke[0].d[i].clamp(0.0, 1.0)) * 255.0) as u8,
            (linear_to_srgb(smoke[1].d[i].clamp(0.0, 1.0)) * 255.0) as u8,
            (linear_to_srgb(smoke[2].d[i].clamp(0.0, 1.0)) * 255.0) as u8,
        ]);
    }
    (out, a)
}

/// 區塊遮罩：框內為 1、框外為 0，邊界以羽化寬度平滑過渡。
/// 沒有框選時整張都是 1。
struct RegionMask {
    /// 像素座標的框線位置與羽化寬度；None＝整張都要處理
    bounds: Option<([f32; 4], f32)>,
}

impl RegionMask {
    fn new(region: Option<Region>, feather: i32, fw: usize, fh: usize) -> Self {
        let Some(r) = region else {
            return Self { bounds: None };
        };
        let (fwf, fhf) = (fw as f32, fh as f32);
        let px = [r.x0 * fwf, r.y0 * fhf, r.x1 * fwf, r.y1 * fhf];
        // 羽化寬度以框的短邊為基準，框拉得再小也不會被過渡帶整個吃掉
        let short = (px[2] - px[0]).min(px[3] - px[1]).max(1.0);
        let f = (short * feather as f32 / 100.0 * 0.5).max(0.5);
        Self {
            bounds: Some((px, f)),
        }
    }

    /// 單軸的過渡權重：[lo, hi] 內為 1，往外 f 個像素平滑降到 0
    fn axis(v: f32, lo: f32, hi: f32, f: f32) -> f32 {
        let d = if v < lo {
            (v - (lo - f)) / f
        } else if v > hi {
            ((hi + f) - v) / f
        } else {
            return 1.0;
        };
        let t = d.clamp(0.0, 1.0);
        // smoothstep：線性過渡在羽化帶兩端會留下看得見的折線
        t * t * (3.0 - 2.0 * t)
    }

    fn at(&self, x: usize, y: usize) -> f32 {
        let Some((b, f)) = self.bounds else {
            return 1.0;
        };
        // 取像素中心，框線落在像素邊界時兩側才對稱
        let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
        Self::axis(px, b[0], b[2], f) * Self::axis(py, b[1], b[3], f)
    }
}

/// 顏色保護：和任一個指定顏色夠接近的像素都不去煙，
/// 相當於 Photoshop 依「顏色範圍」建出來的遮色片。
struct ColorProtect {
    /// 保護色（正規化 sRGB）；空的代表不保護任何顏色
    keys: Vec<[f32; 3]>,
    /// 容許距離的內外界
    t0: f32,
    t1: f32,
}

impl ColorProtect {
    fn new(protect: &[Option<[u8; 3]>; MAX_PROTECT], tolerance: i32) -> Self {
        let keys = protect
            .iter()
            .flatten()
            .map(|c| {
                [
                    c[0] as f32 / 255.0,
                    c[1] as f32 / 255.0,
                    c[2] as f32 / 255.0,
                ]
            })
            .collect();
        // 距離在 sRGB 空間量（與眼睛看到的「顏色像不像」較接近，
        // 也和 Photoshop 的顏色範圍一致）；最大距離為 √3，正規化到 0~1
        let t1 = tolerance as f32 / 100.0 * 0.9;
        Self {
            keys,
            t0: t1 * 0.6,
            t1,
        }
    }

    /// 回傳這個像素「要去煙的比例」：完全命中任一保護色為 0，離每個都夠遠為 1
    fn at(&self, src: &Rgb<u8>) -> f32 {
        if self.keys.is_empty() {
            return 1.0;
        }
        // 取離最近的那個保護色的距離：命中任一色就該被保護
        let mut d = f32::INFINITY;
        for key in &self.keys {
            let dist = (0..3)
                .map(|c| {
                    let v = src[c] as f32 / 255.0 - key[c];
                    v * v
                })
                .sum::<f32>()
                .sqrt()
                / 3f32.sqrt();
            if dist < d {
                d = dist;
            }
        }
        if d <= self.t0 {
            return 0.0;
        }
        if d >= self.t1 || self.t1 <= self.t0 {
            return 1.0;
        }
        // 邊界平滑，避免保護區外圍出現鋸齒狀的硬邊
        let t = (d - self.t0) / (self.t1 - self.t0);
        t * t * (3.0 - 2.0 * t)
    }
}

/// 遮色片預覽：把「不會去煙」的區域疊上紅色，
/// 讓使用者看得到框選範圍與顏色保護實際蓋住哪裡（比照 Photoshop 的快速遮色片）
pub fn mask_overlay(img: &RgbImage, params: SmokeParams) -> RgbImage {
    let p = params.clamped();
    let (fw, fh) = (img.width() as usize, img.height() as usize);
    let mask = RegionMask::new(p.region, p.feather, fw, fh);
    let protect = ColorProtect::new(&p.protect, p.tolerance);
    let mut out = img.clone();
    for y in 0..fh {
        for x in 0..fw {
            let px = out.get_pixel_mut(x as u32, y as u32);
            let covered = 1.0 - mask.at(x, y) * protect.at(px);
            if covered <= 0.0 {
                continue;
            }
            let a = covered * 0.55;
            const RED: [f32; 3] = [220.0, 40.0, 60.0];
            for c in 0..3 {
                px[c] = (px[c] as f32 * (1.0 - a) + RED[c] * a).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// 移除照片中的煙霧，保留煙火細節
pub fn remove_smoke(img: &RgbImage, params: SmokeParams) -> RgbImage {
    let p = params.clamped();
    if p.is_neutral() {
        return img.clone();
    }
    let (fw, fh) = (img.width() as usize, img.height() as usize);
    let lut = srgb_lut();
    let smoke = estimate_smoke(img, p, &lut);

    // --- 4. 相減：J = I − k·S ---
    // 煙霧散射光是加性的，直接扣掉即可；煙火線條的亮度是自身發光，
    // 扣掉底下那層煙霧後仍完整保留。
    // 最小值池化取的是區塊下界，估出的煙霧層比實際低一截，
    // 係數放大到 1.6 倍才能在強度 100 時把煙霧完全扣乾淨
    let k = p.strength as f32 / 100.0 * 1.6;
    let blk = p.black as f32 / 100.0;
    let mask = RegionMask::new(p.region, p.feather, fw, fh);
    let protect = ColorProtect::new(&p.protect, p.tolerance);
    let mut out = RgbImage::new(fw as u32, fh as u32);
    for y in 0..fh {
        for x in 0..fw {
            let src = img.get_pixel(x as u32, y as u32);
            // 框選範圍外、或命中保護色的像素完全不動，
            // 省下整段運算也保證原樣輸出
            let w = mask.at(x, y) * protect.at(src);
            if w <= 0.0 {
                out.put_pixel(x as u32, y as u32, *src);
                continue;
            }
            let k = k * w;
            let i = [
                lut[src[0] as usize],
                lut[src[1] as usize],
                lut[src[2] as usize],
            ];
            let s = [
                smoke[0].d[y * fw + x],
                smoke[1].d[y * fw + x],
                smoke[2].d[y * fw + x],
            ];

            let mut rgb = [0.0f32; 3];
            for c in 0..3 {
                rgb[c] = i[c] - k * s[c];
            }
            // 通道耦合：某通道被扣成負值，代表這裡的煙霧估得比實際還濃，
            // 等量從其他通道一併扣掉。否則紫煙的藍色先歸零、紅色留下來，
            // 會在濃煙散去處留下暗紅褐色的斑塊。
            let m = rgb[0].min(rgb[1]).min(rgb[2]);
            if m < 0.0 {
                for c in 0..3 {
                    rgb[c] += m;
                }
            }
            for c in 0..3 {
                rgb[c] = rgb[c].max(0.0);
            }

            // 過曝像素（某通道已頂到 255）的真實亮度被截斷，逐通道相減
            // 會扣掉「看不見的那一截」，把橘紅色的煙火亮球算成青綠色。
            // 這種像素改用等比例壓暗：亮度照扣，但色相原封不動。
            let vmax8 = src[0].max(src[1]).max(src[2]) as f32;
            let clip_w = ((vmax8 - 235.0) / 20.0).clamp(0.0, 1.0);
            if clip_w > 0.0 {
                let yi = 0.2126 * i[0] + 0.7152 * i[1] + 0.0722 * i[2];
                let ys = 0.2126 * s[0] + 0.7152 * s[1] + 0.0722 * s[2];
                if yi > 0.0 {
                    let scale = (1.0 - k * ys / yi).max(0.0);
                    for c in 0..3 {
                        rgb[c] = rgb[c] * (1.0 - clip_w) + i[c] * scale * clip_w;
                    }
                }
            }
            // 殘霧壓制：以亮度做黑點壓縮，等比縮放三通道以免偏色。
            // 一樣乘上遮罩，羽化帶才不會因為壓黑而出現一圈暗邊
            let blk = blk * w;
            if blk > 0.0 {
                let y_lin = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
                if y_lin > 0.0 {
                    // 壓黑點：低於 cut 的全歸零，其餘線性拉回 0~1
                    let cut = 0.02 * blk;
                    let ny = ((y_lin - cut) / (1.0 - cut)).max(0.0);
                    let k = ny / y_lin;
                    for c in 0..3 {
                        rgb[c] *= k;
                    }
                }
            }
            let px = out.get_pixel_mut(x as u32, y as u32);
            *px = Rgb([
                (linear_to_srgb(rgb[0].min(1.0)) * 255.0).round().clamp(0.0, 255.0) as u8,
                (linear_to_srgb(rgb[1].min(1.0)) * 255.0).round().clamp(0.0, 255.0) as u8,
                (linear_to_srgb(rgb[2].min(1.0)) * 255.0).round().clamp(0.0, 255.0) as u8,
            ]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 產生一張純色圖
    fn solid(w: u32, h: u32, c: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(w, h, Rgb(c))
    }

    #[test]
    fn params_are_clamped_into_range() {
        let p = SmokeParams {
            strength: 500,
            detail: -20,
            black: 999,
            feather: -5,
            tolerance: 400,
            ..Default::default()
        }
        .clamped();
        assert_eq!(
            (p.strength, p.detail, p.black, p.feather, p.tolerance),
            (100, 0, 100, 0, 100)
        );
    }

    #[test]
    fn zero_strength_returns_the_original_untouched() {
        let img = solid(8, 6, [90, 70, 130]);
        let out = remove_smoke(
            &img,
            SmokeParams {
                strength: 0,
                ..Default::default()
            },
        );
        assert_eq!(out, img);
    }

    #[test]
    fn output_keeps_the_input_dimensions() {
        let img = solid(37, 19, [40, 40, 60]);
        let out = remove_smoke(&img, SmokeParams::default());
        assert_eq!((out.width(), out.height()), (37, 19));
    }

    /// 全黑的夜空不該被去煙處理弄出雜訊或抬亮
    #[test]
    fn pure_black_stays_black() {
        let img = solid(16, 16, [0, 0, 0]);
        let out = remove_smoke(&img, SmokeParams::default());
        assert!(out.pixels().all(|p| p.0 == [0, 0, 0]));
    }

    /// 整片均勻的煙霧沒有任何細節，應該被扣到近乎全黑
    #[test]
    fn uniform_smoke_is_removed() {
        let img = solid(64, 64, [120, 100, 170]);
        let out = remove_smoke(
            &img,
            SmokeParams {
                strength: 100,
                detail: 60,
                black: 0,
                ..Default::default()
            },
        );
        let brightest = out.pixels().map(|p| p.0.iter().copied().max().unwrap()).max();
        assert_eq!(brightest, Some(0), "均勻煙霧沒被扣乾淨");
    }

    /// 過曝的煙火亮點在扣掉煙霧後仍須維持原本的色相（不能由橘紅翻成青綠）
    #[test]
    fn clipped_highlights_keep_their_hue() {
        // 紫色煙霧背景中放一顆過曝的橘紅亮球
        let mut img = solid(64, 64, [120, 100, 170]);
        for y in 30..34 {
            for x in 30..34 {
                img.put_pixel(x, y, Rgb([255, 160, 60]));
            }
        }
        let out = remove_smoke(&img, SmokeParams::default());
        let p = out.get_pixel(32, 32).0;
        assert!(
            p[0] > p[1] && p[1] >= p[2],
            "亮球色相反轉了：{p:?} 應維持 R > G >= B"
        );
    }

    /// 極小尺寸不能讓濾波器的視窗計算越界
    #[test]
    fn tiny_images_do_not_panic() {
        for (w, h) in [(1, 1), (1, 7), (7, 1), (2, 3), (3, 2)] {
            let img = solid(w, h, [100, 90, 140]);
            let out = remove_smoke(&img, SmokeParams::default());
            assert_eq!((out.width(), out.height()), (w, h));
        }
    }

    /// 框選範圍外的像素必須原封不動
    #[test]
    fn region_leaves_the_outside_untouched() {
        let img = solid(80, 80, [120, 100, 170]);
        let p = SmokeParams {
            strength: 100,
            feather: 0,
            region: Some(Region {
                x0: 0.5,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0,
            }),
            ..Default::default()
        };
        let out = remove_smoke(&img, p);
        // 最左側完全在框外，右側在框內且已被扣掉
        assert_eq!(out.get_pixel(2, 40).0, [120, 100, 170], "框外被動到了");
        assert!(out.get_pixel(78, 40).0[2] < 170, "框內沒有去煙");
    }

    /// 從右下往左上拉出來的框也要正規化成同一塊
    #[test]
    fn region_is_normalized_whichever_way_it_is_dragged() {
        let forward = Region {
            x0: 0.2,
            y0: 0.3,
            x1: 0.8,
            y1: 0.9,
        };
        let backward = Region {
            x0: 0.8,
            y0: 0.9,
            x1: 0.2,
            y1: 0.3,
        };
        assert_eq!(forward.normalized(), backward.normalized());
    }

    /// 退化成一條線的框視為沒框選（整張處理），不是完全不處理
    #[test]
    fn degenerate_region_falls_back_to_whole_image() {
        let p = SmokeParams {
            region: Some(Region {
                x0: 0.5,
                y0: 0.2,
                x1: 0.5005,
                y1: 0.9,
            }),
            ..Default::default()
        }
        .clamped();
        assert!(p.region.is_none());
    }

    /// 命中保護色的像素不該被去煙
    #[test]
    fn protected_colour_survives() {
        // 紫煙背景中放一塊要保住的青色
        let keep = [40, 200, 210];
        let mut img = solid(80, 80, [120, 100, 170]);
        for y in 20..60 {
            for x in 20..60 {
                img.put_pixel(x, y, Rgb(keep));
            }
        }
        let mut p = SmokeParams {
            strength: 100,
            tolerance: 20,
            ..Default::default()
        };
        assert!(p.add_protect(keep));
        let out = remove_smoke(&img, p);
        assert_eq!(out.get_pixel(40, 40).0, keep, "保護色被扣掉了");
        // 保護色以外的煙霧照樣要被扣掉
        assert!(out.get_pixel(2, 2).0[2] < 170, "保護色以外沒有去煙");
    }

    /// 指定多個保護色時，每一個都要生效
    #[test]
    fn every_protected_colour_survives() {
        let keeps = [[40, 200, 210], [230, 90, 40], [90, 240, 110]];
        let mut img = solid(160, 60, [120, 100, 170]);
        // 三塊各自塗上一個要保住的顏色
        for (k, c) in keeps.iter().enumerate() {
            for y in 20..40 {
                for x in (k as u32 * 50 + 10)..(k as u32 * 50 + 40) {
                    img.put_pixel(x, y, Rgb(*c));
                }
            }
        }
        let mut p = SmokeParams {
            strength: 100,
            tolerance: 20,
            ..Default::default()
        };
        for c in &keeps {
            assert!(p.add_protect(*c));
        }
        let out = remove_smoke(&img, p);
        for (k, c) in keeps.iter().enumerate() {
            assert_eq!(
                out.get_pixel(k as u32 * 50 + 25, 30).0,
                *c,
                "第 {k} 個保護色被扣掉了"
            );
        }
        assert!(out.get_pixel(2, 2).0[2] < 170, "保護色以外沒有去煙");
    }

    /// 保護色有數量上限，重複的顏色不重複佔位
    #[test]
    fn protect_list_rejects_duplicates_and_overflow() {
        let mut p = SmokeParams::default();
        assert!(p.add_protect([10, 20, 30]));
        assert!(!p.add_protect([10, 20, 30]), "同色不該重複加入");
        for i in 1..MAX_PROTECT {
            assert!(p.add_protect([i as u8, 0, 0]));
        }
        assert!(!p.add_protect([9, 9, 9]), "滿了還能再加");
        assert_eq!(p.protect_colors().count(), MAX_PROTECT);
        p.remove_protect(0);
        assert_eq!(p.protect_colors().count(), MAX_PROTECT - 1);
        assert!(p.add_protect([9, 9, 9]), "移除後空出來的位置沒被用到");
        p.clear_protect();
        assert!(!p.has_protect());
    }

    /// 容差 0 時只有幾乎完全同色才受保護，不會整張都不處理
    #[test]
    fn zero_tolerance_barely_protects_anything() {
        let img = solid(64, 64, [120, 100, 170]);
        let mut p = SmokeParams {
            strength: 100,
            tolerance: 0,
            ..Default::default()
        };
        p.add_protect([40, 200, 210]);
        let out = remove_smoke(&img, p);
        let brightest = out.pixels().map(|p| p.0.iter().copied().max().unwrap()).max();
        assert_eq!(brightest, Some(0), "與保護色差很遠卻沒被去煙");
    }

    /// 滑動極值濾波要與暴力法一致
    #[test]
    fn sliding_extremes_match_brute_force() {
        let w = 23;
        let h = 5;
        let mut p = Plane::new(w, h);
        for (i, v) in p.d.iter_mut().enumerate() {
            // 有起伏又不單調的樣本
            *v = ((i * 7 % 13) as f32 - 6.0) / 6.0;
        }
        for r in [1usize, 3, 8, 30] {
            let lo = min_filter(&p, r);
            let hi = max_filter(&p, r);
            for y in 0..h {
                for x in 0..w {
                    let (mut bmin, mut bmax) = (f32::INFINITY, f32::NEG_INFINITY);
                    for yy in y.saturating_sub(r)..(y + r + 1).min(h) {
                        for xx in x.saturating_sub(r)..(x + r + 1).min(w) {
                            bmin = bmin.min(p.d[yy * w + xx]);
                            bmax = bmax.max(p.d[yy * w + xx]);
                        }
                    }
                    assert_eq!(lo.d[y * w + x], bmin, "min r={r} at ({x},{y})");
                    assert_eq!(hi.d[y * w + x], bmax, "max r={r} at ({x},{y})");
                }
            }
        }
    }
}
