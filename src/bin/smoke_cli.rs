//! 去煙演算法的參數實驗工具：
//! cargo run --release --bin smoke_cli -- <輸入> <輸出> [strength] [detail] [black] [x0,y0,x1,y1]
//! 最後一個參數為只去煙的區塊（0~1 相對座標），省略則整張處理。

#[path = "../dehaze.rs"]
mod dehaze;

fn tm_save(img: &image::RgbImage, out: &str) {
    let p = out.replace(".jpg", "_layer.jpg");
    img.save(&p).expect("寫出失敗");
    println!("煙霧層 → {p}");
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("用法：smoke_cli <輸入> <輸出> [strength] [detail] [black]");
        std::process::exit(2);
    }
    let region = a.get(6).and_then(|s| {
        let v: Vec<f32> = s.split(',').filter_map(|t| t.trim().parse().ok()).collect();
        (v.len() == 4).then(|| dehaze::Region {
            x0: v[0],
            y0: v[1],
            x1: v[2],
            y1: v[3],
        })
    });
    // 保護色以環境變數帶入：SMOKE_PROTECT=r,g,b（0~255）、SMOKE_TOL=容差
    let protect = std::env::var("SMOKE_PROTECT").ok().and_then(|s| {
        let v: Vec<u8> = s.split(',').filter_map(|t| t.trim().parse().ok()).collect();
        (v.len() == 3).then(|| [v[0], v[1], v[2]])
    });
    let p = dehaze::SmokeParams {
        strength: a.get(3).and_then(|s| s.parse().ok()).unwrap_or(80),
        detail: a.get(4).and_then(|s| s.parse().ok()).unwrap_or(60),
        black: a.get(5).and_then(|s| s.parse().ok()).unwrap_or(30),
        region,
        protect,
        tolerance: std::env::var("SMOKE_TOL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30),
        ..Default::default()
    };
    let t0 = std::time::Instant::now();
    let img = image::open(&a[1]).expect("讀取失敗").to_rgb8();
    println!("輸入 {}x{}，參數 {:?}", img.width(), img.height(), p);
    if std::env::var("SMOKE_DEBUG").is_ok() {
        let (layer, air) = dehaze::debug_smoke_layer(&img, p);
        println!("煙霧最濃處（線性 RGB）＝{air:?}");
        tm_save(&layer, &a[2]);
    }
    let out = if std::env::var("SMOKE_MASK").is_ok() {
        // 遮色片檢視：紅色蓋住的地方不會被去煙
        dehaze::mask_overlay(&img, p)
    } else {
        dehaze::remove_smoke(&img, p)
    };
    out.save(&a[2]).expect("寫出失敗");
    println!("完成 → {} （{:?}）", a[2], t0.elapsed());
}
