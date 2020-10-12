use regex::Regex;

use core::Route;
use plugin::{Plugin, PluginChain, TransformFileResult, FileChangeResult};
use vfs::VfsItem;

pub struct FilterPlugin;

impl FilterPlugin {
    pub fn new() -> FilterPlugin {
        FilterPlugin
    }
}

lazy_static! {
    static ref TEST: Regex = Regex::new(r"^(.*?)\.spec\.lua$").unwrap();
    static ref UI: Regex = Regex::new(r"^(.*?)\.ui$").unwrap();
}

impl Plugin for FilterPlugin {
    fn transform_file(&self, _plugins: &PluginChain, vfs_item: &VfsItem) -> TransformFileResult {
        match vfs_item {
            &VfsItem::File { .. } => {
                if TEST.is_match(vfs_item.name()) || UI.is_match(vfs_item.name()) {
                    return TransformFileResult::Discard;
                }
                TransformFileResult::Pass
            },
            _ => TransformFileResult::Pass
        }
    }

    fn handle_file_change(&self, _route: &Route) -> FileChangeResult {
        FileChangeResult::Pass
    }
}