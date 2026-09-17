---
title: "Bound tools as commands inside bash-tool"
date: 2026-09-17
author: agent
---

# Bound tools as commands inside bash-tool

The bash tool executes scripts, but its owner's other tool bindings are not available as shell
commands. Users cannot discover those capabilities with normal shell help or compose them with
pipes and command substitution. Its pending questions also cannot be answered across stateless
calls. Golem 1.6 needs a shell that any agent can bind and use with its existing tools.

Success means metadata-driven commands, correct argument and error handling, authorization,
shared owner files, and recovery. Independent tool stderr remains an upstream release requirement.
