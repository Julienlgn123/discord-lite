//! Intégration Windows : instance unique, lancement au démarrage, liens externes,
//! clignotement de la barre des tâches et libération de la mémoire du processus.

use std::{io::Read, io::Write, thread};

use windows::{
    core::HSTRING,
    Win32::{
        Foundation::{CloseHandle, HANDLE, HWND, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{ReadFile, PIPE_ACCESS_INBOUND},
        System::{
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
                PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
            },
            ProcessStatus::EmptyWorkingSet,
            Registry::{RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ},
            Threading::GetCurrentProcess,
        },
        UI::{
            Shell::ShellExecuteW,
            WindowsAndMessaging::{FlashWindowEx, FLASHWINFO, FLASHW_TIMERNOFG, FLASHW_TRAY, SW_SHOWNORMAL},
        },
    },
};

/* ---------- instance unique ---------- */

fn pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    format!(r"\\.\pipe\discord-lite-{user}{}", crate::profile_suffix())
}

/// Si Discord-Lite tourne déjà, lui transmet la commande et renvoie `true`.
pub fn forward_to_running(command: &str) -> bool {
    match std::fs::OpenOptions::new().write(true).open(pipe_name()) {
        Ok(mut f) => f.write_all(command.as_bytes()).is_ok(),
        Err(_) => false,
    }
}

/// Reçoit les commandes des lancements suivants (raccourci, lien discord://).
pub fn listen(on_command: impl Fn(String) + Send + 'static) {
    let name = HSTRING::from(pipe_name());
    thread::spawn(move || loop {
        let pipe = unsafe {
            CreateNamedPipeW(
                &name,
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                0,
                4096,
                0,
                None,
            )
        };
        if pipe == INVALID_HANDLE_VALUE {
            return;
        }
        // Un client déjà connecté avant l'appel renvoie ERROR_PIPE_CONNECTED : c'est un succès.
        let _ = unsafe { ConnectNamedPipe(pipe, None) };
        let text = read_all(pipe);
        unsafe {
            let _ = DisconnectNamedPipe(pipe);
            let _ = CloseHandle(pipe);
        }
        if !text.is_empty() {
            on_command(text);
        }
    });
}

fn read_all(pipe: HANDLE) -> String {
    struct P(HANDLE);
    impl Read for P {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut n = 0u32;
            match unsafe { ReadFile(self.0, Some(buf), Some(&mut n), None) } {
                Ok(()) => Ok(n as usize),
                Err(_) => Ok(0),
            }
        }
    }
    let mut s = String::new();
    let _ = P(pipe).take(64 * 1024).read_to_string(&mut s);
    s
}

/* ---------- lancement au démarrage ---------- */

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub fn set_autostart(enabled: bool) {
    // Un profil de test ne doit jamais toucher au lancement automatique du vrai client.
    if !crate::profile_suffix().is_empty() {
        return;
    }
    unsafe {
        if enabled {
            let Ok(exe) = std::env::current_exe() else { return };
            let cmd: Vec<u16> = format!("\"{}\" --background", exe.display()).encode_utf16().chain([0]).collect();
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(RUN_KEY),
                &HSTRING::from("Discord-Lite"),
                REG_SZ.0,
                Some(cmd.as_ptr() as *const _),
                (cmd.len() * 2) as u32,
            );
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(RUN_KEY), &HSTRING::from("Discord-Lite"));
        }
    }
}

/* ---------- divers ---------- */

/// Ouvre un lien dans le navigateur par défaut (seulement http/https).
pub fn open_external(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    unsafe {
        ShellExecuteW(None, &HSTRING::from("open"), &HSTRING::from(url), None, None, SW_SHOWNORMAL);
    }
}

/// Fait clignoter le bouton de la barre des tâches jusqu'à ce que la fenêtre reprenne le focus.
pub fn flash(hwnd: isize) {
    let info = FLASHWINFO {
        cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
        hwnd: HWND(hwnd as _),
        dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
        uCount: 0,
        dwTimeout: 0,
    };
    unsafe {
        let _ = FlashWindowEx(&info);
    }
}

/// Rend au système la mémoire inutilisée du processus hôte (la fenêtre est cachée).
pub fn trim_self() {
    unsafe {
        let _ = EmptyWorkingSet(GetCurrentProcess());
    }
}
