//! Siphon download engine.

pub mod batch;
pub mod browser;
pub mod checksum;
pub mod disk;
pub mod error;
pub mod extractor;
pub mod filename;
pub mod finishing;
pub mod ftp;
pub mod grab;
pub mod hls;
pub mod http;
pub mod ipc;
pub mod manager;
pub mod mms;
pub mod net;
pub mod organize;
pub mod proxy_bridge;
pub mod rate;
pub mod schedule;
pub mod secrets;
pub mod segments;
pub mod settings;
pub mod source;
pub mod transfer;
pub mod types;
pub mod zip_peek;

pub use error::{Error, ErrorClass, Result};
pub use manager::{Capture, Confirmation, Details, DownloadView, Duplicate, Event, GrabJob, HashJob, Manager, NewDownload, PageInfo, ProbeInfo, QueueState, Ticket, VariantInfo};
pub use settings::Settings;
pub use types::*;
