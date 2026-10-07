# E2B JS SDK compatibility run

`scripts/sdk-tests.sh` runs the official E2B JS SDK sandbox tests against envd-rs in Docker.

## How the SDK reaches envd

With `E2B_DEBUG=true` the SDK (`packages/js-sdk/src/connectionConfig.ts`) skips the E2B API entirely:
`Sandbox.create()` returns the fixed id `debug_sandbox_id` and every envd call goes to `http://localhost:49983`.
The SDK sends no `Authorization` header in this mode, so envd falls back to its default user.

## What the script does

1. Builds `docker/Dockerfile` (Rust release build, Debian runtime with `user`, `sudo`, `python3`, `nodejs`) as `envd-rs:dev`.
2. Starts it with `-p 49983:49983` (`envd -isnotfc -verbose`).
3. Calls `POST /init` with `{"defaultUser":"user","defaultWorkdir":"/home/user"}`, the same call the orchestrator makes for a real sandbox.
4. Clones `e2b-dev/E2B` at the pinned `SDK_COMMIT` into `target/e2b-sdk`, installs it with pnpm and builds `@e2b/dockerfile-utils`.
5. Runs `vitest --project unit tests/sandbox/commands tests/sandbox/files tests/sandbox/pty`.

Extra arguments go to vitest, e.g. `./scripts/sdk-tests.sh --reporter=verbose` or `./scripts/sdk-tests.sh -t "run command"`.

## Last result

SDK commit `3221e8ec`: 25 test files passed, 105 tests passed, 9 skipped.
The skipped tests are marked `skipIf(isDebug)` by the SDK itself (signed URLs, global env vars, killed sandbox); envd-rs covers signing in `tests/files_contract.rs`.
