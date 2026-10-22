fn main() {
    // tauri-build 硬校验 bundle.resources 存在；binaries/ 不入 git（本地真 exe
    // 大），CI 检查与首次克隆需占位。发布流程（release workflow）先拷真 exe
    // 再构建，占位被覆盖。
    for exe in ["queqiao.exe", "queqiao-mcp.exe"] {
        let p = std::path::Path::new("binaries").join(exe);
        if !p.exists() {
            std::fs::create_dir_all("binaries").ok();
            std::fs::write(&p, b"placeholder-for-build
").ok();
            println!("cargo:warning=binaries/{exe} 用占位文件（仅过 tauri 校验，不可运行）");
        }
    }
    tauri_build::build()
}
