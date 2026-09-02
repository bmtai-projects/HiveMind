---
id: test-writing
name: Test Writing
description: Writes tests that can actually fail -- edge cases and failure modes, not happy-path restatements of the code.
---
A test earns its place by failing when the code is wrong. Before writing
one, name what break it would catch; if you cannot, do not write it.

- Read the implementation first and test its real contract -- the branches,
  boundaries, and error paths it actually has. A test written from the
  function name alone tests your guess, not the code.
- Cover the edges, not just the middle: empty input, one element, the
  boundary value and one either side, maximum size, duplicates, wrong
  types, unicode and non-ASCII text, negative and zero, null/None, and
  concurrent or out-of-order arrival where it applies.
- Test failure paths as first-class cases. What happens when the network
  times out, the file is missing, the input is malformed, the disk is
  full, the permission is denied? Assert on the specific error, not merely
  that something was raised.
- One behavior per test, named for the behavior it protects -- a name that
  reads as a sentence about the system beats `test_foo_2`. When it fails a
  year from now, the name should say what broke.
- Assert on real observable outcomes: returned values, written files,
  recorded calls, resulting state. Avoid asserting on internals that will
  change for reasons unrelated to correctness.
- Keep tests deterministic and independent: no dependence on wall-clock
  time, random seeds, network access, ordering between tests, or state
  left by a previous test. Fake the clock and stub the network rather than
  sleeping and hoping.

Beware the test that cannot fail: an assertion that restates the
implementation line for line, a mock so complete that only the mock is
exercised, an assertion that always holds regardless of input. If a test
passes when you deliberately break the code it covers, it is decoration
-- delete it or fix it.

Verify by running. Run the suite, read the real output, and confirm both
that new tests pass and that they fail against broken code -- break it on
purpose once if that is the only way to know. Report the actual results,
including anything still uncovered.
