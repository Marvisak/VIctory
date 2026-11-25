# Instructions for AI agents

This file is for coding agents (Claude Code, Codex, Copilot, Cursor, …) working on VIctory.
Many agents work on this project at once. Most wasted effort comes from duplicate or invisible
work, not from slow work. Read this before you start, and leave what you learn where the next
agent will find it.

## The project

VIctory runs the Xbox Series build of one title natively on Windows. The title's x64 code runs
in our process; we provide the console platform (`victory-hle`), translate its GPU work to
D3D12 (`victory-gpu`) and its RDNA2 shaders to DXIL (`victory-shader`). The rest of the workspace:
`victory-loader`, `victory-pe`, `victory-formats`, `victory-pipeline`, `victory-game` and the
`apps/victory` CLI. Start with `README.md` for status and switches, then `docs/architecture.md`.

## Rule #1: no game data

Read `docs/asset-policy.md`. Never commit, paste or attach anything that comes from the game:
files, dumps, shaders, captured frames, or logs that contain its data. Tools refuse to write
derived output under the repository, so keep dumps and captures outside it.
`cargo xtask install-hooks` makes every commit run `cargo xtask policy --staged`. Do not skip the
hook; if it fails, fix the cause. (On Windows, the hook needs `cargo` on `PATH`.)

## Before you write code

1. **Search for existing work, including issue bodies.** Fixes are often posted as fork branches
   inside issues, not as PRs. Search open PRs *and* issues for the refusal text, the function or
   the shader hash you are working on:
   `gh pr list -R Marvisak/VIctory --search "<keywords>"` and
   `gh issue list -R Marvisak/VIctory --search "<keywords>"`, plus a text search of their bodies.
   If someone is already on it, test or review their work and report numbers instead of
   writing a fourth version.
2. **Measure before choosing.** Pick work by what it unblocks, not by what is easy to spot:
   - Shaders: `victory translate <game dir> --validate --list` gives every program's outcome.
     Run it on `main` and on your branch and compare **program by program**. Count gains *and*
     regressions; totals alone hide a regression behind a gain.
   - Runtime: a diagnostics build (`cargo build --profile diag -p victory --features diagnostics`)
     with `VICTORY_GPU_TIMING=1` shows where the card's time goes, per command and pipeline. Do
     not optimize a part you have not measured: a guess can point at the wrong half of the frame.
3. **Check whether the code already has it behind a switch.** Read the `README.md` switch table
   and search the source for `VICTORY_` before building something new. Some fixes exist but are
   off by default.
4. **Say what you are doing.** For anything bigger than a small fix, comment on the issue with a
   two-line plan, so others can see it is taken.

## Running the title fast

- First-run screens (text size, language, legal) are saved in the data folder after one pass;
  later runs start at the account prompt.
- A diagnostics build presses keys on a schedule (`VICTORY_KEYS=15:Enter,18:Enter`) and saves frames
  (`VICTORY_CAPTURE=<dir outside the repo>`), so a run needs nobody at the controls. Look at the
  frames to know which screen a run reached.
- A real pad or the keyboard reaches the title while its window has focus. Stray input changes
  how far a run gets, so keep hands off (or the pad off) during comparison runs.
- Windows resets the display driver when one GPU submission runs longer than about 2 s (TDR).
  A slow frame can therefore end a run with "device removed" without being a hang. Say which one
  it was in reports.
- On NVIDIA (32-lane waves only), wave64 programs need special forms. Note the GPU vendor in
  every report, because results differ between vendors.

## Pull requests

Merged PRs here are **small and single-purpose**. Most are under about 130 changed lines, with
one change and one test. Follow what they do:

- **Title:** `<Area>: <what now works>`, e.g. `Shaders: image_sample_o with constant offsets`.
  The areas used most in `git log`: `Shaders`, `GPU`, `GPU diagnostics`, `loader`, `xmem`, `PE`,
  `XGameSave`, `Docs`. Use an existing one when it fits.
- **One logical change per PR.** Split preparation, the change, and diagnostics. Do not reformat
  files you do not change: the tree is not `rustfmt`-clean, and whole-file formatting buries the
  diff.
- **A test that fails without the change.** Build shader tests from hand-written ISA words, as
  `resource.rs` and `emit/*_tests.rs` do; no game data. Prove the test fails on `main` before you
  claim it tests the change.
- **Run what CI runs and report the real counts:**
  `cargo clippy --workspace --all-targets --locked -- -D warnings`, the same with
  `--all-features`, `cargo test --workspace --locked`, and `cargo xtask policy`.
- **Description sections:** what and why (link the issue); Change; Results (before → after,
  e.g. validated programs from the census); Testing (hardware, commands, counts); **Not tested**
  (required: an honest "not run in the title" is fine); **AI use** (tool and exact model).
- Never write numbers you did not measure. Name the exact commit and hardware for every number.

## Share what you learn

- **Test reports** on someone else's PR or issue help most when they name the commit, the
  hardware, the switches and the numbers, and say what was *not* tested.
- **Dead ends count as results.** If an approach does not help ("the fix exists but changes 2
  draws a frame", "the budget switch hangs the card"), post it on the relevant issue with the
  numbers. That saves the next agent the same hours.
- Do not post on GitHub (issues, PRs, comments, reviews) unless the person you work for asks
  you to in that session.
