#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod backend;
mod updater;
use anyhow::{Context, Result, ensure};
use backend::*;
use serde_json::{Value, json};
use std::os::windows::{
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::CommandExt,
};
use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tao::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    window::WindowBuilder,
};
use tray_icon::{
    MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem},
};
use wry::{MemoryUsageLevel, WebView, WebViewBuilder, WebViewExtWindows};

#[derive(Clone)]
enum Message {
    Command(Value),
    Ui(Value),
    Tray(TrayIconEvent),
    TrayMenu(MenuEvent),
    Restore,
    UpdateChecked(std::result::Result<Option<updater::Release>, String>),
    UpdateReady(std::result::Result<updater::Prepared, String>),
}
struct App {
    root: PathBuf,
    config: Config,
    profiles: Vec<Profile>,
    jobs: HashMap<String, Arc<AtomicBool>>,
    exit_requested: bool,
    available_update: Option<updater::Release>,
    update_check_started: bool,
    updating: bool,
}
impl App {
    fn open(root: PathBuf) -> Result<Self> {
        let path = root.join("config.json");
        let config: Config = if path.exists() {
            serde_json::from_slice(&fs::read(path)?).context("Invalid config.json")?
        } else {
            let c = Config::default();
            atomic_json(&path, &c)?;
            c
        };
        config.validate()?;
        for location in [&config.backup_root, &config.log_directory] {
            fs::create_dir_all(root_path(&root, location))?;
        }
        let path = root.join("profiles.json");
        let profiles = if path.exists() {
            serde_json::from_slice(&fs::read(path)?).context("Invalid profiles.json")?
        } else {
            let p: Vec<Profile> = vec![];
            atomic_json(&path, &p)?;
            p
        };
        audit(&root, &config, "User opened FileBackup v0.3.2 alpha")?;
        Ok(Self {
            root,
            config,
            profiles,
            jobs: HashMap::new(),
            exit_requested: false,
            available_update: None,
            update_check_started: false,
            updating: false,
        })
    }
    fn request_exit(&mut self) {
        self.exit_requested = true;
        for cancel in self.jobs.values() {
            cancel.store(true, Ordering::Relaxed);
        }
    }
    fn snapshot(&self) -> Result<Value> {
        Ok(
            json!({"type":"snapshot","config":self.config,"backupLocation":root_path(&self.root,&self.config.backup_root),"profiles":self.profiles,"backups":list_backups(&self.root,&self.config)?}),
        )
    }
    fn action(&mut self, v: Value, proxy: &EventLoopProxy<Message>) -> Result<Option<Value>> {
        let action = v["action"].as_str().unwrap_or("");
        let data = &v["data"];
        ensure!(
            !self.updating || matches!(action, "ready" | "refresh" | "openUpdateRelease"),
            "Wait for the update to finish."
        );
        match action {
            "ready" | "refresh" => {
                if !self.update_check_started {
                    self.update_check_started = true;
                    let proxy = proxy.clone();
                    std::thread::spawn(move || {
                        let outcome = updater::check().map_err(|e| format!("{e:#}"));
                        let _ = proxy.send_event(Message::UpdateChecked(outcome));
                    });
                }
                return Ok(Some(self.snapshot()?));
            }
            "openUpdateRelease" => {
                Command::new("explorer.exe")
                    .arg(updater::RELEASE_PAGE)
                    .creation_flags(0x08000000)
                    .spawn()?;
                return Ok(None);
            }
            "performUpdate" => {
                ensure!(
                    self.jobs.is_empty(),
                    "Wait for running backups before updating."
                );
                ensure!(!self.exit_requested, "FileBackup is shutting down.");
                let release = self
                    .available_update
                    .clone()
                    .context("No newer release is available")?;
                let target = std::env::current_exe()?;
                let proxy = proxy.clone();
                audit(
                    &self.root,
                    &self.config,
                    &format!("User requested update to {}", release.tag_name),
                )?;
                self.updating = true;
                std::thread::spawn(move || {
                    let result = updater::prepare(&release, target).map_err(|e| format!("{e:#}"));
                    let _ = proxy.send_event(Message::UpdateReady(result));
                });
                return Ok(Some(
                    json!({"type":"update","version":release_tag(&self.available_update),"busy":true}),
                ));
            }
            "pick" => {
                let folder = data["folder"].as_bool().unwrap_or(false);
                let picker = rfd::FileDialog::new().set_title(if folder {
                    "Choose folder"
                } else {
                    "Choose file"
                });
                let path = if folder {
                    picker.pick_folder()
                } else {
                    picker.pick_file()
                };
                return Ok(path.map(|p| json!({"type":"picked","field":data["field"],"path":p})));
            }
            "saveSettings" => {
                ensure!(
                    self.jobs.is_empty(),
                    "Wait for running backups before changing settings."
                );
                let config: Config = serde_json::from_value(data.clone())?;
                config.validate()?;
                for location in [&config.backup_root, &config.log_directory] {
                    fs::create_dir_all(root_path(&self.root, location))?;
                }
                let changes = self.config.changes(&config)?;
                let previous = self.config.clone();
                atomic_json(&self.root.join("config.json"), &config)?;
                self.config = config;
                audit(&self.root, &previous, &changes)
                    .context("Settings saved, but their changes could not be logged")?;
            }
            "saveProfile" => {
                let mut profile: Profile = serde_json::from_value(data.clone())?;
                valid_name(&date_name(&profile.name)?)?;
                ensure!(
                    std::path::Path::new(&profile.source).exists(),
                    "Select an existing file or folder."
                );
                ensure!(
                    !self
                        .profiles
                        .iter()
                        .any(|p| p.id != profile.id && p.name.eq_ignore_ascii_case(&profile.name)),
                    "A profile with this name already exists."
                );
                let creating = profile.id.is_empty();
                let mut profiles = self.profiles.clone();
                if profile.id.is_empty() {
                    profile.id = id();
                    profiles.push(profile.clone());
                } else {
                    let old = profiles
                        .iter_mut()
                        .find(|p| p.id == profile.id)
                        .context("Profile no longer exists")?;
                    *old = profile.clone();
                }
                atomic_json(&self.root.join("profiles.json"), &profiles)?;
                self.profiles = profiles;
                audit(
                    &self.root,
                    &self.config,
                    &format!(
                        "User {} profile \"{}\"",
                        if creating { "created" } else { "edited" },
                        profile.name
                    ),
                )?;
            }
            "deleteProfile" => {
                ensure!(data["confirmed"] == true, "Deletion requires confirmation.");
                let profile_id = data["id"].as_str().context("Missing profile")?;
                let profile = self
                    .profiles
                    .iter()
                    .find(|p| p.id == profile_id)
                    .context("Profile no longer exists")?
                    .clone();
                let profiles: Vec<_> = self
                    .profiles
                    .iter()
                    .filter(|p| p.id != profile_id)
                    .cloned()
                    .collect();
                atomic_json(&self.root.join("profiles.json"), &profiles)?;
                self.profiles = profiles;
                audit(
                    &self.root,
                    &self.config,
                    &format!("User deleted profile \"{}\"", profile.name),
                )?;
            }
            "openBackup" => {
                let name = data["name"].as_str().context("Missing backup")?;
                let path = backup_path(&self.root, &self.config, name)?;
                Command::new("explorer.exe")
                    .arg(path)
                    .creation_flags(0x08000000)
                    .spawn()?;
                return Ok(None);
            }
            "renameBackup" => {
                let old = data["old"].as_str().context("Missing backup")?;
                let new = data["name"].as_str().context("Missing name")?;
                valid_name(new)?;
                let from = backup_path(&self.root, &self.config, old)?;
                let new = if from.is_file() && !new.to_lowercase().ends_with(".zip") {
                    format!("{new}.zip")
                } else {
                    new.to_owned()
                };
                let to = from.parent().unwrap().join(&new);
                ensure!(!to.exists(), "That name already exists.");
                let _lock = lock_destination(&from)?;
                let _target = lock_destination(&to)?;
                fs::rename(&from, &to)?;
                audit(
                    &self.root,
                    &self.config,
                    &format!("User renamed backup \"{old}\" to \"{new}\""),
                )?;
            }
            "deleteBackup" => {
                ensure!(data["confirmed"] == true, "Deletion requires confirmation.");
                let name = data["name"].as_str().context("Missing backup")?;
                let path = backup_path(&self.root, &self.config, name)?;
                let _lock = lock_destination(&path)?;
                if path.is_dir() {
                    fs::remove_dir_all(&path)?;
                } else {
                    fs::remove_file(&path)?;
                }
                audit(
                    &self.root,
                    &self.config,
                    &format!("User deleted backup \"{name}\""),
                )?;
            }
            "exit" => {
                ensure!(data["confirmed"] == true, "Exit requires confirmation.");
                self.request_exit();
                return Ok(None);
            }
            "start" => {
                ensure!(!self.exit_requested, "FileBackup is shutting down.");
                let request: Request = serde_json::from_value(data.clone())?;
                valid_name(&date_name(&request.name)?)?;
                let job_id = id();
                let cancel = Arc::new(AtomicBool::new(false));
                self.jobs.insert(job_id.clone(), cancel.clone());
                let root = self.root.clone();
                let config = self.config.clone();
                let proxy = proxy.clone();
                let initial = json!({"type":"job","id":job_id,"name":request.name,"state":"running","percent":0,"detail":"Scanning files"});
                std::thread::spawn(move || {
                    let requested_name = request.name.clone();
                    let outcome = prepare(&root, &config, request)
                        .inspect_err(|error| {
                            let _ = audit(
                                &root,
                                &config,
                                &format!("Backup \"{requested_name}\" could not start: {error:#}"),
                            );
                        })
                        .and_then(|p| {
                            execute(p, &root, &config, cancel.clone(), |mut data| {
                                data["type"] = json!("job");
                                data["id"] = json!(job_id);
                                data["state"] = json!("running");
                                let _ = proxy.send_event(Message::Ui(data));
                            })
                        });
                    let (state, detail) = match outcome {
                        Ok(()) => ("complete", "Backup complete".into()),
                        Err(e) => (
                            if cancel.load(Ordering::Relaxed) {
                                "cancelled"
                            } else {
                                "failed"
                            },
                            format!("{e:#}"),
                        ),
                    };
                    let _ = proxy.send_event(Message::Ui(
                        json!({"type":"finished","id":job_id,"state":state,"detail":detail}),
                    ));
                });
                return Ok(Some(initial));
            }
            "cancel" => {
                let job_id = data["id"].as_str().context("Missing job")?;
                if let Some(cancel) = self.jobs.get(job_id) {
                    cancel.store(true, Ordering::Relaxed);
                }
                return Ok(None);
            }
            _ => anyhow::bail!("Unknown action"),
        }
        let mut snapshot = self.snapshot()?;
        snapshot["closeModal"] = json!(true);
        Ok(Some(snapshot))
    }
}
fn release_tag(release: &Option<updater::Release>) -> Option<&str> {
    release.as_ref().map(|r| r.tag_name.as_str())
}
fn background_notification(
    hwnd: windows_sys::Win32::Foundation::HWND,
    guid: u128,
) -> windows_sys::Win32::UI::Shell::NOTIFYICONDATAW {
    use windows_sys::Win32::UI::Shell::{NIF_GUID, NIF_INFO, NIIF_INFO, NOTIFYICONDATAW};
    let mut notification = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        guidItem: windows_sys::core::GUID::from_u128(guid),
        uFlags: NIF_GUID | NIF_INFO,
        dwInfoFlags: NIIF_INFO,
        ..Default::default()
    };
    for (slot, unit) in notification
        .szInfoTitle
        .iter_mut()
        .zip("FileBackup".encode_utf16())
    {
        *slot = unit;
    }
    for (slot, unit) in notification.szInfo.iter_mut().zip(
        "FileBackup is still running in the background. Click its tray icon to reopen it."
            .encode_utf16(),
    ) {
        *slot = unit;
    }
    notification
}
// Named kernel handles are released automatically, including after a crash.
fn instance_handles(name: &str) -> Result<(OwnedHandle, OwnedHandle, bool)> {
    use windows_sys::Win32::{
        Foundation::{ERROR_ALREADY_EXISTS, GetLastError},
        System::Threading::{CreateEventW, CreateMutexW},
    };
    let event_name: Vec<u16> = format!("{name}.Restore")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mutex_name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    // Create the event first so a second launch can signal during initial startup.
    let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr()) };
    if event.is_null() {
        return Err(std::io::Error::last_os_error()).context("Could not create restore event");
    }
    let event = unsafe { OwnedHandle::from_raw_handle(event) };
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr()) };
    let error = unsafe { GetLastError() };
    if mutex.is_null() {
        return Err(std::io::Error::from_raw_os_error(error as i32))
            .context("Could not create instance guard");
    }
    let mutex = unsafe { OwnedHandle::from_raw_handle(mutex) };
    Ok((mutex, event, error == ERROR_ALREADY_EXISTS))
}
fn restore_window(window: &tao::window::Window, webview: &WebView) {
    let _ = webview.set_memory_usage_level(MemoryUsageLevel::Normal);
    let _ = webview.set_visible(true);
    window.set_visible(true);
    window.set_minimized(false);
    window.set_focus();
}
fn launch() -> Result<()> {
    let (instance_guard, restore_event, already_running) =
        instance_handles("Local\\FileBackup.App")?;
    if already_running {
        use windows_sys::Win32::{
            System::Threading::SetEvent,
            UI::WindowsAndMessaging::{
                AllowSetForegroundWindow, FindWindowW, GetWindowThreadProcessId,
            },
        };
        let title: Vec<u16> = "FileBackup v0.3.2 alpha"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        unsafe {
            let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
            if !hwnd.is_null() {
                let mut pid = 0;
                GetWindowThreadProcessId(hwnd, &mut pid);
                if pid != 0 {
                    AllowSetForegroundWindow(pid);
                }
            }
            if SetEvent(restore_event.as_raw_handle()) == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("Could not restore FileBackup");
            }
        }
        return Ok(());
    }

    let root = std::env::current_exe()?
        .parent()
        .context("EXE has no folder")?
        .to_owned();
    let mut app = App::open(root.clone())?;
    updater::cleanup(&root);
    let event_loop = EventLoopBuilder::<Message>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let restore_proxy = proxy.clone();
    std::thread::Builder::new()
        .name("filebackup-restore".into())
        .spawn(move || {
            use windows_sys::Win32::{
                Foundation::WAIT_OBJECT_0,
                System::Threading::{INFINITE, WaitForSingleObject},
            };
            while unsafe { WaitForSingleObject(restore_event.as_raw_handle(), INFINITE) }
                == WAIT_OBJECT_0
            {
                if restore_proxy.send_event(Message::Restore).is_err() {
                    break;
                }
            }
        })?;

    let window = WindowBuilder::new()
        .with_title("FileBackup v0.3.2 alpha")
        .with_window_icon(Some(tao::window::Icon::from_rgba(
            include_bytes!("../assets/icon.rgba").to_vec(),
            64,
            64,
        )?))
        .with_inner_size(tao::dpi::LogicalSize::new(1160., 820.))
        .with_min_inner_size(tao::dpi::LogicalSize::new(760., 600.))
        .build(&event_loop)?;
    use tao::platform::windows::WindowExtWindows;
    use windows_sys::Win32::Graphics::Dwm::{
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmSetWindowAttribute,
    };
    let corners = DWMWCP_DONOTROUND;
    // The window owns this HWND; DWM reads the preference during this call.
    unsafe {
        let _ = DwmSetWindowAttribute(
            window.hwnd() as _,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&corners as *const i32).cast(),
            std::mem::size_of_val(&corners) as u32,
        );
    }
    let tray_menu = Menu::new();
    let open_item = MenuItem::new("Open FileBackup", true, None);
    let exit_item = MenuItem::new("Exit", true, None);
    tray_menu.append_items(&[&open_item, &exit_item])?;
    let tray_guid = 0xf11ebac0_0300_4000_8000_000000000000u128 | u128::from(std::process::id());
    let mut tray = Some(
        TrayIconBuilder::new()
            .with_guid(tray_guid)
            .with_tooltip("FileBackup")
            .with_icon(tray_icon::Icon::from_rgba(
                include_bytes!("../assets/icon.rgba").to_vec(),
                64,
                64,
            )?)
            .with_menu(Box::new(tray_menu))
            .with_menu_on_left_click(false)
            .build()?,
    );
    let tray_proxy = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = tray_proxy.send_event(Message::Tray(event));
    }));
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(Message::TrayMenu(event));
    }));
    let ipc_proxy = proxy.clone();
    let initial_page = AtomicBool::new(true);
    let mut context = wry::WebContext::new(Some(root.join(".webview")));
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_html(include_str!("ui.html"))
        .with_navigation_handler(move |url| {
            url == "about:blank"
                || (url.starts_with("data:text/html;charset=utf-8;base64,")
                    && initial_page.swap(false, Ordering::Relaxed))
        })
        .with_ipc_handler(move |request| {
            if let Ok(value) = serde_json::from_str(request.body()) {
                let _ = ipc_proxy.send_event(Message::Command(value));
            }
        })
        .build(&window)?;
    event_loop.run(move |event, _, control_flow| {
        let _ = &instance_guard;
        *control_flow = ControlFlow::Wait;
        let response = match event {
            Event::UserEvent(Message::Command(value))
                if matches!(value["action"].as_str(), Some("uiIdle" | "uiActive")) =>
            {
                let active = value["action"] == "uiActive" && window.is_visible() && !window.is_minimized();
                let _ = webview.set_memory_usage_level(if active {MemoryUsageLevel::Normal} else {MemoryUsageLevel::Low});
                None
            }
            Event::WindowEvent {event: WindowEvent::Focused(focused), ..} => {
                let active = focused && window.is_visible() && !window.is_minimized();
                let _ = webview.set_memory_usage_level(if active {MemoryUsageLevel::Normal} else {MemoryUsageLevel::Low});
                None
            }
            Event::WindowEvent {event: WindowEvent::Resized(_), ..} => {
                let visible = window.is_visible() && !window.is_minimized();
                let _ = webview.set_visible(visible);
                if !visible {let _ = webview.set_memory_usage_level(MemoryUsageLevel::Low);}
                None
            }
            Event::UserEvent(Message::Command(value)) => match app.action(value, &proxy) {
                Ok(v) => v,
                Err(e) => Some(json!({"type":"error","message":format!("{e:#}")})),
            },
            Event::UserEvent(Message::Ui(value)) => {
                if value["type"] == "finished" {
                    if let Some(id) = value["id"].as_str() {
                        app.jobs.remove(id);
                    }
                    if let Ok(snapshot) = app.snapshot() {
                        let _ = webview.evaluate_script(&format!("receive({snapshot})"));
                    }
                }
                Some(value)
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if app.updating || app.config.close_to_tray {
                    window.set_visible(false);
                    let _ = webview.set_visible(false);
                    let _ = webview.set_memory_usage_level(MemoryUsageLevel::Low);
                    if let Some(tray) = &tray
                        && !app.updating && app.config.tray_notifications_enabled
                    {
                        let notification = background_notification(tray.window_handle(), tray_guid);
                        // The tray owns the HWND and GUID; Windows copies this fixed-size payload.
                        if unsafe {
                            windows_sys::Win32::UI::Shell::Shell_NotifyIconW(
                                windows_sys::Win32::UI::Shell::NIM_MODIFY,
                                &notification,
                            )
                        } == 0
                        {
                            let _ = audit(
                                &app.root,
                                &app.config,
                                "Windows could not show the background notification",
                            );
                        }
                    }
                } else if app.jobs.is_empty() {
                    app.request_exit();
                } else {
                    let _ = webview.evaluate_script("confirmExit()");
                }
                None
            }
            Event::UserEvent(Message::Tray(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }))
            | Event::UserEvent(Message::Tray(TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            })) => {
                restore_window(&window, &webview);
                None
            }
            Event::UserEvent(Message::UpdateChecked(result)) => {
                match result {
                    Ok(Some(release)) => {
                        let version = release.tag_name.clone();
                        app.available_update = Some(release);
                        Some(json!({"type":"update","version":version,"busy":false}))
                    }
                    Ok(None) => None,
                    Err(error) => {
                        let _ = audit(&app.root,&app.config,&format!("GitHub update check failed: {error}"));
                        None
                    }
                }
            }
            Event::UserEvent(Message::UpdateReady(result)) => {
                let result = result.map_err(anyhow::Error::msg).and_then(|prepared| {
                        audit(&app.root,&app.config,"Update verified; restarting FileBackup")?;
                        updater::launch_helper(&prepared)
                    });
                match result {
                    Ok(()) => {app.exit_requested = true;None}
                    Err(error) => {
                        app.updating = false;
                        let message = format!("Update failed: {error:#}");
                        let _ = audit(&app.root,&app.config,&message);
                        Some(json!({"type":"update","version":release_tag(&app.available_update),"busy":false,"error":message}))
                    }
                }
            }
            Event::UserEvent(Message::Restore) => {
                restore_window(&window, &webview);
                None
            }
            Event::UserEvent(Message::TrayMenu(event)) => {
                if event.id == *open_item.id() {
                    restore_window(&window, &webview);
                } else if event.id == *exit_item.id() && !app.updating {
                    if app.jobs.is_empty() {
                        app.request_exit();
                    } else {
                        restore_window(&window, &webview);
                        let _ = webview.evaluate_script("confirmExit()");
                    }
                }
                None
            }
            Event::LoopDestroyed => {
                tray.take();
                None
            }
            _ => None,
        };
        // Wait for worker cleanup before exiting, preserving ZIPs and destination locks.
        if app.exit_requested && app.jobs.is_empty() {
            *control_flow = ControlFlow::Exit;
        }
        if let Some(value) = response {
            let _ = webview.evaluate_script(&format!("receive({value})"));
        }
    });
}
fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let installing = args.get(1).is_some_and(|a| a == "--apply-update");
    let result = if installing {
        updater::apply(&args)
    } else {
        launch()
    };
    if let Err(error) = result {
        rfd::MessageDialog::new()
            .set_title(if installing {
                "FileBackup update failed"
            } else {
                "FileBackup could not start"
            })
            .set_description(format!("{error:#}"))
            .set_level(rfd::MessageLevel::Error)
            .show();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tao::platform::windows::EventLoopBuilderExtWindows;
    #[test]
    fn single_instance_signals_existing_process_and_releases_on_exit() {
        use windows_sys::Win32::{
            Foundation::WAIT_OBJECT_0,
            System::Threading::{SetEvent, WaitForSingleObject},
        };
        if let Ok(name) = std::env::var("FILEBACKUP_INSTANCE_TEST") {
            let (_guard, event, existing) = instance_handles(&name).unwrap();
            assert!(existing, "Second process must not start another app");
            assert_ne!(unsafe { SetEvent(event.as_raw_handle()) }, 0);
            return;
        }
        let name = format!("Local\\FileBackup.Test.{}", id());
        let (guard, event, existing) = instance_handles(&name).unwrap();
        assert!(!existing);
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::single_instance_signals_existing_process_and_releases_on_exit",
            ])
            .env("FILEBACKUP_INSTANCE_TEST", &name)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{}",
            String::from_utf8_lossy(&child.stdout)
        );
        assert_eq!(
            unsafe { WaitForSingleObject(event.as_raw_handle(), 1000) },
            WAIT_OBJECT_0
        );
        drop(guard);
        drop(event);
        let (_guard, _event, existing) = instance_handles(&name).unwrap();
        assert!(!existing, "Closing the owner must allow a fresh launch");
    }
    #[test]
    fn tray_setting_is_backward_compatible_and_notification_targets_the_icon() {
        let legacy: Config = serde_json::from_value(json!({})).unwrap();
        assert!(legacy.close_to_tray);
        assert!(legacy.tray_notifications_enabled);
        let disabled: Config = serde_json::from_value(json!({"CloseToTray":false})).unwrap();
        assert!(!disabled.close_to_tray);
        assert_eq!(
            serde_json::to_value(disabled).unwrap()["CloseToTray"],
            false
        );
        let guid = 0xf11ebac0_0300_4000_8000_000000000001;
        let notification = background_notification(std::ptr::null_mut(), guid);
        assert_eq!(
            notification.guidItem.data1,
            windows_sys::core::GUID::from_u128(guid).data1
        );
        assert_eq!(
            notification.guidItem.data4,
            windows_sys::core::GUID::from_u128(guid).data4
        );
        assert_eq!(
            notification.uFlags,
            windows_sys::Win32::UI::Shell::NIF_GUID | windows_sys::Win32::UI::Shell::NIF_INFO
        );
        let message: Vec<_> = notification
            .szInfo
            .iter()
            .copied()
            .take_while(|&c| c != 0)
            .collect();
        assert!(
            String::from_utf16(&message)
                .unwrap()
                .contains("still running in the background")
        );
    }
    #[test]
    fn exit_cancels_workers_and_waits_for_cleanup() {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut app = App {
            root: PathBuf::new(),
            config: Config::default(),
            profiles: vec![],
            jobs: HashMap::from([("running".into(), cancel.clone())]),
            exit_requested: false,
            available_update: None,
            update_check_started: false,
            updating: false,
        };
        assert!(!cancel.load(Ordering::Relaxed));
        app.request_exit();
        assert!(app.exit_requested);
        assert!(cancel.load(Ordering::Relaxed));
        assert!(
            !app.jobs.is_empty(),
            "Exiting must wait for worker completion"
        );
        app.jobs.remove("running");
        assert!(app.exit_requested && app.jobs.is_empty());
    }
    #[test]
    fn profile_settings_and_backup_actions_persist() {
        let root = std::env::temp_dir().join(format!("filebackup-v0.3-actions-{}", id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("source.txt"), "sample").unwrap();
        let mut app = App::open(root.clone()).unwrap();
        for file in ["config.json", "profiles.json"] {
            assert!(root.join(file).is_file(), "Startup must create {file}");
        }
        for folder in [".backups", ".logs"] {
            assert!(root.join(folder).is_dir(), "Startup must create {folder}");
        }
        assert_eq!(fs::read_dir(root.join(".logs")).unwrap().count(), 1);
        let event_loop = EventLoopBuilder::<Message>::with_user_event()
            .with_any_thread(true)
            .build();
        let proxy = event_loop.create_proxy();
        app.action(json!({"action":"saveProfile","data":{"id":"","name":"School","source":root.join("source.txt"),"zip":false}}),&proxy).unwrap();
        let profile = app.profiles[0].clone();
        let mut changed = profile.clone();
        changed.name = "Documents".into();
        changed.zip = true;
        app.action(json!({"action":"saveProfile","data":changed}), &proxy)
            .unwrap();
        let persisted: Vec<Profile> =
            serde_json::from_slice(&fs::read(root.join("profiles.json")).unwrap()).unwrap();
        assert_eq!(persisted[0].name, "Documents");
        assert!(persisted[0].zip);
        let mut config = app.config.clone();
        config.compression_level = 5;
        config.close_to_tray = false;
        config.tray_notifications_enabled = false;
        config.completed_status_dismiss_seconds = 7;
        app.action(json!({"action":"saveSettings","data":config}), &proxy)
            .unwrap();
        let config: Config =
            serde_json::from_slice(&fs::read(root.join("config.json")).unwrap()).unwrap();
        assert_eq!(config.compression_level, 5);
        assert!(!config.close_to_tray);
        app.action(
            json!({"action":"saveSettings","data":config.clone()}),
            &proxy,
        )
        .unwrap();
        assert!(!config.tray_notifications_enabled);
        assert_eq!(config.completed_status_dismiss_seconds, 7);
        fs::create_dir_all(root.join(".backups/Original")).unwrap();
        app.action(
            json!({"action":"renameBackup","data":{"old":"Original","name":"Renamed"}}),
            &proxy,
        )
        .unwrap();
        assert!(root.join(".backups/Renamed").exists());
        assert!(
            app.action(
                json!({"action":"deleteBackup","data":{"name":"Renamed","confirmed":false}}),
                &proxy
            )
            .is_err()
        );
        app.action(
            json!({"action":"deleteBackup","data":{"name":"Renamed","confirmed":true}}),
            &proxy,
        )
        .unwrap();
        app.action(
            json!({"action":"deleteProfile","data":{"id":profile.id,"confirmed":true}}),
            &proxy,
        )
        .unwrap();
        assert!(app.profiles.is_empty());
        assert!(!root.join(".backups/Renamed").exists());
        let mut log = String::new();
        let log_file = fs::read_dir(root.join(".logs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        use std::io::Read;
        flate2::read::MultiGzDecoder::new(fs::File::open(log_file).unwrap())
            .read_to_string(&mut log)
            .unwrap();
        for expected in [
            "User changed setting CompressionLevel: 6 -> 5",
            "User changed setting CloseToTray: true -> false",
            "User changed setting TrayNotificationsEnabled: true -> false",
            "User changed setting CompletedStatusDismissSeconds: 5 -> 7",
            "User saved settings (no changes)",
            "User created profile \"School\"",
            "User edited profile \"Documents\"",
            "User deleted profile \"Documents\"",
            "User renamed backup \"Original\" to \"Renamed\"",
            "User deleted backup \"Renamed\"",
        ] {
            assert!(log.contains(expected), "Missing event: {expected}");
        }

        assert!(!log.contains("User changed setting BackupRoot"));
        assert!(!log.contains("User changed setting LogCompressionLevel"));
        fs::remove_dir_all(root).unwrap();
    }
}
