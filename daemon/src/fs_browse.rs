//! Remote folder browsing for choosing a session working directory.
//!
//! Directories only, confined to `root` (HOME). Every path is canonicalized before
//! the containment check so symlinks cannot escape. No delete.
//! Renaming a directory breaks resuming the sessions inside it (Kiro stores the
//! absolute cwd), so it is blocked while a live/attached session is inside and
//! needs explicit confirmation when only historical sessions are.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Clone, PartialEq)]
pub struct FsError {
    pub code: &'static str,
    pub message: String,
}

impl FsError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    pub hidden: bool,
    /// Kiro sessions whose cwd is this directory or below it.
    pub session_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    pub path: String,
    pub parent: Option<String>,
    pub root: String,
    /// Sessions whose cwd is exactly this directory.
    pub sessions_here: usize,
    pub entries: Vec<DirEntry>,
}

/// Session cwd lists used for counting and rename protection.
#[derive(Debug, Default, Clone)]
pub struct SessionCwds {
    /// cwd of every known session (historical included).
    pub all: Vec<PathBuf>,
    /// cwd of sessions that are live in a terminal or attached by the daemon.
    pub live: HashSet<PathBuf>,
}

pub struct FsBrowser {
    root: PathBuf,
}

pub fn validate_name(name: &str) -> Result<(), FsError> {
    let bad = |m: &str| Err(FsError::new("BAD_REQUEST", m.to_string()));
    if name.is_empty() || name.trim().is_empty() {
        return bad("文件夹名不能为空");
    }
    if name.len() > 255 {
        return bad("文件夹名超过 255 字节");
    }
    if name == "." || name == ".." {
        return bad("文件夹名不能是 . 或 ..");
    }
    if name.contains('/') || name.contains('\0') {
        return bad("文件夹名不能包含 / ");
    }
    Ok(())
}

impl FsBrowser {
    pub fn new(root: &Path) -> Result<Self, FsError> {
        let root = root
            .canonicalize()
            .map_err(|e| FsError::new("INTERNAL", format!("root unavailable: {e}")))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Canonical existing directory inside root.
    fn resolve(&self, path: &str) -> Result<PathBuf, FsError> {
        let p = Path::new(path);
        if !p.is_absolute() {
            return Err(FsError::new("BAD_REQUEST", "path must be absolute"));
        }
        let canon = p
            .canonicalize()
            .map_err(|_| FsError::new("NOT_FOUND", format!("目录不存在：{path}")))?;
        if !canon.starts_with(&self.root) {
            return Err(FsError::new(
                "FORBIDDEN",
                format!("只能访问 {} 下的目录", self.root.display()),
            ));
        }
        if !canon.is_dir() {
            return Err(FsError::new("BAD_REQUEST", "不是目录"));
        }
        Ok(canon)
    }

    fn entry(&self, path: PathBuf, cwds: &SessionCwds) -> DirEntry {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        DirEntry {
            hidden: name.starts_with('.'),
            session_count: cwds.all.iter().filter(|c| c.starts_with(&path)).count(),
            path: path.to_string_lossy().to_string(),
            name,
        }
    }

    pub fn list(
        &self,
        path: Option<&str>,
        show_hidden: bool,
        cwds: &SessionCwds,
    ) -> Result<Listing, FsError> {
        let dir = match path {
            Some(p) => self.resolve(p)?,
            None => self.root.clone(),
        };
        let read = std::fs::read_dir(&dir)
            .map_err(|e| FsError::new("FORBIDDEN", format!("无法读取目录：{e}")))?;
        let mut entries = Vec::new();
        for e in read.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            // follow symlinks, but only keep those resolving inside root
            let Ok(canon) = e.path().canonicalize() else {
                continue;
            };
            if !canon.is_dir() || !canon.starts_with(&self.root) {
                continue;
            }
            let mut entry = self.entry(e.path(), cwds);
            entry.name = name.clone();
            entry.hidden = name.starts_with('.');
            entries.push(entry);
        }
        entries.sort_by_key(|e| e.name.to_lowercase());
        let parent = (dir != self.root)
            .then(|| dir.parent().map(|p| p.to_string_lossy().to_string()))
            .flatten();
        Ok(Listing {
            sessions_here: cwds.all.iter().filter(|c| *c == &dir).count(),
            path: dir.to_string_lossy().to_string(),
            parent,
            root: self.root.to_string_lossy().to_string(),
            entries,
        })
    }

    pub fn mkdir(&self, parent: &str, name: &str, cwds: &SessionCwds) -> Result<DirEntry, FsError> {
        validate_name(name)?;
        let parent = self.resolve(parent)?;
        let target = parent.join(name);
        if target.symlink_metadata().is_ok() {
            return Err(FsError::new("EXISTS", format!("已存在：{name}")));
        }
        std::fs::create_dir(&target)
            .map_err(|e| FsError::new("INTERNAL", format!("创建失败：{e}")))?;
        Ok(self.entry(target, cwds))
    }

    pub fn rename(
        &self,
        path: &str,
        new_name: &str,
        force: bool,
        cwds: &SessionCwds,
    ) -> Result<DirEntry, FsError> {
        validate_name(new_name)?;
        let src = self.resolve(path)?;
        if src == self.root {
            return Err(FsError::new("FORBIDDEN", "不能重命名 HOME 目录本身"));
        }
        let live = cwds.live.iter().filter(|c| c.starts_with(&src)).count();
        if live > 0 {
            return Err(FsError::new(
                "RENAME_BLOCKED",
                format!("该目录下有 {live} 个正在运行或已接管的会话，请先结束后再重命名"),
            ));
        }
        let historical = cwds.all.iter().filter(|c| c.starts_with(&src)).count();
        if historical > 0 && !force {
            return Err(FsError::new(
                "CONFIRM_REQUIRED",
                format!("该目录下有 {historical} 个历史会话，重命名后将无法续接。确认后重试"),
            ));
        }
        let parent = src
            .parent()
            .ok_or_else(|| FsError::new("FORBIDDEN", "no parent"))?;
        let dst = parent.join(new_name);
        if dst.symlink_metadata().is_ok() {
            return Err(FsError::new("EXISTS", format!("已存在：{new_name}")));
        }
        std::fs::rename(&src, &dst)
            .map_err(|e| FsError::new("INTERNAL", format!("重命名失败：{e}")))?;
        Ok(self.entry(dst, cwds))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn setup(name: &str) -> (PathBuf, FsBrowser) {
        let base = std::env::temp_dir().join(format!("agentdock-fs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("home");
        std::fs::create_dir_all(root.join("Code/proj")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        std::fs::write(root.join("file.txt"), "x").unwrap();
        symlink(base.join("outside"), root.join("escape")).unwrap();
        symlink(root.join("Code"), root.join("code-link")).unwrap();
        let b = FsBrowser::new(&root).unwrap();
        (base, b)
    }

    #[test]
    fn list_dirs_only_hidden_toggle_and_symlink_escape() {
        let (base, b) = setup("list");
        let root = b.root().to_path_buf();
        let cwds = SessionCwds {
            all: vec![
                root.join("Code/proj"),
                root.join("Code/proj"),
                root.join("Code"),
            ],
            ..Default::default()
        };
        let l = b.list(None, false, &cwds).unwrap();
        let names: Vec<_> = l.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Code", "code-link"],
            "no files, no hidden, no escaping symlink"
        );
        assert_eq!(l.parent, None);
        assert_eq!(l.entries[0].session_count, 3);
        let l = b.list(None, true, &cwds).unwrap();
        assert!(l.entries.iter().any(|e| e.name == ".hidden" && e.hidden));

        let code = b
            .list(Some(root.join("Code").to_str().unwrap()), false, &cwds)
            .unwrap();
        assert_eq!(code.sessions_here, 1);
        assert_eq!(code.parent.as_deref(), root.to_str());

        let esc = b
            .list(Some(root.join("escape").to_str().unwrap()), false, &cwds)
            .unwrap_err();
        assert_eq!(esc.code, "FORBIDDEN");
        assert_eq!(
            b.list(Some("/etc"), false, &cwds).unwrap_err().code,
            "FORBIDDEN"
        );
        let dotdot = format!("{}/Code/../../outside", root.display());
        assert_eq!(
            b.list(Some(&dotdot), false, &cwds).unwrap_err().code,
            "FORBIDDEN"
        );
        assert_eq!(
            b.list(Some("relative"), false, &cwds).unwrap_err().code,
            "BAD_REQUEST"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn mkdir_validation() {
        let (base, b) = setup("mkdir");
        let root = b.root().to_str().unwrap().to_string();
        let cwds = SessionCwds::default();
        let e = b.mkdir(&root, "新项目", &cwds).unwrap();
        assert!(Path::new(&e.path).is_dir());
        assert_eq!(b.mkdir(&root, "新项目", &cwds).unwrap_err().code, "EXISTS");
        for bad in ["", "  ", ".", "..", "a/b"] {
            assert_eq!(
                b.mkdir(&root, bad, &cwds).unwrap_err().code,
                "BAD_REQUEST",
                "{bad:?}"
            );
        }
        assert_eq!(
            b.mkdir(&format!("{root}/escape"), "x", &cwds)
                .unwrap_err()
                .code,
            "FORBIDDEN"
        );
        assert!(!base.join("outside/x").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn rename_protects_sessions() {
        let (base, b) = setup("rename");
        let root = b.root().to_path_buf();
        let code = root.join("Code");
        let proj = code.join("proj");
        let mut cwds = SessionCwds {
            all: vec![proj.clone()],
            live: HashSet::from([proj.clone()]),
        };
        let p = code.to_str().unwrap();
        assert_eq!(
            b.rename(p, "Code2", false, &cwds).unwrap_err().code,
            "RENAME_BLOCKED"
        );
        assert_eq!(
            b.rename(p, "Code2", true, &cwds).unwrap_err().code,
            "RENAME_BLOCKED",
            "force cannot bypass live"
        );
        cwds.live.clear();
        assert_eq!(
            b.rename(p, "Code2", false, &cwds).unwrap_err().code,
            "CONFIRM_REQUIRED"
        );
        let e = b.rename(p, "Code2", true, &cwds).unwrap();
        assert!(Path::new(&e.path).is_dir() && !code.exists());
        assert_eq!(
            b.rename(root.to_str().unwrap(), "x", true, &cwds)
                .unwrap_err()
                .code,
            "FORBIDDEN"
        );
        std::fs::create_dir(root.join("A")).unwrap();
        std::fs::create_dir(root.join("B")).unwrap();
        assert_eq!(
            b.rename(root.join("A").to_str().unwrap(), "B", true, &cwds)
                .unwrap_err()
                .code,
            "EXISTS"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }
}
