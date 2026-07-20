# NookOS

> **The open workspace operating system for modern software teams.**

NookOS is an open source control plane for software development.

It does **not** replace your editor.

It does **not** replace Git.

It does **not** replace your AI.

It coordinates everything around them.

---

# The Problem

Modern developers don't work on one computer anymore.

We have:

- work laptops
- personal desktops
- WSL
- Macs
- Linux servers
- cloud machines
- Kubernetes clusters
- AI workstations

We also use:

- Claude Code
- Codex
- Hermes
- tmux
- Git
- Jira
- GitHub Projects
- Linear
- Trello
- Obsidian
- countless internal tools

Every project lives somewhere else.

Every AI session runs somewhere else.

Every Kanban board is somewhere else.

Every terminal is somewhere else.

Every Git repository is somewhere else.

The result is constant context switching.

NookOS exists to eliminate that.

---

# Philosophy

NookOS is an operating system for work.

It owns orchestration.

It owns visibility.

It owns context.

It does **not** replace best-in-class tools.

Instead it unifies them.

---

# Core Principles

## Open Source First

NookOS is designed to be useful to anyone.

Every feature should solve a general workflow problem rather than a company-specific problem.

Extensibility is a requirement.

Customization is a requirement.

Forkability is a requirement.

Everything should expose clean APIs.

Everything should be modular.

No proprietary lock-in.

---

## Human In The Loop

AI should never silently make important decisions.

AI recommends.

Humans approve.

Humans can always interrupt.

Humans can always take over.

---

## Git Is The Source Of Truth

Git represents reality.

The terminal shows execution.

Git shows results.

NookOS observes both.

---

## Integrate Instead Of Replace

Don't rebuild existing software.

Instead integrate it.

Examples:

- Claude Code
- Codex
- Hermes
- tmux
- Git
- Jira
- GitHub
- Kubernetes
- Docker
- OAuth Providers

---

# Vision

Imagine opening NookOS.

Immediately you know:

- What needs attention.
- Which machines are running work.
- Which AI sessions are active.
- Which repositories changed.
- Which tasks are blocked.
- What happened while you were away.

Everything in one place.

---

# Architecture

NookOS consists of three major components.

## Control Plane

Responsible for:

- Authentication
- Multi-tenancy
- Workspaces
- Nodes
- Sessions
- Kanban
- Git metadata
- Activity
- Rolling notes
- AI dispatching
- Notifications

The control plane never edits code.

The control plane never builds software.

The control plane orchestrates.

---

## Nook Node

Runs on every machine.

Responsibilities:

- tmux session management
- PTY management
- Git monitoring
- Docker discovery
- Workspace discovery
- Claude launching
- Hermes launching
- Codex launching
- Resource reporting
- Build monitoring

Nodes establish persistent outbound connections.

No inbound SSH required.

No public ports required.

---

## AI Dispatcher

Small.

Focused.

Responsible for:

- prioritization
- routing
- summarization
- session selection
- task injection

Not responsible for:

- coding
- editing
- deployment
- autonomous decision making

Claude writes code.

Hermes automates.

Nook coordinates.

---

# Multi Tenant

NookOS is multi-tenant from day one.

Tenants are isolated.

Each tenant owns:

- users
- workspaces
- nodes
- sessions
- repositories
- settings
- themes
- integrations

This enables:

- personal deployments
- businesses
- agencies
- enterprises
- hosted SaaS

without architectural rewrites.

---

# Cloud Agnostic

NookOS should never depend on one cloud.

Supported deployment targets:

- Docker Compose
- Kubernetes
- Bare metal
- VMs
- Raspberry Pi
- Home labs
- Cloud providers

Everything should work identically.

---

# Kubernetes Native Thinking

Even when running locally,
architecture should naturally scale.

Concepts:

Tenant

↓

Workspace

↓

Node

↓

Session

↓

Runtime

↓

Task

Everything should be schedulable.

Everything should be observable.

Nothing should assume one machine.

---

# Docker First Development & MCP

Development should require one command.

Example:

docker compose up || `run.sh` -> Repeatable | good seeds | down -v destroys everything | run.sh RECREATES the dev environment.

Entire development environment starts.

Postgres.

Redis (if required).

Control plane.

Node.

Everything.

Production deployments should naturally evolve into Kubernetes.

MCP: NookOS can be controlled via an MCP server as well.

---

# Authentication

NookOS should never become an Identity Provider.

Identity is delegated.

Support any standards-compliant provider.

Examples:

- Authentik
- Keycloak
- Zitadel
- Entra ID
- Okta
- Google
- GitHub
- Auth0
- Generic OAuth2
- Generic OpenID Connect

Authentication flow should be:

Login

↓

Redirect to configured IdP

↓

User approves

"NookOS would like to access your profile."

↓

Return with identity

↓

Create session

Identity belongs to the customer's IdP.

Never ours.

---

# Workspaces

Everything belongs to a workspace.

A workspace contains:

- repositories
- documentation
- notes
- sessions
- AI history
- terminals
- boards
- Git activity

Not computers.

Computers simply host workspaces.

---

# Persistent Sessions

Sessions survive browser refreshes.

Sessions survive reconnects.

Sessions survive computer changes.

Backed by tmux.

Supported runtimes:

- Claude Code
- Hermes
- Codex
- Bash
- Zsh
- Fish
- PowerShell
- Future runtimes

Users can:

Reconnect.

Watch.

Interrupt.

Resume.

Take control.

---

# Git

Git is first-class.

Watch:

Branches

Commits

Diffs

Status

Pull Requests

Merge Requests

Builds

Changed files

Recent history

Every task should expose its Git activity.

---

# Kanban

NookOS federates work.

Supported sources:

Jira

GitHub Projects

Linear

Trello

Local Boards

Future MCP servers

Boards remain authoritative.

NookOS presents one unified experience.

---

# AI Sessions

AI sessions are observable.

Every session shows:

Current task

Workspace

Branch

Machine

Runtime

Terminal

Current activity

Recent Git changes

Rolling notes

Nothing is hidden.

---

# Activity Timeline

Everything produces events.

Examples:

Claude started.

Hermes completed.

Commit created.

Tests passed.

Docker build failed.

Task moved.

PR opened.

Review requested.

Activity is chronological.

Searchable.

Auditable.

---

# Rolling Notes

Every workspace accumulates knowledge.

Morning briefing.

End of day summary.

Ideas.

Decisions.

Architecture notes.

Blockers.

Automatically maintained.

Human editable.

---

# UI Philosophy

NookOS should feel like operating a software studio.

Not chatting with an AI.

---

# Theme Engine

Themes are first-class.

Every visual aspect should be configurable.

Support:

Colors

Fonts

Spacing

Icons

Panels

Window chrome

Terminal appearance

Animations

Users should be able to build and distribute themes.

Theme packs should be installable.

---

# Default Theme

The first official theme should embrace classic hacker-terminal aesthetics.

Inspired by:

- amber CRT terminals
- monochrome terminals
- retro UNIX workstations
- cyberpunk control rooms
- developer tooling

High contrast.

Comfortable.

Focused.

Beautiful.

Professional.

The interface should feel like a mission control center for software engineering.

Good vibes matter.

---

# User Experience

Morning.

Open NookOS.

Everything is waiting.

Machines already connected.

Claude already running.

Hermes already working.

Git already updated.

Boards synchronized.

No reconnecting.

No hunting.

No context rebuilding.

Just continue.

---

# Extensibility

Everything should support plugins.

Nodes.

Themes.

Authentication providers.

Kanban providers.

AI runtimes.

Notifications.

Integrations.

Nothing should require modifying core code.

---

# Technology Stack

Backend

- Rust
- Axum
- Tokio
- SQLx
- PostgreSQL

Frontend

- React with render targets for Web, Desktop and Mobile. backend is 100% in rust no exceptions.

Rust owns the types

Everything starts in Rust.

#[derive(Serialize, Deserialize, ToSchema)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub tenant_id: TenantId,
}

Generate:

OpenAPI
JSON Schema
TypeScript types

Then React consumes generated types.

Infrastructure

- Docker
- Kubernetes

Realtime

- WebSockets

Authentication (.env points at whichever IdP you run — the code must never reference a specific auth provider)

- OAuth2.1
- OpenID Connect
- Passkeys (when supported by configured IdP)

Persistence

- PostgreSQL

Terminal

- tmux

---

# Success

A successful NookOS deployment makes developers forget which computer they're using.

They simply continue their work.

Projects become the center.

Machines become implementation details.

AI becomes observable.

Git becomes the historical record.

NookOS becomes the workspace operating system that connects them all.

# Deployment Example

curl -fsSL https://install.nookos.dev | sh

nook join \
  --server https://nookos.dev \
  --token nook_join_7A9F...

✓ Validating token...

✓ Registering node...

✓ Detecting operating system...

✓ Detecting CPU...

✓ Detecting GPU...

✓ Detecting Docker...

✓ Detecting tmux...

✓ Detecting installed runtimes...

  Claude Code   ✓
  Hermes        ✓
  Codex         ✗

✓ Creating persistent connection...

Node Name:
buildbox

Workspace Root:
/workspace

Status:
Connected

The node immediately appears in the UI.

/////

The node sends something like:

{
  "hostname": "buildbox",
  "platform": "linux",
  "architecture": "x86_64",
  "cpus": 24,
  "memory": 68719476736,
  "gpus": [
    {
      "vendor": "NVIDIA",
      "model": "RTX 3080 Ti"
    }
  ],
  "docker": true,
  "tmux": true,
  "git": "2.48",
  "runtimes": [
    "claude",
    "hermes"
  ]
}

The control plane doesn't SSH to inspect the node. The node reports its capabilities.

////

Workspace discovery

The node asks:

Where should I look?

Maybe:

workspace_roots:
  - ~/workspace
  - ~/src
  - /projects

It discovers:

workspace/
    globex/
    acme/
    widgets/

Git makes this easy because repositories are self-describing.

I think the biggest architectural realization is this

Don't make users think about nodes.

Make them think about workspaces.

Nodes are infrastructure.

Workspaces are where people work.

A workspace can exist on multiple nodes:

Workspace

Widgets

Locations

✓ Desktop

feature/oauth

✓ Laptop

main

✓ Buildbox

CI

✓ GPU

AI generation

Now Nook can answer:

"I need Widgets."

Instead of:

"Which computer had Widgets checked out?" -> BUT WE could HAVE 2 computers with the same repo working on it. Can use git work trees AND other computers.