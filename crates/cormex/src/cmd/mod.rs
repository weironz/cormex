//! The subcommands.
//!
//! One module per group rather than one per command: `login` / `logout` /
//! `status` share the credential and the config, and splitting them would mean
//! three copies of "load the config, resolve the directories, load the key".

pub mod detect;
pub mod doctor;
pub mod import;
pub mod init;
pub mod login;
pub mod logout;
pub mod mcp;
pub mod restore;
pub mod sessions;
pub mod status;
pub mod uninstall;
