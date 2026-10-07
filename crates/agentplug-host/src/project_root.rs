use std::path::{Path, PathBuf};

fn strip_windows_verbatim_prefix(path: &str) -> String {
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{unc}");
    }
    path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
}

pub fn canonical_project_root(path: &Path) -> PathBuf {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    PathBuf::from(strip_windows_verbatim_prefix(&resolved.to_string_lossy()))
}

pub fn project_root(path: &Path) -> PathBuf {
    let canonical = canonical_project_root(path);
    if !canonical.is_absolute() {
        return canonical;
    }
    canonical
        .ancestors()
        .find(|dir| {
            let marker = dir.join(".git");
            marker.is_file() || marker.join("HEAD").exists()
        })
        .map(Path::to_path_buf)
        .unwrap_or(canonical)
}
