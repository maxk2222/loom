#![allow(clippy::result_large_err)]

//! loom-testkit: test infrastructure — mock OpenAI backend, mock agent server,
//! spawnable test harness for integration/failure/load testing.

pub mod harness;
pub mod mock_agent_server;
pub mod mock_backend;
