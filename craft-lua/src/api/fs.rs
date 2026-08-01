use std::cmp::Reverse;
use std::collections::HashSet;
use std::fs::FileType;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use mlua::{Lua, Result as LuaResult, Table};

use crate::api::util::pair::{err_pair, pair, try_pair};
use crate::plugin_permissions::{
    Permission::{FsRead, FsWrite},
    PluginPermissions,
};

pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = craft_storage::paths::home() {
            return home.join(rest);
        }
    } else if path == "~"
        && let Some(home) = craft_storage::paths::home()
    {
        return home;
    }
    PathBuf::from(path)
}

fn make_absolute(path: &str) -> LuaResult<PathBuf> {
    let p = expand_tilde(path);
    if p.is_absolute() {
        Ok(p)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&p))
            .map_err(|e| mlua::Error::runtime(format!("cannot resolve cwd: {e}")))
    }
}

fn path_to_string(p: &Path) -> LuaResult<String> {
    p.to_str()
        .map(|s| s.to_owned())
        .ok_or_else(|| mlua::Error::runtime("non-utf8 path"))
}

fn filetype_str(ft: &FileType) -> &'static str {
    if ft.is_file() {
        "file"
    } else if ft.is_dir() {
        "directory"
    } else if ft.is_symlink() {
        "link"
    } else {
        "unknown"
    }
}

fn collect_dir_entries(
    base: &Path,
    dir: &Path,
    depth: u32,
    max_depth: u32,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<(String, &'static str)>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.strip_prefix(base).ok().and_then(|p| p.to_str()) {
            Some(s) => s.to_owned(),
            None => continue,
        };
        let (type_str, is_dir) = match entry.file_type() {
            Ok(ft) if ft.is_symlink() => match std::fs::metadata(&path) {
                Ok(meta) => (filetype_str(&meta.file_type()), meta.is_dir()),
                Err(_) => ("link", false),
            },
            Ok(ft) => (filetype_str(&ft), ft.is_dir()),
            Err(_) => ("unknown", false),
        };
        out.push((name, type_str));
        if is_dir && depth < max_depth {
            let canonical = match path.canonicalize() {
                Ok(c) => c,
                Err(_) => continue,
            };
            if visited.insert(canonical) {
                collect_dir_entries(base, &path, depth + 1, max_depth, visited, out);
            }
        }
    }
}

pub(crate) fn create_fs_table(lua: &Lua, perms: &PluginPermissions) -> LuaResult<Table> {
    let t = lua.create_table()?;
    let perms = perms.clone();

    let p = perms.clone();
    t.set(
        "read",
        lua.create_async_function(move |_lua, path: String| {
            let p = p.clone();
            async move {
                if !p.is_allowed(FsRead) {
                    return Err(crate::plugin_permissions::denied_error(FsRead));
                }
                let abs = make_absolute(&path)?;
                match tokio::fs::read_to_string(&abs).await {
                    Ok(s) => Ok((Some(s), None)),
                    Err(e) if e.kind() == ErrorKind::InvalidData => {
                        Err(mlua::Error::runtime("non-utf8 content; use read_bytes"))
                    }
                    Err(e) => Ok(err_pair(e)),
                }
            }
        })?,
    )?;

    let p = perms.clone();
    t.set(
        "read_bytes",
        lua.create_async_function(move |lua, path: String| {
            let p = p.clone();
            async move {
                if !p.is_allowed(FsRead) {
                    return Err(crate::plugin_permissions::denied_error(FsRead));
                }
                let abs = make_absolute(&path)?;
                let bytes = try_pair!(tokio::fs::read(&abs).await);
                Ok((Some(lua.create_buffer(bytes)?), None))
            }
        })?,
    )?;

    let p = perms.clone();
    t.set(
        "metadata",
        lua.create_async_function(move |lua, path: String| {
            let p = p.clone();
            async move {
                if !p.is_allowed(FsRead) {
                    return Err(crate::plugin_permissions::denied_error(FsRead));
                }
                let abs = make_absolute(&path)?;
                match tokio::fs::metadata(&abs).await {
                    Ok(meta) => {
                        let tbl = lua.create_table()?;
                        tbl.set("size", meta.len())?;
                        tbl.set("is_file", meta.is_file())?;
                        tbl.set("is_dir", meta.is_dir())?;
                        if let Ok(modified) = meta.modified()
                            && let Ok(dur) = modified.duration_since(UNIX_EPOCH)
                        {
                            tbl.set("mtime", dur.as_secs_f64())?;
                        }
                        Ok((Some(tbl), None))
                    }
                    Err(e) if e.kind() == ErrorKind::NotFound => Ok((None, None)),
                    Err(e) => Ok(err_pair(e)),
                }
            }
        })?,
    )?;

    t.set(
        "dirname",
        lua.create_function(|_, file: String| {
            Ok(Path::new(&file)
                .parent()
                .and_then(|p| p.to_str())
                .map(|s| s.to_owned()))
        })?,
    )?;

    t.set(
        "basename",
        lua.create_function(|_, file: String| {
            Ok(Path::new(&file)
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_owned()))
        })?,
    )?;

    t.set(
        "joinpath",
        lua.create_function(|_, parts: mlua::Variadic<String>| {
            let mut buf = PathBuf::new();
            for part in parts.iter() {
                buf.push(part);
            }
            path_to_string(&buf)
        })?,
    )?;

    t.set(
        "normalize",
        lua.create_function(|_, path: String| {
            let abs = make_absolute(&path)?;
            let mut components = Vec::new();
            for comp in abs.components() {
                match comp {
                    Component::ParentDir => {
                        components.pop();
                    }
                    Component::CurDir => {}
                    _ => components.push(comp),
                }
            }
            let result: PathBuf = components.iter().collect();
            path_to_string(&result)
        })?,
    )?;

    t.set(
        "abspath",
        lua.create_function(|_, path: String| path_to_string(&make_absolute(&path)?))?,
    )?;

    t.set(
        "parents",
        lua.create_function(|lua, start: String| {
            let p = Path::new(&start);
            let tbl = lua.create_table()?;
            let mut i = 1;
            let mut current = p.parent();
            while let Some(parent) = current {
                if let Some(s) = parent.to_str() {
                    tbl.set(i, s)?;
                    i += 1;
                }
                current = parent.parent();
            }
            Ok(tbl)
        })?,
    )?;

    let p = perms.clone();
    t.set(
        "root",
        lua.create_async_function(move |_, (source, marker): (String, mlua::Value)| {
            let p = p.clone();
            async move {
                if !p.is_allowed(FsRead) {
                    return Err(crate::plugin_permissions::denied_error(FsRead));
                }

                let markers: Vec<String> = match marker {
                    mlua::Value::String(s) => vec![s.to_str()?.to_owned()],
                    mlua::Value::Table(t) => {
                        let mut v = Vec::new();
                        for pair in t.sequence_values::<String>() {
                            v.push(pair?);
                        }
                        v
                    }
                    _ => {
                        return Err(mlua::Error::runtime(
                            "fs.root: marker must be a string or list of strings",
                        ));
                    }
                };

                tokio::task::spawn_blocking(move || {
                    let start = Path::new(&source);
                    let start = if start.is_file() || !start.exists() {
                        start.parent().unwrap_or(start)
                    } else {
                        start
                    };

                    let mut dir = make_absolute(start.to_str().unwrap_or_default())?;

                    loop {
                        for m in &markers {
                            if dir.join(m).exists() {
                                return Ok(Some(path_to_string(&dir)?));
                            }
                        }
                        if !dir.pop() {
                            return Ok(None);
                        }
                    }
                })
                .await
                .map_err(|e| mlua::Error::runtime(format!("task failed: {e}")))?
            }
        })?,
    )?;

    t.set(
        "relpath",
        lua.create_function(|_, (base, target): (String, String)| {
            let base_comps: Vec<_> = Path::new(&base).components().collect();
            let target_comps: Vec<_> = Path::new(&target).components().collect();

            let common = base_comps
                .iter()
                .zip(target_comps.iter())
                .take_while(|(a, b)| a == b)
                .count();

            let mut result = PathBuf::new();
            for _ in common..base_comps.len() {
                result.push("..");
            }
            for comp in &target_comps[common..] {
                result.push(comp);
            }
            path_to_string(&result)
        })?,
    )?;

    t.set(
        "ext",
        lua.create_function(|_, file: String| {
            Ok(Path::new(&file)
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_owned()))
        })?,
    )?;

    let p = perms.clone();
    t.set(
        "dir",
        lua.create_async_function(move |lua, (path, opts): (String, Option<Table>)| {
            let p = p.clone();
            async move {
                if !p.is_allowed(FsRead) {
                    return Err(crate::plugin_permissions::denied_error(FsRead));
                }
                let abs = make_absolute(&path)?;
                let max_depth: u32 = match &opts {
                    Some(t) => t.get::<u32>("depth").unwrap_or(1),
                    None => 1,
                };

                let result = tokio::task::spawn_blocking(
                    move || -> Result<Vec<(String, &'static str)>, String> {
                        if !abs.exists() {
                            return Err(format!("dir: path does not exist: {}", abs.display()));
                        }
                        if !abs.is_dir() {
                            return Err(format!("dir: not a directory: {}", abs.display()));
                        }
                        let mut out = Vec::new();
                        let mut visited = HashSet::new();
                        collect_dir_entries(&abs, &abs, 1, max_depth, &mut visited, &mut out);
                        Ok(out)
                    },
                )
                .await
                .map_err(|e| mlua::Error::runtime(format!("task failed: {e}")))?;

                let entries = try_pair!(result);
                let tbl = lua.create_table()?;
                for (i, (name, typ)) in entries.iter().enumerate() {
                    let entry = lua.create_table()?;
                    entry.set(1, name.as_str())?;
                    entry.set(2, *typ)?;
                    tbl.set(i + 1, entry)?;
                }
                Ok((Some(tbl), None))
            }
        })?,
    )?;

    let p = perms.clone();
    t.set(
        "write",
        lua.create_async_function(move |_lua, (path, content): (String, String)| {
            let p = p.clone();
            async move {
                if !p.is_allowed(FsWrite) {
                    return Err(crate::plugin_permissions::denied_error(FsWrite));
                }
                let abs = make_absolute(&path)?;
                Ok(pair(tokio::fs::write(&abs, content).await.map(|()| true)))
            }
        })?,
    )?;

    let p = perms.clone();
    t.set(
        "atomic_write",
        lua.create_async_function(move |_lua, (path, content): (String, String)| {
            let p = p.clone();
            async move {
                if !p.is_allowed(FsWrite) {
                    return Err(crate::plugin_permissions::denied_error(FsWrite));
                }
                let abs = make_absolute(&path)?;
                let result = tokio::task::spawn_blocking(move || {
                    craft_storage::atomic_write(&abs, content.as_bytes())
                })
                .await
                .map_err(|e| mlua::Error::runtime(format!("join error: {e}")))?;
                Ok(pair(result.map(|()| true)))
            }
        })?,
    )?;

    let p = perms.clone();
    t.set(
        "rm",
        lua.create_async_function(move |_lua, (path, opts): (String, Option<Table>)| {
            let p = p.clone();
            async move {
                if !p.is_allowed(FsWrite) {
                    return Err(crate::plugin_permissions::denied_error(FsWrite));
                }
                let abs = make_absolute(&path)?;
                let recursive = opts
                    .as_ref()
                    .and_then(|t| t.get::<bool>("recursive").ok())
                    .unwrap_or(false);
                let force = opts
                    .as_ref()
                    .and_then(|t| t.get::<bool>("force").ok())
                    .unwrap_or(false);
                let result = async {
                    let meta = match tokio::fs::symlink_metadata(&abs).await {
                        Ok(m) => m,
                        Err(e) if force && e.kind() == ErrorKind::NotFound => {
                            return Ok::<(), std::io::Error>(());
                        }
                        Err(e) => return Err(e),
                    };
                    if meta.is_dir() {
                        if recursive {
                            tokio::fs::remove_dir_all(&abs).await
                        } else {
                            tokio::fs::remove_dir(&abs).await
                        }
                    } else {
                        match tokio::fs::remove_file(&abs).await {
                            Ok(()) => Ok(()),
                            Err(e) if meta.file_type().is_symlink() => {
                                tokio::fs::remove_dir(&abs).await.map_err(|_| e)
                            }
                            Err(e) => Err(e),
                        }
                    }
                }
                .await;
                Ok(pair(result.map(|()| true)))
            }
        })?,
    )?;

    let p = perms.clone();
    t.set(
        "mkdir",
        lua.create_async_function(move |_lua, (path, opts): (String, Option<Table>)| {
            let p = p.clone();
            async move {
                if !p.is_allowed(FsWrite) {
                    return Err(crate::plugin_permissions::denied_error(FsWrite));
                }
                let abs = make_absolute(&path)?;
                let parents = opts
                    .as_ref()
                    .and_then(|t| t.get::<bool>("parents").ok())
                    .unwrap_or(false);
                let result = if parents {
                    tokio::fs::create_dir_all(&abs).await
                } else {
                    tokio::fs::create_dir(&abs).await
                };
                Ok(pair(result.map(|()| true)))
            }
        })?,
    )?;

    let p = perms.clone();
    t.set(
        "glob",
        lua.create_async_function(move |lua, (patterns, opts): (mlua::Value, Option<Table>)| {
            let p = p.clone();
            async move {
                if !p.is_allowed(FsRead) {
                    return Err(crate::plugin_permissions::denied_error(FsRead));
                }

                let patterns: Vec<String> = match patterns {
                    mlua::Value::String(s) => vec![s.to_str()?.to_owned()],
                    mlua::Value::Table(t) => {
                        let mut v = Vec::new();
                        for val in t.sequence_values::<String>() {
                            v.push(val?);
                        }
                        v
                    }
                    _ => {
                        return Err(mlua::Error::runtime(
                            "glob: patterns must be a string or array of strings",
                        ));
                    }
                };

                let path = opts.as_ref().and_then(|t| t.get::<String>("path").ok());
                let limit = opts.as_ref().and_then(|t| t.get::<usize>("limit").ok());
                let gitignore = opts
                    .as_ref()
                    .and_then(|t| t.get::<bool>("gitignore").ok())
                    .unwrap_or(true);
                let sort = opts.as_ref().and_then(|t| t.get::<String>("sort").ok());
                let sort_mtime = sort.as_deref() == Some("mtime");

                let result: Result<Vec<String>, String> = tokio::task::spawn_blocking(move || {
                    let root = craft_agent::tools::resolve_search_path(path.as_deref())?;
                    let pattern_refs: Vec<&str> = patterns.iter().map(|s| s.as_str()).collect();

                    let walker =
                        craft_agent::tools::walk_builder_opts(&root, &pattern_refs, gitignore)?
                            .build();

                    let iter = walker
                        .flatten()
                        .filter(|e| e.file_type().is_some_and(|ft| ft.is_file()));

                    let paths: Vec<String> = if sort_mtime {
                        let mut entries: Vec<_> = iter
                            .filter_map(|e| {
                                let p = e.into_path();
                                let mt = craft_agent::tools::mtime(&p);
                                p.to_str().map(|s| (mt, s.to_owned()))
                            })
                            .collect();
                        entries.sort_unstable_by_key(|e| Reverse(e.0));
                        if let Some(lim) = limit {
                            entries.truncate(lim);
                        }
                        entries.into_iter().map(|(_, s)| s).collect()
                    } else {
                        let bounded: Box<dyn Iterator<Item = _>> = match limit {
                            Some(lim) => Box::new(iter.take(lim)),
                            None => Box::new(iter),
                        };
                        bounded
                            .filter_map(|e| e.into_path().to_str().map(|s| s.to_owned()))
                            .collect()
                    };

                    Ok(paths)
                })
                .await
                .map_err(|e| mlua::Error::runtime(format!("task failed: {e}")))?;

                let paths = try_pair!(result.map_err(|e| format!("glob: {e}")));
                let tbl = lua.create_table()?;
                for (i, path) in paths.iter().enumerate() {
                    tbl.set(i + 1, path.as_str())?;
                }
                Ok((Some(tbl), None))
            }
        })?,
    )?;

    let p = perms.clone();
    t.set(
        "grep",
        lua.create_async_function(move |lua, (pattern, opts): (String, Option<Table>)| {
            let p = p.clone();
            async move {
                if !p.is_allowed(FsRead) {
                    return Err(crate::plugin_permissions::denied_error(FsRead));
                }

                let mut params = craft_agent::tools::grep::GrepParams::new(pattern);
                if let Some(opts) = opts {
                    if let Ok(v) = opts.get::<String>("path") {
                        params.path = Some(v);
                    }
                    if let Ok(v) = opts.get::<String>("include") {
                        params.include = Some(v);
                    }
                    if let Ok(v) = opts.get::<usize>("context_before") {
                        params.context_before = v;
                    }
                    if let Ok(v) = opts.get::<usize>("context_after") {
                        params.context_after = v;
                    }
                    if let Ok(v) = opts.get::<usize>("limit") {
                        params.limit = v;
                    }
                    if let Ok(v) = opts.get::<usize>("max_line_bytes") {
                        params.max_line_bytes = v;
                    }
                }

                let result = tokio::task::spawn_blocking(move || {
                    craft_agent::tools::grep::grep_search(params)
                })
                .await
                .map_err(|e| mlua::Error::runtime(format!("task failed: {e}")))?;

                let (base, entries) = try_pair!(result);
                let arr = lua.create_table()?;
                for (i, entry) in entries.iter().enumerate() {
                    let etbl = lua.create_table()?;
                    etbl.set("path", base.join(&entry.path).to_string_lossy().as_ref())?;
                    let groups_tbl = lua.create_table()?;
                    for (gi, group) in entry.groups.iter().enumerate() {
                        let gtbl = lua.create_table()?;
                        let lines_tbl = lua.create_table()?;
                        for (li, line) in group.lines.iter().enumerate() {
                            let ltbl = lua.create_table()?;
                            ltbl.set("line_nr", line.line_nr)?;
                            ltbl.set("text", line.text.as_str())?;
                            ltbl.set("is_match", line.is_match)?;
                            lines_tbl.set(li + 1, ltbl)?;
                        }
                        gtbl.set("lines", lines_tbl)?;
                        groups_tbl.set(gi + 1, gtbl)?;
                    }
                    etbl.set("groups", groups_tbl)?;
                    arr.set(i + 1, etbl)?;
                }
                Ok((Some(arr), None))
            }
        })?,
    )?;

    Ok(t)
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::time::{Duration, SystemTime};

    use super::*;
    use mlua::Lua;
    use tempfile::TempDir;

    const FIRST_CONTENT: &str = "first";
    const REPLACEMENT_CONTENT: &str = "replacement";

    #[tokio::test]
    async fn read_file_ok() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("hello.txt");
        std::fs::write(&file, "world").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let read: mlua::Function = tbl.get("read").unwrap();
        let result: String = read.call_async(file.to_str().unwrap()).await.unwrap();
        assert_eq!(result, "world");
    }

    #[tokio::test]
    async fn dir_lists_entries() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let dir: mlua::Function = tbl.get("dir").unwrap();
        let (result, err): (Table, mlua::Value) = dir
            .call_async::<(Table, mlua::Value)>(tmp.path().to_str().unwrap())
            .await
            .unwrap();
        assert!(matches!(err, mlua::Value::Nil), "dir should succeed");

        let mut names: Vec<String> = Vec::new();
        let mut types: Vec<String> = Vec::new();
        for i in 1..=result.len().unwrap() {
            let entry: Table = result.get(i).unwrap();
            names.push(entry.get::<String>(1).unwrap());
            types.push(entry.get::<String>(2).unwrap());
        }
        names.sort();
        assert_eq!(names, vec!["a.txt", "sub"]);
        assert!(types.contains(&"file".to_owned()));
        assert!(types.contains(&"directory".to_owned()));
    }

    #[tokio::test]
    async fn dir_recursive() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("d")).unwrap();
        std::fs::write(tmp.path().join("d/nested.txt"), "").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let dir: mlua::Function = tbl.get("dir").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("depth", 2).unwrap();

        let (result, err): (Table, mlua::Value) = dir
            .call_async::<(Table, mlua::Value)>((tmp.path().to_str().unwrap(), opts))
            .await
            .unwrap();
        assert!(matches!(err, mlua::Value::Nil));

        let mut names: Vec<String> = Vec::new();
        for i in 1..=result.len().unwrap() {
            let entry: Table = result.get(i).unwrap();
            names.push(entry.get::<String>(1).unwrap());
        }
        names.sort();
        assert!(names.contains(&"d".to_owned()));
        assert!(names.iter().any(|n| n.contains("nested.txt")));
    }

    #[tokio::test]
    async fn dir_nonexistent_returns_nil_err() {
        let tmp = TempDir::new().unwrap();
        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let dir: mlua::Function = tbl.get("dir").unwrap();
        let missing = tmp.path().join("does_not_exist");
        let (val, err): (mlua::Value, mlua::Value) = dir
            .call_async::<(mlua::Value, mlua::Value)>(missing.to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(
            val,
            mlua::Value::Nil,
            "dir should return nil for nonexistent path"
        );
        assert!(
            matches!(err, mlua::Value::String(_)),
            "dir should return error for nonexistent path"
        );
    }

    #[tokio::test]
    async fn metadata_file_dir_and_missing() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("probe.txt");
        std::fs::write(&file, "hello").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let metadata: mlua::Function = tbl.get("metadata").unwrap();

        let f: Table = metadata
            .call_async::<Table>(file.to_str().unwrap())
            .await
            .unwrap();
        assert!(f.get::<bool>("is_file").unwrap());
        assert!(!f.get::<bool>("is_dir").unwrap());
        assert_eq!(f.get::<u64>("size").unwrap(), 5);
        assert!(f.get::<f64>("mtime").unwrap() > 0.0);

        let d: Table = metadata
            .call_async::<Table>(tmp.path().to_str().unwrap())
            .await
            .unwrap();
        assert!(!d.get::<bool>("is_file").unwrap());
        assert!(d.get::<bool>("is_dir").unwrap());

        let missing = tmp.path().join("nope");
        let nil: mlua::Value = metadata
            .call_async(missing.to_str().unwrap())
            .await
            .unwrap();
        assert!(matches!(nil, mlua::Value::Nil));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dir_follows_symlinks() {
        let tmp = TempDir::new().unwrap();
        let real_dir = tmp.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        std::fs::write(real_dir.join("inner.txt"), "").unwrap();
        std::os::unix::fs::symlink(&real_dir, tmp.path().join("link")).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let dir: mlua::Function = tbl.get("dir").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("depth", 2u32).unwrap();

        let (result, err): (Table, mlua::Value) = dir
            .call_async::<(Table, mlua::Value)>((tmp.path().to_str().unwrap(), opts))
            .await
            .unwrap();
        assert!(matches!(err, mlua::Value::Nil));

        let mut names: Vec<String> = Vec::new();
        let mut types: Vec<String> = Vec::new();
        for i in 1..=result.len().unwrap() {
            let entry: Table = result.get(i).unwrap();
            names.push(entry.get::<String>(1).unwrap());
            types.push(entry.get::<String>(2).unwrap());
        }

        assert!(names.iter().any(|n| n.contains("inner.txt")));
        let link_idx = names.iter().position(|n| n == "link").unwrap();
        assert_eq!(types[link_idx], "directory");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dir_dangling_symlink() {
        let tmp = TempDir::new().unwrap();
        std::os::unix::fs::symlink("/nonexistent_target_xyz", tmp.path().join("broken")).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let dir: mlua::Function = tbl.get("dir").unwrap();

        let (result, err): (Table, mlua::Value) = dir
            .call_async::<(Table, mlua::Value)>(tmp.path().to_str().unwrap())
            .await
            .unwrap();
        assert!(matches!(err, mlua::Value::Nil), "dir should succeed");

        let mut found = false;
        for i in 1..=result.len().unwrap() {
            let entry: Table = result.get(i).unwrap();
            let name: String = entry.get::<String>(1).unwrap();
            if name == "broken" {
                let typ: String = entry.get::<String>(2).unwrap();
                assert_eq!(typ, "link");
                found = true;
            }
        }
        assert!(found, "dangling symlink should still appear in listing");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dir_symlink_cycle_does_not_loop() {
        let tmp = TempDir::new().unwrap();
        let child = tmp.path().join("child");
        std::fs::create_dir(&child).unwrap();
        std::os::unix::fs::symlink(tmp.path(), child.join("loop")).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let dir: mlua::Function = tbl.get("dir").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("depth", 10u32).unwrap();

        let (result, err): (Table, mlua::Value) = dir
            .call_async::<(Table, mlua::Value)>((tmp.path().to_str().unwrap(), opts))
            .await
            .unwrap();
        assert!(matches!(err, mlua::Value::Nil));

        let len = result.len().unwrap();
        assert!(
            len < 20,
            "symlink cycle produced {len} entries, expected bounded"
        );
    }

    #[tokio::test]
    async fn read_missing_returns_nil_err() {
        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();

        for func_name in ["read", "read_bytes"] {
            let f: mlua::Function = tbl.get(func_name).unwrap();
            let (val, err): (mlua::Value, mlua::Value) =
                f.call_async("/nonexistent/path").await.unwrap();
            assert_eq!(val, mlua::Value::Nil, "{func_name} should return nil");
            assert!(
                matches!(err, mlua::Value::String(_)),
                "{func_name} should return error"
            );
        }
    }

    #[tokio::test]
    async fn write_creates_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("new.txt");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let write: mlua::Function = tbl.get("write").unwrap();
        let (ok, err): (mlua::Value, mlua::Value) = write
            .call_async((file.to_str().unwrap(), "hello world"))
            .await
            .unwrap();
        assert!(
            matches!(ok, mlua::Value::Boolean(true)),
            "write should succeed"
        );
        assert!(matches!(err, mlua::Value::Nil));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn write_overwrites_existing() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("overwrite.txt");
        std::fs::write(&file, "old").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let write: mlua::Function = tbl.get("write").unwrap();
        write
            .call_async::<(mlua::Value, mlua::Value)>((file.to_str().unwrap(), "new"))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
    }

    #[tokio::test]
    async fn write_to_nonexistent_parent_returns_error() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("no_parent/deep/file.txt");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let write: mlua::Function = tbl.get("write").unwrap();
        let (ok, err): (mlua::Value, mlua::Value) = write
            .call_async((file.to_str().unwrap(), "data"))
            .await
            .unwrap();
        assert!(matches!(ok, mlua::Value::Nil), "should fail");
        assert!(
            matches!(err, mlua::Value::String(_)),
            "should return error string"
        );
    }

    #[tokio::test]
    async fn atomic_write_creates_and_replaces_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("state.json");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let atomic_write: mlua::Function = tbl.get("atomic_write").unwrap();

        for content in [FIRST_CONTENT, REPLACEMENT_CONTENT] {
            let (ok, err): (mlua::Value, mlua::Value) = atomic_write
                .call_async((file.to_str().unwrap(), content))
                .await
                .unwrap();
            assert!(matches!(ok, mlua::Value::Boolean(true)), "should succeed");
            assert!(matches!(err, mlua::Value::Nil), "no error expected");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), content);
        }
    }

    #[tokio::test]
    async fn atomic_write_returns_error_when_parent_is_missing() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("missing/state.json");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let atomic_write: mlua::Function = tbl.get("atomic_write").unwrap();

        let (ok, err): (mlua::Value, mlua::Value) = atomic_write
            .call_async((file.to_str().unwrap(), FIRST_CONTENT))
            .await
            .unwrap();
        assert!(matches!(ok, mlua::Value::Nil), "should fail");
        assert!(
            matches!(err, mlua::Value::String(_)),
            "should return error string"
        );
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn atomic_write_requires_fs_write_permission() {
        const FS_WRITE_PERMISSION: &str = "fs_write";

        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("state.json");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::denied()).unwrap();
        let atomic_write: mlua::Function = tbl.get("atomic_write").unwrap();

        let error = atomic_write
            .call_async::<(mlua::Value, mlua::Value)>((file.to_str().unwrap(), FIRST_CONTENT))
            .await
            .unwrap_err();

        assert!(
            error.to_string().contains(FS_WRITE_PERMISSION),
            "error should mention {FS_WRITE_PERMISSION}: {error}"
        );
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn rm_deletes_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("doomed.txt");
        std::fs::write(&file, "bye").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let (ok, _): (mlua::Value, mlua::Value) =
            rm.call_async(file.to_str().unwrap()).await.unwrap();
        assert!(matches!(ok, mlua::Value::Boolean(true)));
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn rm_nonexistent_returns_error() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("ghost.txt");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let (ok, err): (mlua::Value, mlua::Value) =
            rm.call_async(file.to_str().unwrap()).await.unwrap();
        assert!(
            matches!(ok, mlua::Value::Nil),
            "should fail for nonexistent"
        );
        assert!(matches!(err, mlua::Value::String(_)));
    }

    #[tokio::test]
    async fn rm_force_ignores_missing() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("ghost.txt");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("force", true).unwrap();
        let (ok, err): (mlua::Value, mlua::Value) =
            rm.call_async((file.to_str().unwrap(), opts)).await.unwrap();
        assert!(
            matches!(ok, mlua::Value::Boolean(true)),
            "force should suppress NotFound"
        );
        assert!(matches!(err, mlua::Value::Nil));
    }

    #[tokio::test]
    async fn rm_force_ignores_missing_dir() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("never_existed");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("recursive", true).unwrap();
        opts.set("force", true).unwrap();
        let (ok, err): (mlua::Value, mlua::Value) =
            rm.call_async((dir.to_str().unwrap(), opts)).await.unwrap();
        assert!(matches!(ok, mlua::Value::Boolean(true)));
        assert!(matches!(err, mlua::Value::Nil));
    }

    #[tokio::test]
    async fn rm_empty_dir_without_recursive() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("emptydir");
        std::fs::create_dir(&dir).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let (ok, _): (mlua::Value, mlua::Value) =
            rm.call_async(dir.to_str().unwrap()).await.unwrap();
        assert!(matches!(ok, mlua::Value::Boolean(true)));
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn rm_nonempty_dir_without_recursive_fails() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("nonempty");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("child.txt"), "x").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let (ok, err): (mlua::Value, mlua::Value) =
            rm.call_async(dir.to_str().unwrap()).await.unwrap();
        assert!(
            matches!(ok, mlua::Value::Nil),
            "should fail without recursive"
        );
        assert!(matches!(err, mlua::Value::String(_)));
        assert!(dir.exists(), "non-empty dir should still exist");
    }

    #[tokio::test]
    async fn rm_recursive_removes_tree() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("tree");
        std::fs::create_dir_all(dir.join("sub/deeper")).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("sub/b.txt"), "b").unwrap();
        std::fs::write(dir.join("sub/deeper/c.txt"), "c").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("recursive", true).unwrap();
        let (ok, _): (mlua::Value, mlua::Value) =
            rm.call_async((dir.to_str().unwrap(), opts)).await.unwrap();
        assert!(matches!(ok, mlua::Value::Boolean(true)));
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rm_symlink_removes_link_not_target() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target.txt");
        std::fs::write(&target, "data").unwrap();
        let link = tmp.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let (ok, _): (mlua::Value, mlua::Value) =
            rm.call_async(link.to_str().unwrap()).await.unwrap();
        assert!(matches!(ok, mlua::Value::Boolean(true)));
        assert!(!link.exists(), "symlink should be removed");
        assert!(target.exists(), "target should remain");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rm_recursive_symlink_to_dir_does_not_follow() {
        let tmp = TempDir::new().unwrap();
        let real_dir = tmp.path().join("real");
        std::fs::create_dir_all(real_dir.join("sub")).unwrap();
        std::fs::write(real_dir.join("sub/keep.txt"), "data").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let rm: mlua::Function = tbl.get("rm").unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("recursive", true).unwrap();
        let (ok, _): (mlua::Value, mlua::Value) =
            rm.call_async((link.to_str().unwrap(), opts)).await.unwrap();
        assert!(matches!(ok, mlua::Value::Boolean(true)));
        assert!(!link.exists(), "symlink should be removed");
        assert!(real_dir.exists(), "target dir should remain");
        assert!(
            real_dir.join("sub/keep.txt").exists(),
            "target dir contents should remain"
        );
    }

    #[tokio::test]
    async fn mkdir_creates_single_dir() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("newdir");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let mkdir: mlua::Function = tbl.get("mkdir").unwrap();
        let (ok, _): (mlua::Value, mlua::Value) =
            mkdir.call_async(dir.to_str().unwrap()).await.unwrap();
        assert!(matches!(ok, mlua::Value::Boolean(true)));
        assert!(dir.is_dir());
    }

    #[tokio::test]
    async fn mkdir_without_parents_fails_on_deep_path() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("a/b/c");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let mkdir: mlua::Function = tbl.get("mkdir").unwrap();
        let (ok, err): (mlua::Value, mlua::Value) =
            mkdir.call_async(dir.to_str().unwrap()).await.unwrap();
        assert!(
            matches!(ok, mlua::Value::Nil),
            "should fail without parents option"
        );
        assert!(matches!(err, mlua::Value::String(_)));
    }

    #[tokio::test]
    async fn mkdir_with_parents_creates_nested() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("x/y/z");

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let mkdir: mlua::Function = tbl.get("mkdir").unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("parents", true).unwrap();
        let (ok, _): (mlua::Value, mlua::Value) = mkdir
            .call_async((dir.to_str().unwrap(), opts))
            .await
            .unwrap();
        assert!(matches!(ok, mlua::Value::Boolean(true)));
        assert!(dir.is_dir());
    }

    #[tokio::test]
    async fn mkdir_already_exists_returns_error() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("exists");
        std::fs::create_dir(&dir).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let mkdir: mlua::Function = tbl.get("mkdir").unwrap();
        let (ok, err): (mlua::Value, mlua::Value) =
            mkdir.call_async(dir.to_str().unwrap()).await.unwrap();
        assert!(
            matches!(ok, mlua::Value::Nil),
            "creating existing dir should fail"
        );
        assert!(matches!(err, mlua::Value::String(_)));
    }

    #[tokio::test]
    async fn mkdir_with_parents_idempotent() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("idem");
        std::fs::create_dir(&dir).unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let mkdir: mlua::Function = tbl.get("mkdir").unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("parents", true).unwrap();
        let (ok, _): (mlua::Value, mlua::Value) = mkdir
            .call_async((dir.to_str().unwrap(), opts))
            .await
            .unwrap();
        assert!(
            matches!(ok, mlua::Value::Boolean(true)),
            "parents=true should be idempotent"
        );
    }

    #[tokio::test]
    async fn glob_finds_matching_files() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn main(){}").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "hello").unwrap();
        let dir_str = tmp.path().to_string_lossy().to_string();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", dir_str.as_str()).unwrap();

        let (result, err): (Table, mlua::Value) = glob
            .call_async::<(Table, mlua::Value)>(("*.rs", opts))
            .await
            .unwrap();
        assert!(matches!(err, mlua::Value::Nil));

        let mut paths: Vec<String> = Vec::new();
        for i in 1..=result.len().unwrap() {
            paths.push(result.get::<String>(i).unwrap());
        }
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("a.rs"));
    }

    #[tokio::test]
    async fn glob_multiple_patterns_union() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "").unwrap();
        std::fs::write(tmp.path().join("c.py"), "").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let patterns = lua.create_table().unwrap();
        patterns.set(1, "*.rs").unwrap();
        patterns.set(2, "*.txt").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();

        let (result, err): (Table, mlua::Value) = glob
            .call_async::<(Table, mlua::Value)>((patterns, opts))
            .await
            .unwrap();
        assert!(matches!(err, mlua::Value::Nil));

        let mut paths: Vec<String> = Vec::new();
        for i in 1..=result.len().unwrap() {
            paths.push(result.get::<String>(i).unwrap());
        }
        paths.sort();
        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with("a.rs"));
        assert!(paths[1].ends_with("b.txt"));
    }

    #[tokio::test]
    async fn glob_limit_caps_results() {
        let tmp = TempDir::new().unwrap();
        for i in 0..5 {
            std::fs::write(tmp.path().join(format!("f{i}.rs")), "").unwrap();
        }

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        opts.set("limit", 2).unwrap();

        let (result, err): (Table, mlua::Value) = glob
            .call_async::<(Table, mlua::Value)>(("*.rs", opts))
            .await
            .unwrap();
        assert!(matches!(err, mlua::Value::Nil));
        assert_eq!(result.len().unwrap(), 2);
    }

    #[tokio::test]
    async fn glob_no_matches_returns_empty_table() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();

        let (empty, err2): (Table, mlua::Value) = glob
            .call_async::<(Table, mlua::Value)>(("*.nope", opts))
            .await
            .unwrap();
        assert!(matches!(err2, mlua::Value::Nil));
        assert_eq!(empty.len().unwrap(), 0);
    }

    #[tokio::test]
    async fn glob_invalid_pattern_type_errors() {
        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let err = glob
            .call_async::<(mlua::Value, mlua::Value)>((mlua::Value::Integer(42), mlua::Nil))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("patterns must be a string or array of strings"),
        );
    }

    #[tokio::test]
    async fn glob_invalid_pattern_returns_nil_err() {
        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", "/tmp").unwrap();

        let (val, err): (mlua::Value, mlua::Value) = glob
            .call_async::<(mlua::Value, mlua::Value)>(("[invalid", opts))
            .await
            .unwrap();
        assert_eq!(val, mlua::Value::Nil);
        assert!(
            matches!(&err, mlua::Value::String(s) if s.to_str().unwrap().starts_with("glob: ")),
            "should return nil, err with glob: prefix, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn dir_path_is_file_returns_nil_err() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("not_a_dir.txt");
        std::fs::write(&file, "i am a file").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let dir: mlua::Function = tbl.get("dir").unwrap();

        let (val, err): (mlua::Value, mlua::Value) = dir
            .call_async::<(mlua::Value, mlua::Value)>(file.to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(val, mlua::Value::Nil);
        assert!(
            matches!(&err, mlua::Value::String(s) if s.to_str().unwrap().starts_with("dir: ")),
            "should return nil, err with dir: prefix, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn glob_mtime_sort_newest_first() {
        let tmp = TempDir::new().unwrap();
        let old_path = tmp.path().join("old.rs");
        let new_path = tmp.path().join("new.rs");
        std::fs::write(&old_path, "").unwrap();
        std::fs::write(&new_path, "").unwrap();

        let old_time = SystemTime::now() - Duration::from_secs(60);
        let new_time = SystemTime::now();
        OpenOptions::new()
            .write(true)
            .open(&old_path)
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        OpenOptions::new()
            .write(true)
            .open(&new_path)
            .unwrap()
            .set_modified(new_time)
            .unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        opts.set("sort", "mtime").unwrap();

        let (result, err): (Table, mlua::Value) = glob
            .call_async::<(Table, mlua::Value)>(("*.rs", opts))
            .await
            .unwrap();
        assert!(matches!(err, mlua::Value::Nil));

        let first: String = result.get(1).unwrap();
        let second: String = result.get(2).unwrap();
        assert!(first.ends_with("new.rs"));
        assert!(second.ends_with("old.rs"));
    }

    #[tokio::test]
    async fn glob_no_opts_uses_cwd() {
        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let result = glob
            .call_async::<(Table, mlua::Value)>(("*.rs", mlua::Nil))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn glob_path_option_scopes_to_directory() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("inner.rs"), "").unwrap();
        std::fs::write(tmp.path().join("outer.rs"), "").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let glob: mlua::Function = tbl.get("glob").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", sub.to_str().unwrap()).unwrap();

        let (result, err): (Table, mlua::Value) = glob
            .call_async::<(Table, mlua::Value)>(("*.rs", opts))
            .await
            .unwrap();
        assert!(matches!(err, mlua::Value::Nil));

        let mut paths: Vec<String> = Vec::new();
        for i in 1..=result.len().unwrap() {
            paths.push(result.get::<String>(i).unwrap());
        }
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("inner.rs"));
    }

    #[tokio::test]
    async fn grep_returns_matches_with_context_and_limit() {
        let tmp = TempDir::new().unwrap();
        let mut content = String::new();
        for i in 1..=20 {
            content.push_str(&format!("line_{i}\n"));
        }
        std::fs::write(tmp.path().join("data.txt"), &content).unwrap();
        std::fs::write(tmp.path().join("other.txt"), "no hits here\n").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let grep: mlua::Function = tbl.get("grep").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        let result: Table = grep.call_async::<Table>(("line_", opts)).await.unwrap();
        assert_eq!(result.len().unwrap(), 1);
        let entry: Table = result.get(1).unwrap();
        let path = entry.get::<String>("path").unwrap();
        assert!(path.ends_with("data.txt"));
        assert!(Path::new(&path).is_absolute());
        let groups: Table = entry.get("groups").unwrap();
        assert!(groups.len().unwrap() > 0);
        let line: Table = groups
            .get::<Table>(1)
            .unwrap()
            .get::<Table>("lines")
            .unwrap()
            .get(1)
            .unwrap();
        assert!(line.get::<bool>("is_match").unwrap());
        assert!(line.get::<usize>("line_nr").unwrap() > 0);

        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        opts.set("context_before", 1).unwrap();
        opts.set("context_after", 1).unwrap();
        let result: Table = grep.call_async::<Table>(("line_10", opts)).await.unwrap();
        let lines: Table = result
            .get::<Table>(1)
            .unwrap()
            .get::<Table>("groups")
            .unwrap()
            .get::<Table>(1)
            .unwrap()
            .get("lines")
            .unwrap();
        assert_eq!(lines.len().unwrap(), 3);
        assert!(
            !lines
                .get::<Table>(1)
                .unwrap()
                .get::<bool>("is_match")
                .unwrap()
        );
        assert!(
            lines
                .get::<Table>(2)
                .unwrap()
                .get::<bool>("is_match")
                .unwrap()
        );
        assert!(
            !lines
                .get::<Table>(3)
                .unwrap()
                .get::<bool>("is_match")
                .unwrap()
        );

        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        opts.set("limit", 5).unwrap();
        let result: Table = grep.call_async::<Table>(("line_", opts)).await.unwrap();
        let groups: Table = result.get::<Table>(1).unwrap().get("groups").unwrap();
        assert_eq!(groups.len().unwrap(), 5);

        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        let result: Table = grep
            .call_async::<Table>(("zzz_no_match", opts))
            .await
            .unwrap();
        assert_eq!(result.len().unwrap(), 0);
    }

    #[tokio::test]
    async fn grep_invalid_regex_returns_error() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("x.txt"), "hello\n").unwrap();

        let lua = Lua::new();
        let tbl = create_fs_table(&lua, &PluginPermissions::trusted()).unwrap();
        let grep: mlua::Function = tbl.get("grep").unwrap();

        let opts = lua.create_table().unwrap();
        opts.set("path", tmp.path().to_str().unwrap()).unwrap();
        let (entries, err): (mlua::Value, String) =
            grep.call_async(("[invalid", opts)).await.unwrap();
        assert!(matches!(entries, mlua::Value::Nil));
        assert!(err.contains("invalid regex pattern"));
    }
}
