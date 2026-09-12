//! The per-service mount directories inside the environment's subvolume, made before the
//! StatefulSets so a `subPath` never creates them as root.

use super::*;


/// Every declared volume is a folder inside the env's ONE subvolume — mkdir -p each before a pod
/// binds it as a subPath.
pub(crate) fn mkdir_env_mounts(live: &std::path::Path, services: &[model::Service]) -> Result<(), String> {
    // EVERY mount is validated, before any of them is created (2026-09-12). Dedup is by folder, so
    // a second service declaring the same folder with a different — and invalid — mount skipped the
    // check entirely, and a refusal is only a refusal if it happens before the first mkdir.
    for svc in services {
        for m in &svc.mounts {
            // `create_dir_all` on an unvalidated folder is itself the escape — it would
            // happily mkdir -p outside the subvolume before a pod ever ran.
            model::validate_mount(m)?;
        }
    }
    let mut seen = std::collections::HashSet::new();
    for svc in services {
        for m in &svc.mounts {
            if seen.insert(m.folder.clone()) {
                std::fs::create_dir_all(live.join("volumes").join(&m.folder)).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}
