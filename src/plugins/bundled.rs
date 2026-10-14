//! Plugins shipped inside the nerve binary. Installed on start; updated with nerve unless the user
//! edited a file (then that file is left alone) or deleted it (then it is not recreated).

use std::collections::BTreeMap;
use std::path::Path;

macro_rules! files {
    ($($p:literal),* $(,)?) => { &[$(($p, include_str!(concat!("../../plugins/", $p)))),*] };
}

const FILES: &[(&str, &str)] = files![
    "README.md",
    "_sdk/nerve_plugin.py",
    "blender/plugin.toml",
    "blender/server.py",
    "blender/nerve_bridge.py",
    "blender/nerve_kit.py",
    "unreal/plugin.toml",
    "unreal/server.py",
    "unreal/nerve_unreal_bridge.py",
    "montage/plugin.toml",
    "montage/server.py",
    "montage/sources.py",
    "montage/analysis.py",
    "montage/engine.py",
    "skills/plugin.toml",
    "skills/server.py",
    "skills/library/weapon-3d-blender/scripts/build_katana.py",
    "skills/library/weapon-3d-blender/scripts/build_m4a1.py",
    "skills/library/weapon-3d-blender/scripts/dragonfly.py",
    "skills/library/weapon-3d-blender/scripts/katana_mats.py",
    "skills/library/weapon-3d-blender/scripts/m4_mats.py",
    "skills/library/weapon-3d-blender/scripts/m4a1_outlines.json",
    "skills/library/weapon-3d-blender/scripts/render_views.py",
    "skills/library/weapon-3d-blender/scripts/wlib.py",
    "skills/library/weapon-3d-blender/SKILL.md",
];

const RECORD: &str = ".bundled.json";

pub fn install(dir: &Path) {
    let rec_path = dir.join(RECORD);
    let mut rec: BTreeMap<String, String> = std::fs::read_to_string(&rec_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let mut changed = false;
    for (rel, content) in FILES {
        let path = dir.join(rel);
        let new_hash = crate::util::sha256_hex(content.as_bytes());
        let prev = rec.get(*rel).cloned();
        let write = match (std::fs::read(&path).ok(), &prev) {
            (None, None) => true,                 // first install
            (None, Some(_)) => false,             // the user deleted it
            (Some(disk), Some(h)) => {
                let disk_hash = crate::util::sha256_hex(&disk);
                if disk_hash == new_hash {
                    false
                } else {
                    disk_hash == *h // untouched since we wrote it -> update
                }
            }
            (Some(_), None) => false, // a file we never wrote: not ours to replace
        };
        if write {
            if let Some(p) = path.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            match std::fs::write(&path, content.as_bytes()) {
                Ok(()) => tracing::info!("bundled plugin file installed: {rel}"),
                Err(e) => {
                    tracing::warn!("cannot install bundled plugin file {rel}: {e}");
                    continue;
                }
            }
        }
        let now_hash = std::fs::read(&path).map(|d| crate::util::sha256_hex(&d)).ok();
        // remember what we shipped only while the disk copy is ours
        if now_hash.as_deref() == Some(new_hash.as_str()) && prev.as_deref() != Some(new_hash.as_str()) {
            rec.insert(rel.to_string(), new_hash);
            changed = true;
        }
    }
    if changed {
        let _ = crate::util::atomic_write(&rec_path, serde_json::to_string_pretty(&rec).unwrap_or_default().as_bytes());
    }
}
