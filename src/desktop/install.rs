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
fn launcher(path: &Path, body: &str) -> Result<()> {
    backup(path)?;
    atomic_write(path, format!("#!/bin/sh\n{body}\n").as_bytes())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}
pub(super) fn skill() -> Result<Value> {
    let shared = paths::home().join(".agents/skills/recall");
    fs::create_dir_all(&shared)?;
    atomic_write(&shared.join("SKILL.md"), SKILL.as_bytes())?;
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
pub(super) fn desktop() -> Result<()> {
    let root = root()?;
    let binary = std::env::current_exe()?;
    if !engine()?.is_file() {
        bail!("Build the engine before setup");
    }
    let home = paths::home();
    let bin = home.join(".local/bin");
    fs::create_dir_all(&bin)?;
    let config = config_path()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let data = paths::support().parent().unwrap().to_path_buf();
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
    let plugin = config.join("omarchy/plugins/ferdous.recall");
    fs::create_dir_all(&plugin)?;
    let colors = fs::read_to_string(shell.join("shell/Commons/qmldir"))
        .unwrap_or_default()
        .contains("singleton ShellColor");
    for entry in fs::read_dir(root.join("omarchy/plugin"))? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let dest = plugin.join(entry.file_name());
        backup(&dest)?;
        let mut text = fs::read_to_string(entry.path())?;
        if colors {
            text = text.replace("Color.menu", "ShellColor.menu");
        }
        atomic_write(&dest, text.as_bytes())?;
    }
    let service = config.join("systemd/user/recall-for-omarchy.service");
    backup(&service)?;
    // systemd unit quoting is distinct from shell quoting; escape all specifiers and quoted-string characters.
    let executable = binary
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    atomic_write(&service,format!("[Unit]\nDescription=Recall local semantic search for Omarchy\nPartOf=graphical-session.target\nAfter=graphical-session.target\n\n[Service]\nType=simple\nExecStart=\"{executable}\" daemon\nRestart=on-failure\nRestartSec=5\nNice=10\nUMask=0077\nTimeoutStopSec=15\n\n[Install]\nWantedBy=graphical-session.target\n").as_bytes())?;
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
    let desktop = data.join("applications/io.github.ferdousbhai.Recall.desktop");
    let desktop_exec = bin
        .join("recall")
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    atomic_write(&desktop,format!("[Desktop Entry]\nType=Application\nName=Recall\nComment=Search local files by meaning\nExec=\"{desktop_exec}\" toggle\nIcon=io.github.ferdousbhai.Recall\nCategories=Utility;\nTerminal=false\n").as_bytes())?;
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
    checked(
        Command::new("omarchy-shell")
            .args(["shell", "rescanPlugins"])
            .env("OMARCHY_PATH", &shell),
    )?;
    checked(
        Command::new("omarchy")
            .args(["plugin", "enable", "ferdous.recall"])
            .env("OMARCHY_PATH", &shell),
    )?;
    println!(
        "{}",
        json!({"installed":true,"shortcut":"Super+Ctrl+Shift+F","settings":"recall settings"})
    );
    Ok(())
}
