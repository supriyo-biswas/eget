#[cfg(all(target_os = "linux", feature = "extras"))]
mod appimage;
pub mod archive;
pub mod cli;
pub mod compat;
pub mod db;
mod desktop;
pub mod installer;
pub mod manifest;
pub mod model;
pub mod policy;
pub mod scope;
pub mod source;
mod template;
