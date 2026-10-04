//! Platform-agnostic plugins. Each reaches the OS only through a small host trait (or the core
//! [`pairly_core::Platform`]), which the Linux daemon and the Android app implement.
//!
//! Apps build the set they want and hand each to [`pairly_core::NodeBuilder::plugin`].
#![forbid(unsafe_code)]

pub mod battery;
pub mod clipboard;
pub mod findmy;
pub mod notification;
mod peers;
pub mod ping;
pub mod share;
