//! The crate's integration tests: ONE binary, not one per file (MAIN-657).
//!
//! Cargo builds every `tests/*.rs` as its own binary, each linking the whole
//! crate graph — 171 of them came to 22 GB of linked output, and three of the
//! four minutes CI spent on Rust was the relink. Every file therefore lives
//! under `tests/it/` and is declared here instead, which costs one `mod` line
//! per suite and buys back the other 170 links.
//!
//! The root is `tests/it/main.rs` rather than `tests/it.rs` because a crate
//! root's `mod foo;` resolves to a SIBLING — `tests/foo.rs` — so the only way
//! to keep the root outside the directory is a `#[path]` attribute on all 171
//! declarations. One line per suite is the point; 342 is not.
//!
//! Two consequences a new file has to respect: `include_str!` resolves against
//! `tests/it/`, and process-global state is now shared with every other suite
//! in this binary — anything touching the environment takes `common::env_guard`.
//!
//! ONE file is deliberately not here: `tests/panic_logging.rs`, which asserts on
//! a `tracing` callsite whose interest is cached process-wide on first use, and
//! so needs a process nothing else has panicked in. See its own header.

// Shared fixtures rather than a suite of its own.
mod common;

mod activity_visibility;
mod admin_fake;
mod agent_commands;
mod agent_mtls;
mod app_config_endpoint;
mod auth_request_path;
mod backlog_invisible;
mod board_automation;
mod board_health;
mod board_workspace;
mod build_failure_ladder;
mod build_handback;
mod build_loop_pin;
mod build_loop_routes;
mod build_loop_switch;
mod build_port_leases;
mod build_port_leases_placement;
mod build_stack_reap;
mod build_worktree_prune;
mod builds_converge;
mod bulk_tasks;
mod checkout_announce;
mod checkout_kind;
mod checkout_tenant_follows_workspace;
mod claim_lease;
mod comment_unblock;
mod conflict_repair;
mod cross_org_own_nodes;
mod cross_tenant_placement_both_engines;
mod desktop_local_boot;
mod desktop_local_session;
// Supervision by pid and signal; there is no Windows equivalent to assert.
#[cfg(unix)]
mod desktop_sidecar_lifecycle;
mod dev_signin;
mod discovery_cross_tenant;
mod dispatch_order;
mod dispatch_ownership;
mod done_invisible;
mod email_imap;
mod email_inbound;
mod email_investigation;
mod email_links;
mod email_reply;
mod executor_selection;
mod first_run_identity;
mod forge_token_check;
mod forge_webhook;
mod held_on_nodes_agreement;
mod human_request_changes;
mod identity_fake;
mod identity_routes_fake;
mod implicit_tenant_admin;
mod interactions;
mod invite_fake;
mod invite_registration;
mod job_reaper;
mod jobs_fake;
mod local_accounts;
mod local_install_node_ownership;
mod loop_job_execution;
mod loop_job_messages;
mod loop_job_review_target;
mod loop_jobs;
mod loop_kind_wall;
mod loop_skill_channels;
mod loop_worktree_lifecycle;
mod loops_switch;
mod mailer_postmark;
mod mailer_smtp;
mod managed_content;
mod mcp_build_runs;
mod mcp_notebook;
mod mcp_tenant_isolation;
mod mcp_tunnels;
mod merge_reconcile;
mod migrate_dev_tolerance;
mod multi_instance;
mod node_acts_for_its_session;
mod node_capacity;
mod node_enrolment;
mod node_fake;
mod node_owner;
mod node_placement;
mod node_token_person_gated_routes;
mod node_visibility;
mod notebook;
mod notebook_fake;
mod notebook_node_principal;
mod notebook_unique_names;
mod notifications;
mod notifications_fake;
mod oidc_degraded;
mod operator_authorize_runtime;
mod operator_writes;
mod overview_visibility;
mod panic_safety_net;
mod permission_predicate;
mod placement_across_tenants;
mod placement_and_gitops_auth;
mod port_leases;
mod port_safety;
mod pr_hygiene;
mod queue_ejection_repair;
mod queued_job_endings;
mod queued_reason_kind;
mod rbac_across_tenants;
mod read_model_fake;
mod reconcile_preview;
mod reconcile_tombstones;
mod repo_completeness;
mod repo_settings;
mod review_column;
mod review_force;
mod review_loop_session;
mod row_mapping;
mod runtime_auth_codex;
mod runtime_auth_device_flow;
mod runtime_auth_sessionless;
mod scoped_tokens;
mod secret_items;
mod session_chat;
mod session_expiry_seam;
mod session_fake;
mod session_isolation;
mod session_owner_only;
mod session_ownership;
mod session_reconcile;
mod session_retention;
mod session_stopped;
mod session_visibility;
mod settings_upsert;
mod single_instance_boot;
mod sqlite_boot;
mod sqlite_person_id_default;
mod sqlite_scaffold;
mod sqlite_upgrade;
mod squash_restamp;
mod stalled_job_reap_both_engines;
mod task_attachments;
mod task_checkout_ids;
mod task_description_revisions;
mod task_fake;
mod task_move_by_type;
mod task_parent;
mod task_reports;
mod task_routes_fake;
mod task_type;
mod task_visibility;
mod task_workspace;
mod task_workspace_refs;
mod taught_skills;
mod tenant_ca;
mod tenant_isolation;
mod text_uuid_join;
mod tunnel_surface;
mod tunnel_websocket;
mod user_content;
mod verdict_silence;
mod visibility_agreement;
mod visibility_one_definition;
mod workspace_build_loop;
mod workspace_build_loop_status;
mod workspace_fake;
mod workspace_gh_token;
mod workspace_git_key;
mod workspace_review_loop;
mod workspace_review_loop_status;
mod workspace_runs_listing;
mod workspace_session_spec;
mod workspaces_collection;
mod worktree_delete_route;
