use std::env;
use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

// ponytail: static plutot qu'un State Tauri - l'app n'a qu'une fenetre et une
// requete a la fois. Passer par .manage() si un jour il y en a plusieurs.
//
// Timestamp (ms) avant lequel le masquage-au-blur est interdit. Le CLI `claude`
// vole le focus, et l'evenement Focused(false) arrive parfois
// APRES la fin de la requete - un simple booleen "occupe" laissait donc la barre
// disparaitre juste au moment d'afficher la reponse.
static GARDE_JUSQU_A: AtomicU64 = AtomicU64::new(0);

// ponytail: 1500 ms couvre le retard d'evenement observe. A ajuster si la barre
// se masque encore trop tot, ou si elle s'attarde apres un vrai clic ailleurs.
const GRACE_APRES_REPONSE_MS: u64 = 1500;

// Cours choisi par prefixe ("mgr ...") : reste actif pour les questions suivantes,
// jusqu'au prochain prefixe ou au redemarrage de l'app.
static COURS_ACTIF: Mutex<Option<String>> = Mutex::new(None);

fn maintenant_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn garder(duree_ms: u64) {
    GARDE_JUSQU_A.store(maintenant_ms() + duree_ms, Ordering::SeqCst);
}

#[tauri::command]
async fn interroger_claude(prompt: String) -> Result<String, String> {
    // Couvre toute la requete (cap a 10 min) puis laisse une grace au retour,
    // y compris sur les retours anticipes d'analyser().
    garder(10 * 60 * 1000);
    let resultat = analyser(prompt).await;
    garder(GRACE_APRES_REPONSE_MS);
    resultat
}

fn resoudre(chemin: &str, home: &str) -> String {
    match chemin.strip_prefix("~/") {
        Some(reste) => format!("{}/{}", home, reste),
        None => chemin.to_string(),
    }
}

/// Modele par defaut : le plus rapide et le moins cher de la gamme courante.
/// Surclassable par une ligne `model:` (alias `opus`/`sonnet`, ou un id complet).
const MODELE_DEFAUT: &str = "claude-haiku-4-5";

/// Si le premier mot du prompt designe un sous-dossier de `racine` (debut du nom,
/// insensible a la casse, 3 caracteres min : "mgr" -> MGR850, "ges800" -> GES800),
/// rend (ce dossier, le reste du prompt). Sinon (None, prompt intact).
fn choisir_cours(prompt: &str, racine: &str) -> (Option<String>, String) {
    let prompt = prompt.trim();
    let (mot, reste) = prompt.split_once(char::is_whitespace).unwrap_or((prompt, ""));
    let mot = mot.trim_end_matches(':').to_lowercase();
    if mot.chars().count() < 3 || !mot.chars().all(|c| c.is_alphanumeric()) {
        return (None, prompt.to_string());
    }
    let trouve = fs::read_dir(racine)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .find(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().to_lowercase().starts_with(&mot))
                .unwrap_or(false)
        });
    match trouve {
        Some(dossier) => (Some(dossier.to_string_lossy().into_owned()), reste.trim().to_string()),
        None => (None, prompt.to_string()),
    }
}

/// Lit ~/.claude-hud-dossiers et rend (dossier de travail, dossiers, modele).
///
/// Un chemin par ligne, `#` pour commenter, `~` resolu, chemins inexistants
/// ignores en silence. Une ligne `cwd: <chemin>` choisit le dossier depuis lequel
/// Claude est lance ; a defaut ~/Documents. Les autres lignes deviennent des
/// dossiers de reference (le "RAG" : pas d'index, le CLI grep et lit lui-meme).
/// Une ligne `cours: <chemin>` donne le dossier parent des cours (voir choisir_cours).
fn config_dossiers(home: &str) -> (String, Vec<String>, String, Option<String>) {
    let defaut = format!("{}/Documents", home);
    let config = format!("{}/.claude-hud-dossiers", home);
    let Ok(contenu) = fs::read_to_string(&config) else {
        return (defaut, Vec::new(), MODELE_DEFAUT.to_string(), None);
    };

    let mut travail = None;
    let mut modele = None;
    let mut cours = None;
    let mut sources = Vec::new();
    for ligne in contenu.lines().map(str::trim) {
        if ligne.is_empty() || ligne.starts_with('#') {
            continue;
        }
        if let Some(nom) = ligne.strip_prefix("model:") {
            let nom = nom.trim();
            if !nom.is_empty() {
                modele = Some(nom.to_string());
            }
            continue;
        }
        if let Some(chemin) = ligne.strip_prefix("cours:") {
            let chemin = resoudre(chemin.trim(), home);
            if std::path::Path::new(&chemin).is_dir() {
                cours = Some(chemin);
            }
            continue;
        }
        match ligne.strip_prefix("cwd:") {
            Some(chemin) => {
                let chemin = resoudre(chemin.trim(), home);
                if std::path::Path::new(&chemin).is_dir() {
                    travail = Some(chemin);
                }
            }
            None => {
                let chemin = resoudre(ligne, home);
                if std::path::Path::new(&chemin).is_dir() {
                    sources.push(chemin);
                }
            }
        }
    }
    (
        travail.unwrap_or(defaut),
        sources,
        modele.unwrap_or_else(|| MODELE_DEFAUT.to_string()),
        cours,
    )
}

/// Concatene les RESUME.md du dossier de travail et de ses sous-dossiers directs,
/// injectes au lancement : Claude repond sans aller-retour de lecture quand ils suffisent.
fn resumes(dossier: &str) -> String {
    let mut chemins: Vec<_> = fs::read_dir(dossier)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .map(|p| p.join("RESUME.md"))
        .chain([std::path::Path::new(dossier).join("RESUME.md")])
        .filter(|p| p.is_file())
        .collect();
    chemins.sort();
    chemins
        .iter()
        .filter_map(|p| Some(format!("## {}\n\n{}", p.display(), fs::read_to_string(p).ok()?)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Chemin absolu du CLI `claude`.
///
/// Une app lancee depuis le Finder n'herite PAS du PATH du shell : macOS lui donne
/// /usr/bin:/bin:/usr/sbin:/sbin, sans ~/.local/bin ni /opt/homebrew/bin. Un simple
/// Command::new("claude") echoue donc en .app alors qu'il marche en dev.
fn chemin_claude(home: &str) -> String {
    let candidats = [
        format!("{}/.local/bin/claude", home),
        "/opt/homebrew/bin/claude".to_string(),
        "/usr/local/bin/claude".to_string(),
        format!("{}/.bun/bin/claude", home),
        format!("{}/.npm-global/bin/claude", home),
    ];
    candidats
        .into_iter()
        .find(|c| std::path::Path::new(c).is_file())
        // Dernier recours : le PATH, utile si l'app est lancee depuis un terminal.
        .unwrap_or_else(|| "claude".to_string())
}

async fn analyser(prompt: String) -> Result<String, String> {
    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    let (mut dossier_claude, sources, modele, racine_cours) = config_dossiers(&home);

    let mut prompt = prompt;
    if let Some(racine) = &racine_cours {
        let (choisi, reste) = choisir_cours(&prompt, racine);
        let mut actif = COURS_ACTIF.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(dossier) = choisi {
            *actif = Some(dossier);
            prompt = reste;
        }
        if let Some(dossier) = actif.clone() {
            dossier_claude = dossier;
        }
    }
    // Prefixe seul ("mgr") : on change de cours sans interroger Claude.
    if prompt.is_empty() {
        let nom = std::path::Path::new(&dossier_claude)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        return Ok(format!("Cours : {}", nom));
    }

    // Le dossier de travail est toujours annonce : sans ca, Claude y a bien acces
    // mais rien ne l'invite a y chercher, et il repond de memoire.
    let mut dossiers = vec![dossier_claude.clone()];
    dossiers.extend(sources.iter().cloned());

    // Pas d'incitation a fouiller : chaque lecture de fichier est un aller-retour
    // modele de plusieurs secondes. Le CLAUDE.md du cours dit ou chercher.
    let instruction = format!(
        "Réponds à : {}\n\n\
         Réponds directement si les résumés ou tes connaissances suffisent. \
         Sinon cherche dans : {} et cite le fichier utilisé.",
        prompt,
        dossiers.join(", ")
    );

    let binaire = chemin_claude(&home);
    // Le CLI peut lui-meme appeler node/git : on lui rend un PATH utilisable, que
    // le Finder ne fournit pas.
    let chemin_env = format!(
        "{home}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    );

    // Chaque question est independante : pas de --continue, sinon l'historique
    // est renvoye a chaque fois et tout ralentit.
    let mut cmd = Command::new(&binaire);
    let texte_resumes = resumes(&dossier_claude);
    if !texte_resumes.is_empty() {
        cmd.args([
            "--append-system-prompt",
            &format!(
                "Resumes du cours, deja lus pour toi (ne les relis pas) :\n\n{}",
                texte_resumes
            ),
        ]);
    }
    cmd.current_dir(&dossier_claude)
        .env("PATH", &chemin_env)
        // Mesure : 507 des 612 tokens d'une reponse de quiz etaient du thinking,
        // meme en --effort low. Sans, la reponse arrive nettement plus vite.
        .env("MAX_THINKING_TOKENS", "0")
        .args(["-p", &instruction, "--model", &modele, "--dangerously-skip-permissions"])
        // Sans les plugins/hooks/MCP du ~/.claude/settings.json ni les skills :
        // ~2s de demarrage en moins et un contexte bien plus court.
        .args(["--setting-sources", "project,local", "--strict-mcp-config"])
        .args(["--disable-slash-commands", "--effort", "low"]);
    for source in &sources {
        cmd.args(["--add-dir", source]);
    }

    // Le chemin est dans le message : une erreur de lancement veut presque toujours
    // dire "binaire introuvable", et savoir lequel a ete tente fait gagner du temps.
    let sortie = cmd
        .output()
        .map_err(|e| format!("Erreur CLI Claude ({}) : {}", binaire, e))?;

    if sortie.status.success() {
        Ok(String::from_utf8_lossy(&sortie.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&sortie.stderr).trim().to_string())
    }
}

#[tauri::command]
fn cacher_fenetre(app: AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.hide();
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    fn object_setClass(
        obj: *mut objc2::runtime::AnyObject,
        cls: *const objc2::runtime::AnyClass,
    ) -> *mut objc2::runtime::AnyObject;
}

/// Sous-classe de NSPanel qui accepte le statut de fenetre cle.
///
/// La fenetre est `decorations: false`, donc borderless, et macOS refuse le
/// clavier a une fenetre sans bordure : `canBecomeKeyWindow` repond non par
/// defaut, et `makeKeyWindow` etait purement ignore. Seul un override le corrige.
#[cfg(target_os = "macos")]
fn classe_panel_hud() -> &'static objc2::runtime::AnyClass {
    use objc2::declare::ClassBuilder;
    use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
    use objc2::{class, sel};
    use std::sync::OnceLock;

    // Pointeur brut plutot que `&AnyObject` : une reference porte une duree de vie
    // que `MethodImplementation` juge "pas assez generale".
    extern "C" fn peut_devenir_cle(_this: *mut AnyObject, _cmd: Sel) -> Bool {
        Bool::YES
    }

    // OnceLock : enregistrer deux fois le meme nom de classe fait paniquer.
    static CLASSE: OnceLock<usize> = OnceLock::new();
    let adresse = *CLASSE.get_or_init(|| {
        let mut constructeur = ClassBuilder::new("HudPanelNonActivante", class!(NSPanel))
            .expect("nom de classe deja pris");
        unsafe {
            constructeur.add_method(
                sel!(canBecomeKeyWindow),
                peut_devenir_cle as extern "C" fn(*mut AnyObject, Sel) -> Bool,
            );
        }
        constructeur.register() as *const AnyClass as usize
    });
    unsafe { &*(adresse as *const AnyClass) }
}

/// Convertit la NSWindow de Tauri en NSPanel non-activante.
///
/// C'est LE point qui bloquait : une `NSWindow` normale ne peut pas s'afficher
/// par-dessus le plein ecran d'une AUTRE app, quels que soient les bits de
/// collectionBehavior (`FullScreenAuxiliary` ne vaut que pour le plein ecran de
/// sa propre app). Seule une `NSPanel` avec le style `NonactivatingPanel` le peut
/// - c'est le mecanisme derriere Raycast, Alfred et Spotlight.
///
/// Non-activante veut dire : la barre prend le clavier sans rendre l'app active,
/// donc macOS ne te teleporte plus sur l'espace d'origine.
///
/// A n'appeler qu'une fois, au demarrage : changer la classe d'un objet vivant
/// est irreversible.
#[cfg(target_os = "macos")]
fn convertir_en_panel(win: &tauri::WebviewWindow) {
    use objc2::runtime::AnyObject;
    use objc2::msg_send;

    let Ok(pointeur) = win.ns_window() else {
        return;
    };
    unsafe {
        let ns_win: *mut AnyObject = pointeur as _;
        object_setClass(ns_win, classe_panel_hud());

        // NSWindowStyleMaskNonactivatingPanel = 1 << 7
        let style: usize = msg_send![ns_win, styleMask];
        let () = msg_send![ns_win, setStyleMask: style | 128usize];
        let () = msg_send![ns_win, setFloatingPanel: true];
        // Sans ca, la panel se masque des que l'app passe inactive - ce qui est
        // permanent pour une app non-activante.
        let () = msg_send![ns_win, setHidesOnDeactivate: false];
    }
}

#[cfg(not(target_os = "macos"))]
fn convertir_en_panel(_win: &tauri::WebviewWindow) {}

/// Reglages NSWindow qui font la discretion de la barre.
///
/// Reappliques a CHAQUE affichage et pas seulement au demarrage : Tauri
/// reinitialise la NSWindow apres le setup (alwaysOnTop, visible:false), ce qui
/// effacait `CanJoinAllSpaces` - la barre restait alors prisonniere de l'espace
/// ou elle etait nee et ne suivait pas un slide a trois doigts.
#[cfg(target_os = "macos")]
fn appliquer_style_macos(win: &tauri::WebviewWindow) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let Ok(pointeur) = win.ns_window() else {
        return;
    };
    unsafe {
        let ns_win: *mut AnyObject = pointeur as _;
        // 1 CanJoinAllSpaces | 16 Stationary | 64 IgnoresCycle | 256 FullScreenAuxiliary.
        // Ces bits ne suffisent QUE sur une NSPanel (voir convertir_en_panel).
        let () = msg_send![ns_win, setCollectionBehavior: 337usize];
        // NSScreenSaverWindowLevel : au-dessus du contenu plein ecran, qui passe
        // lui-meme au-dessus du niveau 101 utilise jusqu'ici.
        let () = msg_send![ns_win, setLevel: 1000isize];
        // NSUInteger : un isize ici fait paniquer objc2 au lancement.
        let () = msg_send![ns_win, setSharingType: 0usize];
    }
}

/// Presente la barre sur l'espace courant sans activer l'app.
///
/// `orderFrontRegardless` affiche une fenetre meme quand l'app est inactive, et
/// la pose sur l'espace ou tu te trouves. C'est ce qu'il faut ici : activer
/// l'app (activateIgnoringOtherApps) faisait l'inverse - macOS te ramenait sur
/// l'espace de la fenetre au lieu d'amener la fenetre sur le tien.
#[cfg(target_os = "macos")]
fn presenter_sans_activer(win: &tauri::WebviewWindow) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let Ok(pointeur) = win.ns_window() else {
        return;
    };
    unsafe {
        let ns_win: *mut AnyObject = pointeur as _;
        let () = msg_send![ns_win, orderFrontRegardless];
        // Une panel non-activante ne devient jamais cle d'elle-meme : sans ca la
        // barre s'affiche mais n'accepte aucune frappe. `makeKeyWindow` donne le
        // clavier sans activer l'app, contrairement a makeKeyAndOrderFront.
        let () = msg_send![ns_win, makeKeyWindow];
    }
}

#[cfg(not(target_os = "macos"))]
fn appliquer_style_macos(_win: &tauri::WebviewWindow) {}

#[cfg(not(target_os = "macos"))]
fn presenter_sans_activer(_win: &tauri::WebviewWindow) {}

fn ecran_du_curseur(win: &tauri::WebviewWindow) -> Option<tauri::Monitor> {
    let app = win.app_handle();
    app.cursor_position()
        .ok()
        .and_then(|p| app.monitor_from_point(p.x, p.y).ok().flatten())
        .or_else(|| win.primary_monitor().ok().flatten())
}

fn chemin_position() -> String {
    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    format!("{}/.claude-hud-position", home)
}

/// Position choisie a la souris, en decalage par rapport a l'origine de l'ecran.
///
/// Un decalage et non une position absolue : la barre retrouve ainsi le meme coin
/// sur n'importe quel ecran, quelle que soit la disposition des moniteurs.
fn lire_decalage() -> Option<(i32, i32)> {
    let contenu = fs::read_to_string(chemin_position()).ok()?;
    let (x, y) = contenu.trim().split_once(' ')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

// ponytail: une ecriture par evenement Moved, donc plusieurs par glissement. Le
// fichier fait 10 octets ; throttler risquerait de perdre la position finale,
// aucun evenement ne signalant la fin d'un glissement.
fn ecrire_decalage(x: i32, y: i32) {
    let _ = fs::write(chemin_position(), format!("{} {}", x, y));
}

/// Affiche la barre sur l'ecran ou se trouve le curseur, apres avoir reapplique
/// les reglages NSWindow (voir `appliquer_style_macos`).
fn afficher_sur_ecran_actif(win: &tauri::WebviewWindow) {
    if let (Some(ecran), Ok(taille)) = (ecran_du_curseur(win), win.outer_size()) {
        let origine = ecran.position();
        let dimensions = ecran.size();
        let (largeur, hauteur) = (dimensions.width as i32, dimensions.height as i32);
        let (l_barre, h_barre) = (taille.width as i32, taille.height as i32);

        let (dx, dy) = lire_decalage().unwrap_or((
            // Defaut : centre horizontalement, au premier tiers vertical - a
            // hauteur de regard plutot qu'au milieu, ou la barre masque ce qu'on lit.
            (largeur - l_barre) / 2,
            (hauteur - h_barre) / 3,
        ));

        // Bornage : un decalage enregistre sur un grand ecran placerait la barre
        // hors champ sur un plus petit, et elle serait introuvable.
        let dx = dx.clamp(0, (largeur - l_barre).max(0));
        let dy = dy.clamp(0, (hauteur - h_barre).max(0));

        let _ = win.set_position(tauri::PhysicalPosition::new(origine.x + dx, origine.y + dy));
    }

    appliquer_style_macos(win);

    // Une fenetre deja visible ignore show() : macOS n'a alors aucune raison de la
    // replacer, et elle reste sur l'espace ou elle etait. Le hide() force la
    // re-presentation, donc le deplacement vers l'espace courant.
    if win.is_visible().unwrap_or(false) {
        let _ = win.hide();
    }
    let _ = win.show();
    let _ = win.unminimize();
    // Pose la fenetre sur l'espace courant AVANT de demander le focus : l'ordre
    // inverse laisse macOS decider, et il choisit de te deplacer toi.
    presenter_sans_activer(win);
    let _ = win.set_focus();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .invoke_handler(tauri::generate_handler![interroger_claude, cacher_fenetre])
        .setup(|app| {
            let app_handle = app.handle().clone();

            // Accessory : pas d'icone Dock, pas de barre de menus, et surtout macOS
            // n'active plus l'app comme une app normale - c'est ce qui forcait un
            // retour a l'espace d'origine au lieu d'afficher la barre sur l'espace
            // courant.
            #[cfg(target_os = "macos")]
            let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            if let Some(win) = app.get_webview_window("main") {
                // La conversion en NSPanel est definitive : une seule fois, ici.
                convertir_en_panel(&win);
                appliquer_style_macos(&win);
            }

            // Discretion : la barre se masque des qu'elle perd le focus (clic
            // ailleurs, Cmd+Tab, changement d'espace). Cmd+Shift+Space la rappelle.
            if let Some(win) = app.get_webview_window("main") {
                let fenetre = win.clone();
                win.on_window_event(move |evenement| {
                    match evenement {
                        tauri::WindowEvent::Focused(false) => {
                            if maintenant_ms() >= GARDE_JUSQU_A.load(Ordering::SeqCst) {
                                let _ = fenetre.hide();
                            }
                        }
                        // Glissement par le chevron : on retient le coin choisi.
                        tauri::WindowEvent::Moved(position) => {
                            if let Ok(Some(ecran)) = fenetre.current_monitor() {
                                let origine = ecran.position();
                                ecrire_decalage(position.x - origine.x, position.y - origine.y);
                            }
                        }
                        _ => {}
                    }
                });
            }

            let bascule: Shortcut = "Cmd+Shift+Space".parse().unwrap();
            app.global_shortcut().on_shortcut(bascule, move |_app, _shortcut, event| {
                if event.state() == ShortcutState::Pressed {
                    if let Some(win) = app_handle.get_webview_window("main") {
                        // Toggle : 2e appui = fermeture.
                        //
                        // Ce toggle avait ete retire parce qu'il se trompait quand
                        // la barre pouvait rester coincee sur un autre espace :
                        // "visible" ne voulait pas dire "sous tes yeux". Depuis la
                        // conversion en NSPanel elle existe sur tous les espaces,
                        // donc les deux sont enfin equivalents.
                        if win.is_visible().unwrap_or(false) {
                            let _ = win.hide();
                        } else {
                            garder(GRACE_APRES_REPONSE_MS);
                            afficher_sur_ecran_actif(&win);
                        }
                    }
                }
            })?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Erreur au lancement");
}

#[cfg(test)]
mod tests {
    use super::config_dossiers;

    /// Reproduit le calcul de `afficher_sur_ecran_actif` pour le verifier seul :
    /// la fonction reelle a besoin d'une fenetre vivante, pas testable ici.
    fn borner(decalage: (i32, i32), ecran: (i32, i32), barre: (i32, i32)) -> (i32, i32) {
        (
            decalage.0.clamp(0, (ecran.0 - barre.0).max(0)),
            decalage.1.clamp(0, (ecran.1 - barre.1).max(0)),
        )
    }

    #[test]
    fn la_position_memorisee_reste_dans_l_ecran() {
        let barre = (1280, 400);
        let petit = (1920, 1080);

        // Position venue d'un grand ecran : ramenee dans les bornes du petit.
        assert_eq!(borner((3000, 1800), petit, barre), (640, 680));
        // Position deja valide : inchangee.
        assert_eq!(borner((100, 50), petit, barre), (100, 50));
        // Coin bas gauche demande : x=0 conserve, y ramene au bas exact.
        assert_eq!(borner((0, 99999), petit, barre), (0, 680));
        // Ecran plus petit que la barre : pas de borne negative, on colle a 0.
        assert_eq!(borner((500, 500), (800, 300), barre), (0, 0));
    }

    #[test]
    fn lit_les_dossiers_valides_et_ignore_le_reste() {
        let home = std::env::temp_dir().join("claude_hud_test_home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("Notes")).unwrap();
        std::fs::create_dir_all(home.join("Cours")).unwrap();
        std::fs::create_dir_all(home.join("Travail")).unwrap();
        std::fs::create_dir_all(home.join("Documents")).unwrap();
        let home_str = home.to_string_lossy().to_string();

        std::fs::write(
            home.join(".claude-hud-dossiers"),
            "# mes sources\n\ncwd: ~/Travail\nmodel: sonnet\n~/Notes\n/chemin/qui/nexiste/pas\n  ~/Cours  \n",
        )
        .unwrap();

        std::fs::create_dir_all(home.join("Travail/sem1")).unwrap();
        std::fs::write(home.join("Travail/sem1/RESUME.md"), "ITIL").unwrap();
        let r = super::resumes(&home.join("Travail").to_string_lossy());
        assert!(r.contains("sem1/RESUME.md") && r.contains("ITIL"), "resume injecte");
        assert_eq!(super::resumes("/nexiste/pas"), "");

        let (travail, sources, modele, _) = config_dossiers(&home_str);
        assert_eq!(travail, format!("{}/Travail", home_str), "cwd: doit etre lu");
        assert_eq!(modele, "sonnet", "model: doit etre lu");
        assert_eq!(
            sources,
            vec![
                format!("{}/Notes", home_str),
                format!("{}/Cours", home_str),
            ],
            "commentaire, ligne vide, espaces et chemin inexistant doivent sauter ; ~ doit etre resolu"
        );

        // cwd: pointant nulle part -> on retombe sur ~/Documents plutot que de
        // lancer Claude depuis un dossier fantome.
        std::fs::write(home.join(".claude-hud-dossiers"), "cwd: /nexiste/pas\n").unwrap();
        let (travail, sources, modele, _) = config_dossiers(&home_str);
        assert_eq!(travail, format!("{}/Documents", home_str));
        assert!(sources.is_empty());
        assert_eq!(modele, super::MODELE_DEFAUT, "sans model:, on garde le defaut");

        // Pas de fichier de config du tout -> defaut, aucune source, pas de panique.
        std::fs::remove_file(home.join(".claude-hud-dossiers")).unwrap();
        let (travail, sources, modele, _) = config_dossiers(&home_str);
        assert_eq!(travail, format!("{}/Documents", home_str));
        assert!(sources.is_empty());
        assert_eq!(modele, super::MODELE_DEFAUT);

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn le_prefixe_choisit_le_cours() {
        let racine = std::env::temp_dir().join("claude_hud_test_cours");
        let _ = std::fs::remove_dir_all(&racine);
        std::fs::create_dir_all(racine.join("MGR850")).unwrap();
        std::fs::create_dir_all(racine.join("GES800")).unwrap();
        let r = racine.to_string_lossy().to_string();
        let mgr = Some(format!("{}/MGR850", r));

        assert_eq!(super::choisir_cours("mgr quelle menace ?", &r), (mgr.clone(), "quelle menace ?".into()));
        assert_eq!(super::choisir_cours("MGR850: q", &r), (mgr.clone(), "q".into()));
        assert_eq!(super::choisir_cours("mgr", &r), (mgr, "".into()));
        // Cours absent, mot trop court, mot ordinaire : prompt intact.
        assert_eq!(super::choisir_cours("ate800 q", &r), (None, "ate800 q".into()));
        assert_eq!(super::choisir_cours("ge q", &r), (None, "ge q".into()));
        assert_eq!(super::choisir_cours("quelle pratique ?", &r), (None, "quelle pratique ?".into()));

        let _ = std::fs::remove_dir_all(&racine);
    }
}