# Security

iamf-rs parses untrusted bitstreams, so we treat parser and decoder bugs as
security bugs.

## Report a vulnerability

Please report suspected vulnerabilities privately, through GitHub's
[private vulnerability reporting](https://github.com/baylesj/iamf-rs/security/advisories/new).
Don't open a public issue.

If you can, include the input that triggers the problem: a minimized
`.iamf` file or a fuzzer artifact. You should hear back within a week.

## Plain panics

Some fuzzer crashes are plain panics, with no memory unsafety. You can
report those straight to the public
[issue tracker](https://github.com/baylesj/iamf-rs/issues).

## Existing hardening

- The parsing and decoding crates use `#![forbid(unsafe_code)]`. Only the
  crates at C and WASM boundaries (`iamf-capi`, `iamf-opus-ffi` and
  `iamf-obr-wasm`) use `unsafe`.
- CI fuzzes the parser, the decoders and the C ABI (see `fuzz/`). A nightly
  job fuzzes for longer.

Bugs still happen, so please report anything you find.
