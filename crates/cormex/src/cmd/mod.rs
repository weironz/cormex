//! The subcommands.
//!
//! One module per group rather than one per command: `login` / `logout` /
//! `status` share the credential and the config, and splitting them would mean
//! three copies of "load the config, resolve the directories, load the key".

pub mod login;
pub mod logout;
pub mod status;
