//! Platform-agnostic plugins. Each reaches the OS only through a small host trait (or the core
//! [`pairly_core::Platform`]), which the Linux daemon and the Android app implement.
//!
//! Apps build the set they want and hand each to [`pairly_core::NodeBuilder::plugin`].
#![forbid(unsafe_code)]

pub mod battery;
pub mod clipboard;
pub mod command;
pub mod contacts;
pub mod files;
pub mod findmy;
pub mod input;
pub mod media;
pub mod notification;
mod peers;
pub mod ping;
pub mod power;
pub mod screen;
pub mod share;
pub mod sms;
pub mod telephony;
