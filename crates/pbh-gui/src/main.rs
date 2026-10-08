//! PeerBanHelper-RS 原生 GUI 壳（Tauri v2）。
//!
//! 职责（对齐 PLAN「原生 GUI（Tauri 托盘壳）」）：
//! 1. 以子进程拉起 `pbh`（崩溃自动重启；日志重定向到 `pbh-gui.log`）；
//! 2. 系统托盘：显示主窗口 / 在浏览器打开 WebUI / 退出（结束子进程）；
//! 3. WebView 指向 `http://127.0.0.1:<port>`（**复用上游 WebUI dist**，不重写前端）；
//! 4. 关闭窗口 = 隐藏到托盘（对齐 qBittorrent 习惯）；
//! 5. 单实例：若 `<port>` 已有服务监听 ⇒ 附加模式，不再拉起子进程。
//!
//! 用法：`pbh-gui [--pbh-path <pbh.exe>] [--data-dir <dir>] [--port 9898]`

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::net::TcpStream;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};

struct Args {
    pbh_path: String,
    data_dir: String,
    port: u16,
}

impl Args {
    fn clone_for_spawn(&self) -> Self {
        Self {
            pbh_path: self.pbh_path.clone(),
            data_dir: self.data_dir.clone(),
            port: self.port,
        }
    }
}

fn parse_args() -> Args {
    // 部署布局（pbh-gui.exe 与 pbh.exe/data 同目录）优先：双击启动（无参数）时
    // 拉起同目录的 pbh.exe；开发环境（repo 内运行）回退 target/release 布局。
    let exe = std::env::current_exe().ok();
    let exe_dir = exe.as_ref().and_then(|p| p.parent()).map(|p| p.to_path_buf());
    let default_pbh = exe_dir
        .as_ref()
        .map(|d| d.join("pbh.exe"))
        .filter(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "target/release/pbh.exe".to_string());
    let mut args = Args {
        pbh_path: default_pbh,
        data_dir: String::new(),
        port: 9898,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--pbh-path" => {
                if let Some(v) = iter.next() {
                    args.pbh_path = v;
                }
            }
            "--data-dir" => {
                if let Some(v) = iter.next() {
                    args.data_dir = v;
                }
            }
            "--port" => {
                if let Some(v) = iter.next() {
                    args.port = v.parse().unwrap_or(9898);
                }
            }
            _ => {}
        }
    }
    if args.data_dir.is_empty() {
        // 默认：exe 同级 data 目录（部署布局）；开发环境回退 repo 的 ../../data
        args.data_dir = exe_dir
            .as_ref()
            .map(|d| d.join("data"))
            .filter(|p| p.is_dir())
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| {
                exe.as_ref()
                    .and_then(|p| p.parent())
                    .and_then(|p| p.join("../../data").canonicalize().ok())
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "data".to_string());
    }
    args
}

/// 探测 `127.0.0.1:port` 是否可建立 TCP 连接（pbh 的 axum 监听即视为就绪）。
fn port_open(port: u16) -> bool {
    let addr = format!("127.0.0.1:{port}");
    match addr.parse() {
        Ok(addr) => TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok(),
        Err(_) => false,
    }
}

fn spawn_child(args: &Args) -> std::io::Result<Child> {
    let mut cmd = Command::new(&args.pbh_path);
    cmd.arg("--data").arg(&args.data_dir);
    // 日志重定向：GUI 是 Windows GUI 子系统进程（无控制台），不重定向时子进程
    // 的 stdout/stderr（tracing 全部输出）直接丢失。追加写入 data 目录的
    // pbh-gui.log，同时作为外部看门狗的日志心跳源（冻结诊断依赖）。
    let log_path = std::path::Path::new(&args.data_dir).join("pbh-gui.log");
    if let Ok(log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let log_err = log.try_clone()?;
        cmd.stdout(std::process::Stdio::from(log));
        cmd.stderr(std::process::Stdio::from(log_err));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn()
}

fn open_in_browser(url: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = Command::new("cmd")
            .args(["/c", "start", "", url])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
}

struct ChildHandle(Mutex<Option<Child>>);

impl ChildHandle {
    fn kill(&self) {
        if let Ok(mut slot) = self.0.lock() {
            if let Some(child) = slot.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            *slot = None;
        }
    }
}

fn main() {
    let args = parse_args();
    let url = format!("http://127.0.0.1:{}", args.port);

    // 附加模式：服务已在运行（上次 GUI 留下的子进程，或用户手动启动）⇒ 不再拉起
    let attach = port_open(args.port);
    let child = Arc::new(ChildHandle(Mutex::new(None)));
    if !attach {
        match spawn_child(&args) {
            Ok(c) => {
                *child.0.lock().unwrap() = Some(c);
                // 监督线程：崩溃自动重启（封禁/游标状态均在 DB，重启无损）
                let child = Arc::clone(&child);
                let spawn_args = args.clone_for_spawn();
                std::thread::spawn(move || loop {
                    std::thread::sleep(Duration::from_secs(2));
                    // 先取退出状态再判断：exit code 可区分正常退出（0）/ access
                    // violation（0xC0000005）/ 栈溢出（0xC00000FD）/ abort（0x80000003），
                    // 是「静默退出」排查的唯一线索（panic=abort 时 panic 消息可能丢失）
                    let status = child
                        .0
                        .lock()
                        .unwrap()
                        .as_mut()
                        .and_then(|c| c.try_wait().ok().flatten());
                    if let Some(s) = status {
                        let code = format!("{s:?}");
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        // GUI 自身无控制台（eprintln 丢失），重启记录写日志文件
                        let log_path = std::path::Path::new(&spawn_args.data_dir)
                            .join("pbh-gui.log");
                        if let Ok(mut f) =
                            std::fs::OpenOptions::new().create(true).append(true).open(&log_path)
                        {
                            use std::io::Write as _;
                            let _ = writeln!(
                                f,
                                "[pbh-gui] {ts} pbh 进程退出（{code}），5 秒后重启"
                            );
                        }
                        eprintln!("[pbh-gui] pbh 进程退出（{code}），5 秒后重启");
                        std::thread::sleep(Duration::from_secs(5));
                        if let Ok(c) = spawn_child(&spawn_args) {
                            *child.0.lock().unwrap() = Some(c);
                        }
                    }
                });
            }
            Err(e) => eprintln!("[pbh-gui] 拉起 pbh 失败: {e}（将以附加模式继续）"),
        }
    }

    tauri::Builder::default()
        .on_window_event(|window, event| {
            // 关闭窗口 = 隐藏到托盘（不退出进程）
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(move |app| {
            let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
            let open = MenuItem::with_id(app, "open", "在浏览器打开 WebUI", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出 PeerBanHelper", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &open, &quit])?;

            let handle: AppHandle = app.handle().clone();
            let child = Arc::clone(&child);
            let url_menu = url.clone();
            TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().expect("缺少窗口图标").clone())
                .tooltip("PeerBanHelper")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |_app, event| {
                    let win = handle.get_webview_window("main");
                    match event.id().as_ref() {
                        "show" => {
                            if let Some(w) = win {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                        "open" => open_in_browser(&url_menu),
                        "quit" => {
                            child.kill();
                            handle.exit(0);
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    // 左键单击托盘 = 显示主窗口
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        if let Some(w) = tray.app_handle().get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                })
                .build(app)?;

            // 等 pbh 就绪（最多 30 秒）再显示主窗口，避免看到连接错误页
            let port = args.port;
            let win = app.get_webview_window("main").expect("主窗口缺失");
            let silent_path = std::path::Path::new(&args.data_dir).join("silent_login_token");
            // GUI 无控制台：诊断日志写入 data/pbh-gui.log（静默登录失败会表现为骨架屏）
            let log_path = silent_path
                .parent()
                .map(|p| p.join("pbh-gui.log"));
            let gui_log = move |msg: &str| {
                if let Some(p) = &log_path {
                    if let Ok(mut f) =
                        std::fs::OpenOptions::new().create(true).append(true).open(p)
                    {
                        use std::io::Write as _;
                        let _ = writeln!(f, "[pbh-gui] {msg}");
                    }
                }
            };
            std::thread::spawn(move || {
                // 等 pbh 就绪：最多 120 秒（冷启动含 SQLite 迁移可能较慢）。
                // 窗口 visible=false 且未导航，就绪前用户不会看到连接错误页。
                let mut ready = false;
                for _ in 0..120 {
                    if port_open(port) {
                        ready = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                if !ready {
                    // 兜底：显示本地占位页（无网络依赖，避免白屏/错误页），
                    // 并后台持续重试——监督线程会不断拉起 pbh，就绪后自动切入
                    gui_log("等待 pbh 就绪超时（120s），显示占位页并后台重试");
                    const WAITING: &str = "data:text/html;charset=utf-8,%3Cmeta%20charset%3Dutf-8%3E%3Cbody%20style%3D'font-family:system-ui%3Bdisplay:flex%3Balign-items:center%3Bjustify-content:center%3Bheight:90vh'%3E%3Cdiv%20style%3D'text-align:center'%3E%3Ch2%3EPeerBanHelper%20%E6%AD%A3%E5%9C%A8%E5%90%AF%E5%8A%A8%E2%80%A6%3C%2Fh2%3E%3Cp%3E%E6%9C%8D%E5%8A%A1%E5%B0%B1%E7%BB%AA%E5%90%8E%E5%B0%86%E8%87%AA%E5%8A%A8%E8%BF%9B%E5%85%A5%20WebUI%3B%E8%8B%A5%E9%95%BF%E6%97%B6%E9%97%B4%E6%97%A0%E5%93%8D%E5%BA%94%EF%BC%8C%E8%AF%B7%E6%9F%A5%E7%9C%8B%20data%2Fpbh-gui.log%3C%2Fp%3E%3C%2Fdiv%3E%3Cscript%3EsetTimeout(()%3D%3Elocation.reload()%2C5000)%3C%2Fscript%3E%3C%2Fbody%3E";
                    if let Ok(u) = WAITING.parse() {
                        let _ = win.navigate(u);
                    }
                    let _ = win.show();
                    let _ = win.set_focus();
                    while !port_open(port) {
                        std::thread::sleep(Duration::from_secs(5));
                    }
                    gui_log("pbh 已就绪（后台重试成功），切换到 WebUI");
                }
                // 静默登录：导航到带 ?silentLogin= 的 URL（对齐上游 WebUITab 的 URL
                // 拼接 + Javalin accessManager 豁免）——middleware 校验通过即放行并
                // 种会话 cookie，GUI 内免输入 token，前端零感知
                match std::fs::read_to_string(&silent_path) {
                    Ok(tok) if !tok.trim().is_empty() => {
                        let target =
                            format!("http://127.0.0.1:{port}/?silentLogin={}", tok.trim());
                        match target.parse() {
                            Ok(u) => {
                                let result = win.navigate(u);
                                gui_log(&format!(
                                    "静默登录导航 → {target}（结果 {result:?}）"
                                ));
                            }
                            Err(e) => gui_log(&format!("静默登录 URL 解析失败: {e}")),
                        }
                    }
                    other => gui_log(&format!(
                        "silent_login_token 不可用（{:?}），跳过静默登录",
                        other.is_err()
                    )),
                }
                let _ = win.show();
                let _ = win.set_focus();
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running pbh-gui");
}
