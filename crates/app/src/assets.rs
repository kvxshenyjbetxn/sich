//! Asset source: gpui-kit's bundled Lucide icons plus any extra icons we register.

use std::borrow::Cow;

use gpui_kit::{AssetSource, SharedString};

gpui_kit::assets::icon_assets!(ExtraIcons, [
    SquareTerminal,
    Settings,
    House,
    Folder,
    FolderPlus,
    Layers,
    GitBranch,
    FileDiff,
    Columns2,
    Rows2,
    Plus,
    RefreshCw,
    Ellipsis,
    Trash,
    ChevronRight,
    ChevronDown,
    Terminal,
    LoaderCircle,
    CircleAlert,
    CircleCheck,
    CircleX,
]);

/// Agent icons (single-color SVGs, tinted at render time): `(asset path, bytes)`.
pub const AGENT_SVGS: &[(&str, &[u8])] = &[
    ("agents/claude.svg", include_bytes!("../assets/agents/claude.svg")),
    ("agents/codex.svg", include_bytes!("../assets/agents/codex.svg")),
    ("agents/gemini.svg", include_bytes!("../assets/agents/gemini.svg")),
    ("agents/generic.svg", include_bytes!("../assets/agents/generic.svg")),
];

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = AGENT_SVGS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(*bytes)));
        }
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.extend(AGENT_SVGS.iter().filter(|(p, _)| p.starts_with(path)).map(|(p, _)| SharedString::from(*p)));
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_svgs_are_valid_looking_and_loadable() {
        for (path, bytes) in AGENT_SVGS {
            let text = std::str::from_utf8(bytes).expect("utf-8");
            assert!(text.contains("<svg") && text.contains("</svg>"), "{path}");
            assert!(text.contains("currentColor"), "{path} must be tintable");
            let loaded = AppAssets.load(path).unwrap().expect("loadable");
            assert_eq!(loaded.as_ref(), *bytes);
        }
    }
}
