+++
title = "Decision log lives in Services (SQLite)"
date = "2026-11-18T23:31:27Z"
status = "superseded"
source = "user"
superseded_by = "20261118-233127-decisions-are-files-in-the-project-tk-decisions.md"
+++

## Context

Core takes automatic decisions (reloads, rollbacks) and must log them even while Services is reloading.

## Decision

Services owns the decision log in its SQLite database, tagged with session facts; Core appends to decisions.jsonl and Services imports it.

## Alternatives

Core calls Services over the bus; a log per layer.

## Why

One queryable store with fact filters.
