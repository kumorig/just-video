//! Just Video: standalone Steam Frame VR player reading directly from SMB shares
//! or the headset's own storage.

#[cfg(feature = "decode")]
pub mod audio;
pub mod config;
#[cfg(feature = "decode")]
pub mod decode;
pub mod inventory;
#[cfg(feature = "decode")]
pub mod library;
pub mod local;
#[cfg(feature = "decode")]
pub mod media;
#[cfg(feature = "decode")]
pub mod playability;
pub mod readahead;
pub mod smb;
pub mod srvsvc;
pub mod subtitles;
#[cfg(feature = "decode")]
pub mod ui;
#[cfg(feature = "decode")]
pub mod vr;
#[cfg(feature = "decode")]
pub mod xr;
