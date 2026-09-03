//! Inbound message queue for the ABB buzz harness.
//!
//! Manages per-channel message queues with per-channel in-flight tracking.
//! When the harness is ready to prompt the agent, it flushes the channel with
//! the oldest pending message, draining ALL messages for that channel into a
//! single batch. Multiple channels can be in-flight simultaneously; each
//! channel is independent.
//!
//! Ported from upstream buzz-acp `queue.rs` @ c3132c3 and trimmed to ABB's
//! transport (docs/buzz-port-sync.md): upstream queued signed `nostr::Event`s
//! and had the agent publish replies with the `buzz` CLI; ABB queues plain
//! chat messages ([`InboundMsg`]) and delivers the agent's turn text
//! synchronously at turn end. The queue state machine itself (dedup modes,
//! in-flight tracking, retry/backoff, cancel+steer merging, native-steer
//! withholding) is transport-agnostic and unchanged.
//!
//! ## Dedup modes
//!
//! - **Drop** (default) — while a prompt is in-flight for channel C, new messages
//!   for channel C are silently dropped (debug-logged). Messages for other channels
//!   still queue normally.
//! - **Queue** — all messages accumulate; batched on the next flush cycle.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Dedup policy for messages arriving while a channel's prompt is in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DedupMode {
    /// Drop new messages for an in-flight channel (default).
    #[default]
    Drop,
    /// Queue them for the next flush of that channel.
    Queue,
}

/// Maximum events queued per channel before oldest events are dropped.
const MAX_PENDING_PER_CHANNEL: usize = 500;

/// Maximum events drained into a single batch.
const MAX_BATCH_EVENTS: usize = 50;

/// Maximum retry attempts before a batch is dead-lettered.
pub(crate) const MAX_RETRIES: u32 = 10;

/// Base retry delay in seconds (doubled each attempt).
const BASE_RETRY_DELAY_SECS: u64 = 5;

/// Cap on retry delay in seconds.
const MAX_RETRY_DELAY_SECS: u64 = 300;

/// Buffer added to `max_turn_duration` to derive the in-flight deadline.
const IN_FLIGHT_DEADLINE_BUFFER_SECS: u64 = 100;

/// Default in-flight deadline: default max_turn (7200s) + 100s buffer.
const DEFAULT_IN_FLIGHT_DEADLINE_SECS: u64 = 7300;

/// A chat message delivered by the ABB bridge for agent processing.
///
/// Replaces upstream's signed `nostr::Event` (docs/buzz-port-sync.md, 案 1):
/// ABB has no nostr identity world, so the harness keys on the bot-local
/// message id and carries only what prompt framing needs.
#[derive(Debug, Clone)]
pub struct InboundMsg {
    /// Bot-local unique message id (hex string; dedup / steer-withhold keying).
    pub id_hex: String,
    /// Author role as labelled by the bridge ("user", "system", …). Rendered
    /// into prompt event blocks; role values are bridge-owned.
    pub author_role: String,
    /// Message text content.
    pub text: String,
    /// Unix seconds when the bridge accepted the message (ordering key).
    pub ts_secs: i64,
    /// Tag identifying which rule (or mode) matched this message.
    pub prompt_tag: String,
}

/// A message waiting in the queue.
#[derive(Debug, Clone)]
pub struct QueuedEvent {
    pub channel_id: Uuid,
    pub msg: InboundMsg,
    pub received_at: Instant,
}

/// A single message inside a [`FlushBatch`].
#[derive(Debug, Clone)]
pub struct BatchEvent {
    pub msg: InboundMsg,
    pub received_at: Instant,
}

/// Why a batch's prior turn was cancelled — controls how `format_prompt`
/// frames the merged re-prompt.
///
/// 裁剪（阶段 2）：上游的 `Interrupt` 档在本 harness 不可达——桥侧只有消息
/// 到达即合并的 steer 语义与 `/cancel`，故仅保留 [`CancelReason::Steer`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    /// A message arrived while the agent was working; it should **continue**
    /// and incorporate the message if relevant.
    Steer,
}

/// A batch of events to prompt the agent with.
#[derive(Debug, Clone)]
pub struct FlushBatch {
    pub channel_id: Uuid,
    pub events: Vec<BatchEvent>,
    /// Events from a cancelled batch that triggered this re-prompt.
    /// Empty for normal (non-cancel) batches. When non-empty, `format_prompt()`
    /// produces a merged prompt with annotated sections, framed per
    /// [`cancel_reason`](Self::cancel_reason).
    pub cancelled_events: Vec<BatchEvent>,
    /// How the prior turn was cancelled, when [`cancelled_events`] is non-empty.
    /// `None` for normal (non-merge) batches; falls back to the gentler
    /// [`Steer`](CancelReason::Steer) framing if a merge somehow lacks a reason
    /// (see [`MergeFraming::for_reason`]).
    pub cancel_reason: Option<CancelReason>,
}

/// Per-channel event queue with per-channel in-flight enforcement.
///
/// # State Machine
///
/// ```text
/// State:
///   queues:               Map<channel_id, VecDeque<QueuedEvent>>  (capped at MAX_PENDING_PER_CHANNEL)
///   in_flight_channels:   HashSet<Uuid>
///   in_flight_deadlines:  Map<channel_id, Instant>                (auto-expire after in_flight_deadline)
///   retry_after:          Map<channel_id, Instant>
///   retry_counts:         Map<channel_id, u32>                    (dead-letter after MAX_RETRIES)
///   dedup_mode:           DedupMode
///
/// Transitions:
///   push(event):
///     if dedup_mode == Drop AND in_flight_channels.contains(event.channel_id):
///       debug log + discard
///     else if queues[channel].len() >= MAX_PENDING_PER_CHANNEL:
///       drop oldest (pop_front), warn, push_back new event
///     else:
///       queues[event.channel_id].push_back(event)
///
///   flush_next() → Option<FlushBatch>:
///     expire any stuck in-flight entries past their deadline
///     candidates = channels where queue non-empty
///                  AND NOT in in_flight_channels
///                  AND (no retry_after OR retry_after[c] <= now)
///     if candidates empty: return None
///     channel = pick candidate with oldest head event (min received_at)
///     events = drain up to MAX_BATCH_EVENTS from queues[channel]
///     in_flight_channels.insert(channel)
///     in_flight_deadlines.insert(channel, now + in_flight_deadline)
///     return Some(FlushBatch { channel, events })
///
///   mark_complete(channel_id):
///     in_flight_channels.remove(channel_id)
///     in_flight_deadlines.remove(channel_id)
///     retry_counts.remove(channel_id)
///     clean up expired retry_after entry if present
///
///   requeue(batch):
///     increment retry_counts[channel]
///     if retry_counts[channel] > MAX_RETRIES: dead-letter (log ERROR, return batch to caller)
///     else: push_front with original received_at, set exponential backoff retry_after with jitter
/// ```
pub struct EventQueue {
    queues: HashMap<Uuid, VecDeque<QueuedEvent>>,
    in_flight_channels: HashSet<Uuid>,
    /// Per-channel deadline for auto-expiring stuck in-flight entries.
    in_flight_deadlines: HashMap<Uuid, Instant>,
    /// Number of events in each in-flight batch (for expiry logging).
    in_flight_batch_sizes: HashMap<Uuid, usize>,
    retry_after: HashMap<Uuid, Instant>,
    /// Per-channel retry attempt counter for exponential backoff / dead-lettering.
    retry_counts: HashMap<Uuid, u32>,
    dedup_mode: DedupMode,
    /// Events from cancelled batches, keyed by channel. Merged into the next
    /// `FlushBatch` for that channel as `cancelled_events` so `format_prompt()`
    /// can produce annotated "[Previous request — interrupted]" sections.
    cancelled_batches: HashMap<Uuid, Vec<BatchEvent>>,
    /// Why each channel's cancelled batch was cancelled (steer vs interrupt).
    /// Set by `requeue_as_cancelled`, consumed by `flush_next` to set
    /// `FlushBatch::cancel_reason`. Keyed by channel, cleared on flush.
    cancel_reasons: HashMap<Uuid, CancelReason>,
    /// Events withheld from `queues` while a goose-native steer is in flight
    /// for that event. Invisible to `flush_next` / `has_flushable_work` /
    /// `drain` (the events have been moved out of `queues`), so the queue's
    /// no-double-deliver invariant holds without any change to the hot drain
    /// path. Populated by [`mark_native_steer_pending`]; drained back to the
    /// queue front by [`release_native_steer`] (preserving original
    /// `received_at` fairness, same discipline as `requeue_preserve_timestamps`
    /// at line 453). Bulk recovery on in-flight deadline expiry is performed
    /// by `flush_next` / `has_flushable_work` (recover, not log-and-drop —
    /// the events were never delivered to the agent).
    withheld_native_steer: HashMap<Uuid, Vec<QueuedEvent>>,
    /// Duration after which an in-flight channel is auto-expired as orphaned.
    /// Must be strictly greater than `max_turn_duration` so a turn running to
    /// the hard cap returns via `mark_complete` before the backstop fires.
    in_flight_deadline: Duration,
}

impl EventQueue {
    /// Create a new empty event queue with the given dedup mode.
    ///
    /// Uses [`DEFAULT_IN_FLIGHT_DEADLINE_SECS`] for the in-flight backstop.
    /// Call [`with_in_flight_deadline`](Self::with_in_flight_deadline) to
    /// derive the deadline from the configured `max_turn_duration`.
    pub fn new(dedup_mode: DedupMode) -> Self {
        Self {
            queues: HashMap::new(),
            in_flight_channels: HashSet::new(),
            in_flight_deadlines: HashMap::new(),
            in_flight_batch_sizes: HashMap::new(),
            retry_after: HashMap::new(),
            retry_counts: HashMap::new(),
            dedup_mode,
            cancelled_batches: HashMap::new(),
            cancel_reasons: HashMap::new(),
            withheld_native_steer: HashMap::new(),
            in_flight_deadline: Duration::from_secs(DEFAULT_IN_FLIGHT_DEADLINE_SECS),
        }
    }

    /// Set the in-flight backstop deadline from the configured max turn
    /// duration, preserving the 100s buffer for cancel-drain grace + respawn.
    pub fn with_in_flight_deadline(mut self, max_turn_duration_secs: u64) -> Self {
        self.in_flight_deadline =
            Duration::from_secs(max_turn_duration_secs + IN_FLIGHT_DEADLINE_BUFFER_SECS);
        self
    }

    /// Monotonically extend an existing in-flight deadline for `channel_id`.
    ///
    /// Called when a successful steer grants a fresh turn budget. The new
    /// deadline is `max(current, now + max_turn_secs + buffer)` — it never
    /// moves backward. If the channel is not in-flight (already completed
    /// via `mark_complete`), this is a no-op: a late ack never resurrects
    /// a deadline.
    pub fn extend_in_flight_deadline(&mut self, channel_id: Uuid, max_turn_secs: u64) {
        if let Some(current) = self.in_flight_deadlines.get_mut(&channel_id) {
            let extended = Instant::now()
                + Duration::from_secs(max_turn_secs + IN_FLIGHT_DEADLINE_BUFFER_SECS);
            if extended > *current {
                tracing::info!(
                    %channel_id,
                    "extending in-flight deadline by {max_turn_secs}s + {IN_FLIGHT_DEADLINE_BUFFER_SECS}s buffer"
                );
                *current = extended;
            }
        }
    }

    /// Push an event into the queue for its channel.
    ///
    /// In [`DedupMode::Drop`], events for any currently in-flight channel are
    /// silently discarded (debug-logged).
    ///
    /// Returns `true` if the event was accepted, `false` if dropped.
    pub fn push(&mut self, event: QueuedEvent) -> bool {
        if matches!(self.dedup_mode, DedupMode::Drop)
            && self.in_flight_channels.contains(&event.channel_id)
        {
            tracing::debug!(
                channel_id = %event.channel_id,
                "dropping event for in-flight channel (drop mode)"
            );
            return false;
        }
        let queue = self.queues.entry(event.channel_id).or_default();
        // Enforce per-channel depth cap: drop oldest to make room.
        if queue.len() >= MAX_PENDING_PER_CHANNEL {
            queue.pop_front();
            tracing::warn!(
                channel_id = %event.channel_id,
                limit = MAX_PENDING_PER_CHANNEL,
                "queue depth cap reached — dropped oldest event"
            );
        }
        queue.push_back(event);
        true
    }

    /// Try to flush the next batch.
    ///
    /// Returns `None` if all non-in-flight, non-throttled queues are empty.
    /// Otherwise picks the channel with the oldest pending event (FIFO fairness
    /// across channels), drains ALL events for that channel into a single batch,
    /// inserts into `in_flight_channels`, and returns the batch.
    pub fn flush_next(&mut self) -> Option<FlushBatch> {
        let now = Instant::now();

        // Auto-expire any stuck in-flight entries that missed mark_complete.
        let expired: Vec<Uuid> = self
            .in_flight_deadlines
            .iter()
            .filter(|(_, deadline)| now >= **deadline)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            let lost_events = self.in_flight_batch_sizes.remove(&id).unwrap_or(0);
            tracing::error!(
                channel_id = %id,
                lost_events,
                deadline_secs = self.in_flight_deadline.as_secs(),
                "BUG: in-flight channel expired without mark_complete — \
                 auto-releasing; {lost_events} dispatched event(s) orphaned"
            );
            self.in_flight_channels.remove(&id);
            self.in_flight_deadlines.remove(&id);
            // Recover any withheld goose-native steer events for the expired
            // channel back to the queue front so normal dispatch delivers
            // them. Unlike the in-flight batch above (already delivered to a
            // now-hung prompt — nothing to recover), these events were never
            // delivered to the agent.
            self.recover_withheld_for_expired_channel(id);
        }

        // Find the channel whose head event has the oldest received_at,
        // excluding in-flight channels and throttled channels.
        let channel_id = self
            .queues
            .iter()
            .filter(|(id, q)| {
                !q.is_empty()
                    && !self.in_flight_channels.contains(id)
                    && self.retry_after.get(id).is_none_or(|&t| t <= now)
            })
            .min_by_key(|(_, q)| q.front().unwrap().received_at)
            .map(|(id, _)| *id);

        // Fallback: if no queued events are ready but a channel has cancelled
        // events waiting (e.g., explicit !cancel with no new @mention), flush
        // those as a regular batch (re-dispatch unchanged).
        let channel_id = match channel_id {
            Some(id) => id,
            None => {
                let cancelled_id = self
                    .cancelled_batches
                    .keys()
                    .find(|id| !self.in_flight_channels.contains(id))
                    .copied();
                let id = cancelled_id?;
                // Move cancelled events into the regular events slot.
                // No new events to merge — re-dispatch the original batch.
                let cancelled = self.cancelled_batches.remove(&id).unwrap_or_default();
                let cancel_reason = self.cancel_reasons.remove(&id);
                self.in_flight_channels.insert(id);
                self.in_flight_deadlines
                    .insert(id, now + self.in_flight_deadline);
                self.in_flight_batch_sizes.insert(id, cancelled.len());
                return Some(FlushBatch {
                    channel_id: id,
                    events: cancelled,
                    cancelled_events: vec![],
                    cancel_reason,
                });
            }
        };

        // Drain up to MAX_BATCH_EVENTS; leave any remainder in the queue.
        let queue = self.queues.entry(channel_id).or_default();
        let drain_count = MAX_BATCH_EVENTS.min(queue.len());
        let mut events: Vec<BatchEvent> = queue
            .drain(..drain_count)
            .map(|qe| BatchEvent {
                msg: qe.msg,
                received_at: qe.received_at,
            })
            .collect();
        // Batch consumers — `format_prompt` scope and reply-anchor selection —
        // require the LAST message to be the newest. Stable sort: same-second
        // messages keep acceptance order.
        events.sort_by_key(|be| be.msg.ts_secs);

        // Remove the queue entry if now empty.
        if self.queues.get(&channel_id).is_some_and(|q| q.is_empty()) {
            self.queues.remove(&channel_id);
        }

        self.in_flight_channels.insert(channel_id);
        self.in_flight_deadlines
            .insert(channel_id, now + self.in_flight_deadline);
        self.in_flight_batch_sizes.insert(channel_id, events.len());

        // Merge any cancelled events stored by requeue_as_cancelled().
        let cancelled_events = self
            .cancelled_batches
            .remove(&channel_id)
            .unwrap_or_default();
        let cancel_reason = if cancelled_events.is_empty() {
            self.cancel_reasons.remove(&channel_id);
            None
        } else {
            self.cancel_reasons.remove(&channel_id)
        };

        Some(FlushBatch {
            channel_id,
            events,
            cancelled_events,
            cancel_reason,
        })
    }

    /// Mark the prompt for `channel_id` as complete.
    ///
    /// Removes the channel from `in_flight_channels` and `in_flight_deadlines`.
    ///
    /// If the channel was NOT requeued (no active `retry_after` throttle), the
    /// retry counter is reset — the channel is healthy and the next failure
    /// starts fresh. If the channel WAS requeued, `retry_counts` is left intact
    /// so the backoff sequence continues on the next attempt.
    ///
    /// Also cleans up any already-expired `retry_after` entry.
    pub fn mark_complete(&mut self, channel_id: Uuid) {
        self.in_flight_channels.remove(&channel_id);
        self.in_flight_deadlines.remove(&channel_id);
        self.in_flight_batch_sizes.remove(&channel_id);
        let now = Instant::now();
        match self.retry_after.get(&channel_id) {
            // Active throttle → channel was requeued; keep retry_counts intact.
            Some(&deadline) if deadline > now => {}
            // Expired or absent throttle → successful completion; reset counter
            // and clean up the stale retry_after entry.
            Some(_) => {
                self.retry_after.remove(&channel_id);
                self.retry_counts.remove(&channel_id);
            }
            None => {
                self.retry_counts.remove(&channel_id);
            }
        }
    }

    /// Re-queue a batch of events that failed to process.
    ///
    /// Events are pushed back to the **front** of the channel's queue so they
    /// are processed first on the next flush cycle. This prevents event loss
    /// when session creation or `session/prompt` fails transiently.
    ///
    /// Original `received_at` timestamps are preserved so the channel retains
    /// its fairness position. The retry delay comes from exponential backoff,
    /// not from resetting received_at.
    ///
    /// After [`MAX_RETRIES`] attempts the batch is dead-lettered: logged at
    /// ERROR and returned to the caller (rather than requeued) so a visible
    /// failure notice can be posted to the channel. Returns `None` when the
    /// batch was requeued for another attempt.
    ///
    /// Note: does NOT remove from `in_flight_channels` — caller must call
    /// `mark_complete` separately.
    pub fn requeue(&mut self, batch: FlushBatch) -> Option<FlushBatch> {
        let channel_id = batch.channel_id;
        let attempt = {
            let count = self.retry_counts.entry(channel_id).or_insert(0);
            *count += 1;
            *count
        };

        if attempt > MAX_RETRIES {
            tracing::error!(
                channel_id = %channel_id,
                attempt,
                events = batch.events.len(),
                "dead-lettering batch after {} retries — discarding {} events",
                MAX_RETRIES,
                batch.events.len(),
            );
            self.retry_counts.remove(&channel_id);
            // Also clear retry_after so fresh traffic on this channel isn't
            // throttled by stale backoff from the discarded poison batch.
            self.retry_after.remove(&channel_id);
            return Some(batch);
        }

        // Exponential backoff: BASE * 2^(attempt-1), capped at MAX, with ±20% jitter.
        let base_secs = BASE_RETRY_DELAY_SECS.saturating_mul(1u64 << (attempt - 1).min(6));
        let capped_secs = base_secs.min(MAX_RETRY_DELAY_SECS);
        // Jitter: multiply by 0.8..1.2 using subsecond nanos as entropy source.
        let jitter = {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos();
            0.8 + (nanos as f64 / u32::MAX as f64) * 0.4
        };
        let delay = Duration::from_secs_f64(capped_secs as f64 * jitter);

        tracing::warn!(
            channel_id = %channel_id,
            attempt,
            max = MAX_RETRIES,
            delay_secs = delay.as_secs_f64(),
            events = batch.events.len(),
            "requeueing failed batch with backoff"
        );

        let queue = self.queues.entry(channel_id).or_default();
        // Push to front in reverse order so original order is preserved.
        for be in batch.events.into_iter().rev() {
            queue.push_front(QueuedEvent {
                channel_id,
                msg: be.msg,
                received_at: be.received_at, // preserve original timestamp (#46)
            });
        }
        // Enforce per-channel cap: trim oldest (back) events if requeue pushed
        // the queue over the limit. Without this, repeated requeue+push cycles
        // can grow the queue unboundedly.
        while queue.len() > MAX_PENDING_PER_CHANNEL {
            queue.pop_back();
            tracing::warn!(
                channel_id = %channel_id,
                limit = MAX_PENDING_PER_CHANNEL,
                "requeue overflow — dropped oldest event to enforce cap"
            );
        }
        self.retry_after.insert(channel_id, Instant::now() + delay);
        None
    }

    /// Re-queue a batch preserving original `received_at` timestamps.
    ///
    /// Used when a batch was flushed but no agent was available — we want to
    /// retry without penalizing the channel's position in the fairness queue
    /// and without imposing a retry throttle.
    ///
    /// Does NOT set `retry_after`. Does NOT remove from `in_flight_channels` —
    /// caller must call `mark_complete` separately.
    pub fn requeue_preserve_timestamps(&mut self, batch: FlushBatch) {
        let channel_id = batch.channel_id;
        let queue = self.queues.entry(channel_id).or_default();
        // Push to front in reverse order so original order is preserved.
        for be in batch.events.into_iter().rev() {
            queue.push_front(QueuedEvent {
                channel_id,
                msg: be.msg,
                received_at: be.received_at,
            });
        }
        // Enforce per-channel cap: trim newest (back) events if over limit.
        while queue.len() > MAX_PENDING_PER_CHANNEL {
            queue.pop_back();
            tracing::warn!(
                channel_id = %channel_id,
                limit = MAX_PENDING_PER_CHANNEL,
                "requeue_preserve overflow — dropped newest event to enforce cap"
            );
        }
    }

    /// Requeue a cancelled batch so its events appear as `cancelled_events`
    /// in the next `FlushBatch` for this channel (enabling the annotated
    /// merged-prompt format in `format_prompt()`).
    ///
    /// `reason` records why the turn was cancelled (steer vs interrupt) so the
    /// merged prompt is framed correctly. On a double-cancel, the most recent
    /// reason wins.
    ///
    /// Unlike `requeue_preserve_timestamps`, events are NOT pushed back into
    /// the generic queue — they are stored separately and merged by
    /// `flush_next()`. No retry throttle, no backoff.
    pub fn requeue_as_cancelled(&mut self, batch: FlushBatch, reason: CancelReason) {
        let entry = self.cancelled_batches.entry(batch.channel_id).or_default();
        // Preserve any already-cancelled events from a prior cancel (double-cancel).
        entry.extend(batch.cancelled_events);
        entry.extend(batch.events);
        self.cancel_reasons.insert(batch.channel_id, reason);
    }

    /// Number of channels with pending events.
    pub fn pending_channels(&self) -> usize {
        self.queues.len()
    }

    /// Drop all queued (non-in-flight) events for a channel.
    ///
    /// Used when the agent is removed from a channel — any pending events
    /// for that channel are stale and should not be prompted. Does NOT
    /// affect in-flight prompts (those will complete normally; the agent
    /// may fail to act if it lost access, but that's handled by the relay).
    ///
    /// Also clears any `retry_after` throttle for the channel.
    ///
    /// Returns the event IDs of dropped events so the caller can clean up
    /// any reactions (👀) that were added at queue-push time.
    pub fn drain_channel(&mut self, channel_id: Uuid) -> Vec<String> {
        let ids = self
            .queues
            .remove(&channel_id)
            .map(|q| q.into_iter().map(|e| e.msg.id_hex).collect())
            .unwrap_or_default();
        self.retry_after.remove(&channel_id);
        self.retry_counts.remove(&channel_id);
        self.cancelled_batches.remove(&channel_id);
        self.cancel_reasons.remove(&channel_id);
        self.withheld_native_steer.remove(&channel_id);
        // Preserve in_flight_channels AND in_flight_deadlines: the in-flight
        // task will eventually complete (calling mark_complete) or the deadline
        // will expire (auto-cleaning the channel). Removing deadlines without
        // removing in_flight_channels would disable auto-expiry and leave a
        // wedged task permanently blocking the channel.
        ids
    }

    /// Whether a prompt is currently in-flight for the given channel.
    pub fn is_channel_in_flight(&self, channel_id: Uuid) -> bool {
        self.in_flight_channels.contains(&channel_id)
    }

    // ── Goose-native steer withhold (side table) ──────────────────────────
    //
    // While a goose-native `_goose/unstable/session/steer` write is in flight
    // for a specific queued event, that event is moved out of `queues` into
    // `withheld_native_steer` so `flush_next` / `has_flushable_work` / the
    // contiguous drain at line 285 cannot see it — closing the race window
    // between `mark_complete` (which clears `in_flight_channels`) and the
    // ack arriving on the main loop. On `Success` the event is consumed
    // (`remove_event`); on `Err` / `PromptCompletedNeutral` it is released
    // back to the queue front (`release_native_steer`), preserving its
    // original `received_at` for FIFO fairness.

    /// Move a queued event out of `queues[channel_id]` into the side table
    /// to withhold it from `flush_next` while a goose-native steer is in
    /// flight.
    ///
    /// Returns `true` if the event was found and withheld, `false` if the
    /// event id was not present in `queues[channel_id]` (race-safe no-op:
    /// the event may have already been drained, removed, or never queued).
    ///
    /// Must be called synchronously from the mode-gate fork immediately
    /// after `pool.send_steer` returns `Ok(())` and before any watcher task
    /// is spawned, so the withhold is established before `mark_complete` /
    /// any subsequent `flush_next` tick can run.
    pub fn mark_native_steer_pending(&mut self, channel_id: Uuid, event_id: &str) -> bool {
        let Some(q) = self.queues.get_mut(&channel_id) else {
            return false;
        };
        let Some(pos) = q.iter().position(|qe| qe.msg.id_hex == event_id) else {
            return false;
        };
        let qe = q
            .remove(pos)
            .expect("position came from iter so remove must succeed");
        if q.is_empty() {
            self.queues.remove(&channel_id);
        }
        self.withheld_native_steer
            .entry(channel_id)
            .or_default()
            .push(qe);
        true
    }

    /// Release a single withheld event back to the front of
    /// `queues[channel_id]`, preserving its original `received_at`.
    ///
    /// Called on `SteerAck::Err(_)` and `SteerAck::PromptCompletedNeutral`
    /// (delivery unknown after prompt completion; restoring queued event
    /// for normal dispatch). Idempotent: a no-op if the event was already
    /// removed or never withheld.
    ///
    /// Push-to-front matches the discipline of `requeue_preserve_timestamps`
    /// at line 453, preserving fairness across channels.
    pub fn release_native_steer(&mut self, channel_id: Uuid, event_id: &str) {
        let Some(entries) = self.withheld_native_steer.get_mut(&channel_id) else {
            return;
        };
        let Some(pos) = entries.iter().position(|qe| qe.msg.id_hex == event_id) else {
            return;
        };
        let qe = entries.remove(pos);
        if entries.is_empty() {
            self.withheld_native_steer.remove(&channel_id);
        }
        // Push to FRONT so original `received_at` keeps the event at the head
        // of the channel's queue. Per-channel cap is enforced below in case
        // a flood of events arrived during the ack window.
        let queue = self.queues.entry(channel_id).or_default();
        queue.push_front(qe);
        while queue.len() > MAX_PENDING_PER_CHANNEL {
            queue.pop_back();
            tracing::warn!(
                channel_id = %channel_id,
                limit = MAX_PENDING_PER_CHANNEL,
                "release_native_steer overflow — dropped newest event to enforce cap"
            );
        }
    }

    /// Drop a specific event by id from both the side table and the main
    /// queue.
    ///
    /// Called on `SteerAck::Success` — the agent received the steer, so the
    /// event has been "delivered" via the non-cancelling path and must not
    /// be redelivered via normal dispatch. Idempotent across both stores.
    pub fn remove_event(&mut self, channel_id: Uuid, event_id: &str) {
        if let Some(entries) = self.withheld_native_steer.get_mut(&channel_id) {
            entries.retain(|qe| qe.msg.id_hex != event_id);
            if entries.is_empty() {
                self.withheld_native_steer.remove(&channel_id);
            }
        }
        if let Some(q) = self.queues.get_mut(&channel_id) {
            q.retain(|qe| qe.msg.id_hex != event_id);
            if q.is_empty() {
                self.queues.remove(&channel_id);
            }
        }
    }

    /// Bulk-release every withheld event for `channel_id` back to the queue
    /// front, preserving relative FIFO order.
    ///
    /// Called from the `in_flight_deadline` expiry blocks in
    /// `flush_next` and `has_flushable_work` — if a steer ack never arrives
    /// (read loop hung, watcher never posted), the withheld events would
    /// otherwise be permanently orphaned. Recover, do not log-and-drop: the
    /// events were never delivered to the agent, so normal dispatch must
    /// have a chance to deliver them.
    ///
    /// Iterates the stored entries in reverse so per-entry `push_front`
    /// composes to original-FIFO order at the queue front (same discipline
    /// as `requeue_preserve_timestamps` at line 453).
    fn recover_withheld_for_expired_channel(&mut self, channel_id: Uuid) {
        let Some(entries) = self.withheld_native_steer.remove(&channel_id) else {
            return;
        };
        let n = entries.len();
        let queue = self.queues.entry(channel_id).or_default();
        for qe in entries.into_iter().rev() {
            queue.push_front(qe);
        }
        while queue.len() > MAX_PENDING_PER_CHANNEL {
            queue.pop_back();
            tracing::warn!(
                channel_id = %channel_id,
                limit = MAX_PENDING_PER_CHANNEL,
                "withheld-steer recovery overflow — dropped newest event to enforce cap"
            );
        }
        tracing::warn!(
            channel_id = %channel_id,
            recovered = n,
            "in-flight expiry recovered withheld steer event(s) — \
             steer ack never arrived; normal dispatch will deliver"
        );
    }
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new(DedupMode::Drop)
    }
}

/// Extract a leading slash command from message content.
///
/// ACP connectors (claude-agent-acp, codex-acp) detect slash commands by
/// checking whether the **first** prompt content block starts with `/`. ABB
/// messages may lead with a mention of the bot's display name, so this strips
/// leading mention tokens (`@word`, multi-word display names from
/// `known_names`) and returns the remainder iff it is a slash command.
///
/// Returns `Some("/goal ship it")` when the first non-mention token starts
/// with `/` followed by an ASCII alphanumeric; `None` otherwise. A `/`
/// appearing later in the text (e.g. `"@Eva see /tmp/foo"`) never matches.
pub fn extract_slash_command(content: &str, known_names: &[&str]) -> Option<String> {
    // Longest-first so "Dawn Smith" wins over "Dawn".
    let mut names: Vec<&str> = known_names
        .iter()
        .copied()
        .filter(|n| !n.trim().is_empty())
        .collect();
    names.sort_by_key(|n| std::cmp::Reverse(n.len()));

    let mut rest = content.trim_start();
    loop {
        if rest.starts_with("nostr:npub1") || rest.starts_with("nostr:nprofile1") {
            // NIP-27 inline reference — skip the whole token.
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            rest = rest[end..].trim_start();
        } else if let Some(after_at) = rest.strip_prefix('@') {
            // Known display names first (longest match wins, case-insensitive,
            // must end at whitespace or end-of-string), then a single-word
            // token of the characters ABB allows in plain @mentions.
            let name_len = names
                .iter()
                .find_map(|name| {
                    let candidate = after_at.get(..name.len())?;
                    if !candidate.eq_ignore_ascii_case(name) {
                        return None;
                    }
                    match after_at[name.len()..].chars().next() {
                        None => Some(name.len()),
                        Some(c) if c.is_whitespace() => Some(name.len()),
                        _ => None,
                    }
                })
                .or_else(|| {
                    let len = after_at
                        .find(|c: char| {
                            !(c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
                        })
                        .unwrap_or(after_at.len());
                    (len > 0).then_some(len)
                });
            // Bare '@' (no name matched) is not a mention.
            rest = after_at[name_len?..].trim_start();
        } else {
            break;
        }
    }

    let mut chars = rest.chars();
    (chars.next() == Some('/') && chars.next().is_some_and(|c| c.is_ascii_alphanumeric()))
        .then(|| rest.to_string())
}

/// Return the slash command for a batch, if it qualifies for pass-through.
///
/// Pass-through is deliberately conservative: exactly one message, no cancelled
/// carryover (a cancel + re-prompt needs the merged context format), and
/// content that is a slash command after leading mentions.
pub fn slash_command_for_batch(batch: &FlushBatch, known_names: &[&str]) -> Option<String> {
    if batch.events.len() != 1 || !batch.cancelled_events.is_empty() {
        return None;
    }
    extract_slash_command(&batch.events[0].msg.text, known_names)
}

/// Channel metadata for prompt formatting (supplied by the ABB bridge).
#[derive(Debug, Clone, Default)]
pub struct PromptChannelInfo {
    /// Chat display name (group name / contact name), when the bridge has one.
    pub name: String,
    /// Chat type: `"dm"` for single chats, anything else for group chats.
    pub channel_type: String,
    /// Chat description / announcement, when the bridge exposes one.
    pub description: Option<String>,
}

/// Maximum length (in characters) of a channel description rendered into `<context>`.
///
/// Limits prompt bloat from unusually long descriptions. Multi-line
/// descriptions keep their line breaks but are rendered as an indented block
/// (see [`append_channel_description`]) so an embedded newline can never
/// Maximum length (in characters) of a channel description rendered into `<context>`.
///
/// Limits prompt bloat from unusually long descriptions. Multi-line
/// descriptions keep their line breaks but are rendered as an indented block
/// (see [`append_channel_description`]) so an embedded newline can never
/// spoof another `<context>` field.
const MAX_DESCRIPTION_LEN: usize = 500;

/// Append a `Description: …` field to a `<context>` body when non-empty.
///
/// Preserves the author's paragraph structure: a single-line description is
/// rendered inline (`Description: …`), while a multi-line description is
/// rendered as an indented block so line breaks and blank lines survive into
/// the agent's context. Every continuation line is indented by two spaces —
/// real `<context>` fields always start at column 0, so an embedded line like
/// `Scope: injected` stays visibly part of the description and cannot spoof
/// another field. Truncates at [`MAX_DESCRIPTION_LEN`] characters (before
/// indentation) with a `…` marker.
fn append_channel_description(s: &mut String, channel_info: Option<&PromptChannelInfo>) {
    let desc = match channel_info.and_then(|ci| ci.description.as_deref()) {
        Some(d) if !d.is_empty() => d,
        _ => return,
    };
    // Normalize every logical line separator a renderer or model may honor,
    // trim per-line trailing whitespace, and drop leading/trailing blank lines
    // while keeping interior blank lines (paragraph breaks) intact. CRLF is
    // collapsed first so it remains one break rather than becoming two.
    let unified = desc.replace("\r\n", "\n").replace(
        [
            '\r', '\u{0085}', '\u{2028}', '\u{2029}', '\u{000b}', '\u{000c}',
        ],
        "\n",
    );
    let normalized = unified
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    let normalized = normalized.trim_matches('\n').trim_end();
    if normalized.trim().is_empty() {
        return;
    }
    // Truncate at a character boundary (not byte boundary) to avoid splitting
    // multi-byte sequences.
    let truncated = if normalized.chars().count() > MAX_DESCRIPTION_LEN {
        let end = normalized
            .char_indices()
            .nth(MAX_DESCRIPTION_LEN)
            .map(|(i, _)| i)
            .unwrap_or(normalized.len());
        format!("{}…", &normalized[..end])
    } else {
        normalized.to_string()
    };
    // Channel metadata is untrusted prompt content. Escape semantic delimiters
    // before embedding it in `<context>` so text such as `</context>` cannot
    // terminate the section or introduce another model-visible section.
    let escaped = crate::buzz::prompt_framing::escape_semantic_text(&truncated);
    if escaped.contains('\n') {
        // Multi-line: indented block. Blank lines stay blank; content lines
        // are indented so field-like text remains visually subordinate.
        let indented: String = escaped
            .lines()
            .map(|line| {
                if line.is_empty() {
                    String::new()
                } else {
                    format!("  {line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        s.push_str(&format!("\nDescription:\n{indented}"));
    } else {
        s.push_str(&format!("\nDescription: {escaped}"));
    }
}

/// Format the per-message `[Message]` block for a single [`BatchEvent`].
///
/// Includes: message id, channel (name + UUID), author role, time and content.
/// Upstream also rendered sender pubkeys, event kind, raw tags and parsed
/// thread structure — ABB messages carry none of that (see [`InboundMsg`]).
pub(crate) fn format_event_block(
    channel_id: Uuid,
    channel_info: Option<&PromptChannelInfo>,
    be: &BatchEvent,
) -> String {
    let time = chrono::DateTime::from_timestamp(be.msg.ts_secs, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| be.msg.ts_secs.to_string());

    let channel_display = match channel_info {
        Some(ci) if !ci.name.is_empty() => format!("{} (#{channel_id})", ci.name),
        _ => channel_id.to_string(),
    };

    format!(
        "Message ID: {}\n\
         Channel: {channel_display}\n\
         From: {}\n\
         Time: {time}\n\
         Content: {}",
        be.msg.id_hex, be.msg.author_role, be.msg.text,
    )
}

/// Format a `<context>` hints section for the turn.
///
/// ABB virtualbot chats are flat: every dispatched message is a top-level chat
/// message (no reply chains, no thread tags). Scope is `dm` for single chats
/// and `channel` for groups. The delivery note is the one ABB-specific
/// instruction that must ride with every turn (the base prompt states the same
/// rules; repeated here in case the session was started without it).
fn format_context_hints(
    channel_id: Uuid,
    channel_info: Option<&PromptChannelInfo>,
    is_dm: bool,
) -> String {
    let channel_display = match channel_info {
        Some(ci) if !ci.name.is_empty() => format!("{} (#{channel_id})", ci.name),
        _ => channel_id.to_string(),
    };
    let scope = if is_dm { "dm" } else { "channel" };

    let mut s = format!(
        "Scope: {scope}\n\
         Channel: {channel_display}"
    );
    append_channel_description(&mut s, channel_info);
    s.push_str(
        "\nDelivery: your reply text is captured at the end of this turn and \
         delivered to this chat by the bridge. Do NOT try to publish, send, or \
         execute anything to deliver it — just output the reply.",
    );
    crate::buzz::prompt_framing::semantic_section("context", &s)
}

/// Arguments for [`format_prompt`] beyond the required [`FlushBatch`].
#[derive(Default)]
pub struct FormatPromptArgs<'a> {
    pub channel_info: Option<&'a PromptChannelInfo>,
    /// When true, base_prompt and system_prompt are delivered via the system
    /// role (session/new) and omitted from the user message. When false
    /// (legacy agents), they are injected as `<base>` and `<system>` sections.
    pub has_system_prompt_support: bool,
    /// Base prompt content for legacy agents (protocol_version < 2).
    pub base_prompt: Option<&'a str>,
    /// System prompt content for legacy agents (protocol_version < 2).
    pub system_prompt: Option<&'a str>,
    /// Set once this session's standing context has already been delivered —
    /// only meaningful for legacy agents; modern agents are gated by
    /// `has_system_prompt_support` regardless. Defaults to `false`.
    pub standing_context_sent: bool,
}

/// Format the `<base>` section for the base prompt.
///
/// Single source of truth for the `<base>` framing so the format is defined in
/// exactly one place across all dispatch paths (batch flush, heartbeat,
/// initial message).
pub(crate) fn base_section(base_prompt: &str) -> String {
    crate::buzz::prompt_framing::semantic_section("base", base_prompt.trim_end())
}

/// Format a [`FlushBatch`] into the per-section prompt blocks for the agent.
///
/// Produces a stable prompt with these sections (in order):
/// 0. Standing context — `<base>`, `<system>`. Legacy agents only (see
///    `has_system_prompt_support` / `standing_context_sent`).
/// 1. `<context>` — scope, channel name, description, delivery note.
/// 2. `<user-message>` / `<user-messages>` — the triggering message(s).
///
/// Each section is returned as its own block rather than one joined string so
/// an oversized section can be elided in place and the agent reconstructs the
/// full prompt by joining the blocks.
pub fn format_prompt(batch: &FlushBatch, args: &FormatPromptArgs<'_>) -> Vec<String> {
    // Scope is always derived from the LAST message in the batch — that's the
    // one the agent is responding to.
    if batch.events.is_empty() {
        tracing::error!("format_prompt called with empty batch — returning empty prompt");
        return Vec::new();
    }
    let is_dm = args
        .channel_info
        .map(|ci| ci.channel_type == "dm")
        .unwrap_or(false);

    let mut sections: Vec<String> = Vec::with_capacity(6);

    // Standing context — base prompt and system prompt. Modern agents received
    // both via the system role in session/new. Legacy agents get them here, in
    // the session's first message only; `standing_context_sent` means an
    // earlier message in this session already carried them.
    if !args.has_system_prompt_support && !args.standing_context_sent {
        if let Some(bp) = args.base_prompt {
            sections.push(base_section(bp));
        }
        if let Some(sp) = args.system_prompt {
            sections.push(crate::buzz::prompt_framing::semantic_section("system", sp));
        }
    }

    // Context hints (with the delivery note).
    sections.push(format_context_hints(
        batch.channel_id,
        args.channel_info,
        is_dm,
    ));

    // Cancelled + re-prompt framing. When a turn was cancelled to deliver new
    // messages mid-flight, the merged prompt is framed two ways depending on
    // why it was cancelled (see [`CancelReason`]):
    // - `Interrupt`: the new request *supersedes* the interrupted work.
    // - `Steer` (default): a message arrived while the agent was working; it
    //   should *continue* its work and weave the message in if relevant.
    let has_cancelled = !batch.cancelled_events.is_empty();
    let framing = MergeFraming::for_reason(batch.cancel_reason);

    // 4a. Cancelled messages section.
    if has_cancelled {
        let mut body = String::new();
        for (i, be) in batch.cancelled_events.iter().enumerate() {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&format!(
                "--- Message {} ({}) ---\n{}",
                i + 1,
                be.msg.prompt_tag,
                format_event_block(batch.channel_id, args.channel_info, be)
            ));
        }
        sections.push(crate::buzz::prompt_framing::semantic_section(
            framing.prior_tag,
            &body,
        ));
    }

    // 4b. Message block(s).
    let event_section = if batch.events.len() == 1 {
        let be = &batch.events[0];
        if has_cancelled {
            crate::buzz::prompt_framing::semantic_section(
                framing.new_tag,
                &format!(
                    "--- Message 1 ({}) ---\n{}",
                    be.msg.prompt_tag,
                    format_event_block(batch.channel_id, args.channel_info, be)
                ),
            )
        } else {
            crate::buzz::prompt_framing::semantic_section_with_attributes(
                "buzz-event",
                &[("type", be.msg.prompt_tag.as_str())],
                &format_event_block(batch.channel_id, args.channel_info, be),
            )
        }
    } else {
        let mut body = String::new();
        for (i, be) in batch.events.iter().enumerate() {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&format!(
                "--- Message {} ({}) ---\n{}",
                i + 1,
                be.msg.prompt_tag,
                format_event_block(batch.channel_id, args.channel_info, be)
            ));
        }
        let count = batch.events.len().to_string();
        crate::buzz::prompt_framing::semantic_section_with_attributes(
            if has_cancelled {
                framing.new_tag
            } else {
                "buzz-events"
            },
            &[("count", count.as_str())],
            &body,
        )
    };
    sections.push(event_section);

    // 4c. Closing note for cancel + re-prompt.
    if has_cancelled {
        sections.push(framing.closing_note.to_string());
    }

    sections
}

/// Prompt-framing strings for a merged (cancel + re-prompt) turn, selected by
/// [`CancelReason`]. `Interrupt` frames the new messages as superseding the
/// prior work; `Steer` (the default mid-turn path) frames them as messages
/// that arrived while the agent was working, to be woven in without abandoning
/// the in-progress task.
struct MergeFraming {
    /// Tag for the prior (cancelled) messages section.
    prior_tag: &'static str,
    /// Tag for newly arrived message sections.
    new_tag: &'static str,
    /// Closing instruction appended after the message block(s).
    closing_note: &'static str,
}

impl MergeFraming {
    fn for_reason(_reason: Option<CancelReason>) -> Self {
        // We never capture the agent's partial work — session/cancel is
        // terminal and returns nothing — so this section holds the
        // *original request*, not a transcript. The header must not
        // overclaim preserved state.
        MergeFraming {
            prior_tag: "what-you-were-working-on",
            new_tag: "new-message-arrived-while-you-were-working",
            closing_note: "Note: A new message arrived while you were working. Continue your \
                 in-progress work and incorporate the new message if it's relevant; if it's \
                 unrelated, you may briefly acknowledge it and carry on.",
        }
    }
}

/// Framing strings for the goose-native steer path (lib.rs mode-gate),
/// pulled from the same source-of-truth as the cancel+merge fallback
/// (`MergeFraming::for_reason(Some(CancelReason::Steer))`).
///
/// Returns `(new_tag, closing_note)`. Native-steer renders only
/// the new-message header + the single message block + the closing note —
/// no `prior_header`, no original-request section, because the in-flight
/// turn already has all of that in context. The two paths share
/// these strings so an agent receiving either transport gets the same
/// "weave it in, don't abandon your work" orientation (Eva's drift-proof
/// requirement: native and fallback must not diverge in UX).
pub(crate) fn native_steer_framing() -> (&'static str, &'static str) {
    let framing = MergeFraming::for_reason(Some(CancelReason::Steer));
    (framing.new_tag, framing.closing_note)
}
