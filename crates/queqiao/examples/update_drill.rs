//! 客户端自主更新链演练（临时 home，不碰真实安装目录）：
//! check_latest → 带进度下载 → 客户端版 apply bat → 执行 → 断言三件套
//! 换新与随包脚本补齐。上线前/改动更新链后各跑一次。
//! 用法：cargo run -p queqiao --example update_drill

use queqiao::update::{check_latest, stage_update_with_progress, write_apply_bat};
#[cfg(windows)]
use std::os::windows::process::CommandExt;

fn main() {
    let base = std::env::temp_dir().join(format!("qq-upd-drill-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    // 低版本视角确保发现 latest。
    let rel = check_latest("0.0.1")
        .expect("查询 latest 失败")
        .expect("无新版可演练");
    println!("发现 {}（发布于 {}）", rel.tag, rel.published_at);

    let last_pct = std::cell::Cell::new(-1i32);
    let staging = stage_update_with_progress(&rel, &base, |recv, total| {
        if let Some(pct) = (recv * 100).checked_div(total).map(|v| v as i32)
            && pct != last_pct.get()
        {
            last_pct.set(pct);
            print!(
                "\r下载进度 {pct}%（{} / {} MB）",
                recv / 1048576,
                total / 1048576
            );
        }
    })
    .expect("下载/解包失败");
    println!("\nstaging 就绪：{}", staging.display());

    // 放"旧版"占位三件套（内容标记 old），apply 后应全部变新。
    for name in ["queqiao.exe", "queqiao-mcp.exe", "queqiao-app.exe"] {
        std::fs::write(base.join(name), b"old").unwrap();
    }
    // 防拉起真客户端：staging 里删掉 app exe——bat 的 copy 分支跳过、
    // 占位被 ren 走后 start 目标不存在（cmd 对无效占位也不会拉起进程）。
    std::fs::remove_file(staging.join("queqiao-app.exe")).unwrap();
    let bat = write_apply_bat(&staging, &base, true).expect("写 apply bat 失败");
    println!("执行 apply bat …");
    // 不 wait bat：其 start 的子进程继承管道，等它会被拉起的进程挂住
    // （真机教训）；bat 自行收尾，这里睡 8s 后断言其效果。
    #[allow(clippy::zombie_processes)]
    let _bat_child = std::process::Command::new(&bat)
        .creation_flags(0x0800_0000)
        .spawn()
        .expect("跑 bat 失败");
    std::thread::sleep(std::time::Duration::from_secs(8));

    for name in ["queqiao.exe", "queqiao-mcp.exe"] {
        let content = std::fs::read(base.join(name)).unwrap();
        assert_ne!(content, b"old", "{name} 未被换新");
        println!("  {name} 已换新（{} 字节）", content.len());
    }
    // app：占位被 ren、copy 跳过——主文件不在、.old 在（链路两步都走到）。
    assert!(
        !base.join("queqiao-app.exe").exists(),
        "app 占位应被 ren 走"
    );
    assert!(base.join("queqiao-app.exe.old").exists(), "app .old 应存在");
    println!("  queqiao-app.exe 的 ren/条件拷贝分支均按预期执行");
    assert!(
        base.join("migrate-from-node.ps1").exists(),
        "迁移脚本未补齐"
    );
    assert!(!staging.exists(), "staging 未清理");
    println!("演练通过：三件套换链 + 脚本补齐 + staging 清理 ✓");
    let _ = std::fs::remove_dir_all(&base);
}
