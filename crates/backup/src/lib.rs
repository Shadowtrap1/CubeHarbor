//! Backup and restore of Minecraft server data.
//!
//! The operator uses this crate to plan Jobs ([`cron`], [`store::backend`]); the Jobs run the
//! `cubeharbor-backup` binary, which uses the rest. To add a storage backend, see [`store`].

pub mod archive;
pub mod cron;
pub mod run;
pub mod store;
