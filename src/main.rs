#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Discord-Lite : le client web officiel de Discord dans une coquille WebView2 minimale.
//! Pas d'Electron, pas de Node, pas de module natif : un seul exe de quelques Mo,
//! la télémétrie coupée au niveau réseau et un moteur mis au régime dès que la
//! fenêtre n'est plus à l'écran.

mod system;
mod wv2;

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use serde::{Deserialize, Serialize};
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    platform::windows::{IconExtWindows, WindowExtWindows},
    window::{Icon, Theme, Window, WindowBuilder},
};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};
use wry::{MemoryUsageLevel, NewWindowResponse, WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows, WebViewExtWindows};

const HOME: &str = "https://discord.com/app";
const TRAY_RGBA: &[u8] = include_bytes!("../assets/tray.rgba");
const TRAY_BADGE_RGBA: &[u8] = include_bytes!("../assets/tray-badge.rgba");

/// Profil isolé pour les tests (`DISCORD_LITE_PROFILE=bench`) : autre dossier de
/// données, autre canal d'instance unique, jamais de lancement automatique.
pub fn profile_suffix() -> String {
    match std::env::var("DISCORD_LITE_PROFILE") {
        Ok(p) if !p.trim().is_empty() => format!("-{}", p.trim()),
        _ => String::new(),
    }
}

fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join(format!("Discord-Lite{}", profile_suffix()))
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Settings {
    /// Animations réduites, processus de rendu limités, V8 optimisé pour la mémoire.
    eco: bool,
    block_telemetry: bool,
    autostart: bool,
    close_to_tray: bool,
    hw_accel: bool,
    width: f64,
    height: f64,
    maximized: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            eco: true,
            block_telemetry: true,
            autostart: false,
            close_to_tray: true,
            hw_accel: true,
            width: 1280.0,
            height: 800.0,
            maximized: false,
        }
    }
}

impl Settings {
    fn path() -> PathBuf {
        data_dir().join("settings.json")
    }
    fn load() -> Self {
        std::fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    fn save(&self) {
        let _ = std::fs::create_dir_all(data_dir());
        if let Ok(b) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(Self::path(), b);
        }
    }
}

/// Options du moteur Chromium : tout ce qui tourne en fond sans servir à Discord est coupé.
fn browser_args(s: &Settings) -> String {
    let mut disabled = vec![
        "msWebOOUI",
        "msPdfOOUI",
        "msSmartScreenProtection",
        "Translate",
        "OptimizationHints",
        "MediaRouter",
        "AutofillServerCommunication",
        // Chromium garde un processus de rendu vide « au cas où » : ~20 Mo pour rien.
        "SpareRendererForSitePerProcess",
    ];
    if s.eco {
        disabled.push("BackForwardCache");
    }
    let mut args = format!(
        "--disable-features={} --process-per-site --disable-background-networking --no-pings \
         --disable-domain-reliability --disable-breakpad --autoplay-policy=no-user-gesture-required \
         --js-flags=--optimize-for-size",
        disabled.join(",")
    );
    if s.eco {
        // Discord suit le réglage « réduire les animations » du système : avatars, GIF et
        // transitions ne tournent plus en boucle, c'est le premier poste de CPU au repos.
        args.push_str(" --force-prefers-reduced-motion --renderer-process-limit=2");
    }
    if !s.hw_accel {
        args.push_str(" --disable-gpu --disable-gpu-compositing");
    }
    if let Ok(port) = std::env::var("DISCORD_LITE_CDP") {
        args.push_str(&format!(" --remote-debugging-port={}", port.trim()));
    }
    args
}

enum UserEvent {
    TrayClick,
    Menu(String),
    Command(String),
    Title(String),
    Blocked,
    Notified,
}

struct MenuItems {
    eco: CheckMenuItem,
    telemetry: CheckMenuItem,
    autostart: CheckMenuItem,
    close_to_tray: CheckMenuItem,
    hw: CheckMenuItem,
    stats: MenuItem,
}

struct App {
    window: Window,
    view: Option<WebView>,
    settings: Settings,
    tray: Option<TrayIcon>,
    items: MenuItems,
    blocking: Arc<AtomicBool>,
    blocked: Arc<AtomicU64>,
    shown_blocked: u64,
    mentions: u32,
    badge: bool,
    focused: bool,
    hidden: bool,
}

impl App {
    fn show(&mut self) {
        self.window.set_visible(true);
        self.window.set_minimized(false);
        self.window.set_focus();
        self.set_hidden(false);
    }

    fn hide(&mut self) {
        self.window.set_visible(false);
        self.set_hidden(true);
    }

    /// Fenêtre invisible (zone de notification ou réduite) : le rendu s'arrête, les
    /// minuteries JS sont ralenties et WebView2 rend sa mémoire au système. La connexion
    /// reste ouverte : messages, notifications et appels vocaux continuent.
    fn set_hidden(&mut self, hidden: bool) {
        if self.hidden == hidden {
            return;
        }
        self.hidden = hidden;
        let Some(v) = &self.view else { return };
        if hidden {
            let _ = v.set_visible(false);
            // Pendant un appel on évite la purge agressive (micro-coupures possibles).
            if !wv2::playing_audio(v) {
                let _ = v.set_memory_usage_level(MemoryUsageLevel::Low);
            }
            system::trim_self();
        } else {
            let _ = v.set_memory_usage_level(MemoryUsageLevel::Normal);
            let _ = v.set_visible(true);
            let _ = v.focus();
        }
    }

    /// Titre Discord : « (3) Discord | #salon » = 3 mentions, « • Discord » = non-lus.
    fn on_title(&mut self, title: String) {
        let t = title.trim();
        let mentions = t
            .strip_prefix('(')
            .and_then(|r| r.split_once(')'))
            .and_then(|(n, _)| n.trim().trim_end_matches('+').parse::<u32>().ok())
            .unwrap_or(0);
        let unread = mentions > 0 || t.starts_with('•');
        self.window.set_title(if t.is_empty() { "Discord-Lite" } else { t });
        if mentions > self.mentions && !self.focused {
            system::flash(self.window.hwnd());
        }
        self.mentions = mentions;
        if let Some(tray) = &self.tray {
            let tip = match mentions {
                0 if unread => "Discord-Lite — messages non lus".to_string(),
                0 => "Discord-Lite".to_string(),
                1 => "Discord-Lite — 1 mention".to_string(),
                n => format!("Discord-Lite — {n} mentions"),
            };
            let _ = tray.set_tooltip(Some(tip));
            if self.badge != (mentions > 0) {
                self.badge = mentions > 0;
                let _ = tray.set_icon(tray_icon(self.badge));
            }
        }
    }

    fn update_stats(&mut self) {
        let n = self.blocked.load(Ordering::Relaxed);
        if n != self.shown_blocked {
            self.shown_blocked = n;
            self.items.stats.set_text(format!("Télémétrie bloquée : {n} requête{}", if n > 1 { "s" } else { "" }));
        }
    }

    fn on_menu(&mut self, id: &str, control: &mut ControlFlow) {
        match id {
            "open" => self.show(),
            "reload" => {
                if let Some(v) = &self.view {
                    let _ = v.reload();
                }
            }
            "cache" => {
                if let Some(v) = &self.view {
                    wv2::clear_cache(v);
                }
            }
            "telemetry" => {
                self.settings.block_telemetry = self.items.telemetry.is_checked();
                self.blocking.store(self.settings.block_telemetry, Ordering::Relaxed);
                self.settings.save();
            }
            "autostart" => {
                self.settings.autostart = self.items.autostart.is_checked();
                system::set_autostart(self.settings.autostart);
                self.settings.save();
            }
            "tray" => {
                self.settings.close_to_tray = self.items.close_to_tray.is_checked();
                self.settings.save();
            }
            // Ces deux options changent les paramètres du moteur : il faut le relancer.
            "eco" | "hw" => {
                self.settings.eco = self.items.eco.is_checked();
                self.settings.hw_accel = self.items.hw.is_checked();
                self.quit(control, true);
            }
            "quit" => self.quit(control, false),
            _ => {}
        }
    }

    fn save_geometry(&mut self) {
        if !self.window.is_visible() || self.window.is_minimized() {
            return;
        }
        self.settings.maximized = self.window.is_maximized();
        if !self.settings.maximized {
            let size = self.window.inner_size().to_logical::<f64>(self.window.scale_factor());
            self.settings.width = size.width;
            self.settings.height = size.height;
        }
    }

    fn quit(&mut self, control: &mut ControlFlow, restart: bool) {
        self.save_geometry();
        self.settings.save();
        // Libère le moteur avant de partir pour qu'une relance reprenne un dossier propre.
        self.view = None;
        self.tray = None;
        if restart {
            if let Ok(exe) = std::env::current_exe() {
                let mut cmd = std::process::Command::new(exe);
                cmd.arg(format!("--restart={}", std::process::id()));
                if self.hidden {
                    cmd.arg("--background");
                }
                let _ = cmd.spawn();
            }
        }
        *control = ControlFlow::Exit;
    }
}

fn tray_icon(badge: bool) -> Option<tray_icon::Icon> {
    let rgba = if badge { TRAY_BADGE_RGBA } else { TRAY_RGBA };
    tray_icon::Icon::from_rgba(rgba.to_vec(), 32, 32).ok()
}

fn build_tray(proxy: &EventLoopProxy<UserEvent>, s: &Settings) -> (Option<TrayIcon>, MenuItems) {
    let items = MenuItems {
        eco: CheckMenuItem::with_id("eco", "Mode éco (redémarre)", true, s.eco, None),
        telemetry: CheckMenuItem::with_id("telemetry", "Bloquer la télémétrie", true, s.block_telemetry, None),
        autostart: CheckMenuItem::with_id("autostart", "Lancer au démarrage de Windows", true, s.autostart, None),
        close_to_tray: CheckMenuItem::with_id("tray", "Fermer = réduire dans la zone de notification", true, s.close_to_tray, None),
        hw: CheckMenuItem::with_id("hw", "Accélération matérielle (redémarre)", true, s.hw_accel, None),
        stats: MenuItem::with_id("stats", "Télémétrie bloquée : 0 requête", false, None),
    };
    let menu = Menu::new();
    let _ = menu.append(&MenuItem::with_id("open", "Ouvrir Discord-Lite", true, None));
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&items.eco);
    let _ = menu.append(&items.telemetry);
    let _ = menu.append(&items.hw);
    let _ = menu.append(&items.autostart);
    let _ = menu.append(&items.close_to_tray);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id("reload", "Recharger", true, None));
    let _ = menu.append(&MenuItem::with_id("cache", "Vider le cache", true, None));
    let _ = menu.append(&items.stats);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id("quit", "Quitter", true, None));

    let p = Mutex::new(proxy.clone());
    TrayIconEvent::set_event_handler(Some(move |e| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
            let _ = p.lock().unwrap().send_event(UserEvent::TrayClick);
        }
    }));
    let p = Mutex::new(proxy.clone());
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let _ = p.lock().unwrap().send_event(UserEvent::Menu(e.id.0));
    }));

    let mut builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Discord-Lite");
    if let Some(icon) = tray_icon(false) {
        builder = builder.with_icon(icon);
    }
    (builder.build().ok(), items)
}

/// Après « redémarrer », attend que l'ancienne instance et son moteur soient partis.
fn wait_for_previous(pid: u32) {
    use windows::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            WaitForSingleObject(h, 10_000);
            let _ = CloseHandle(h);
        }
    }
    // Le processus navigateur de WebView2 se ferme juste après son hôte.
    std::thread::sleep(std::time::Duration::from_millis(800));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let background = args.iter().any(|a| a == "--background");
    let restart = args.iter().find_map(|a| a.strip_prefix("--restart=")).and_then(|p| p.parse::<u32>().ok());
    let url_arg = args.iter().find(|a| !a.starts_with("--")).cloned();
    let command = url_arg.as_ref().map_or_else(|| "show".to_string(), |u| format!("url {u}"));

    match restart {
        Some(pid) => wait_for_previous(pid),
        // Instance unique : un second lancement passe la main à celle qui tourne.
        None if system::forward_to_running(&command) => return,
        None => {}
    }

    let settings = Settings::load();
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title("Discord-Lite")
        .with_window_icon(Icon::from_resource(1, None).ok())
        .with_inner_size(LogicalSize::new(settings.width.max(480.0), settings.height.max(360.0)))
        .with_min_inner_size(LogicalSize::new(480.0, 360.0))
        .with_maximized(settings.maximized)
        .with_theme(Some(Theme::Dark))
        .with_visible(!background)
        .build(&event_loop)
        .expect("création de la fenêtre");

    let _ = std::fs::create_dir_all(data_dir());
    let mut ctx = WebContext::new(Some(data_dir().join("WebView2")));
    let start_url = url_arg.filter(|u| wv2::is_discord(u)).unwrap_or_else(|| HOME.to_string());

    let p_title = proxy.clone();
    let view = WebViewBuilder::new_with_web_context(&mut ctx)
        .with_url(&start_url)
        .with_additional_browser_args(&browser_args(&settings))
        .with_background_color((49, 51, 56, 255))
        .with_devtools(cfg!(debug_assertions) || std::env::var_os("DISCORD_LITE_CDP").is_some())
        .with_autoplay(true)
        .with_document_title_changed_handler(move |t| {
            let _ = p_title.send_event(UserEvent::Title(t));
        })
        .with_navigation_handler(|url| {
            // Seul Discord s'affiche dans l'appli ; un lien externe part dans le navigateur.
            if url.starts_with("http") && !wv2::is_discord(&url) {
                system::open_external(&url);
                return false;
            }
            true
        })
        .with_new_window_req_handler(|url: String, _| {
            // Fenêtre ouverte vide puis redirigée (liaison de comptes) : on la laisse s'ouvrir.
            if url == "about:blank" {
                return NewWindowResponse::Allow;
            }
            system::open_external(&url);
            NewWindowResponse::Deny
        })
        .build(&window)
        .expect("création de la vue WebView2");

    let blocking = Arc::new(AtomicBool::new(settings.block_telemetry));
    let blocked = Arc::new(AtomicU64::new(0));
    {
        let p = proxy.clone();
        let _ = wv2::block_telemetry(&view, blocking.clone(), blocked.clone(), move || {
            let _ = p.send_event(UserEvent::Blocked);
        });
    }
    let _ = wv2::grant_permissions(&view);
    {
        let p = proxy.clone();
        wv2::on_notification(&view, move || {
            let _ = p.send_event(UserEvent::Notified);
        });
    }
    {
        let p = proxy.clone();
        system::listen(move |cmd| {
            let _ = p.send_event(UserEvent::Command(cmd));
        });
    }
    system::set_autostart(settings.autostart);

    let (tray, items) = build_tray(&proxy, &settings);
    let mut app = App {
        window,
        view: Some(view),
        settings,
        tray,
        items,
        blocking,
        blocked,
        shown_blocked: 0,
        mentions: 0,
        badge: false,
        focused: !background,
        hidden: false,
    };
    if background {
        app.set_hidden(true);
    }

    event_loop.run(move |event, _, control| {
        *control = ControlFlow::Wait;
        let _ = &ctx;
        match event {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => {
                    if app.settings.close_to_tray && app.tray.is_some() {
                        app.save_geometry();
                        app.settings.save();
                        app.hide();
                    } else {
                        app.quit(control, false);
                    }
                }
                WindowEvent::Focused(f) => app.focused = f,
                // Réduite dans la barre des tâches = invisible : même régime que cachée.
                WindowEvent::Resized(size) => app.set_hidden(size.width == 0 || size.height == 0 || !app.window.is_visible()),
                _ => {}
            },
            Event::UserEvent(e) => match e {
                UserEvent::TrayClick => {
                    if app.window.is_visible() && !app.window.is_minimized() {
                        app.hide();
                    } else {
                        app.show();
                    }
                }
                UserEvent::Menu(id) => app.on_menu(&id, control),
                UserEvent::Command(cmd) => {
                    if let Some(url) = cmd.strip_prefix("url ").filter(|u| wv2::is_discord(u)) {
                        if let Some(v) = &app.view {
                            let _ = v.load_url(url);
                        }
                    }
                    app.show();
                }
                UserEvent::Title(t) => app.on_title(t),
                UserEvent::Blocked => app.update_stats(),
                UserEvent::Notified => {
                    if !app.focused {
                        system::flash(app.window.hwnd());
                    }
                }
            },
            _ => {}
        }
    });
}
