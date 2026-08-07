fn main() {
    slint_build::compile("ui/app.slint").expect("slint compile failed");

    // Windows：把 ABB 图标内嵌进 exe（快捷方式/任务栏/窗口/资源管理器都显示图标）。
    // 用 installer/ABB.ico（由 app-assets/icon-1024.png 生成的多尺寸 ico）。
    #[cfg(target_os = "windows")]
    {
        if std::path::Path::new("installer/ABB.ico").exists() {
            let mut res = winres::WindowsResource::new();
            res.set_icon("installer/ABB.ico");
            res.compile().expect("winres 编译图标资源失败");
        }
    }
}
