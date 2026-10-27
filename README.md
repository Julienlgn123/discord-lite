<p align="center"><img src="assets/wordmark.svg" height="72" alt="Discord Lite"></p>

# Discord-Lite

Client Discord ultra léger pour Windows : le client web officiel (discord.com/app) dans une coquille Rust + WebView2. Pas d'Electron, pas de Node — un exe de < 1 Mo.

## Optimisations
- **Télémétrie bloquée** au niveau réseau (`/api/v*/science`, `/metrics`, Sentry, relais d'erreurs, Cloudflare RUM, Google Analytics…) : réponse 204 locale, seules ces URL sont interceptées.
- **Fenêtre cachée ou réduite** : rendu stoppé, timers ralentis, WebView2 en `MemoryUsageLevel::Low`, working set de l'hôte vidé. Messages, notifications et vocal continuent.
- **Mode éco** (par défaut) : animations réduites forcées (avatars/GIF/transitions), 2 processus de rendu max, BackForwardCache coupé.
- Moteur dégraissé : pas de renderer de réserve, pas de traduction, SmartScreen, crash reports, domain reliability, background networking, pings.
- V8 `--optimize-for-size`, build LTO fat + strip.

## Fonctionnalités
Zone de notification (badge mentions, compteur de requêtes bloquées), fermer = réduire, lancement au démarrage (`--background`), instance unique, liens externes dans le navigateur, micro/caméra/notifications accordés à Discord, clignotement barre des tâches sur mention, vider le cache, accélération matérielle on/off.

## Build
```
cargo build --release
```
Test isolé : `DISCORD_LITE_PROFILE=bench DISCORD_LITE_CDP=9335 discord-lite.exe --background`.

Projet non officiel, non affilié à Discord Inc.
