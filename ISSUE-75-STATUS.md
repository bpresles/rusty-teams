# Issue #75 : état au 20/11/2026 (à ne pas commiter)

Note de reprise. Ce fichier n'est pas suivi par Git : à supprimer une fois le travail repris.

## Où on en est

- Branche `gpu-video-acceleration-f99996a8`. Rien n'est commité ni poussé.
- Deux commentaires publiés sur #75 :
  - mesures sur 0.5.0 : https://github.com/danieldaeschle/rusty-teams/issues/75#issuecomment-7041090250
  - mesures caméra et proposition de résolution : https://github.com/danieldaeschle/rusty-teams/issues/75#issuecomment-7044546137
- Le mainteneur n'a que 0.5.0 : les commentaires ne doivent pas parler de « lots » locaux.

## Changements locaux non commités (prototype)

1. Mesure du partage : `share_load.rs` et l'entrée « share load » de la timeline (`call/mod.rs`).
2. Correctif de limite d'envoi : `RequestFilter::forget` (`send_control.rs`), appelé quand `apply_send_limit` renvoie `Ok(None)` ou une erreur (`call/mod.rs`). Commit prévu : `fix(calls): a send limit that could not be applied is admitted again`.
3. Partage macOS par ScreenCaptureKit en NV12 vers VideoToolbox, sans copie : `screen/` (macos_picker, macos_sck, macos_share, native_plan), `share_frame.rs`, `screen.rs`, `video_send.rs`, `standout.rs` (`is_active`). Repli : `CALLING_SCREEN_CAPTURE=legacy`. Commit prévu : `feat(macos): screen share through ScreenCaptureKit…`.
   - Défaut connu : l'aperçu local (`video_send.rs`, `show()`) convertit toute l'image 4K en I420 sur le thread de capture (62 à 690 ms par image d'aperçu, partage à 0,8–6,9 i/s).

Résultat des tests : pas d'amélioration ressentie. La charge vient surtout de la caméra locale.

## Constats (test du 20/11/2026, 15:44 UTC)

- Caméra locale allumée : app ~100 % CPU, contre ~33 % caméra coupée ; WindowServer +20 points.
- Caméra distante : coût négligeable (décodage matériel OK).
- Capture caméra peu coûteuse : YUYV 720p30, 0,6 à 1,9 ms de conversion. Le coût est en aval, cause non identifiée.
- Aucun effet caméra actif.
- Mac bridé thermiquement : cœurs P à l'arrêt, tout tourne sur les 4 cœurs E.
- Réseau sain. Les pics de RTT suivent la saturation CPU (corrélation seulement).
- `sample` est inutilisable : il manque la moitié des threads.

## Prochaine étape (non validée)

Faire implémenter par un agent Sonnet les éléments 1 à 3 de la proposition :

1. Mesure CPU par thread toutes les 2 s dans le rapport d'appel : `thread_info` Mach avec le nom du thread, effets actifs, `ProcessInfo.thermalState`.
2. Correctif de limite d'envoi : déjà fait, à commiter.
3. Aperçu du partage réduit sur GPU (`VTPixelTransferSession`, ≤ 1280 px de large, ~5 i/s), hors du thread de capture.

Ensuite, refaire le test à deux (Mac refroidi, pas de compilation juste avant) et donner le scénario pas à pas.

Puis, seulement si la mesure par thread le justifie :

4. Caméra macOS par AVFoundation en CVPixelBuffer NV12 vers VideoToolbox et l'aperçu GPUI, sans copie. Garder nokhwa en repli.
5. Pool de threads persistant pour les effets caméra (aujourd'hui, `std::thread::scope` à chaque image).

## Divers

- Données et scripts de mesure dans le scratchpad de la session (effaçables au redémarrage) : `measure-tools/phases.py`, `measure-tools/tl.py`.
- À corriger dans ces scripts : `watch-light.sh` et `watch-full.sh` (pgrep se trouve lui-même), `percore.py` (`datetime.now(datetime.UTC)`).
- Ne pas toucher `.serena/`. Jamais de `git stash` nu.
