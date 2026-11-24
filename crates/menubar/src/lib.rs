//! The recordings pipeline's menu bar app (Rust port of recordings_menubar.py, menubar_rows.py,
//! menubar_icons.py) and the pieces `make_app` shares with it.

/// YYYYMMDD.N from install.sh / package.sh (build.rs), or "dev".
pub const VERSION: &str = env!("TT_BUILD_VERSION");

pub mod app;
pub mod icons;
pub mod jobs;
pub mod notify;
pub mod preview;
pub mod recent;
pub mod rows;
pub mod setup;
pub mod status;
pub mod update;
