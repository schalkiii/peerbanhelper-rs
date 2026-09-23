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
    let mut args = Args {
        pbh_path: "target/release/pbh.exe".to_string(),
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
        // 默认：pbh 可执行文件同级的 data 目录（repo 布局为 ../../data）
        let exe = std::env::current_exe().unwrap_or_default();
        args.data_dir = exe
            .parent()
            .and_then(|p| p.join("../../data").canonicalize().ok())
            .map(|p| p.to_string_lossy().into_owned())
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
                    let exited = child
                        .0
                        .lock()
                        .unwrap()
                        .as_mut()
                        .map(|c| c.try_wait().map(|s| s.is_some()).unwrap_or(false))
                        .unwrap_or(true);
                    if exited {
                        eprintln!("[pbh-gui] pbh 进程退出，5 秒后重启");
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
            std::thread::spawn(move || {
                for _ in 0..30 {
                    if port_open(port) {
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                let _ = win.show();
                let _ = win.set_focus();
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running pbh-gui");
}
