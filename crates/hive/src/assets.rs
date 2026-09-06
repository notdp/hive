//! Embedded asset trees written to disk heal-on-drift (the cvim toolkit,
//! the plugin marketplace).

use std::fs;
use std::path::Path;

use anyhow::Result;

/// Write an embedded asset tree under `root`, rewriting any file whose
/// on-disk copy drifted from the embedded content (heal-on-drift). Used by
/// the cvim toolkit and the plugin marketplace materialization.
pub(crate) fn materialize_asset_tree(root: &Path, files: &[(&str, &str, bool)]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for (rel, content, executable) in files {
        let path = root.join(rel);
        if fs::read_to_string(&path).ok().as_deref() != Some(*content) {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, content)?;
        }
        if *executable {
            let mut perms = fs::metadata(&path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms)?;
        }
    }
    Ok(())
}
