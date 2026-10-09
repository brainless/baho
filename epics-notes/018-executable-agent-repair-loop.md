# Epic 018 implementation note: executable agent repair loop

Date: 2026-10-09
Status: Implemented in tooling; real-build smoke test and live trial remain blocked by worker confinement.
Related epic: [018 — Executable Investigation and Agent Repair Loop](../epics/018-executable-agent-repair-loop.md)

## Implemented workflow

The excluded `tools/synth` package now has bounded synthetic full-pipeline probes and a `synth repair` entry point. A probe reports candidate body membership, ordered source-row provenance, diagnostics, structured plan evidence, and, for supported Role-equality SQL, oracle comparison. A regression distinguishes a populated row with an empty Role cell from a wholly blank record.

The repair harness records versioned attempts and structured events, freezes a neutral synthetic regression, guards the coder's file scope, and compares ordered rows and supported plan semantics. It pins a committed source revision, prepares isolated baseline and candidate builds, runs fixed check suites, and requires an independent review of the same patch hash before acceptance. Separate Rig histories serve investigation, coding, and review. The investigator supplies a reduced synthetic pair after baseline reproduction; the harness recomputes its restricted SQL oracle before freezing the regression. Failed or unavailable gates do not accept a patch.

The implementation is in `tools/synth/src/probe.rs`, `repair.rs`, `repair_build.rs`, `repair_live.rs`, `repair_agents.rs`, and `repair_entry.rs`, with supporting changes in `dataset.rs`, `lib.rs`, and `main.rs`. Product crates gained no agent runtime dependency. Epic 017 was not implemented.

## Verification

Task handoffs reported passing focused probe and repair tests, 162 synth tests, synth formatting and compile checks, Clippy with warnings denied, and workspace formatting, compile checks, and tests. The deterministic real-build smoke test was added but remains ignored on this host because the confinement worker cannot start. These results establish the offline gates; they do not establish a successful end-to-end repair attempt.

## Trial outcome and remaining work

The explicitly initiated trial stopped at the harmless confinement preflight: `sandbox-exec` running `/usr/bin/true` returned exit 71 with `sandbox_apply: Operation not permitted`. An escalated retry was interrupted before it returned a result. No candidate build, private replay, provider call, or candidate execution occurred. The outcome is **inconclusive: worker unavailable**. Semantic errors and human corrections were not observable; model cost was zero. The interrupted approval wait took about 1,605 seconds and is not trial execution time.

Run the deterministic real-build smoke test and then a new live attempt on a host with working constrained execution. The new attempt must revalidate its inputs and use a new attempt ID. No patch has been accepted and there is no trial evidence to justify batch orchestration.
