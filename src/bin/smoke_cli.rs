//! 去煙演算法的參數實驗工具：
//! cargo run --release --bin smoke_cli -- <輸入> <輸出> [strength] [detail] [black]

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
    let p = dehaze::SmokeParams {
        strength: a.get(3).and_then(|s| s.parse().ok()).unwrap_or(80),
        detail: a.get(4).and_then(|s| s.parse().ok()).unwrap_or(60),
        black: a.get(5).and_then(|s| s.parse().ok()).unwrap_or(30),
    };
    let t0 = std::time::Instant::now();
    let img = image::open(&a[1]).expect("讀取失敗").to_rgb8();
    println!("輸入 {}x{}，參數 {:?}", img.width(), img.height(), p);
    if std::env::var("SMOKE_DEBUG").is_ok() {
        let (layer, air) = dehaze::debug_smoke_layer(&img, p);
        println!("煙霧最濃處（線性 RGB）＝{air:?}");
        tm_save(&layer, &a[2]);
    }
    let out = dehaze::remove_smoke(&img, p);
    out.save(&a[2]).expect("寫出失敗");
    println!("完成 → {} （{:?}）", a[2], t0.elapsed());
}
