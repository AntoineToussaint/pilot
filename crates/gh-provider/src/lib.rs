//! # pilot-gh
//!
//! GitHub event provider for pilot. Uses a single GraphQL query per poll
//! cycle to fetch all PRs with comments, threads, and review status.

mod client;
mod graphql;
mod poller;

pub use client::GhClient;
pub use poller::GhPoller;
