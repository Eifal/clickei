fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        let mut res = winres::WindowsResource::new();
        res.set_icon("src/icons/clickei.ico");
        res.set("ProductName", "Clickei");
        res.set("FileDescription", "Clickei — macro recorder & auto-clicker");
        res.compile().expect("failed to embed exe icon/resources");
    }
}
