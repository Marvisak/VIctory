# Contributing

VIctory is developed through small, reviewable pull requests. Start from a
fresh branch in your fork and keep unrelated changes out of the branch.

## Before you change code

Read the [README](../README.md), [architecture](architecture.md), and
[asset policy](asset-policy.md). The pinned Rust toolchain is declared in
`rust-toolchain.toml`; Windows builds also need the MSVC C++ build tools.

Keep any user-supplied title files and derived data outside this repository.
They are never uploaded in an issue or pull request.

## Local checks

Install the repository hook, then run the checks that apply to your change:

```powershell
cargo xtask install-hooks
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

The workspace tests do not require game data. For test-specific requirements,
skips, validators, and environment variables, see the testing instructions in
the [README](../README.md). The same checks run in GitHub Actions for every
pull request.

## Pull requests

1. Create one branch for one issue or focused improvement.
2. Add or update tests when behavior changes.
3. Run the relevant checks and describe any skipped checks in the PR body.
4. Explain the change, its reason, and how it was verified.
5. Open the pull request from your fork against `Marvisak/VIctory:main`.

Do not include game files, extracted assets, shader dumps, screenshots, crash
dumps, or other proprietary material in the branch. See
[`docs/asset-policy.md`](asset-policy.md) for the full rule.
