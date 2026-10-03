# Factory Droid ACP fixes for issue 1

This change addresses [issue 1](https://github.com/zaesho/monocode/issues/1) in the TypeScript provider, the Node host, and the native Rust provider. It is based on the committed `gpui-native` snapshot `b585761`. The fix uses a separate worktree to preserve unfinished native UI changes in the original checkout.

## Behavior and regression coverage

| Finding | Changed behavior | Verification |
| --- | --- | --- |
| F01 | Cancel, stop, and forget invalidate startup before spawn, binding, and prompt dispatch. Startup closes its client and stops any child created during invalidation. | Delayed resolver tests in both adapters. |
| F02 | Cancellation retires the connection and process. A follow-up waits for retirement, then resumes on a new connection. | Both adapters hold child termination pending and verify that no follow-up spawns early. TypeScript also injects old output after the new turn starts. |
| F03 | Edit mode only automatically approves `read`, `search`, and `edit`. Missing kinds, `delete`, `move`, and `switch_mode` require review. Sparse requests recover previously reported tool kinds. | Shared policy tests and sparse execute-permission tests in both adapters. |
| F04 | An unexpected exit closes pending requests without setting the user-cancellation flag. The turn rejects and reports an error. | Crash tests verify rejection and an error event. |
| F05 | Permission tasks catch reply failures. Shared request dispatch preserves wire order. Stop drains pending permission replies before closing the client and removing the child. | TypeScript stop-with-pending-permission test, synchronous request-order tests, and Node host transport tests. |
| F06 | Permission selection uses offered semantic kinds and preserves the original option identifier. A missing choice produces cancellation. | Exact manual, automatic, and planning response assertions. Installed Droid also offered `proceed_once` and `cancel`. |
| F07 | A failed resume preserves the existing binding and rejects the turn. It does not start a new conversation. | Transient storage-error tests in both adapters. |
| F08 | The adapter registers the approval waiter before emitting the approval event. | Synchronous-denial tests in both adapters. |
| F09 | A model switch waits for its configuration before applying the requested effort. Unsupported, rejected, and ineffective settings fail before prompting. | Delayed configuration tests in both adapters and a rejected-effort test. |
| F10 | Configuration and mode notifications update cached effective selections. Control replies preserve notifications received while the request was pending. | TypeScript model-fallback regression and mode-notification races in both adapters. |
| F11 | Cancellation resolves permissions with the ACP `cancelled` outcome. Late requests cannot create approval UI or automatic approvals. | Pending and late permission tests, plus installed Droid cancellation. |
| F12 | Unloaded or partial Droid catalogs preserve saved model IDs and settings. Only a completed catalog normalizes unsupported settings. | TypeScript and Rust model-resolver tests. |
| F13 | Partial catalogs stay eligible for refresh. Native discovery publishes accumulated effort metadata before each further request. | Native timeout followed by a successful retry. |
| F14 | Exit and failed-start cleanup release approval senders. Closed clients suppress permission writes. | Three repeated crash cycles verify that weak references to native live sessions expire. |
| F15 | Usage cache keys include environment and account. Remote sessions report that usage is unavailable because the host has no usage RPC. | Cache isolation and rendered-footer tests. No desktop usage request runs for a remote environment. |
| F16 | The credential reader honors the inherited Factory home root. Configured executable paths report that their effective account home cannot be determined. | Home-precedence tests and a wrapper test that returns before reading credentials. |
| C01 | Numeric UI approval IDs map back to complete JSON-RPC identifiers, including string type. | String, number, and internal-ID collision tests in both ACP wrappers. Installed Droid 0.228.0 emitted a numeric permission ID. |
| C02 | The reader supports `auth.v2.keyring` with the `Factory CLI` service and `auth-encryption-key` account. It uses macOS Keychain, Linux Secret Service through `secret-tool`, or Windows Credential Manager. | A synthetic encrypted keyring fixture and Linux-target and Windows-target type checks of the credential reader. No live keyring credential was read for these tests. |

ACP defines the cancellation boundary and permission outcome in its [prompt-turn protocol](https://agentclientprotocol.com/protocol/v1/prompt-turn). Permission replies must select an offered option under the [tool-call protocol](https://agentclientprotocol.com/protocol/v1/tool-calls). Model and mode snapshots follow the [session configuration protocol](https://agentclientprotocol.com/protocol/v1/session-config-options).

## Validation

- The regression tests reproduced nine TypeScript failures before implementation. A further mode-notification regression failed in both adapters before its fix.
- All 4,098 web tests pass, with 13 skipped. This includes the Droid, Cursor, Grok, and Hermes providers, shared ACP helpers, model recovery, usage routing, and the footer.
- All 1,267 tests pass across the Rust core, provider, and integration crates. The 35 ignored provider tests require installed CLIs.
- All 1,577 native engine tests pass with the default features, with four ignored tests. The narrower attention suite also passes all 156 tests.
- All 119 Node host tests pass, with two skipped tests. This includes Droid over the host process transport.
- Frontend and host TypeScript checks, the Vite production build, the host bundle build, and the Tauri Cargo check pass.
- Clippy passes for all targets in the Rust core, provider, and integration crates with `-D warnings`. The changed Rust and TypeScript files pass their formatter checks.
- Installed Droid 0.228.0 catalog discovery passes and returns per-model effort metadata. A separate spec-mode probe receives the actual offered permission choices, replies with cancellation, and observes `stopReason: cancelled`.
- The Linux and Windows credential readers type-check against their targets in isolation. The complete Windows cross-build cannot compile `ring` because this Mac lacks Windows C headers.

Live Windows and Linux keyring access, full native app interaction, and production-host deployment remain unverified.
