//! TUI widgets — dropdown menus, status bar, and interactive selectors.
//!
//! Each widget module owns its data model (`*Item` structs) and rendering
//! logic (`build_*_lines` functions). The generic [`dropdown::DropdownState`]
//! provides shared navigation (up/down/scroll/filter) so menus stay DRY.

pub mod dropdown;
pub mod file_menu;
pub mod model_menu;
pub mod provider_menu;
pub mod queue_preview;
pub mod session_menu;
pub mod shortcuts_overlay;
pub mod status_bar;
