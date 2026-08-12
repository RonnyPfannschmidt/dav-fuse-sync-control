//! Shared test harness.
//!
//! A directory module, so cargo does not compile it as its own test target.

// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

pub mod guard;
pub mod nextcloud;
pub mod rclone;
pub mod tree;
