fn main() {
    // 视觉改造（像素风）：注册 @slint_pixel 库路径（slint-pixel path dep 的 library_paths）
    let config = slint_build::CompilerConfiguration::new()
        .with_library_paths(slint_pixel::library_paths());
    slint_build::compile_with_config("ui/app.slint", config).expect("slint compile failed");

    // Windows：把 ABB 图标内嵌进 exe（快捷方式/任务栏/窗口/资源管理器都显示图标）。
    // 用 installer/ABB.ico（由 app-assets/icon-1024.png 生成的多尺寸 ico）。
    #[cfg(target_os = "windows")]
    {
        // 图标文件变化时强制重编（winres 在构建期读它）
        println!("cargo:rerun-if-changed=installer/ABB.ico");
        if std::path::Path::new("installer/ABB.ico").exists() {
            let mut res = winres::WindowsResource::new();
            res.set_icon("installer/ABB.ico");
            res.compile().expect("winres 编译图标资源失败");
        }
    }
}
