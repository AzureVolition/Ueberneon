// ── Skills 模块 ──
//
// 技能 = 磁盘上的一个目录：<root>/<name>/SKILL.md
// 支持两个根目录（优先级从高到低）：
//   1. <project>/.ueberneon/skills/<name>/SKILL.md
//   2. ~/.ueberneon/skills/<name>/SKILL.md
//
// SKILL.md 支持可选 frontmatter：
//   ---
//   name: hallmark
//   description: 一句话说明
//   category: design
//   version: 1.0.0
//   ---
//   <指令正文>

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const SKILL_MANIFEST: &str = "SKILL.md";
/// 单个技能指令的最大体积（防止 load_skill 撑爆上下文）。
pub const MAX_SKILL_BYTES: usize = 64 * 1024;

/// 技能元数据（用于注册表/面板展示）。
#[derive(Debug, Clone)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub category: Option<String>,
    pub version: String,
    pub root: PathBuf,
}

/// 已加载的技能（含指令正文）。
#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub name: String,
    pub description: String,
    pub category: Option<String>,
    pub version: String,
    pub path: PathBuf,
    pub instructions: String,
}

/// 磁盘技能 + DB 状态合并后的注册表条目（面板数据源）。
#[derive(Debug, Clone, PartialEq)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub category: Option<String>,
    pub version: String,
    pub root: PathBuf,
    pub status: String,
    pub usage_count: i64,
    pub last_run_at: Option<String>,
}

/// 用户级技能根目录：~/.ueberneon/skills
pub fn user_skills_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".ueberneon").join("skills")
}

/// 项目级技能根目录：<project>/.ueberneon/skills
pub fn project_skills_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(".ueberneon").join("skills")
}

/// 扫描两个根目录，返回全部技能（按名称排序；同名时项目级优先）。
pub fn discover(project_dir: &Path) -> Vec<SkillMeta> {
    discover_with_roots(&project_skills_dir(project_dir), &user_skills_dir())
}

/// 显式指定两个根目录的扫描版本（测试可注入临时目录）。
pub fn discover_with_roots(project_root: &Path, user_root: &Path) -> Vec<SkillMeta> {
    let mut metas = Vec::new();
    scan_root(project_root, &mut metas);
    scan_root(user_root, &mut metas);
    metas.sort_by(|a, b| a.name.cmp(&b.name));
    metas.dedup_by(|a, b| a.name == b.name);
    metas
}

/// 按名称加载技能指令。找不到时返回错误（含查找路径）。
pub fn load(project_dir: &Path, name: &str) -> Result<LoadedSkill, String> {
    load_with_roots(&project_skills_dir(project_dir), &user_skills_dir(), name)
}

/// 显式指定两个根目录的加载版本（测试可注入临时目录）。
pub fn load_with_roots(
    project_root: &Path,
    user_root: &Path,
    name: &str,
) -> Result<LoadedSkill, String> {
    let roots = [project_root, user_root];
    for root in &roots {
        let manifest = root.join(name).join(SKILL_MANIFEST);
        if !manifest.is_file() {
            continue;
        }
        let content = std::fs::read_to_string(&manifest)
            .map_err(|e| format!("load_skill: failed to read {}: {e}", manifest.display()))?;
        if content.len() > MAX_SKILL_BYTES {
            return Err(format!(
                "load_skill: '{}' exceeds the {} byte limit",
                name, MAX_SKILL_BYTES
            ));
        }
        let fm = frontmatter(&content);
        let description = fm.get("description").cloned().unwrap_or_default();
        let category = fm.get("category").cloned();
        let version = fm
            .get("version")
            .cloned()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "file".to_string());
        return Ok(LoadedSkill {
            name: name.to_string(),
            description,
            category,
            version,
            path: manifest,
            instructions: strip_frontmatter(&content),
        });
    }
    Err(format!(
        "load_skill: skill '{}' not found (looked in {} and {})",
        name,
        roots[0].display(),
        roots[1].display()
    ))
}

/// 扫描磁盘并同步 DB 状态：为每个磁盘技能补状态行，清理已消失技能的状态。
/// 面板与 `load_skill` 共用此视图。
pub fn registry(project_dir: &Path) -> Vec<SkillEntry> {
    let metas = discover(project_dir);
    let names: Vec<String> = metas.iter().map(|m| m.name.clone()).collect();
    let states = crate::db::with_db(|conn| {
        for n in &names {
            let _ = crate::db::metadata::skill::ensure(conn, n);
        }
        let _ = crate::db::metadata::skill::prune_missing(conn, &names);
        crate::db::metadata::skill::list(conn).unwrap_or_default()
    });
    let mut by_name: HashMap<String, crate::db::metadata::skill::SkillStateRow> =
        states.into_iter().map(|s| (s.name.clone(), s)).collect();

    let mut entries = Vec::with_capacity(metas.len());
    for meta in metas {
        let state = by_name.remove(&meta.name);
        let (status, usage_count, last_run_at) = match state {
            Some(s) => (s.status, s.usage_count, s.last_run_at),
            None => ("enabled".to_string(), 0, None),
        };
        entries.push(SkillEntry {
            name: meta.name,
            description: meta.description,
            category: meta.category,
            version: meta.version,
            root: meta.root,
            status,
            usage_count,
            last_run_at,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// 启动时同步入口（当前实现由 [`registry`] 完成）。
pub fn sync_registry(project_dir: &Path) {
    let _ = registry(project_dir);
}

/// 卸载技能：删除技能目录（项目级优先）并清理状态。
pub fn uninstall(project_dir: &Path, name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("uninstall: invalid skill name".into());
    }
    let roots = [project_skills_dir(project_dir), user_skills_dir()];
    for root in &roots {
        let dir = root.join(name);
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| format!("uninstall: failed to remove {}: {e}", dir.display()))?;
            break;
        }
    }
    let _ = crate::db::with_db_result(|conn| crate::db::metadata::skill::delete(conn, name));
    Ok(())
}

/// 安装技能到用户级目录 `~/.ueberneon/skills/<name>/`。
/// - git 地址（http(s)/git@/ssh/.git）→ `git clone`
/// - 本地路径 → 递归拷贝目录
/// 成功后返回技能名。
pub fn install(source: &str) -> Result<String, String> {
    install_into(source, &user_skills_dir())
}

/// 安装到指定根目录（测试可注入临时目录）。
fn install_into(source: &str, target_root: &Path) -> Result<String, String> {
    let source = source.trim();
    if source.is_empty() {
        return Err("install skill: source is empty".into());
    }
    std::fs::create_dir_all(&target_root).map_err(|e| {
        format!(
            "install skill: failed to create {}: {e}",
            target_root.display()
        )
    })?;

    let is_git = source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("git@")
        || source.starts_with("ssh://")
        || source.starts_with("file://")
        || source.ends_with(".git");

    let name = if is_git {
        sanitize_name(
            source
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("imported_skill")
                .trim_end_matches(".git"),
        )
    } else {
        let path = Path::new(source);
        if !path.is_dir() {
            return Err(format!(
                "install skill: '{}' is not a local directory",
                source
            ));
        }
        // 优先取源目录 frontmatter 里的 name，否则取目录名
        let manifest = path.join(SKILL_MANIFEST);
        let dir_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("imported_skill");
        let fm_name = manifest
            .is_file()
            .then(|| {
                std::fs::read_to_string(&manifest)
                    .ok()
                    .and_then(|c| frontmatter(&c).get("name").cloned())
            })
            .flatten();
        sanitize_name(fm_name.as_deref().unwrap_or(dir_name))
    };

    if name.is_empty() {
        return Err("install skill: could not derive a skill name from the source".into());
    }

    let target = target_root.join(&name);
    if target.exists() {
        return Err(format!(
            "install skill: '{}' already exists at {}",
            name,
            target.display()
        ));
    }

    if is_git {
        if let Some((repo_url, subdir)) = parse_github_tree(source) {
            clone_github_subdir(&repo_url, &subdir, &target)?;
        } else {
            let output = std::process::Command::new("git")
                .args([
                    "clone",
                    "--depth",
                    "1",
                    source,
                    target.to_str().unwrap_or_default(),
                ])
                .output()
                .map_err(|e| format!("install skill: failed to run git clone: {e}"))?;
            if !output.status.success() {
                let _ = std::fs::remove_dir_all(&target);
                return Err(format!(
                    "install skill: git clone failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
    } else if let Err(e) = copy_dir(Path::new(source), &target) {
        let _ = std::fs::remove_dir_all(&target);
        return Err(format!("install skill: failed to copy '{}': {}", source, e));
    }

    // 校验 SKILL.md 存在；不存在则回滚
    if !target.join(SKILL_MANIFEST).is_file() {
        let _ = std::fs::remove_dir_all(&target);
        return Err(format!(
            "install skill: '{}' has no {} at its root",
            name, SKILL_MANIFEST
        ));
    }

    let _ = crate::db::with_db_result(|conn| crate::db::metadata::skill::ensure(conn, &name));
    Ok(name)
}

/// 解析 GitHub 的 tree 子目录链接：
/// `https://github.com/<owner>/<repo>/tree/<ref>/<subdir...>`
/// → `(repo_url, subdir)`。
fn parse_github_tree(url: &str) -> Option<(String, String)> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() < 5 || parts[2] != "tree" {
        return None;
    }
    let owner = parts[0];
    let repo = parts[1].trim_end_matches(".git");
    let repo_url = format!("https://github.com/{owner}/{repo}.git");
    // parts[3] 是 ref（main/master/commit hash），子目录从 parts[4..] 开始
    let subdir = parts[4..].join("/");
    Some((repo_url, subdir))
}

/// 用稀疏检出把 GitHub 仓库的某个子目录克隆到 target。
fn clone_github_subdir(repo_url: &str, subdir: &str, target: &Path) -> Result<(), String> {
    let tmp = std::env::temp_dir().join(format!(
        "ueberneon-skill-clone-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&tmp);

    let clone = std::process::Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "--filter=blob:none",
            "--sparse",
            repo_url,
            tmp.to_str().unwrap_or_default(),
        ])
        .output();
    let clone = match clone {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!(
                "install skill: git clone failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!("install skill: failed to run git clone: {e}"));
        }
    };

    let sparse = std::process::Command::new("git")
        .args([
            "-C",
            tmp.to_str().unwrap_or_default(),
            "sparse-checkout",
            "set",
            subdir,
        ])
        .output();
    let sparse = match sparse {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!(
                "install skill: sparse-checkout failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!("install skill: sparse-checkout failed: {e}"));
        }
    };
    let _ = (clone, sparse);

    let src = tmp.join(subdir);
    if !src.is_dir() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!(
            "install skill: subdir '{}' not found in repo",
            subdir
        ));
    }
    let result = copy_dir(&src, target);
    let _ = std::fs::remove_dir_all(&tmp);
    result.map_err(|e| format!("install skill: failed to copy skill dir: {e}"))
}

/// 递归拷贝目录。
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn sanitize_name(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn scan_root(root: &Path, out: &mut Vec<SkillMeta>) {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let manifest = dir.join(SKILL_MANIFEST);
        if !manifest.is_file() {
            continue;
        }
        let name = match dir.file_name().and_then(|s| s.to_str()) {
            Some(n) if !n.is_empty() && !n.starts_with('.') => n.to_string(),
            _ => continue,
        };
        let (description, category, version) = read_meta(&manifest);
        out.push(SkillMeta {
            name,
            description,
            category,
            version,
            root: dir,
        });
    }
}

fn read_meta(path: &Path) -> (String, Option<String>, String) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (String::new(), None, "file".into()),
    };
    let fm = frontmatter(&content);
    (
        fm.get("description").cloned().unwrap_or_default(),
        fm.get("category").cloned(),
        fm.get("version")
            .cloned()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "file".to_string()),
    )
}

/// 极简 frontmatter 解析：`---` 围栏内的 `key: value`。
fn frontmatter(content: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return map;
    }
    let rest = &trimmed[3..];
    let end = rest.find("\n---").unwrap_or(rest.len());
    for line in rest[..end].lines() {
        if let Some((k, v)) = line.split_once(':') {
            map.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    map
}

/// 去掉 frontmatter，返回指令正文。
fn strip_frontmatter(content: &str) -> String {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return content.to_string();
    }
    let rest = &trimmed[3..];
    if let Some(idx) = rest.find("\n---") {
        rest[idx + 4..].trim_start().to_string()
    } else {
        content.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_skill(root: &Path, name: &str, body: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join(SKILL_MANIFEST);
        let mut f = std::fs::File::create(&manifest).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        manifest
    }

    #[test]
    fn discover_and_load_skill() {
        let tmp = std::env::temp_dir().join(format!("_skill_test_{}", std::process::id()));
        let project = tmp.join("project");
        let user = tmp.join("user");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        let project_root = project.join(".ueberneon").join("skills");
        let user_root = user.join("skills");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&user_root).unwrap();

        make_skill(
            &project_root,
            "alpha",
            "---\nname: alpha\ndescription: test skill\ncategory: design\nversion: 1.2.0\n---\n# alpha\n\nuse it well.\n",
        );
        make_skill(
            &user_root,
            "beta",
            "---\ndescription: user skill\n---\nbeta instructions\n",
        );

        let metas = discover_with_roots(&project_root, &user_root);
        assert_eq!(metas.len(), 2);
        assert_eq!(metas[0].name, "alpha");
        assert_eq!(metas[0].description, "test skill");
        assert_eq!(metas[0].category.as_deref(), Some("design"));
        assert_eq!(metas[0].version, "1.2.0");
        assert_eq!(metas[1].name, "beta");

        let loaded = load_with_roots(&project_root, &user_root, "alpha").unwrap();
        assert_eq!(loaded.instructions, "# alpha\n\nuse it well.\n");
        assert_eq!(loaded.description, "test skill");

        let missing = load_with_roots(&project_root, &user_root, "nope");
        assert!(missing.is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn install_copies_local_dir() {
        let tmp = std::env::temp_dir().join(format!("_skill_install_{}", std::process::id()));
        let src = tmp.join("src-skill");
        std::fs::create_dir_all(&src).unwrap();
        let mut f = std::fs::File::create(src.join("SKILL.md")).unwrap();
        f.write_all(
            b"---\nname: installed_demo\ndescription: installed\n---\ninstalled instructions\n",
        )
        .unwrap();

        let target_root = tmp.join("target");
        let name = install_into(src.to_str().unwrap(), &target_root).unwrap();
        assert_eq!(name, "installed_demo");
        let manifest = target_root.join("installed_demo").join("SKILL.md");
        assert!(manifest.is_file());

        // 已存在时报错
        let dup = install_into(src.to_str().unwrap(), &target_root);
        assert!(dup.is_err());

        // 缺 SKILL.md 的目录安装后回滚
        let bad = tmp.join("bad-skill");
        std::fs::create_dir_all(&bad).unwrap();
        let bad_result = install_into(bad.to_str().unwrap(), &target_root);
        assert!(bad_result.is_err());
        assert!(!target_root.join("bad-skill").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn registry_overlays_state_and_uninstall_removes() {
        let tmp = std::env::temp_dir().join(format!("_skill_registry_{}", std::process::id()));
        let project = tmp.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let project_root = project.join(".ueberneon").join("skills");
        std::fs::create_dir_all(&project_root).unwrap();
        make_skill(
            &project_root,
            "registry_test",
            "---\ndescription: registry test\ncategory: demo\n---\nbody\n",
        );

        let entries = registry(&project);
        assert!(entries.iter().any(|e| e.name == "registry_test"));
        let entry = entries.iter().find(|e| e.name == "registry_test").unwrap();
        assert_eq!(entry.status, "enabled");

        crate::db::with_db(|conn| {
            crate::db::metadata::skill::set_status(conn, "registry_test", "disabled").unwrap();
        });
        let entries = registry(&project);
        let entry = entries.iter().find(|e| e.name == "registry_test").unwrap();
        assert_eq!(entry.status, "disabled");

        uninstall(&project, "registry_test").unwrap();
        assert!(!project_root.join("registry_test").exists());
        let entries = registry(&project);
        assert!(!entries.iter().any(|e| e.name == "registry_test"));
        crate::db::with_db(|conn| {
            assert!(
                crate::db::metadata::skill::get(conn, "registry_test")
                    .unwrap()
                    .is_none()
            );
        });

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parses_github_tree_url() {
        let (repo, subdir) =
            parse_github_tree("https://github.com/joshp123/ai-stack/tree/main/skills/grill-me")
                .unwrap();
        assert_eq!(repo, "https://github.com/joshp123/ai-stack.git");
        assert_eq!(subdir, "skills/grill-me");

        assert!(parse_github_tree("https://github.com/joshp123/ai-stack").is_none());
        assert!(parse_github_tree("https://github.com/joshp123/ai-stack/blob/main/x").is_none());
        assert!(parse_github_tree("https://example.com/repo/tree/main/x").is_none());
    }

    #[test]
    fn sparse_clone_installs_subdir() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tmp = std::env::temp_dir().join(format!("_skill_git_{}", std::process::id()));
        let repo = tmp.join("repo");
        std::fs::create_dir_all(repo.join("skills").join("demo")).unwrap();
        std::fs::write(
            repo.join("skills").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: demo\n---\nbody\n",
        )
        .unwrap();

        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap()
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        run(&["add", "."]);
        assert!(run(&["commit", "-m", "init"]).status.success());

        let target_root = tmp.join("target");
        let repo_url = format!("file://{}", repo.display());
        clone_github_subdir(&repo_url, "skills/demo", &target_root.join("demo")).unwrap();
        assert!(target_root.join("demo").join("SKILL.md").is_file());
        assert_eq!(
            std::fs::read_to_string(target_root.join("demo").join("SKILL.md")).unwrap(),
            "---\nname: demo\ndescription: demo\n---\nbody\n"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn project_root_wins_over_user_root() {
        let tmp = std::env::temp_dir().join(format!("_skill_dup_{}", std::process::id()));
        let project = tmp.join("project");
        let user = tmp.join("user");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        let project_root = project.join(".ueberneon").join("skills");
        let user_root = user.join("skills");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&user_root).unwrap();

        make_skill(
            &project_root,
            "same",
            "---\ndescription: project copy\n---\nproject body\n",
        );
        make_skill(
            &user_root,
            "same",
            "---\ndescription: user copy\n---\nuser body\n",
        );

        let metas = discover_with_roots(&project_root, &user_root);
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].description, "project copy");

        let loaded = load_with_roots(&project_root, &user_root, "same").unwrap();
        assert_eq!(loaded.instructions, "project body\n");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
