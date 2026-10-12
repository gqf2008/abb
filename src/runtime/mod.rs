//! buzz — vendored upstream buzz-acp (`crates/buzz-acp` of github.com/block/buzz),
//! ported into the ABB process and trimmed for single-process use.
//!
//! Source pinned at `buzz-acp-port-c3132c3` (upstream commit c3132c3); Apache-2.0,
//! license text in [`UPSTREAM-LICENSE.txt`]. The per-file sync ledger that used to record this
//! provenance（连同 rsync 规则）was removed together with the buzz-agent fork (0685e66), so the
//! pin is **historical provenance only** — never infer upstream behaviour from ABB-local
//! changes, and do not re-open a sync/rsync process from this comment.
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
pub mod log_out;
pub mod oneshot;
pub mod pool;
pub mod prompt_framing;
pub mod queue;
