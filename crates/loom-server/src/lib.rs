//! loom-server: central gateway, model registry, router, global tier-2
//! scheduler/queue, client registry, admin API, config sync, usage, branding.

pub mod admin;
pub mod auth;
pub mod client_registry;
pub mod config;
pub mod dashboard_static;
pub mod db;
pub mod events;
pub mod gateway;
pub mod grpc_service;
pub mod instance_lock;
pub mod lua;
pub mod metrics;
pub mod model_registry;
pub mod notes;
pub mod redisx;
pub mod router;
pub mod router_engine;
pub mod scheduler;
pub mod session;
pub mod settings;
pub mod tls_ca;
pub mod usage;
pub mod ws_agent;
