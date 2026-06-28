//! Top-level dashboard navigation slot. The user picks one of these in the
//! sidebar; the coordinator's `view` dispatches to the matching pane.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Home,
    Apps,
    Activity,
    Settings,
}
