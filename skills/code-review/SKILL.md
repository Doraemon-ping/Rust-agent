---
name: code-review
description: Review Rust code for correctness, safety, and clarity. Use when asked to review a change, hunt for bugs, or audit a module.
---

# Code Review

Work through this checklist in order and report findings as a short list.

1. **Correctness** - does the code do what its name and docs claim? Look for
   off-by-one errors, mishandled error paths, and `unwrap`/`expect` on
   fallible work.
2. **Safety** - untrusted input, path handling, and anything crossing a trust
   boundary. Flag panics that user input can reach.
3. **Clarity** - names, dead code, and comments that merely restate the code.
4. **Tests** - is the new behaviour covered by a test that would fail if the
   change were reverted?

Finish with the single change you would make first.
