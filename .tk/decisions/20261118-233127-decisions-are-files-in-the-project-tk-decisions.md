+++
title = "Decisions are files in the project (.tk/decisions)"
date = "2026-11-18T23:31:27Z"
status = "accepted"
source = "user"
supersedes = "20261118-233127-decision-log-lives-in-services-sqlite.md"
+++

## Context

The first decision log stored decisions in the host's SQLite database, away from the code they explain.

## Decision

Each decision is a Markdown file with TOML front matter in <project>/.tk/decisions/, committed with the code. Core's own decisions about the host stay in the host's data folder (decisions.jsonl) and show as Host decisions. SQLite is only used where we need a cache.

## Alternatives

Keep SQLite (decisions not reviewable, lost with the machine); one decisions.md (merge conflicts between parallel sessions and branches).

## Why

Decisions belong to the project: they are reviewed and committed like code, travel with branches, and an agent reading the repo finds them. A .tk folder per project mirrors .claude.
