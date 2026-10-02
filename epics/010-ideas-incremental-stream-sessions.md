# Epic 010: Ideas — Incremental Results and API Stream Sessions

Status: Ideas — reference for future epics; not approved for implementation

Motivating run: None supplied. This document records an architecture discussion
following Epic 009's batched source loading. No API or saved-format feature is
requested for the current CSV milestone.

User intent: Let CLI, GUI, and a future API process rows incrementally. A future
API should accept JSON or JSONL rows, allow a caller to continue a particular
operation by stream ID, and handle requests without that ID independently.
Saved formats may eventually define operations such as adding a derived column.

## Purpose and relationship to Epic 009

Epic 009 concerns reading selected source rows in batches and limiting retained
source memory. It does not require a partial result to be visible after each
batch. Today, the streaming executor evaluates a filter as each row arrives,
but accumulates matching rows into a complete materialized view. CLI output and
the GUI result view appear after execution completes. Full-file inspection,
source verification, grounding, and run recording may also delay the first
visible result. This idea separates incremental computation from incremental
delivery; later epics can address them independently.

## Proposed direction

### A processing session in core

A format-independent processing session would own a validated, versioned plan,
its input schema, operation state, resource limits, row/provenance position, and
diagnostics. It would accept an ordered batch of rows and return zero or more
result events. A finalization step would mark the input complete and produce
final diagnostics and any final result. The session should have typed errors
and deterministic behavior for a given plan and ordered input. It should not
know about HTTP, API stream IDs, CLI formatting, or GUI frames.

Filters and projections can emit accepted rows as they arrive. Running sum,
count, minimum, maximum, and average can update compact state (average needs
sum and count). A grouped aggregate needs state per group. Distinct needs a
seen-key set. Exact median, sorting, and some other operations may require
growing state or temporary storage. Every operation needs an explicit memory
or spill policy; batching input alone does not bound result or aggregate state.

An emitted aggregate is provisional while more rows may arrive. Result events
must distinguish an append, an update or replacement, a diagnostic, and a
final result. Consumers should never mistake a running value or incomplete
row count for a final materialized view. Errors after partial delivery also
need a defined outcome rather than silently treating earlier events as final.

### A future API stream registry

The API layer would create an opaque stream ID and keep a registry from that ID
to an active processing session. Calls carrying the ID append rows to that
session in order. A request without a stream ID starts an independent request
or one-shot operation; it must not inherit another stream's state. The API
owns routing, authorization, lifecycle, expiration, cancellation, concurrency,
and limits on active sessions. Core owns the meaning of processing a new batch.

JSONL is a natural transport for row or result events; JSON can represent a
one-shot request or final response. Transport framing and event schemas need
their own versioning. A stream ID identifies a processing session, not a
source file or saved format. The API must define whether retries can replay a
batch, how ordering is enforced, and how it prevents duplicate application.
An in-memory registry is a possible first implementation, with explicit loss
of active state on server restart; durable resumption would require a separate
state and checkpoint design.

### Saved formats

A future saved format could hold a named, versioned schema and validated plan,
including a derived-column rule. Creating a session would resolve and pin one
format version. Later edits to the saved format would affect new sessions, not
change an active calculation. The saved-format store and its access policy are
separate from the live stream registry. A derived column that depends only on
one row can emit that row immediately; a derived value based on an aggregate
needs update or finalization semantics.

### CLI and GUI consumers

CLI and GUI can use the same core processing interface without maintaining an
API registry. CLI may choose final-only stdout, streamed row output, or
progress events on stderr. GUI may hold running values and show provisional
rows or aggregates while work continues. Neither presentation choice changes
the plan's deterministic final result. Source inspection, hashing, and any
grounding required before plan validation remain distinct from result
streaming and should be measured before promising lower time to first result.

## Boundaries and tradeoffs

- `baho-exec` owns deterministic incremental operation state; `baho-core`
  coordinates source metadata, validated plans, and sessions. Source adapters
  turn CSV or future JSON/JSONL inputs into the same row contract.
- The future API adapter owns stream IDs and transport concerns. API session
  state may be held in memory, but must have resource limits and an explicit
  lifetime. No process-global mutable state is required in core.
- Results retain source coordinates and raw-value distinctions where the input
  provides them. API-supplied rows need a defined provenance identity and
  ordering rule before they can share the same contract as file rows.
- A partial view is not an immutable `MaterializedView`. Publishing partial
  state needs an event or snapshot contract and a clear completion marker.
- Streaming output improves time to first result, not necessarily total scan
  time. Materializing a complete output can still consume memory or storage.

## Candidate follow-up epics

1. Define the incremental executor/session contract, result event types,
   finalization, and per-operation resource policy using synthetic row input.
2. Add optional progressive CLI and GUI presentation, with measured latency
   and explicit provisional states while preserving complete run artifacts.
3. Specify saved formats, versioning, validation, and how sessions pin a
   format version.
4. Design a JSON/JSONL API, stream registry, authentication, lifecycle,
   ordering, retries, limits, and restart behavior.

## Questions to resolve before implementation

- Which result events and revision numbers let clients apply aggregate updates
  safely? Are provisional rows retractable if a later validation error occurs?
- Which operations must support incremental output, and which may emit only on
  finalization? What memory and temporary-storage budgets apply?
- Are API streams finite uploads with an explicit close, or can they remain
  open indefinitely? What timeout and cancellation policy applies?
- What are the ordering, retry, and idempotency rules for appended batches?
- Must active sessions survive a server restart, or is an in-memory registry
  with explicit loss acceptable initially?
- How should source identity and provenance work for JSON/JSONL rows supplied
  across multiple API calls?

This epic records ideas only. It supplies context for narrower, reviewed epics
and introduces no implementation tasks or acceptance gate for the current
milestone.
