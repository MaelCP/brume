# brume

Barre de commande invisible pour macOS. `Cmd+Shift+Space` fait apparaître une barre
transparente : on tape une question, Claude CLI répond dedans.

- Invisible au partage d'écran et aux captures (`NSWindowSharingNone`)
- Absente de Cmd+Tab, du Dock et de la barre des menus (app *accessory*)
- Se masque seule dès qu'elle perd le focus (clic ailleurs, changement d'espace)
- Chaque question est indépendante (pas d'historique renvoyé)

| Touche | Effet |
|---|---|
| `Cmd+Shift+Space` | affiche / masque la barre |
| `Entrée` | envoie la question tapée |
| `Échap` | vide et masque |

Glisse le `❯` pour déplacer la barre : la position est retenue, en décalage par
rapport au coin de l'écran (`~/.claude-hud-position`), donc elle retrouve le même
coin sur n'importe quel moniteur. Supprime ce fichier pour revenir au défaut.

## Dossiers

`~/.claude-hud-dossiers`, un chemin par ligne, `~` résolu, `#` pour commenter.
Les chemins inexistants sont ignorés.

```
# depuis où Claude est lancé (défaut : ~/Documents)
cwd: ~/Projets/mon-cours

# dossier parent des cours (voir plus bas)
cours: ~/Documents/ETS

# dossiers de référence, consultables pour répondre
~/Documents/Cours
~/Notes
```

Avec `cours:`, commencer la question par le début du nom d'un sous-dossier
(`mgr`, `ges800`, `mti`…) lance Claude depuis ce cours, et il le reste pour les
questions suivantes. Le préfixe seul (`mgr` + Entrée) change de cours sans rien
demander. Mot non reconnu : la question part telle quelle, dans le cours actif.

Pas d'index ni de base vectorielle : les dossiers de référence sont passés en
`--add-dir` et le CLI grep les fichiers lui-même. Les `RESUME.md` du dossier `cwd:`
et de ses sous-dossiers directs sont injectés au lancement (`--append-system-prompt`) :
pas d'aller-retour de lecture quand ils suffisent.

## Prérequis

`claude` CLI installé et authentifié.

## Dev

```sh
npm install
npm run tauri dev
```
