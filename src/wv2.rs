//! Accès direct aux API WebView2 que wry n'expose pas : blocage de la télémétrie,
//! permissions (micro, caméra, notifications), notifications et vidage du cache.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

use webview2_com::{
    take_pwstr, ClearBrowsingDataCompletedHandler, Microsoft::Web::WebView2::Win32::*, NotificationReceivedEventHandler,
    PermissionRequestedEventHandler, WebResourceRequestedEventHandler,
};
use windows::{
    core::{Interface, HSTRING, PWSTR},
    Win32::System::Com::IStream,
};
use wry::{WebView, WebViewExtWindows};

/// Requêtes de télémétrie et de traçage envoyées par le client web de Discord.
/// Seules ces adresses passent par le filtre : le reste du trafic n'est jamais
/// intercepté, donc aucun coût par requête pour l'application elle-même.
pub const TELEMETRY: &[&str] = &[
    // Statistiques d'usage (« science ») et métriques de performance.
    "*://discord.com/api/v*/science*",
    "*://*.discord.com/api/v*/science*",
    "*://discord.com/api/science*",
    "*://discord.com/api/v*/metrics*",
    "*://*.discord.com/api/v*/metrics*",
    // Rapports de plantage (Sentry), directs ou via le relais de Discord.
    "*://discord.com/error-reporting-proxy/*",
    "*://*.discord.com/error-reporting-proxy/*",
    "*://sentry.io/*",
    "*://*.sentry.io/*",
    // Mesures d'audience tierces.
    "*://static.cloudflareinsights.com/*",
    "*://discord.com/cdn-cgi/rum*",
    "*://www.google-analytics.com/*",
    "*://*.google-analytics.com/*",
    "*://www.googletagmanager.com/*",
    "*://*.doubleclick.net/*",
    "*://*.hotjar.com/*",
];

pub fn host_of(uri: &str) -> String {
    let rest = uri.split_once("://").map_or(uri, |x| x.1);
    let h = rest.split(['/', '?', '#']).next().unwrap_or("");
    let h = h.rsplit_once('@').map_or(h, |x| x.1);
    h.split(':').next().unwrap_or(h).to_ascii_lowercase()
}

/// Domaines qui restent dans l'application ; tout le reste s'ouvre dans le navigateur.
pub fn is_discord(uri: &str) -> bool {
    let host = host_of(uri);
    ["discord.com", "discordapp.com", "discord.gg", "discord.new", "discordapp.net", "discord.media"]
        .iter()
        .any(|d| host == *d || host.strip_suffix(d).is_some_and(|p| p.ends_with('.')))
}

/// Répond « 204 No Content » à chaque requête de télémétrie : le client croit
/// qu'elle est partie, ne réessaie pas et n'affiche aucune erreur.
pub fn block_telemetry(
    view: &WebView,
    enabled: Arc<AtomicBool>,
    counter: Arc<AtomicU64>,
    on_block: impl Fn() + 'static,
) -> windows::core::Result<()> {
    unsafe {
        let wv = view.webview();
        let env = view.environment();
        for pattern in TELEMETRY {
            wv.AddWebResourceRequestedFilter(&HSTRING::from(*pattern), COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL)?;
        }
        let mut token = 0i64;
        wv.add_WebResourceRequested(
            &WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                if !enabled.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let resp = env.CreateWebResourceResponse(
                    None::<&IStream>,
                    204,
                    &HSTRING::from("No Content"),
                    &HSTRING::from("Access-Control-Allow-Origin: *"),
                )?;
                args.SetResponse(&resp)?;
                counter.fetch_add(1, Ordering::Relaxed);
                on_block();
                Ok(())
            })),
            &mut token,
        )?;
    }
    Ok(())
}

/// Accorde d'office à Discord (et à lui seul) ce dont il a besoin pour les appels
/// vocaux, la vidéo, les notifications et le presse-papiers.
pub fn grant_permissions(view: &WebView) -> windows::core::Result<()> {
    unsafe {
        let mut token = 0i64;
        view.webview().add_PermissionRequested(
            &PermissionRequestedEventHandler::create(Box::new(|_, args| {
                let Some(args) = args else { return Ok(()) };
                let mut uri = PWSTR::null();
                args.Uri(&mut uri)?;
                if !is_discord(&take_pwstr(uri)) {
                    return Ok(());
                }
                let mut kind = COREWEBVIEW2_PERMISSION_KIND_UNKNOWN_PERMISSION;
                args.PermissionKind(&mut kind)?;
                if matches!(
                    kind,
                    COREWEBVIEW2_PERMISSION_KIND_MICROPHONE
                        | COREWEBVIEW2_PERMISSION_KIND_CAMERA
                        | COREWEBVIEW2_PERMISSION_KIND_NOTIFICATIONS
                        | COREWEBVIEW2_PERMISSION_KIND_CLIPBOARD_READ
                        | COREWEBVIEW2_PERMISSION_KIND_AUTOPLAY
                ) {
                    args.SetState(COREWEBVIEW2_PERMISSION_STATE_ALLOW)?;
                }
                Ok(())
            })),
            &mut token,
        )?;
    }
    Ok(())
}

/// Prévient l'application à chaque notification (pour faire clignoter la barre des
/// tâches) ; l'affichage reste celui de Windows.
pub fn on_notification(view: &WebView, f: impl Fn() + 'static) {
    unsafe {
        let Ok(w) = view.webview().cast::<ICoreWebView2_24>() else { return };
        let mut token = 0i64;
        let _ = w.add_NotificationReceived(
            &NotificationReceivedEventHandler::create(Box::new(move |_, _| {
                f();
                Ok(())
            })),
            &mut token,
        );
    }
}

/// Vide le cache disque de Chromium (souvent plusieurs centaines de Mo pour Discord)
/// sans toucher à la session : on reste connecté.
pub fn clear_cache(view: &WebView) {
    unsafe {
        if let Ok(p) = view
            .webview()
            .cast::<ICoreWebView2_13>()
            .and_then(|w| w.Profile())
            .and_then(|p| p.cast::<ICoreWebView2Profile2>())
        {
            let kinds = COREWEBVIEW2_BROWSING_DATA_KINDS(
                COREWEBVIEW2_BROWSING_DATA_KINDS_DISK_CACHE.0 | COREWEBVIEW2_BROWSING_DATA_KINDS_CACHE_STORAGE.0,
            );
            let _ = p.ClearBrowsingData(kinds, &ClearBrowsingDataCompletedHandler::create(Box::new(|_| Ok(()))));
        }
    }
}

pub fn playing_audio(view: &WebView) -> bool {
    unsafe {
        let mut playing = windows::core::BOOL(0);
        view.webview()
            .cast::<ICoreWebView2_8>()
            .and_then(|w| w.IsDocumentPlayingAudio(&mut playing))
            .is_ok()
            && playing.as_bool()
    }
}
