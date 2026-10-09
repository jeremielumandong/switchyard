//! The API workspace: a Postman-style client (collections, requests, environments,
//! scripts, runs), ported from AgentOps's API Workbench. The UI lives in [`workbench`];
//! [`compat`] answers what it expects from its host shell. Domain logic, storage and
//! sending are in `switchyard-api`, run on the core runtime.

pub mod compat;
pub mod generated;
pub mod workbench;
