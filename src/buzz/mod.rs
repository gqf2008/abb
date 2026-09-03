//! buzz — vendored upstream buzz-acp (`crates/buzz-acp` of github.com/block/buzz),
//! ported into the ABB process and trimmed for single-process use.
//!
//! Source pinned at `buzz-acp-port-c3132c3` (upstream commit c3132c3); Apache-2.0,
//! license text in [`UPSTREAM-LICENSE.txt`]; per-file provenance and sync rules
//! in `docs/buzz-port-sync.md` — rsync the six sync-listed files from upstream,
//! never copy ABB-local changes back.
//!
//! Trim port's shape: only `acp`, `pool`, `queue`, `prompt_framing` and the
//! ABB-side `harness` carry production logic. Everything relay/config/
//! observer/usage related was deleted in the port; the process is
//! message-driven — no heartbeat, no REST context fetch, no reactions — and
//! replies are captured as turn text ([`pool::PromptResult::final_text`]) for
//! synchronous delivery by the ABB bridge.

pub mod acp;
pub mod harness;
pub mod keys;
pub mod pool;
pub mod prompt_framing;
pub mod queue;
