//! Panel placement (was `layout_manager.rs` + `layout_manager_get_panel_rect` in the FFI). std only.

use crate::config::EDMConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Tiling,
    Floating,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug)]
pub struct LayoutManager {
    pub panel_name: String,
    pub layout: Layout,
}

impl LayoutManager {
    /// `tiling` in the config was parsed but never used before; now it picks the layout.
    pub fn from_config(config: &EDMConfig) -> Self {
        Self {
            panel_name: config.interface_name().to_string(),
            layout: if config.tiling { Layout::Tiling } else { Layout::Floating },
        }
    }

    pub fn panel_rect(&self, screen_w: u32, screen_h: u32) -> Rect {
        match self.layout {
            Layout::Tiling => Rect { x: 0, y: 0, width: screen_w, height: screen_h },
            Layout::Floating => Rect {
                x: screen_w / 4,
                y: screen_h / 4,
                width: screen_w / 2,
                height: screen_h / 2,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rects() {
        let mut lm = LayoutManager { panel_name: "p".into(), layout: Layout::Tiling };
        assert_eq!(lm.panel_rect(1920, 1080), Rect { x: 0, y: 0, width: 1920, height: 1080 });
        lm.layout = Layout::Floating;
        assert_eq!(lm.panel_rect(1920, 1080), Rect { x: 480, y: 270, width: 960, height: 540 });
    }

    #[test]
    fn follows_config() {
        let mut c = EDMConfig { interface: Some("Hyproman".into()), ..Default::default() };
        let lm = LayoutManager::from_config(&c);
        assert_eq!((lm.panel_name.as_str(), lm.layout), ("Hyproman", Layout::Tiling));
        c.tiling = false;
        assert_eq!(LayoutManager::from_config(&c).layout, Layout::Floating);
    }
}
