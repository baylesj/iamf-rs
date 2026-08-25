# Security

iamf-rs parses untrusted bitstreams, so parser and decoder bugs are
security bugs. The parsing crates are `#![forbid(unsafe_code)]` and
fuzzed in CI (`fuzz/`), but bugs happen.

Please report suspected vulnerabilities privately via GitHub's
[private vulnerability reporting](https://github.com/baylesj/iamf-rs/security/advisories/new)
rather than a public issue. Include the input that triggers the problem
(a minimized `.iamf` file or fuzzer artifact) if you can. You should
hear back within a week.

Crashes found by fuzzing that are plain panics (not memory unsafety) can
go straight to the public issue tracker.
