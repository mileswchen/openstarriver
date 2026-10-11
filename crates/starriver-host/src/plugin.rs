// Plugin registry + install/uninstall file operations.
//
// A Plugin is a bundle of one or more skills (SKILL.md files) installed from a
// local directory or a git URL. Its skills are copied into ~/.starriver/skills/
// with a "<plugin>-<skill>" prefix so they are discovered by the SkillManager.
// Installed plugins are tracked in ~/.starriver/plugins.yaml.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Metadata for one installed plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntry {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub skills: Vec<String>,
    /// FPM P3: frontend UI contribution (tabs/permissions/theme); None for pure-skill plugins without UI.
    #[serde(default)]
    pub ui: Option<PluginUiEntry>,
    #[serde(default)]
    pub source_url: String,
    #[serde(default)]
    pub installed_at: String,
}

/// Plugin UI manifest (`ui/manifest.json`, P3). `entry`/`theme` are paths relative to `ui/`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginUiEntry {
    /// Entry HTML, e.g. `index.html` (the iframe loads `starriver-plugin://<name>/ui/<entry>`).
    pub entry: String,
    pub tabs: Vec<PluginUiTab>,
    /// Declared permission whitelist (`starriver-protocol` permission constant strings).
    #[serde(default)]
    pub permissions: Vec<String>,
    /// Optional theme CSS file path (e.g. `theme.css`); its content is delivered with `plugins.list`.
    #[serde(default)]
    pub theme: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginUiTab {
    pub id: String,
    pub title: String,
}

/// Registry persisted to ~/.starriver/plugins.yaml (mirrors ProviderRegistry pattern).
pub struct PluginRegistry {
    plugins: Vec<PluginEntry>,
    config_path: PathBuf,
}

impl PluginRegistry {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_path(Self::default_path())
    }

    /// Construct a registry rooted at an arbitrary config path (used by tests).
    pub fn with_path(config_path: PathBuf) -> anyhow::Result<Self> {
        let plugins = if config_path.exists() {
            let content = std::fs::read_to_string(&config_path).unwrap_or_default();
            serde_yaml::from_str(&content).unwrap_or_default()
        } else {
            Vec::new()
        };
        Ok(Self {
            plugins,
            config_path,
        })
    }

    fn default_path() -> PathBuf {
        home_dir().join(".starriver").join("plugins.yaml")
    }

    pub fn list(&self) -> &[PluginEntry] {
        &self.plugins
    }

    pub fn get(&self, name: &str) -> Option<&PluginEntry> {
        self.plugins.iter().find(|p| p.name == name)
    }

    pub fn add(&mut self, entry: PluginEntry) -> anyhow::Result<()> {
        self.plugins.retain(|p| p.name != entry.name);
        self.plugins.push(entry);
        self.save()
    }

    pub fn remove(&mut self, name: &str) -> anyhow::Result<()> {
        self.plugins.retain(|p| p.name != name);
        self.save()
    }

    fn save(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let yaml = serde_yaml::to_string(&self.plugins)?;
        std::fs::write(&self.config_path, yaml)?;
        Ok(())
    }
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Recursively find all SKILL.md files under a directory.
fn find_skill_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(find_skill_files(&p));
            } else if p.file_name().map(|n| n == "SKILL.md").unwrap_or(false) {
                out.push(p);
            }
        }
    }
    out
}

/// Best-effort extraction of a frontmatter `name:` value from a markdown file.
fn frontmatter_name(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let fm = content.strip_prefix("---\n")?.split_once("\n---")?.0;
    for line in fm.lines() {
        if let Some(v) = line.strip_prefix("name:") {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// Skill name from its SKILL.md (frontmatter) or the containing directory name.
fn skill_name_from_file(path: &Path) -> String {
    frontmatter_name(path)
        .or_else(|| {
            path.parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "skill".to_string())
}

/// Plugin name from PLUGIN.md frontmatter or the directory name.
fn plugin_name_from_dir(dir: &Path) -> String {
    let plugin_md = dir.join("PLUGIN.md");
    if plugin_md.exists() {
        if let Some(n) = frontmatter_name(&plugin_md) {
            return n;
        }
    }
    dir.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "plugin".to_string())
}

/// Derive a safe plugin name from a git URL (last path segment, minus .git).
fn derive_plugin_name(source: &str) -> String {
    let cleaned = source.trim_end_matches('/');
    let seg = cleaned.rsplit(['/', ':']).next().unwrap_or("plugin");
    seg.trim_end_matches(".git").to_string()
}

/// Install a plugin from a local directory or a git URL into the user's HOME-based
/// skill/plugin dirs. See [`install_plugin_into`].
pub fn install_plugin(source: &str) -> anyhow::Result<PluginEntry> {
    install_plugin_into(&home_dir(), source)
}

/// Install a plugin rooted at `root` (used by tests to avoid touching real HOME).
/// Copies each bundled SKILL.md into <root>/.starriver/skills/<plugin>-<skill>/SKILL.md
/// and records the plugin in <root>/.starriver/plugins.yaml.
fn install_plugin_into(root: &Path, source: &str) -> anyhow::Result<PluginEntry> {
    // 1. Resolve the source directory (local path, or git clone into a staging dir).
    let source_path = Path::new(source);
    let staging: Option<PathBuf> = if source_path.is_dir() {
        None
    } else {
        let name = derive_plugin_name(source);
        let staging_dir = root.join(".starriver").join("plugins-src").join(&name);
        if staging_dir.exists() {
            std::fs::remove_dir_all(&staging_dir)?;
        }
        let ok = Command::new("git")
            .args([
                "clone",
                "--depth",
                "1",
                source,
                staging_dir.to_str().unwrap_or(""),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            anyhow::bail!("failed to clone plugin source: {source}");
        }
        Some(staging_dir)
    };
    let dir = staging.as_deref().unwrap_or(source_path);
    let cleanup = || -> std::io::Result<()> {
        if let Some(st) = &staging {
            std::fs::remove_dir_all(st)?;
        }
        Ok(())
    };

    // 2. Derive plugin name.
    let plugin_name = plugin_name_from_dir(dir);

    // 3. Scan for SKILL.md files.
    let skills = find_skill_files(dir);

    // 4. Copy the optional ui/ asset directory (FPM P3) and parse its manifest.
    let ui = install_ui_assets(root, dir)?;

    if skills.is_empty() && ui.is_none() {
        let _ = cleanup();
        anyhow::bail!("no SKILL.md or ui/ found in {dir:?}");
    }

    // 5. Copy each SKILL.md into the skills dir with a plugin-prefixed name.
    let sk_dir = root.join(".starriver").join("skills");
    std::fs::create_dir_all(&sk_dir)?;
    let mut skill_names = Vec::new();
    for f in &skills {
        let sname = skill_name_from_file(f);
        let target = sk_dir.join(format!("{plugin_name}-{sname}"));
        std::fs::create_dir_all(&target)?;
        std::fs::copy(f, target.join("SKILL.md"))?;
        skill_names.push(sname);
    }
    let _ = cleanup();

    // 6. Register in plugins.yaml.
    let mut reg = PluginRegistry::with_path(root.join(".starriver").join("plugins.yaml"))?;
    let installed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();
    let entry = PluginEntry {
        name: plugin_name,
        description: format!("Plugin with {} skill(s)", skill_names.len()),
        version: "0.1.0".into(),
        skills: skill_names,
        ui,
        source_url: source.to_string(),
        installed_at,
    };
    reg.add(entry.clone())?;
    Ok(entry)
}

/// Copy `<dir>/ui/` assets to `<root>/.starriver/plugins/<name>/ui/` and parse
/// `ui/manifest.json` into a [`PluginUiEntry`]; returns None if there is no ui/ directory.
fn install_ui_assets(root: &Path, dir: &Path) -> anyhow::Result<Option<PluginUiEntry>> {
    let ui_dir = dir.join("ui");
    if !ui_dir.is_dir() {
        return Ok(None);
    }
    let plugin_name = plugin_name_from_dir(dir);
    let target_root = root
        .join(".starriver")
        .join("plugins")
        .join(&plugin_name)
        .join("ui");
    std::fs::create_dir_all(&target_root)?;
    copy_dir(&ui_dir, &target_root)?;

    let manifest_path = ui_dir.join("manifest.json");
    let manifest = if manifest_path.exists() {
        let text = std::fs::read_to_string(&manifest_path)?;
        serde_json::from_str::<PluginUiEntry>(&text)?
    } else {
        PluginUiEntry {
            entry: "index.html".into(),
            tabs: Vec::new(),
            permissions: Vec::new(),
            theme: None,
        }
    };
    // The entry must stay inside ui/ (reject path traversal).
    if !is_safe_relative(&manifest.entry) || !target_root.join(&manifest.entry).is_file() {
        anyhow::bail!(
            "plugin ui manifest entry must be a file inside ui/: {}",
            manifest.entry
        );
    }
    if let Some(theme) = manifest.theme.as_deref() {
        if !is_safe_relative(theme) || !target_root.join(theme).is_file() {
            anyhow::bail!("plugin ui theme must be a file inside ui/: {theme}");
        }
    }
    Ok(Some(manifest))
}

/// A relative path must be a plain filename/subdirectory; reject `..`, absolute paths, and empty strings.
fn is_safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !Path::new(path).is_absolute()
        && path
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Uninstall a plugin from the user's HOME dirs. See [`uninstall_plugin_from`].
pub fn uninstall_plugin(name: &str) -> anyhow::Result<()> {
    uninstall_plugin_from(&home_dir(), name)
}

/// Uninstall a plugin rooted at `root`.
fn uninstall_plugin_from(root: &Path, name: &str) -> anyhow::Result<()> {
    let mut reg = PluginRegistry::with_path(root.join(".starriver").join("plugins.yaml"))?;
    let entry = reg
        .get(name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("plugin not installed: {name}"))?;
    let sk_dir = root.join(".starriver").join("skills");
    for s in &entry.skills {
        let target = sk_dir.join(format!("{name}-{s}"));
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
    }
    // FPM P3: remove the UI asset directory (including the theme).
    let ui_dir = root.join(".starriver").join("plugins").join(name);
    if ui_dir.exists() {
        std::fs::remove_dir_all(&ui_dir)?;
    }
    reg.remove(name)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugins_file(root: &Path) -> PathBuf {
        root.join(".starriver").join("plugins.yaml")
    }

    #[test]
    fn registry_add_get_remove() {
        let root = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_path(plugins_file(root.path())).unwrap();
        assert!(reg.list().is_empty());
        reg.add(PluginEntry {
            name: "p1".into(),
            description: "d".into(),
            version: "0.1".into(),
            skills: vec!["s1".into()],
            ui: None,
            source_url: "/x".into(),
            installed_at: "0".into(),
        })
        .unwrap();
        assert_eq!(reg.list().len(), 1);
        assert!(reg.get("p1").is_some());
        reg.remove("p1").unwrap();
        assert!(reg.list().is_empty());
    }

    #[test]
    fn registry_persists() {
        let root = tempfile::tempdir().unwrap();
        {
            let mut reg = PluginRegistry::with_path(plugins_file(root.path())).unwrap();
            reg.add(PluginEntry {
                name: "p".into(),
                description: "d".into(),
                version: "0.1".into(),
                skills: vec!["a".into(), "b".into()],
                ui: None,
                source_url: "/src".into(),
                installed_at: "1".into(),
            })
            .unwrap();
        }
        let reg = PluginRegistry::with_path(plugins_file(root.path())).unwrap();
        assert_eq!(reg.list().len(), 1);
        assert_eq!(reg.get("p").unwrap().skills, vec!["a", "b"]);
    }

    #[test]
    fn frontmatter_name_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("SKILL.md");
        std::fs::write(&f, "---\nname: my-skill\ndescription: x\n---\n\nbody\n").unwrap();
        assert_eq!(frontmatter_name(&f).as_deref(), Some("my-skill"));
    }

    #[test]
    fn install_from_local_dir_copies_skills() {
        let root = tempfile::tempdir().unwrap();
        // Build a fake plugin dir with two skills.
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("write-book")).unwrap();
        std::fs::write(
            src.path().join("write-book").join("SKILL.md"),
            "---\nname: write-book\n---\n\n# Write Book\n",
        )
        .unwrap();
        std::fs::create_dir_all(src.path().join("outline")).unwrap();
        std::fs::write(
            src.path().join("outline").join("SKILL.md"),
            "---\nname: outline\n---\n\n# Outline\n",
        )
        .unwrap();

        let entry = install_plugin_into(root.path(), src.path().to_str().unwrap()).unwrap();
        // Plugin name from the dir basename.
        let dir_name = src
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(entry.name, dir_name);
        assert_eq!(entry.skills.len(), 2);
        assert!(entry.skills.contains(&"write-book".into()));

        // Skills copied with plugin prefix.
        let sk = root.path().join(".starriver").join("skills");
        assert!(sk
            .join(format!("{dir_name}-write-book"))
            .join("SKILL.md")
            .exists());
        assert!(sk
            .join(format!("{dir_name}-outline"))
            .join("SKILL.md")
            .exists());

        // Registered in plugins.yaml.
        let reg = PluginRegistry::with_path(plugins_file(root.path())).unwrap();
        assert!(reg.get(&dir_name).is_some());
    }

    #[test]
    fn install_rejects_dir_without_skills() {
        let root = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        assert!(install_plugin_into(root.path(), src.path().to_str().unwrap()).is_err());
    }

    #[test]
    fn install_copies_ui_assets_and_manifest() {
        let root = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("ui")).unwrap();
        std::fs::write(
            src.path().join("ui").join("index.html"),
            "<!doctype html><script src=\"main.js\"></script>",
        )
        .unwrap();
        std::fs::write(src.path().join("ui").join("main.js"), "window.hello = 1;").unwrap();
        std::fs::write(
            src.path().join("ui").join("manifest.json"),
            r#"{
                "entry": "index.html",
                "tabs": [{ "id": "board", "title": "Board" }],
                "permissions": ["session:read"],
                "theme": "theme.css"
            }"#,
        )
        .unwrap();
        std::fs::write(
            src.path().join("ui").join("theme.css"),
            ":root { --bg: #111; }",
        )
        .unwrap();

        let entry = install_plugin_into(root.path(), src.path().to_str().unwrap()).unwrap();
        let ui = entry.ui.expect("ui manifest recorded");
        assert_eq!(ui.entry, "index.html");
        assert_eq!(ui.tabs.len(), 1);
        assert_eq!(ui.tabs[0].id, "board");
        assert_eq!(ui.permissions, vec!["session:read"]);
        assert_eq!(ui.theme.as_deref(), Some("theme.css"));

        let dir_name = src
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let ui_dir = root
            .path()
            .join(".starriver")
            .join("plugins")
            .join(&dir_name)
            .join("ui");
        assert!(ui_dir.join("index.html").exists());
        assert!(ui_dir.join("main.js").exists());
        assert!(ui_dir.join("theme.css").exists());
    }

    #[test]
    fn install_rejects_ui_entry_traversal() {
        let root = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("ui")).unwrap();
        std::fs::write(src.path().join("ui").join("index.html"), "x").unwrap();
        std::fs::write(
            src.path().join("ui").join("manifest.json"),
            r#"{ "entry": "../evil.html", "tabs": [] }"#,
        )
        .unwrap();
        assert!(install_plugin_into(root.path(), src.path().to_str().unwrap()).is_err());
    }

    #[test]
    fn ui_only_plugin_is_allowed() {
        let root = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("ui")).unwrap();
        std::fs::write(src.path().join("ui").join("index.html"), "x").unwrap();
        std::fs::write(
            src.path().join("ui").join("manifest.json"),
            r#"{ "entry": "index.html", "tabs": [{ "id": "t", "title": "T" }], "permissions": [] }"#,
        )
        .unwrap();
        let entry = install_plugin_into(root.path(), src.path().to_str().unwrap()).unwrap();
        assert!(entry.ui.is_some());
        assert!(entry.skills.is_empty());
    }

    #[test]
    fn uninstall_removes_skills_and_registry() {
        let root = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("s1")).unwrap();
        std::fs::write(
            src.path().join("s1").join("SKILL.md"),
            "---\nname: s1\n---\n\nbody\n",
        )
        .unwrap();
        let entry = install_plugin_into(root.path(), src.path().to_str().unwrap()).unwrap();
        let sk = root.path().join(".starriver").join("skills");
        assert!(sk.join(format!("{}-s1", entry.name)).exists());

        uninstall_plugin_from(root.path(), &entry.name).unwrap();
        assert!(!sk.join(format!("{}-s1", entry.name)).exists());
        assert!(PluginRegistry::with_path(plugins_file(root.path()))
            .unwrap()
            .get(&entry.name)
            .is_none());
    }

    #[test]
    fn derive_plugin_name_from_url() {
        assert_eq!(
            derive_plugin_name("https://github.com/a/writing.git"),
            "writing"
        );
        assert_eq!(
            derive_plugin_name("git@github.com:a/writing.git"),
            "writing"
        );
        assert_eq!(derive_plugin_name("/local/path/to/writing"), "writing");
    }
}
