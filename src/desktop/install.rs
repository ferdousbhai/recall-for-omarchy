use super::*;
use std::os::unix::fs::symlink;
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}
fn backup(path: &Path) -> Result<()> {
    if path.exists() {
        let name = format!(
            "{}.bak.{}",
            path.display(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        );
        fs::copy(path, name)?;
    }
    Ok(())
}
/// A launcher this command wrote (for any installation: a release, a package or a checkout).
fn generated_launcher(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|text| {
        let mut lines = text.lines();
        lines.next() == Some("#!/bin/sh")
            && lines.all(|line| {
                line.starts_with("exec '")
                    || line == "exec omarchy-shell \"$@\""
                    || line.starts_with("export OMARCHY_PATH='")
            })
    })
}
fn launcher(path: &Path, body: &str) -> Result<()> {
    // Rewriting one of our own launchers (moving to a new installation) needs no backup.
    if !generated_launcher(path) {
        backup(path)?;
    }
    atomic_write(path, format!("#!/bin/sh\n{body}\n").as_bytes())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}
/// Beside the skill, saying whose it is, as other tools' skills do: what syncs a person's own skills between their
/// computers (Omacloud) leaves a marked folder to the Recall installed on each.
const SKILL_MARKER: &str = ".managed-by-recall";
pub(super) fn skill() -> Result<Value> {
    let shared = paths::home().join(".agents/skills/recall");
    fs::create_dir_all(&shared)?;
    atomic_write(&shared.join("SKILL.md"), SKILL.as_bytes())?;
    atomic_write(&shared.join(SKILL_MARKER), b"")?;
    let codex = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home().join(".codex"))
        .join("skills/recall");
    let claude = paths::home().join(".claude/skills/recall");
    let mut links = Vec::new();
    for entry in [codex, claude] {
        if entry.symlink_metadata().is_err() {
            fs::create_dir_all(entry.parent().unwrap())?;
            symlink(&shared, &entry)?;
            links.push(entry);
        }
    }
    Ok(json!({"installed":shared.join("SKILL.md"),"links":links}))
}
/// The running Omarchy shell's root (an `omarchy dev link` checkout, say), else $OMARCHY_PATH or /usr/share/omarchy.
fn shell_root() -> Result<PathBuf> {
    let mut shell = std::env::var_os("OMARCHY_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/usr/share/omarchy".into());
    let instances = Command::new("qs").args(["list", "--all"]).output()?;
    for line in String::from_utf8_lossy(&instances.stdout).lines() {
        if let Some((_, path)) = line.split_once("Config path:") {
            let path = Path::new(path.trim());
            if path.file_name().is_some_and(|p| p == "shell.qml")
                && path
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|p| p == "shell")
            {
                shell = path.parent().unwrap().parent().unwrap().to_path_buf();
                break;
            }
        }
    }
    Ok(shell)
}
fn config_home() -> PathBuf {
    config_path().parent().unwrap().parent().unwrap().to_path_buf()
}
fn plugin_dir() -> PathBuf {
    config_home().join("omarchy/plugins/ferdous.recall")
}
/// Copy this installation's panel into the user's Omarchy plugins; true when a file changed.
/// The plugin folder belongs to Recall, so files are replaced without backups.
fn install_plugin(shell: &Path) -> Result<bool> {
    let colors = fs::read_to_string(shell.join("shell/Commons/qmldir"))
        .unwrap_or_default()
        .contains("singleton ShellColor");
    copy_plugin(&root()?.join("omarchy/plugin"), &plugin_dir(), colors)
}
/// The QML beside the manifest, and the translation catalogs in `i18n/` that the panel reads at run time. A catalog
/// that is no longer shipped is removed, or the panel would keep showing a language's outdated strings.
fn copy_plugin(source: &Path, plugin: &Path, colors: bool) -> Result<bool> {
    let mut changed = false;
    for folder in ["", "i18n"] {
        let from = source.join(folder);
        if !from.is_dir() {
            continue;
        }
        let to = plugin.join(folder);
        fs::create_dir_all(&to)?;
        let mut shipped = Vec::new();
        for entry in fs::read_dir(&from)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            shipped.push(entry.file_name());
            let dest = to.join(entry.file_name());
            let mut text = fs::read_to_string(entry.path())?;
            if colors && dest.extension().is_some_and(|e| e == "qml") {
                text = text.replace("Color.menu", "ShellColor.menu");
            }
            if fs::read(&dest).ok().as_deref() != Some(text.as_bytes()) {
                atomic_write(&dest, text.as_bytes())?;
                changed = true;
            }
        }
        if folder == "i18n" {
            for entry in fs::read_dir(&to)? {
                let entry = entry?;
                if entry.path().extension().is_some_and(|e| e == "json") && !shipped.contains(&entry.file_name()) {
                    fs::remove_file(entry.path())?;
                    changed = true;
                }
            }
        }
    }
    Ok(changed)
}
/// The app launcher entry, with `Name[code]` and `Comment[code]` for every language whose catalog translates
/// `desktop.name` or `desktop.comment`: the launcher then follows the desktop's language like the panel does. The
/// catalogs are the panel's (omarchy/plugin/i18n), so a language is added in one file.
fn desktop_entry(exec: &str, catalogs: &Path) -> String {
    let catalog = |path: &Path| -> Option<Value> { serde_json::from_slice(&fs::read(path).ok()?).ok() };
    // A desktop entry is line-based, so a value is one line with backslashes doubled; anything else is left out.
    let line = |catalog: &Value, key: &str| -> Option<String> {
        let text = catalog[key].as_str()?.trim();
        (!text.is_empty() && !text.chars().any(char::is_control)).then(|| text.replace('\\', "\\\\"))
    };
    let english = catalog(&catalogs.join("en.json")).unwrap_or(Value::Null);
    let name = line(&english, "desktop.name").unwrap_or_else(|| "Recall".into());
    let comment = line(&english, "desktop.comment").unwrap_or_else(|| "Search local files by meaning".into());
    let mut entry = format!("[Desktop Entry]\nType=Application\nName={name}\n");
    let mut translated = Vec::new();
    for file in fs::read_dir(catalogs).into_iter().flatten().flatten() {
        let path = file.path();
        let code = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        // Omarchy names a regional language `pt-BR`; a desktop entry's locale is `pt_BR`.
        let valid = !code.is_empty() && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if valid && code != "en" && path.extension().is_some_and(|e| e == "json") {
            if let Some(catalog) = catalog(&path) {
                translated.push((code.replace('-', "_"), catalog));
            }
        }
    }
    translated.sort_by(|a, b| a.0.cmp(&b.0));
    // A translation that repeats the English adds nothing; the launcher falls back to the plain key by itself.
    for (code, catalog) in &translated {
        if let Some(text) = line(catalog, "desktop.name").filter(|text| *text != name) {
            entry += &format!("Name[{code}]={text}\n");
        }
    }
    entry += &format!("Comment={comment}\n");
    for (code, catalog) in &translated {
        if let Some(text) = line(catalog, "desktop.comment").filter(|text| *text != comment) {
            entry += &format!("Comment[{code}]={text}\n");
        }
    }
    entry + &format!("Exec=\"{exec}\" toggle\nIcon=io.github.ferdousbhai.Recall\nCategories=Utility;\nTerminal=false\n")
}
fn desktop_file() -> PathBuf {
    paths::support().parent().unwrap().join("applications/io.github.ferdousbhai.Recall.desktop")
}
/// Write the launcher entry for this installation; true when it changed.
fn install_desktop_entry() -> Result<bool> {
    let exec = paths::home()
        .join(".local/bin/recall")
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    let entry = desktop_entry(&exec, &root()?.join("omarchy/plugin/i18n"));
    let path = desktop_file();
    if fs::read(&path).ok().as_deref() == Some(entry.as_bytes()) {
        return Ok(false);
    }
    atomic_write(&path, entry.as_bytes())?;
    Ok(true)
}
fn rescan_plugins(shell: &Path) -> Result<()> {
    checked(
        Command::new("omarchy-shell")
            .args(["shell", "rescanPlugins"])
            .env("OMARCHY_PATH", shell),
    )
}
/// When the service starts, bring what `setup` copied for this user up to this version: the panel and the skill.
/// A package update replaces the binaries but cannot write into home folders; the restarted service does that.
/// Nothing is created that setup did not create first.
pub(super) fn refresh() -> Result<()> {
    if plugin_dir().join("manifest.json").is_file() {
        let shell = shell_root()?;
        if install_plugin(&shell)? {
            rescan_plugins(&shell)?;
        }
    }
    // The launcher entry carries the translations this version ships.
    if desktop_file().is_file() {
        install_desktop_entry()?;
    }
    let skill = paths::home().join(".agents/skills/recall/SKILL.md");
    if skill.is_file() {
        if fs::read(&skill)? != SKILL.as_bytes() {
            atomic_write(&skill, SKILL.as_bytes())?;
        }
        // A skill installed before the marker existed gets it now.
        let marker = skill.with_file_name(SKILL_MARKER);
        if !marker.exists() {
            atomic_write(&marker, b"")?;
        }
    }
    Ok(())
}
fn unit(executable: &str) -> String {
    format!("[Unit]\nDescription=Recall local semantic search for Omarchy\nPartOf=graphical-session.target\nAfter=graphical-session.target\n\n[Service]\nType=simple\nExecStart=\"{executable}\" daemon\nRestart=on-failure\nRestartSec=5\nNice=10\nUMask=0077\nTimeoutStopSec=15\n\n[Install]\nWantedBy=graphical-session.target\n")
}
/// A unit file this command wrote, whichever installation it started.
fn generated_unit(path: &Path) -> bool {
    let without_exec = |text: &str| text.lines().filter(|l| !l.starts_with("ExecStart=")).collect::<Vec<_>>().join("\n");
    fs::read_to_string(path).is_ok_and(|text| without_exec(&text) == without_exec(&unit("")))
}
pub(super) fn desktop() -> Result<()> {
    let root = root()?;
    let binary = std::env::current_exe()?;
    if !engine()?.is_file() {
        bail!("Build the engine before setup");
    }
    let home = paths::home();
    let bin = home.join(".local/bin");
    fs::create_dir_all(&bin)?;
    let config = config_home();
    let data = paths::support().parent().unwrap().to_path_buf();
    let shell = shell_root()?;
    launcher(
        &bin.join("recall"),
        &format!("exec {} \"$@\"", shell_quote(&binary)),
    )?;
    launcher(
        &bin.join("recall-client"),
        &format!("exec {} client \"$@\"", shell_quote(&binary)),
    )?;
    launcher(
        &bin.join("recall-engine"),
        &format!("exec {} \"$@\"", shell_quote(&engine()?)),
    )?;
    launcher(
        &bin.join("recall-shell"),
        &format!(
            "export OMARCHY_PATH={}\nexec omarchy-shell \"$@\"",
            shell_quote(&shell)
        ),
    )?;
    skill()?;
    if !config_path().exists() {
        save_config(&defaults())?;
    }
    install_plugin(&shell)?;
    let service = config.join("systemd/user/recall-for-omarchy.service");
    if !generated_unit(&service) {
        backup(&service)?;
    }
    // systemd unit quoting is distinct from shell quoting; escape all specifiers and quoted-string characters.
    let executable = binary
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    atomic_write(&service, unit(&executable).as_bytes())?;
    // Opt out with RECALL_NO_KEYBINDING=1. The block is marked so it can be removed cleanly.
    let bindings = config.join("hypr/bindings.lua");
    let marker = "-- recall-for-omarchy: start";
    if bindings.exists() && std::env::var_os("RECALL_NO_KEYBINDING").is_none() {
        let original = fs::read_to_string(&bindings)?;
        if !original.contains(marker) {
            backup(&bindings)?;
            let text = format!("{original}\n{marker}\no.bind(\"SUPER + CTRL + SHIFT + F\", \"Recall semantic search\", \"recall toggle\")\n-- recall-for-omarchy: end\n");
            atomic_write(&bindings, text.as_bytes())?;
            checked(Command::new("hyprctl").arg("reload"))?;
            // `hyprctl configerrors` exits 0 even when the config has errors; inspect its output instead.
            let report = Command::new("hyprctl").arg("configerrors").output()?;
            let errors = String::from_utf8_lossy(&report.stdout).trim().to_string();
            if !report.status.success() || !(errors.is_empty() || errors.eq_ignore_ascii_case("no errors")) {
                atomic_write(&bindings, original.as_bytes())?;
                let _ = Command::new("hyprctl").arg("reload").status();
                bail!("Hyprland reported config errors after adding the Recall keybinding; restored bindings.lua: {errors}");
            }
        }
    }
    // Migrate from the upstream app ID used by releases up to v0.4.2.
    for old in [
        data.join("applications/io.github.pdstn12.Recall.desktop"),
        data.join("icons/hicolor/scalable/apps/io.github.pdstn12.Recall.svg"),
    ] {
        if old.exists() {
            fs::remove_file(&old)?;
        }
    }
    install_desktop_entry()?;
    let icon = data.join("icons/hicolor/scalable/apps/io.github.ferdousbhai.Recall.svg");
    fs::create_dir_all(icon.parent().unwrap())?;
    fs::copy(root.join("omarchy/assets/recall.svg"), icon)?;
    checked(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    checked(Command::new("systemctl").args([
        "--user",
        "enable",
        "recall-for-omarchy.service",
    ]))?;
    checked(Command::new("systemctl").args(["--user", "restart", "recall-for-omarchy.service"]))?;
    rescan_plugins(&shell)?;
    checked(
        Command::new("omarchy")
            .args(["plugin", "enable", "ferdous.recall"])
            .env("OMARCHY_PATH", &shell),
    )?;
    // Releases up to v0.5.0 installed as tarballs under ~/.local/share/recall-for-omarchy/releases. The launchers and
    // the service now start this installation, so those copies are unused. Settings, index and models are elsewhere.
    let releases = data.join("recall-for-omarchy/releases");
    if let Ok(old) = releases.canonicalize() {
        if !binary.starts_with(&old) {
            fs::remove_dir_all(&old)?;
            let _ = fs::remove_dir(releases.parent().unwrap());
        }
    }
    println!(
        "{}",
        json!({"installed":true,"shortcut":"Super+Ctrl+Shift+F","settings":"recall settings"})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_launcher_entry_is_translated_from_the_panel_catalogs() {
        let catalogs = tempfile::tempdir().unwrap();
        let write = |name: &str, value: Value| fs::write(catalogs.path().join(name), value.to_string()).unwrap();
        write("en.json", json!({"desktop.name":"Recall","desktop.comment":"Search local files by meaning"}));
        write("da.json", json!({"desktop.name":"Recall","desktop.comment":"Søg i lokale filer efter mening"}));
        write("pt-BR.json", json!({"desktop.comment":"Pesquise arquivos locais\npelo significado","search.private":"x"}));
        write("ar.json", json!({"desktop.name":"ريكول","desktop.comment":"ابحث في الملفات"}));
        fs::write(catalogs.path().join("notes.txt"), "not a catalog").unwrap();
        let entry = desktop_entry("/home/me/.local/bin/recall", catalogs.path());
        assert_eq!(
            entry,
            "[Desktop Entry]\nType=Application\nName=Recall\nName[ar]=ريكول\nComment=Search local files by meaning\n\
             Comment[ar]=ابحث في الملفات\nComment[da]=Søg i lokale filer efter mening\n\
             Exec=\"/home/me/.local/bin/recall\" toggle\nIcon=io.github.ferdousbhai.Recall\nCategories=Utility;\nTerminal=false\n"
        );
        // Without catalogs the entry is still complete, in English.
        let bare = desktop_entry("recall", &catalogs.path().join("missing"));
        assert!(bare.contains("Name=Recall\nComment=Search local files by meaning\nExec=\"recall\" toggle\n"));
    }
    #[test]
    fn the_shipped_english_catalog_names_the_launcher_entry() {
        let catalogs = Path::new(env!("CARGO_MANIFEST_DIR")).join("omarchy/plugin/i18n");
        let entry = desktop_entry("recall", &catalogs);
        assert!(entry.contains("\nName=Recall\n") && entry.contains("\nComment=Search local files by meaning\n"), "{entry}");
    }
    #[test]
    fn the_panel_is_copied_with_its_catalogs_and_stale_ones_are_removed() {
        let source = tempfile::tempdir().unwrap();
        let plugin = tempfile::tempdir().unwrap();
        fs::create_dir(source.path().join("i18n")).unwrap();
        fs::write(source.path().join("Panel.qml"), "color: Commons.Color.menu.text").unwrap();
        fs::write(source.path().join("i18n/en.json"), "{\"a\":\"Color.menu\"}").unwrap();
        fs::create_dir(plugin.path().join("i18n")).unwrap();
        fs::write(plugin.path().join("i18n/xx.json"), "{}").unwrap();
        assert!(copy_plugin(source.path(), plugin.path(), true).unwrap());
        assert_eq!(fs::read_to_string(plugin.path().join("Panel.qml")).unwrap(), "color: Commons.ShellColor.menu.text");
        // Only QML is adapted to an older shell's colour singleton; a catalog is copied as it is.
        assert_eq!(fs::read_to_string(plugin.path().join("i18n/en.json")).unwrap(), "{\"a\":\"Color.menu\"}");
        assert!(!plugin.path().join("i18n/xx.json").exists());
        assert!(!copy_plugin(source.path(), plugin.path(), true).unwrap(), "a second copy changes nothing");
    }
}
