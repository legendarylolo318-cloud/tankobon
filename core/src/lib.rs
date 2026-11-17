//! Tankobon core: database, models, sources, downloads, image pipeline.

pub mod archive;
pub mod backup;
pub mod db;
pub mod downloads;
pub mod imaging;
pub mod models;
pub mod paths;
pub mod settings;
pub mod source;
pub mod updates;
pub mod util;

pub use db::Db;
pub use models::*;
pub use settings::Settings;

#[cfg(test)]
pub(crate) mod testutil;
