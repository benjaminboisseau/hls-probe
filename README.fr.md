# hls-probe

*[English version](README.md)*

Une petite sonde en ligne de commande pour flux HLS. Donnez-lui n'importe quelle URL `.m3u8` : elle vous dit ce qu'elle contient et si le flux est sain :

- **Inventaire complet** — l'échelle ABR vidéo (bande passante, average bandwidth, résolution, frame rate, codecs, groupes de renditions), les renditions audio (langue, canaux, caractéristiques d'accessibilité), les sous-titres et les playlists i-frame de trick-play, chacun dans sa propre section.
- **Contrôles de conformité** — durées de segment dépassant `EXT-X-TARGETDURATION` (RFC 8216 §4.3.3.1), variantes référençant un groupe de renditions non déclaré, attributs `CODECS`/`RESOLUTION` manquants, bandes passantes en double, comptage des discontinuités, dérive entre `EXT-X-PROGRAM-DATE-TIME` et la somme des durées de segments.
- **Mesure des segments** (`--measure`) — télécharge des segments et rapporte TTFB, temps de téléchargement, débit, format conteneur (MPEG-TS vs fMP4/CMAF, détection du segment d'init) et le débit mesuré face au `BANDWIDTH` déclaré (que la RFC 8216 §4.3.4.2 définit comme le débit crête par segment : le dépasser est un vrai problème de conformité).
- **Monitoring live** (`--live`) — interroge une media playlist live à la moitié de la target duration, compte les rafraîchissements avec et sans progression, et estime le retard du bord de playlist par rapport au temps réel via les tags PDT lorsqu'ils sont présents.
- **Sortie JSON** (`--json`) et code de sortie non nul en cas d'erreur de conformité : l'outil s'intègre directement dans des scripts de monitoring ou une CI.

Né de l'exploitation de plateformes de distribution live (têtes de réseau IPTV/OTT) où la première question est toujours : *l'origine produit-elle une playlist saine, et à quelle distance du direct sommes-nous ?*

## Installation

```
cargo install --git https://github.com/benjaminboisseau/hls-probe
```

Ou clonez puis `cargo build --release` ; le binaire se trouve dans `target/release/hls-probe`.

## Utilisation

Inventaire d'une master playlist :

```
$ hls-probe https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8
master playlist: https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8
5 variant(s):
    6221600 bps  1920x1080   -      mp4a.40.2,avc1.640028
    ...
  no issues found
```

Suivre l'avancement d'un flux live (suit la variante de plus haut débit) :

```
$ hls-probe --live -n 10 https://example.com/live/master.m3u8
live: 10 refreshes over 14s, 5 new segment(s), 0 stale refresh(es)
  edge lag: avg 11.7s, min 9.4s, max 14.1s (PDT-based)
```

| Option | Effet |
| --- | --- |
| `--live` | interroge une playlist live et mesure son comportement |
| `-n, --refreshes <N>` | nombre de rafraîchissements à observer (10 par défaut) |
| `--all` | analyse chaque variante d'une master playlist |
| `--measure` | télécharge des segments : TTFB, débit, bitrate réel vs déclaré, conteneur |
| `-s, --segments <N>` | nombre de segments à échantillonner avec `--measure` (3 par défaut) |
| `--json` | sortie machine |

Codes de sortie : `0` sain ou avertissements seulement, `2` au moins une erreur de conformité, `1` échec réseau/parsing.

## Feuille de route

- Low-latency HLS (segments partiels, preload hints, rechargement bloquant)
- Monitoring live multi-variantes avec comparaison d'alignement
- Analyse des playlists de renditions audio/sous-titres

## Licence

MIT
