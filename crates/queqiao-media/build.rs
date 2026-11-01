// 编译 vendor 的 Skype SILK C 源（固定点解码器）——rust-silk 项目验证过的
// 方案（MSVC cc 可编译、零 bindgen；arm 汇编 .S 不收，x64 走纯 C 路径）。
// 许可：源文件头保留 Skype Limited 2006-2012 BSD 风格声明（再分发须保留）。
use std::fs;
use std::path::{Path, PathBuf};

fn collect_c_sources(dir: &Path) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("c") {
            sources.push(path);
        }
    }
    sources.sort();
    sources
}

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = root.join("vendor").join("silk");
    let src_dir = vendor.join("src");
    let include_dir = vendor.join("interface");

    println!("cargo:rerun-if-changed={}", src_dir.display());
    println!("cargo:rerun-if-changed={}", include_dir.display());

    let sources = collect_c_sources(&src_dir);
    assert!(!sources.is_empty(), "SILK C 源缺失：{}", src_dir.display());

    let mut build = cc::Build::new();
    build
        .files(sources)
        .include(&include_dir)
        .include(&src_dir)
        .flag_if_supported("-O3")
        .flag_if_supported("-w")
        .flag_if_supported("/W0");
    build.compile("silk_sdk");
}
