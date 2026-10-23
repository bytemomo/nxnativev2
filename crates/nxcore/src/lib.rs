//! nxnativev2 core: everything the app does, without a user interface.
//!
//! [`bundle`] unpacks what the packaged app carries (patched nxnative, Python, .NET runtime, clang / lld, spirv-cross).
//! The flow: Apple tools ([`xip`] SDK, [`metal`] compiler) once → [`games`] import (game files and keys) → [`pipeline`] build of the
//! IPA with the nxnative scripts → [`apple`] sign-in and install. [`store`] lays out the data folder, [`settings`] the user settings.
pub mod apple;
pub mod bundle;
pub mod export;
pub mod games;
pub mod logging;
pub mod metal;
pub mod nsp;
pub mod patch;
pub mod pipeline;
pub mod settings;
pub mod store;
pub mod xip;
