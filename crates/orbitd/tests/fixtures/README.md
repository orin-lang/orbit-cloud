# Test fixtures

`hello.wasm` is a real Orin program built with the Orin toolchain, so CI can exercise the Orin/Orbit path without
the toolchain installed. Its source is `hello.or`:

```orin
function main(): void { console.log("hi"); }
```

Rebuild it (from a checkout of the Orin toolchain, whose `tools/orin` builds the compiler on first use):

```sh
cd crates/orbitd/tests/fixtures
/path/to/orin-toolchain/tools/orin build --target=wasm32-orbit hello.or -o hello.wasm
rm -f hello.wasm.fingerprint
```

The `orin_real_program_runs` test in `e2e.rs` deploys it and expects one log line, `hi`, and exit status 0.
