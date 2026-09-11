---
title: "HTTP requests fail silently on WASM target"
date: 2026-01-18
author: John A. De Goes
closed: 2026-01-28
plan: "dev-docs/plans/done/example.md"
---

# HTTP requests fail silently on WASM target

> **Template only — fictional, not project history.** This is a worked example of the dev-docs
> workflow; its `HttpClient`/`wstd`-vs-`reqwest` scenario is invented, not a record of clank's
> actual HTTP transport work.

## Problem

When running on `wasm32-wasip2`, outbound HTTP calls from `ask` to model providers fail without any error message. The process exits `0` as if the call succeeded, but no model response is produced.

## Impact

The WASM target is unusable for any workflow that requires model inference. This affects all `ask` invocations on the WASM target.

## Context

The native target works correctly. The failure is specific to `wasm32-wasip2`. No error appears in `/var/log/http.log`. The issue was first reported during integration testing of the Golem deployment target.

## Out of Scope

This issue does not cover streaming response support or timeout configuration. Those are separate capability questions.

## Resolution

Resolved by introducing an `HttpClient` trait with target-specific implementations. See plan and realized design for full details.
