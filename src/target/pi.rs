/// `.pi` target adapter stub.
///
/// Future: Pi-native agent lowering and config-entry writing.
///
/// V0: stub only — no per-target behavior yet.
use crate::lock::ItemKind;
use crate::types::DestPath;

use super::TargetAdapter;

#[derive(Debug)]
pub struct PiAdapter;

impl TargetAdapter for PiAdapter {
    fn name(&self) -> &str {
        ".pi"
    }

    fn skill_variant_key(&self) -> Option<&str> {
        Some("pi")
    }

    fn default_dest_path(&self, kind: ItemKind, name: &str) -> Option<DestPath> {
        match kind {
            ItemKind::Skill => Some(DestPath::from(format!("skills/{name}").as_str())),
            _ => None,
        }
    }
}
