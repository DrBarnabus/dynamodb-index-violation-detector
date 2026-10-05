//! Scans a DynamoDB table for items that violate GSI, LSI or TTL key
//! expectations. The binary drives these modules from a TUI; integration tests
//! drive them directly against DynamoDB Local.

pub mod assemble;
pub mod aws;
pub mod config;
pub mod domain;
pub mod export;
pub mod pipeline;
pub mod rules;
pub mod scan;
pub mod state;
pub mod tui;
