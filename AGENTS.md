# Notes for contributors and coding agents

These rules live in the repository so they hold no matter who, or which agent, makes the change.

## Versioning

JetsonTR uses a deliberately humble `MAJOR.MINOR.PATCH` scheme, written as `#.#.##`:

- **Each release of improvements bumps the third number** (PATCH): `0.2.1` → `0.2.2` → … → `0.2.99`.
- **Bump the second number** (MINOR) only after roughly 100 improvements, resetting PATCH: `0.2.99` → `0.3.0`.
- **Leave MAJOR at 0.** There is no 1.0 plan.
- Never write leading zeros (`0.2.01`): Cargo and SemVer reject them. `#.#.##` means "count up to two digits", not "pad to two digits".
- Not every commit needs a bump. Bump when publishing a release.

Release steps:

1. Set `version` in `Cargo.toml` and build, so `Cargo.lock` updates too.
2. Commit as `Release X.Y.Z`.
3. Tag it with an annotated tag `vX.Y.Z`.
4. Push the branch and the tag.
5. Create a GitHub release with two or three lines on what changed.

## Language

- Repository material is written in English first.
- `README.ko.md` is the Korean translation. Keep it in sync with `README.md`.
