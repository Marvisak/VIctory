# Agent instructions

Read this file before changing code, opening issues, creating pull requests, reviewing changes, or merging branches.

## 1. Coordination is mandatory

The coordination system has two canonical entry points:

- [Agent Coordination Desk (#50)](https://github.com/vanya890/VIctory/issues/50): task queue, claims, blockers, handoffs, and ownership.
- [Upstream Coordination Index (#11)](https://github.com/vanya890/VIctory/issues/11): upstream findings, dependencies, import decisions, and integration status.

Before starting work:

1. Search the open issues and the two coordination issues for an existing task covering the same root cause.
2. Read recent comments and check whether another agent has claimed overlapping files or behavior.
3. Claim exactly one existing `TASK-ID` in #50. Include your agent identifier, precise scope, files/functions, and expected output.
4. If no canonical issue exists, create one focused issue only after checking for duplicates; then add its `TASK-ID` to #50.
5. Do not begin overlapping implementation while another active claim owns that scope. Coordinate a handoff first.
6. Review tasks are first-class work. An agent may claim an independent review of another agent's PR without claiming implementation ownership.

Use these status words consistently:

- `claiming`: work has started and scope is reserved.
- `finding`: evidence-backed result or newly discovered risk.
- `blocked`: progress requires a named dependency or decision.
- `handoff`: work is ready for another named role/agent.
- `done`: the promised output is complete; link the canonical result.
- `release`: the claim is relinquished without completion.

A completed research task does not mean its implementation PR is merged. Track implementation, review, and integration separately.

## 2. Keep the repository and issue tracker clean

- One canonical issue per root cause. Do not create one issue per agent, experiment, log line, or minor subcase.
- Keep #50 as a compact dispatch board. Put technical evidence in the relevant canonical issue or PR, then link it from #50.
- Keep #11 as an index, not a second copy of every technical report.
- Prefer updating an existing issue over creating a near-duplicate.
- Do not paste full CI logs, repeated benchmark tables, or entire upstream descriptions into multiple issues. Summarize and link the source.
- Separate confirmed facts, reproductions, hypotheses, and recommendations.
- Close an issue only when its tracked result is integrated or explicitly abandoned. A green upstream CI run is not proof that the fork has integrated the change.
- Do not claim that a task is complete while a required test, review, rebase, or integration step remains.
- Review comments must identify concrete code paths, invariants, failure modes, and evidence. Generic approvals such as "looks good" are not meaningful review.

## 3. Branch and pull-request policy

There are exactly two permanent branches:

- `main`: stable, release-ready branch.
- `dev`: active integration branch for accepted work.

All feature, fix, experiment, and upstream-import branches are temporary.

- Never push implementation changes directly to `main` or `dev`. Use a focused pull request.
- Start a short-lived branch from the latest `dev`; target normal implementation PRs at `dev`.
- Record the exact base and head SHA before opening or updating a PR. If the base moves, rebase or merge the new base, inspect the complete resulting diff, and rerun all affected checks.
- Do not keep long-lived per-upstream import branches. Do not create a new branch when a focused PR branch already exists and can be safely reused.
- One PR should solve one coherent problem. Split unrelated diagnostics, refactors, and behavior changes unless they are inseparable.
- When two PRs touch the same file, subsystem, or invariant, serialize their integration: merge/rebase one, then re-review the second against the new combined tree.
- After merge, delete the temporary branch when permissions and tooling allow. Do not delete a branch before confirming its PR is merged and no other work depends on it.
- Agents ARE AUTHORIZED to merge qualifying PRs into `dev` and to promote `dev` into `main`, but only after satisfying the gates in sections 4 and 5. Do not interpret the previous human-only merge restriction as active; it is superseded by this policy.
- The PR author/implementer may perform the final merge after independent reviewers have completed their work. The author may never count their own review as an independent review.
- Use GitHub's PR merge operation and supply the exact reviewed head SHA as the expected head. Never force-push a permanent branch or bypass branch protection, required checks, required reviews, or repository rules.
- If the agent lacks write permission or GitHub protection rejects the merge, do not work around the restriction. Report the exact blocker and hand off to an authorized agent/maintainer.

## 4. AI review and integration gates

The goal is not to collect superficial approvals. Every implementation PR must survive independent inspection and an explicit attempt to find defects.

### 4.1 Independent reviewers

Before merging any implementation PR into `dev`:

1. Obtain at least **two independent AI reviews** of the exact PR head:
   - **Correctness reviewer:** checks semantics, architecture, API/ABI compatibility, ownership/lifetimes, concurrency, error paths, and regressions.
   - **Adversarial reviewer:** assumes the implementation is wrong and actively searches for counterexamples, boundary cases, races, resource exhaustion, malformed input, and tests that would pass while the implementation is still incorrect.
2. Reviewers must be separate agent instances with separate review contexts. Prefer different model families or materially different reasoning configurations when available. Two passes by the same agent do not count as two independent reviews.
3. The implementation author must not write either review, approve their own work, or present their own self-review as independent evidence.
4. Reviewers must inspect the actual diff and relevant call sites. Do not rely solely on the PR description, author's summary, another reviewer's conclusion, or green CI.
5. Each reviewer must publish a review in the PR containing:
   - exact head SHA reviewed;
   - unique review `TASK-ID` from #50, with a direct link to the claim and handoff;
   - a non-secret agent-instance identifier, model family/name, and reasoning configuration/effort used for this review;
   - files/functions and invariants inspected;
   - concrete checks and counterexamples attempted;
   - findings with severity and evidence, or explicit statement that no blocking finding was found;
   - test gaps and residual risks;
   - verdict: `APPROVE`, `REQUEST_CHANGES`, or `BLOCKED`.

The #50 claim is the auditable record of a separate review context. Multiple reviews posted under one GitHub account may count as independent only when their TASK-IDs and agent-instance identifiers show distinct instances that inspected the diff separately. A GitHub username, different wording, or separate comment alone is not proof of independence.
6. A review is not independent if its author merely paraphrases an earlier review. Reviewers should initially inspect independently, then challenge each other's findings.
7. The implementer must respond to every finding. Fixes require a fresh review of the changed lines and affected invariants. A reviewer must not silently withdraw a finding because the author disagrees.
8. Any unresolved P0/P1 finding blocks merge. P2/P3 findings may remain only when their impact, rationale, and follow-up issue are recorded and they do not undermine correctness or safety.
9. If two reviewers disagree, a third independent reviewer adjudicates. Do not resolve a technical disagreement by majority vote without evidence.
10. If separate reviewers are unavailable, do not fabricate reviewer identities or claim a multi-agent review occurred. Keep the PR open and request review.

GitHub formal review approvals should be used when the reviewer has permission to submit them. If the repository requires a formal approval that the available agents cannot provide, the PR remains blocked until an authorized reviewer supplies it. PR comments are useful evidence but must never be misrepresented as GitHub APPROVE reviews.

### 4.2 Required validation for every PR

A PR is merge-ready only when all of the following are true:

- The diff is focused, based on the current target branch, and free of unexplained generated files, binaries, proprietary assets, or unrelated changes.
- The exact current PR head has completed every applicable required CI check successfully. Earlier green runs do not validate a newer commit.
- The final combined tree has been checked for conflicts and interactions with already merged or queued changes.
- Formatting is a required exact-head check: run `cargo fmt --all -- --check` after the final commit. If the current CI workflow does not run it, execute it separately and record the exact SHA, command, and result in the PR. Clippy is not a substitute for rustfmt.
- Required unit, integration, regression, policy, formatting, static-analysis, and platform-specific checks have been run. A skipped check is not evidence that the underlying behavior works.
- Tests cover the reported failure, normal behavior, negative cases, boundaries, and relevant failure/recovery paths.
- The two independent reviews in section 4.1 are complete, all blocking findings are resolved, and the final head has been re-reviewed after fixes.
- The PR has no unresolved review threads or unexplained failing/pending checks.
- The mergeability state and exact base/head SHAs have been rechecked immediately before merging.

For this repository, the normal Windows CI gate includes:

- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- `cargo test --workspace --locked`

The workspace tests include the repository asset-policy check. Follow [docs/asset-policy.md](docs/asset-policy.md) and [docs/architecture.md](docs/architecture.md). Do not add Linux CI as a merge blocker while the repository's current development policy is Windows-only.

### 4.3 Evidence quality and risk-based testing

- For every result, state the exact commit SHA, command, OS, adapter/GPU, driver where relevant, and passed/failed/ignored counts. Distinguish GitHub Actions CI from local hardware tests.
- For GPU/D3D12 changes, use targeted tests and, where available:
  - hardware adapter with default features;
  - hardware with diagnostics and D3D12 debug layer;
  - Microsoft WARP with diagnostics and D3D12 debug layer;
  - a second vendor where the behavior is vendor-dependent.
- Hardware-dependent claims require hardware evidence. If the required device is unavailable, state that explicitly and keep the change out of `main` until the evidence is obtained. A `dev` merge is allowed only when the change is safely testable without that hardware or is explicitly marked experimental/blocked from release; do not describe it as hardware-validated.
- A microbenchmark is not an FPS result. Do not extrapolate per-page, allocator, shader, or copy timings into game frame-rate claims without a measured same-scene comparison.
- Never say a test passed if it was not run. Never infer a successful run from a green status belonging to a different SHA.

### 4.4 Permission to merge into dev

An AI agent MAY merge a PR into `dev` without waiting for the project owner when every applicable requirement in sections 4.1–4.3 is satisfied.

Before the merge:

1. Re-fetch the PR and confirm it is open, not draft, mergeable, and targets `dev`.
2. Re-check the exact head SHA, base SHA, review results, unresolved threads, and CI status.
3. Confirm no newer commit was pushed after the final reviews and successful checks.
4. Merge using the exact reviewed head SHA as the expected head. If it changed, stop and repeat the affected review and validation.
5. After merging, verify the resulting `dev` commit and run/inspect the post-merge CI on that resulting commit. If post-merge CI fails, investigate immediately, stop further dependent merges, and fix or revert with a documented reason.

This is explicit authorization for agents to merge into `dev`. Do not ask the owner for routine permission when all gates pass.

## 5. Release gate: promoting dev into main

Merging into `main` is permitted for AI agents, but it is a release operation and has a higher bar than normal integration.

- The only normal promotion path is a focused PR from the current `dev` into the current `main`. Do not merge an individual feature branch directly into `main`.
- Promote a reviewed snapshot of `dev`; do not combine unrelated, unfinished work merely to make a release.
- At least **three independent AI review passes** are required for the `dev` → `main` PR:
  1. release-diff auditor: checks every commit and file included since the exact current `main` SHA, including main-only commits and accidental scope expansion;
  2. correctness/security auditor: rechecks critical invariants, unsafe code, resource lifetime, concurrency, data integrity, and failure handling;
  3. adversarial release auditor: searches for regressions, missing tests, incompatible interactions, incomplete features, and evidence gaps.
- At least two of the three reviewers must not have implemented the changes under review. The reviewer roles must be performed by distinct agent instances; do not count repeated passes by one agent.
- All P0/P1 findings must be resolved. P2/P3 residual risks must be explicitly documented and accepted in the PR with a reason and follow-up issue where appropriate.
- The complete Windows CI suite must pass on the exact release candidate/merge candidate. Re-run after any rebase, new commit, conflict resolution, or change to the candidate tree.
- All hardware-dependent changes included in the release must have the required real-hardware evidence. WARP or synthetic tests alone do not prove vendor hardware behavior.
- Verify that `dev` contains no accidental proprietary data, forbidden assets, debug dumps, local credentials, secrets, or unreviewed workflow/security changes.
- Review the full `main...dev` change set and commit graph. Identify main-only commits and explain how they are preserved. Do not overwrite or discard changes that exist only on `main`.
- Confirm the release notes or PR body accurately describe included changes, known limitations, and validation evidence.

An AI agent MAY merge the `dev` → `main` PR without routine human approval when every release requirement above and all GitHub repository protections are satisfied. If any requirement cannot be verified, the release is blocked; do not lower the bar to force a merge.

After promotion, verify the new `main` SHA and its CI. If the release checks fail, stop further promotion and prepare a narrow corrective PR or revert. Do not silently leave `main` red.

## 6. Upstream import procedure

For every upstream issue or PR:

1. Verify the current upstream state, exact head SHA, base SHA, changed files, and whether it is merged, closed, superseded, or still active.
2. Read the PR description, review comments, requested changes, and later follow-up commits. A stale green CI result does not validate a newer head.
3. Compare the candidate with the fork's current `dev`, not only with fork `main` or upstream `main`.
4. Identify dependencies, overlapping changes, API/ABI effects, and any behavior intentionally omitted by the upstream author.
5. Check whether an equivalent fix is already present in the fork.
6. Port only the smallest coherent change. Do not stack alternative implementations or reserve the same resource range twice.
7. Preserve upstream attribution and link the source commit/PR in the fork PR.
8. Add or retain regression tests for the actual failure mode, including negative/boundary cases.
9. Re-run checks on the final fork PR head after every rebase or merge that changes the combined tree.
10. Apply the review and merge gates in sections 4 and 5. An upstream approval or green upstream CI is supporting evidence, not a substitute for fork review and validation.

Do not cherry-pick a large integration PR merely because it makes a title reach a scene. First isolate its dependencies and independently review each subsystem.

## 7. Workflow security

- Never execute upstream PR/build/test code in a job that has repository write permissions, secrets, or a token capable of mutating refs/issues/PRs.
- Keep CI and scheduled triage read-only: fetch metadata and Git objects, compute merge trees/patch IDs, run candidate tests in isolated jobs without write credentials, and publish reports only where safe.
- Candidate-controlled code must not receive repository secrets, privileged tokens, or write access. Do not use `pull_request_target` to check out and execute untrusted PR code with elevated permissions.
- Merge authorization in this file applies to an agent acting after it has independently verified completed evidence. It does NOT authorize a candidate-controlled workflow to merge itself, approve itself, change branch protections, publish secrets, or bypass required checks.
- If a separate publisher or merge service is introduced, isolate it from candidate execution and independently verify pinned candidate heads, artifacts, and expected refs. Do not trust reports or bundles produced in a candidate-controlled workspace without validation.
- Never use force-push, admin bypass, disabling branch protection, fabricated reviews, or manually forged green statuses to get a merge through.
- Human review is welcome and may be required by actual GitHub repository settings, but it is NOT a blanket policy requirement in this file. Qualified AI agents are explicitly authorized to make merge decisions under the gates above.
- If repository rules require a human or a permission unavailable to agents, obey the rule and report the blocker. This policy does not grant GitHub permissions or override repository configuration.

## 8. Correctness rules for low-level changes

Pay particular attention to:

- unsafe pointer validity, lifetime, alignment, aliasing, and readable/writable range contracts;
- integer overflow and bounds before pointer arithmetic or D3D12 calls;
- COM/resource lifetime through command recording and execution;
- descriptor heap bounds, null slots, dynamic ranges, and page-table reservations;
- lock ordering, re-entrancy, concurrent Release/eviction, and snapshot-before-unprotect invariants;
- subresource, mip, array/depth, plane-count, block-compression, and copy-footprint semantics;
- shader cache keys and all translation options that affect generated DXIL.

An upstream author's safety comment is a claim to verify against every fork call site, not proof by itself.

## 9. Asset and data policy

Nothing from GTA VI or another proprietary title may be committed. Do not add executables, game files, extracted assets, shader dumps, screenshots, decompiled/disassembled code, or data derived from them. Use synthetic fixtures and user-supplied local paths. The only existing exception is documented in [docs/asset-policy.md](docs/asset-policy.md); agents must not create new exceptions.

## 10. Required handoff format

Every completed task must leave a concise comment in its canonical issue containing:

- upstream issue/PR and exact commit SHA;
- fork `main`/`dev` status and relevant file/function comparison;
- changed files and behavior reviewed;
- tests with exact command, platform/GPU/WARP, and counts;
- independent reviewers, exact reviewed head SHA, their verdicts, and links to findings;
- unresolved review objections, risks, and dependencies;
- verdict: `ready to port`, `needs changes`, `blocked`, `merged to dev`, or `released to main`;
- link to the fork PR and the resulting merge commit SHA when merged.

Then add one short `done`, `blocked`, or `handoff` entry to #50. Do not duplicate the full technical report there.
