fn main() {
    // 把 assets/icon.ico 嵌入 Windows 執行檔（檔案總管與工作列圖示）
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.compile().expect("嵌入應用程式圖示失敗");
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
