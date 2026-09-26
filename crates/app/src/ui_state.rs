//! UI state kept across restarts, in `~/Library/Application Support/ubergit/state.json`:
//! panel sizes, window placement, screen mode, focus and tabs, and each workdir's selected
//! repo. A missing, unreadable or outdated file just means the defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gpui_kit::base::ResizableState;
use gpui_kit::{
    App, AppContext as _, Bounds, DisplayId, Entity, Pixels, Size, Window, WindowBounds, point, px, size,
};
use serde::{Deserialize, Serialize};

use crate::workspace::{Panel, ScreenMode};

/// Bump when the format changes incompatibly; files with another version are ignored.
const VERSION: u32 = 1;

pub const DEFAULT_WINDOW_SIZE: Size<Pixels> = size(px(1600.), px(960.));

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    pub version: u32,
    pub window: Option<SavedWindow>,
    pub layout: SavedLayout,
    pub screen_mode: ScreenMode,
    pub show_command_log: bool,
    pub focused: Panel,
    /// The side panel the main view shows while it has focus.
    pub last_side: Panel,
    /// Selected tab of each side panel that has more than one.
    pub tabs: BTreeMap<Panel, usize>,
    pub workdirs: BTreeMap<PathBuf, WorkdirState>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            version: VERSION,
            window: None,
            layout: SavedLayout::default(),
            screen_mode: ScreenMode::Normal,
            show_command_log: true,
            focused: Panel::Repos,
            last_side: Panel::Repos,
            tabs: BTreeMap::new(),
            workdirs: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkdirState {
    pub selected_repo: Option<PathBuf>,
}

/// Panel sizes along each split, in pixels. Only their proportions matter: a split
/// scales them to whatever space it has.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedLayout {
    /// Repos, side panels, main.
    pub columns: Vec<f32>,
    /// Files, Branches, Commits, Stash.
    pub side: Vec<f32>,
    /// Main view, command log.
    pub main: Vec<f32>,
}

/// The window's top-left corner relative to its display, and its content size. The display
/// is named by UUID because display ids change when monitors are replugged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub mode: WindowMode,
    pub display: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowMode {
    #[default]
    Windowed,
    Maximized,
    Fullscreen,
}

/// `UBERGIT_STATE` names another file. Scripted screenshot runs leave the real one alone.
pub fn state_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("UBERGIT_STATE").filter(|p| !p.is_empty()) {
        return Some(path.into());
    }
    if cfg!(feature = "screenshot") && std::env::var_os("UBERGIT_SCRIPT").is_some() {
        return None;
    }
    dirs::data_dir().map(|dir| dir.join("ubergit/state.json"))
}

impl UiState {
    pub fn load(path: &Path) -> Self {
        let read = || -> Option<Self> {
            let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
            if value.get("version")?.as_u64()? != u64::from(VERSION) {
                return None;
            }
            serde_json::from_value(value).ok()
        };
        read().unwrap_or_default()
    }

    /// Writes the state to `path`, keeping the file's entries for other workdirs (another
    /// ubergit may have saved them since this one started).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut workdirs = Self::load(path).workdirs;
        workdirs.extend(self.workdirs.clone());
        let json = serde_json::to_vec_pretty(&Self { workdirs, ..self.clone() })?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Write, then rename over the old file, so a crash can't leave half a file.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }
}

impl SavedWindow {
    /// The window's placement now. The size is the content size, which is what opening a
    /// window takes; its bounds include the title bar. While the window is fullscreen or
    /// maximized, this is `previous` (its windowed placement) with the new mode.
    pub fn capture(window: &Window, cx: &App, previous: Option<&SavedWindow>) -> Self {
        let (bounds, mode) = match window.window_bounds() {
            WindowBounds::Windowed(bounds) => (bounds, WindowMode::Windowed),
            WindowBounds::Maximized(bounds) => (bounds, WindowMode::Maximized),
            WindowBounds::Fullscreen(bounds) => (bounds, WindowMode::Fullscreen),
        };
        if mode != WindowMode::Windowed
            && let Some(previous) = previous
        {
            return Self { mode, ..previous.clone() };
        }
        let size = if mode == WindowMode::Windowed { window.viewport_size() } else { bounds.size };
        Self {
            x: bounds.origin.x.as_f32().round(),
            y: bounds.origin.y.as_f32().round(),
            width: size.width.as_f32().round(),
            height: size.height.as_f32().round(),
            mode,
            display: window.display(cx).and_then(|d| d.uuid().ok()).map(|uuid| uuid.to_string()),
        }
    }

    fn bounds(&self) -> Bounds<Pixels> {
        Bounds::new(point(px(self.x), px(self.y)), size(px(self.width), px(self.height)))
    }
}

/// Where to open the window: where it was, if that display is still connected and the
/// window would be reachable on it. Otherwise its old size, centred on the main display.
pub fn window_placement(saved: Option<&SavedWindow>, cx: &App) -> (WindowBounds, Option<DisplayId>) {
    let saved = saved.filter(|s| {
        [s.x, s.y, s.width, s.height].iter().all(|v| v.is_finite()) && s.width > 0. && s.height > 0.
    });
    let Some(saved) = saved else {
        return (WindowBounds::Windowed(Bounds::centered(None, DEFAULT_WINDOW_SIZE, cx)), None);
    };
    let bounds = saved.bounds();
    let display = saved.display.as_ref().and_then(|uuid| {
        cx.displays()
            .into_iter()
            .find(|d| d.uuid().is_ok_and(|u| u.to_string() == *uuid))
    });
    match display {
        Some(display) if reachable(bounds, display.bounds()) => {
            let bounds = match saved.mode {
                WindowMode::Windowed => WindowBounds::Windowed(bounds),
                WindowMode::Maximized => WindowBounds::Maximized(bounds),
                WindowMode::Fullscreen => WindowBounds::Fullscreen(bounds),
            };
            (bounds, Some(display.id()))
        }
        _ => (WindowBounds::Windowed(Bounds::centered(None, bounds.size, cx)), None),
    }
}

/// Whether enough of the window's title bar is on the display to drag it by.
fn reachable(window: Bounds<Pixels>, display: Bounds<Pixels>) -> bool {
    const TITLE_BAR: Pixels = px(28.);
    let title_bar = Bounds::new(window.origin, size(window.size.width, TITLE_BAR));
    let visible = title_bar.intersect(&display).size;
    visible.width >= px(100.) && visible.height >= TITLE_BAR / 2.
}

/// A split's state, starting from `sizes` when there is one per panel.
pub fn resizable(sizes: &[f32], panels: usize, cx: &mut App) -> Entity<ResizableState> {
    cx.new(|cx| {
        let mut state = ResizableState::default();
        if sizes.len() == panels && sizes.iter().all(|s| s.is_finite() && *s > 0.) {
            // The split has no size before its first layout, so inserting keeps every
            // size as given. The first layout then scales them to fit.
            for &size in sizes {
                state.insert_panel(Some(px(size)), None, cx);
            }
        }
        state
    })
}

/// A split's panel sizes, to save.
pub fn sizes(state: &Entity<ResizableState>, cx: &App) -> Vec<f32> {
    state.read(cx).sizes().iter().map(|size| size.as_f32().round()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> UiState {
        UiState {
            window: Some(SavedWindow {
                x: 40.,
                y: 60.,
                width: 1400.,
                height: 900.,
                mode: WindowMode::Windowed,
                display: Some("69B0D1A2-0000-0000-0000-000000000000".into()),
            }),
            layout: SavedLayout {
                columns: vec![400., 360., 640.],
                side: vec![200., 200., 200., 80.],
                main: vec![600., 200.],
            },
            screen_mode: ScreenMode::Half,
            show_command_log: false,
            focused: Panel::Main,
            last_side: Panel::Commits,
            tabs: BTreeMap::from([(Panel::Files, 0), (Panel::Branches, 2), (Panel::Commits, 1)]),
            workdirs: BTreeMap::from([(
                PathBuf::from("/work/services"),
                WorkdirState { selected_repo: Some("/work/services/api".into()) },
            )]),
            ..UiState::default()
        }
    }

    #[test]
    fn round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ubergit/state.json");
        sample().save(&path).unwrap();
        assert_eq!(UiState::load(&path), sample());
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(json.contains(r#""focused": "main""#), "{json}");
        assert!(json.contains(r#""screen_mode": "half""#), "{json}");
    }

    #[test]
    fn falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        assert_eq!(UiState::load(&path), UiState::default());
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(UiState::load(&path), UiState::default());
        let mut old = serde_json::to_value(sample()).unwrap();
        old["version"] = 0.into();
        std::fs::write(&path, old.to_string()).unwrap();
        assert_eq!(UiState::load(&path), UiState::default());
        std::fs::write(&path, r#"{"version": 1, "focused": "nowhere"}"#).unwrap();
        assert_eq!(UiState::load(&path), UiState::default());
    }

    #[test]
    fn keeps_other_workdirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        sample().save(&path).unwrap();
        let other = UiState {
            workdirs: BTreeMap::from([(
                PathBuf::from("/work/web"),
                WorkdirState { selected_repo: Some("/work/web/site".into()) },
            )]),
            ..UiState::default()
        };
        other.save(&path).unwrap();
        let loaded = UiState::load(&path);
        assert_eq!(loaded.workdirs.len(), 2);
        assert_eq!(loaded.screen_mode, ScreenMode::Normal);
    }

    #[test]
    fn off_screen_windows_are_unreachable() {
        let display = Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.)));
        let window = |x: f32, y: f32| Bounds::new(point(px(x), px(y)), size(px(1400.), px(900.)));
        assert!(reachable(window(100., 50.), display));
        assert!(reachable(window(1700., 900.), display));
        assert!(!reachable(window(1850., 50.), display));
        assert!(!reachable(window(-1350., 50.), display));
        assert!(!reachable(window(100., 1070.), display));
        assert!(!reachable(window(100., -900.), display));
    }
}
