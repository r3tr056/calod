//! Backing data structure for Redis **Streams** (the core subset; consumer
//! groups are a later phase).
//!
//! A stream is an append-only, id-ordered log of entries. Each entry has an id
//! of the form `<ms>-<seq>` (see [`StreamId`]) and an ordered list of binary
//! field/value pairs (`Vec<(Vec<u8>, Vec<u8>)>`). Field and value bytes are
//! binary-safe.
//!
//! # Ordering and ids
//!
//! Entry ids are strictly increasing: [`StreamData::add`] rejects any id that is
//! less than or equal to the current [`StreamData::last_id`], and also rejects
//! the special "zero" id `0-0` ([`StreamId::MIN`]). The `*` auto-id used by
//! `XADD foo *` is computed by [`StreamData::next_auto_id`] from the current
//! wall-clock millisecond.
//!
//! Entries are stored in a [`BTreeMap`] keyed by [`StreamId`], so range queries
//! ([`StreamData::range`]) and "read everything after id X"
//! ([`StreamData::read_after`]) are simple ordered map ranges.
//!
//! # Deletion and trimming
//!
//! [`StreamData::delete`] removes specific ids and [`StreamData::trim_maxlen`]
//! caps the stream to its newest `maxlen` entries. Neither operation lowers
//! `last_id` (ids are never reused). The largest id ever removed is tracked in
//! `max_deleted_id`, mirroring Redis's `max-deleted-entry-id` book-keeping.
//!
//! # Token parsing
//!
//! The free functions [`parse_id`], [`parse_range_start`] and
//! [`parse_range_end`] parse the textual id tokens used by stream commands. The
//! special command tokens `*`, `$`, `-` (as the "smallest" id outside a range
//! context) and `+` that depend on stream/command state are resolved by the
//! caller; only the plain/range forms are handled here.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound::{Excluded, Unbounded};

/// A single binary-safe field/value pair within a stream entry.
pub type StreamField = (Vec<u8>, Vec<u8>);

/// A stream entry: its id paired with its ordered field/value pairs.
pub type StreamEntry = (StreamId, Vec<StreamField>);

/// A stream entry id, `<ms>-<seq>`.
///
/// Ordered first by `ms` then by `seq`, which is exactly the lexical/temporal
/// ordering Redis uses. Derives [`Ord`] over the `(ms, seq)` tuple via field
/// declaration order.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default, Serialize, Deserialize,
)]
pub struct StreamId {
    pub ms: u64,
    pub seq: u64,
}

impl StreamId {
    /// The smallest possible id, `0-0`. Used as the lower bound for ranges and
    /// rejected by [`StreamData::add`] as the "zero" id.
    pub const MIN: StreamId = StreamId { ms: 0, seq: 0 };

    /// The largest possible id, `u64::MAX-u64::MAX`. Used as the upper bound for
    /// ranges (the `+` token).
    pub const MAX: StreamId = StreamId {
        ms: u64::MAX,
        seq: u64::MAX,
    };

    /// Render the id in Redis's `"ms-seq"` text form.
    pub fn to_redis_string(&self) -> String {
        format!("{}-{}", self.ms, self.seq)
    }
}

/// The backing store for a single stream key.
///
/// `entries` maps each id to its (ordered) field/value pairs. `last_id` is the
/// id of the most recently added entry (it never decreases). `max_deleted_id`
/// is the largest id ever removed via [`delete`](StreamData::delete) or
/// [`trim_maxlen`](StreamData::trim_maxlen).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StreamData {
    /// Serialized as a sequence of `(id, fields)` pairs rather than a native map,
    /// so the [`StreamId`] struct keys round-trip through serializers that require
    /// string map keys (e.g. JSON) as well as bincode — mirroring the binary-key
    /// handling for hashes.
    #[serde(with = "serde_entries")]
    entries: BTreeMap<StreamId, Vec<StreamField>>,
    last_id: StreamId,
    max_deleted_id: StreamId,
    /// Total number of entries ever added (never decreases), for `XINFO STREAM`
    /// `entries-added` and consumer-group lag accounting.
    #[serde(default)]
    entries_added: u64,
    /// Consumer groups defined over this stream, keyed by group name. (Phase 8b)
    #[serde(default)]
    groups: HashMap<String, ConsumerGroup>,
}

impl StreamData {
    /// Create an empty stream.
    pub fn new() -> Self {
        StreamData::default()
    }

    /// Number of entries currently stored.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the stream holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The id of the most recently added entry (`0-0` if nothing was ever
    /// added). This never decreases, even after deletion or trimming.
    pub fn last_id(&self) -> StreamId {
        self.last_id
    }

    /// The id a `*` auto-add should use given the current wall-clock `now_ms`.
    ///
    /// If `now_ms` is strictly greater than `last_id.ms` the new id is
    /// `(now_ms, 0)`; otherwise the clock has not advanced past the last entry's
    /// millisecond, so the sequence is bumped: `(last_id.ms, last_id.seq + 1)`.
    pub fn next_auto_id(&self, now_ms: u64) -> StreamId {
        if now_ms > self.last_id.ms {
            StreamId { ms: now_ms, seq: 0 }
        } else {
            StreamId {
                ms: self.last_id.ms,
                seq: self.last_id.seq.saturating_add(1),
            }
        }
    }

    /// Append an entry with an explicit `id`.
    ///
    /// Returns [`StreamError::ZeroId`] if `id` is `0-0` and
    /// [`StreamError::IdTooSmall`] if `id <= last_id`. On success the entry is
    /// inserted, `last_id` is updated to `id`, and `id` is returned.
    pub fn add(&mut self, id: StreamId, fields: Vec<StreamField>) -> Result<StreamId, StreamError> {
        if id == StreamId::MIN {
            return Err(StreamError::ZeroId);
        }
        if id <= self.last_id {
            return Err(StreamError::IdTooSmall);
        }
        self.entries.insert(id, fields);
        self.last_id = id;
        self.entries_added += 1;
        Ok(id)
    }

    /// Entries with id in `[start, end]` (inclusive on both ends), ascending.
    ///
    /// If `rev` is set the result is reversed (descending). `count`, when
    /// `Some`, limits the number of returned entries *after* ordering. If
    /// `start > end` the range is empty.
    pub fn range(
        &self,
        start: StreamId,
        end: StreamId,
        rev: bool,
        count: Option<usize>,
    ) -> Vec<StreamEntry> {
        if start > end {
            return Vec::new();
        }
        let iter = self
            .entries
            .range(start..=end)
            .map(|(id, fields)| (*id, fields.clone()));
        if rev {
            collect_limited(iter.rev(), count)
        } else {
            collect_limited(iter, count)
        }
    }

    /// Entries with id strictly greater than `after`, ascending, `count`-limited.
    ///
    /// This is the primitive behind `XREAD ... STREAMS key <id>`: it returns
    /// everything newer than the last id the caller has seen.
    pub fn read_after(&self, after: StreamId, count: Option<usize>) -> Vec<StreamEntry> {
        let iter = self
            .entries
            .range((Excluded(after), Unbounded))
            .map(|(id, fields)| (*id, fields.clone()));
        collect_limited(iter, count)
    }

    /// Delete the given `ids`; returns how many of them actually existed.
    ///
    /// `last_id` is left unchanged (ids are never reused). The largest id
    /// actually removed advances `max_deleted_id`.
    pub fn delete(&mut self, ids: &[StreamId]) -> usize {
        let mut removed = 0;
        for &id in ids {
            if self.entries.remove(&id).is_some() {
                removed += 1;
                if id > self.max_deleted_id {
                    self.max_deleted_id = id;
                }
            }
        }
        removed
    }

    /// Trim the stream to at most `maxlen` of its newest entries.
    ///
    /// Removes the `len - maxlen` smallest ids (the oldest entries) when the
    /// stream is longer than `maxlen`, advances `max_deleted_id` to the largest
    /// removed id, and returns the number of entries removed. `last_id` is left
    /// unchanged.
    pub fn trim_maxlen(&mut self, maxlen: usize) -> usize {
        let len = self.entries.len();
        if len <= maxlen {
            return 0;
        }
        let to_remove = len - maxlen;
        // Collect the smallest `to_remove` ids (BTreeMap iterates ascending).
        let victims: Vec<StreamId> = self.entries.keys().take(to_remove).copied().collect();
        for id in &victims {
            self.entries.remove(id);
        }
        if let Some(&largest) = victims.last() {
            if largest > self.max_deleted_id {
                self.max_deleted_id = largest;
            }
        }
        victims.len()
    }

    /// Total number of entries ever added to this stream (never decreases).
    pub fn entries_added(&self) -> u64 {
        self.entries_added
    }

    /// The largest id ever removed via delete/trim (`0-0` if none).
    pub fn max_deleted_id(&self) -> StreamId {
        self.max_deleted_id
    }

    /// The largest id currently present in the stream, if any.
    pub fn max_entry_id(&self) -> Option<StreamId> {
        self.entries.keys().next_back().copied()
    }

    /// The oldest entry `(id, fields)` still present, if any.
    pub fn first_entry(&self) -> Option<StreamEntry> {
        self.entries.iter().next().map(|(id, f)| (*id, f.clone()))
    }

    /// The newest entry `(id, fields)` still present, if any.
    pub fn last_entry(&self) -> Option<StreamEntry> {
        self.entries
            .iter()
            .next_back()
            .map(|(id, f)| (*id, f.clone()))
    }

    /// Number of consumer groups defined over this stream.
    pub fn groups_count(&self) -> usize {
        self.groups.len()
    }

    /// Set the stream's `last_id` (and optionally `max_deleted_id`) — `XSETID`.
    /// `entries_added` is overridden when provided.
    pub fn set_last_id(
        &mut self,
        id: StreamId,
        entries_added: Option<u64>,
        max_deleted: Option<StreamId>,
    ) {
        self.last_id = id;
        if let Some(n) = entries_added {
            self.entries_added = n;
        }
        if let Some(md) = max_deleted {
            self.max_deleted_id = md;
        }
    }

    // ------------------------------------------------------------------
    // Consumer groups (Phase 8b)
    // ------------------------------------------------------------------

    /// Whether a group with this name exists.
    pub fn group_exists(&self, name: &str) -> bool {
        self.groups.contains_key(name)
    }

    /// Create a consumer group whose initial `last_delivered_id` is `start`
    /// (the caller resolves `$` to [`StreamData::last_id`]). Returns `false` if a
    /// group of that name already exists (`XGROUP CREATE`).
    pub fn create_group(&mut self, name: &str, start: StreamId) -> bool {
        if self.groups.contains_key(name) {
            return false;
        }
        self.groups.insert(
            name.to_string(),
            ConsumerGroup {
                last_delivered_id: start,
                ..ConsumerGroup::default()
            },
        );
        true
    }

    /// Set a group's `last_delivered_id` (`XGROUP SETID`); `false` if no group.
    pub fn set_group_id(&mut self, name: &str, id: StreamId) -> bool {
        match self.groups.get_mut(name) {
            Some(g) => {
                g.last_delivered_id = id;
                true
            }
            None => false,
        }
    }

    /// Destroy a group (`XGROUP DESTROY`); `true` if it existed.
    pub fn destroy_group(&mut self, name: &str) -> bool {
        self.groups.remove(name).is_some()
    }

    /// Create a consumer in a group (`XGROUP CREATECONSUMER`). Returns `None` if
    /// the group is absent, `Some(true)` if created, `Some(false)` if it existed.
    pub fn create_consumer(&mut self, group: &str, consumer: &str, now_ms: u64) -> Option<bool> {
        let g = self.groups.get_mut(group)?;
        if g.consumers.contains_key(consumer) {
            return Some(false);
        }
        g.consumers.insert(
            consumer.to_string(),
            Consumer {
                seen_time_ms: now_ms,
                active_time_ms: now_ms,
                ..Consumer::default()
            },
        );
        Some(true)
    }

    /// Delete a consumer (`XGROUP DELCONSUMER`); its pending entries are removed
    /// from the group PEL. Returns `None` if no group, else the number of pending
    /// entries that were owned by the consumer.
    pub fn del_consumer(&mut self, group: &str, consumer: &str) -> Option<usize> {
        let g = self.groups.get_mut(group)?;
        let removed = match g.consumers.remove(consumer) {
            Some(c) => {
                for id in &c.pending {
                    g.pending.remove(id);
                }
                c.pending.len()
            }
            None => 0,
        };
        Some(removed)
    }

    /// `XREADGROUP ... >`: deliver up to `count` never-before-delivered entries
    /// (id `> last_delivered_id`) to `consumer`, advancing `last_delivered_id` and
    /// (unless `noack`) recording them in the group/consumer PEL. Returns `None`
    /// if the group is absent.
    pub fn group_read_new(
        &mut self,
        group: &str,
        consumer: &str,
        count: Option<usize>,
        now_ms: u64,
        noack: bool,
    ) -> Option<Vec<StreamEntry>> {
        let last_delivered = self.groups.get(group)?.last_delivered_id;
        let entries: Vec<StreamEntry> = {
            let it = self
                .entries
                .range((Excluded(last_delivered), Unbounded))
                .map(|(id, f)| (*id, f.clone()));
            match count {
                Some(n) => it.take(n).collect(),
                None => it.collect(),
            }
        };
        let g = self.groups.get_mut(group)?;
        g.consumers.entry(consumer.to_string()).or_default();
        for (id, _) in &entries {
            g.last_delivered_id = *id;
            g.entries_read += 1;
            if !noack {
                g.pending.insert(
                    *id,
                    PendingEntry {
                        consumer: consumer.to_string(),
                        delivery_time_ms: now_ms,
                        delivery_count: 1,
                    },
                );
                if let Some(c) = g.consumers.get_mut(consumer) {
                    c.pending.insert(*id);
                }
            }
        }
        if let Some(c) = g.consumers.get_mut(consumer) {
            c.seen_time_ms = now_ms;
            if !entries.is_empty() {
                c.active_time_ms = now_ms;
            }
        }
        Some(entries)
    }

    /// `XREADGROUP ... <id>`: re-read the consumer's own pending entries with id
    /// `>= from` (the history/PEL read). Does not change delivery counts. Deleted
    /// underlying entries are returned with empty fields. `None` if no group.
    pub fn group_read_history(
        &mut self,
        group: &str,
        consumer: &str,
        from: StreamId,
        count: Option<usize>,
        now_ms: u64,
    ) -> Option<Vec<StreamEntry>> {
        let ids: Vec<StreamId> = {
            let g = self.groups.get(group)?;
            g.pending
                .range(from..)
                .filter(|(_, p)| p.consumer == consumer)
                .map(|(id, _)| *id)
                .take(count.unwrap_or(usize::MAX))
                .collect()
        };
        let result: Vec<StreamEntry> = ids
            .iter()
            .map(|id| (*id, self.entries.get(id).cloned().unwrap_or_default()))
            .collect();
        if let Some(g) = self.groups.get_mut(group) {
            if let Some(c) = g.consumers.get_mut(consumer) {
                c.seen_time_ms = now_ms;
            }
        }
        Some(result)
    }

    /// Acknowledge (`XACK`) the given ids, removing them from the group PEL and
    /// their owning consumer. Returns `None` if no group, else the count removed.
    pub fn ack(&mut self, group: &str, ids: &[StreamId]) -> Option<usize> {
        let g = self.groups.get_mut(group)?;
        let mut n = 0;
        for id in ids {
            if let Some(p) = g.pending.remove(id) {
                n += 1;
                if let Some(c) = g.consumers.get_mut(&p.consumer) {
                    c.pending.remove(id);
                }
            }
        }
        Some(n)
    }

    /// `XPENDING key group` summary form. `None` if no group.
    pub fn pending_summary(&self, group: &str) -> Option<PendingSummary> {
        let g = self.groups.get(group)?;
        let mut per: HashMap<String, usize> = HashMap::new();
        for p in g.pending.values() {
            *per.entry(p.consumer.clone()).or_insert(0) += 1;
        }
        let mut consumers: Vec<(String, usize)> = per.into_iter().collect();
        consumers.sort();
        Some(PendingSummary {
            count: g.pending.len(),
            min_id: g.pending.keys().next().copied(),
            max_id: g.pending.keys().next_back().copied(),
            consumers,
        })
    }

    /// `XPENDING key group [IDLE ms] start end count [consumer]` extended form.
    /// `None` if no group.
    #[allow(clippy::too_many_arguments)]
    pub fn pending_extended(
        &self,
        group: &str,
        start: StreamId,
        end: StreamId,
        count: usize,
        consumer: Option<&str>,
        min_idle_ms: Option<u64>,
        now_ms: u64,
    ) -> Option<Vec<PendingDetail>> {
        let g = self.groups.get(group)?;
        let mut out = Vec::new();
        for (id, p) in g.pending.range(start..=end) {
            if let Some(cn) = consumer {
                if p.consumer != cn {
                    continue;
                }
            }
            let idle = now_ms.saturating_sub(p.delivery_time_ms);
            if let Some(mi) = min_idle_ms {
                if idle < mi {
                    continue;
                }
            }
            out.push(PendingDetail {
                id: *id,
                consumer: p.consumer.clone(),
                idle_ms: idle,
                delivery_count: p.delivery_count,
            });
            if out.len() >= count {
                break;
            }
        }
        Some(out)
    }

    /// `XCLAIM`: transfer ownership of the listed `ids` to `consumer` when their
    /// idle time is `>= opt.min_idle_ms`. Entries whose underlying stream entry
    /// was deleted are dropped from the PEL and not returned. Returns the claimed
    /// `(id, fields)` pairs, or `None` if no group.
    pub fn claim(
        &mut self,
        group: &str,
        consumer: &str,
        ids: &[StreamId],
        opt: &ClaimOptions,
        now_ms: u64,
    ) -> Option<Vec<StreamEntry>> {
        if !self.groups.contains_key(group) {
            return None;
        }
        self.groups
            .get_mut(group)
            .unwrap()
            .consumers
            .entry(consumer.to_string())
            .or_default();
        let mut claimed = Vec::new();
        for &id in ids {
            let exists = self.entries.contains_key(&id);
            let g = self.groups.get_mut(group).unwrap();
            let in_pel = g.pending.contains_key(&id);
            if !in_pel && !opt.force {
                continue;
            }
            if !exists {
                if let Some(p) = g.pending.remove(&id) {
                    if let Some(c) = g.consumers.get_mut(&p.consumer) {
                        c.pending.remove(&id);
                    }
                }
                continue;
            }
            if in_pel {
                let idle = now_ms.saturating_sub(g.pending.get(&id).unwrap().delivery_time_ms);
                if idle < opt.min_idle_ms {
                    continue;
                }
            }
            let prev_consumer = g.pending.get(&id).map(|p| p.consumer.clone());
            let prev_count = g.pending.get(&id).map(|p| p.delivery_count).unwrap_or(0);
            if let Some(pc) = &prev_consumer {
                if pc != consumer {
                    if let Some(c) = g.consumers.get_mut(pc) {
                        c.pending.remove(&id);
                    }
                }
            }
            let delivery_time = opt
                .set_time_ms
                .unwrap_or_else(|| now_ms.saturating_sub(opt.set_idle_ms.unwrap_or(0)));
            let delivery_count = if let Some(rc) = opt.set_retry {
                rc
            } else if opt.justid {
                prev_count
            } else {
                prev_count + 1
            };
            g.pending.insert(
                id,
                PendingEntry {
                    consumer: consumer.to_string(),
                    delivery_time_ms: delivery_time,
                    delivery_count,
                },
            );
            if let Some(c) = g.consumers.get_mut(consumer) {
                c.pending.insert(id);
                c.seen_time_ms = now_ms;
                c.active_time_ms = now_ms;
            }
            let fields = self.entries.get(&id).cloned().unwrap_or_default();
            claimed.push((id, fields));
        }
        Some(claimed)
    }

    /// `XAUTOCLAIM`: scan the group PEL from `start`, claiming up to `count`
    /// entries idle `>= min_idle_ms` for `consumer`. Returns the resume cursor
    /// (`0-0` when the scan is complete), the claimed `(id, fields)` pairs, and
    /// the ids whose underlying entries were deleted (and thus purged from the
    /// PEL). `None` if no group.
    #[allow(clippy::too_many_arguments)]
    pub fn autoclaim(
        &mut self,
        group: &str,
        consumer: &str,
        min_idle_ms: u64,
        start: StreamId,
        count: usize,
        now_ms: u64,
        justid: bool,
    ) -> Option<AutoclaimResult> {
        if !self.groups.contains_key(group) {
            return None;
        }
        self.groups
            .get_mut(group)
            .unwrap()
            .consumers
            .entry(consumer.to_string())
            .or_default();
        let candidates: Vec<StreamId> = self
            .groups
            .get(group)
            .unwrap()
            .pending
            .range(start..)
            .map(|(id, _)| *id)
            .collect();
        let attempts = count.max(1);
        let mut claimed = Vec::new();
        let mut deleted = Vec::new();
        let mut cursor = StreamId::MIN;
        for id in candidates {
            if claimed.len() >= attempts {
                cursor = id;
                break;
            }
            let exists = self.entries.contains_key(&id);
            let g = self.groups.get_mut(group).unwrap();
            let idle = now_ms.saturating_sub(g.pending.get(&id).unwrap().delivery_time_ms);
            if idle < min_idle_ms {
                continue;
            }
            if !exists {
                if let Some(p) = g.pending.remove(&id) {
                    if let Some(c) = g.consumers.get_mut(&p.consumer) {
                        c.pending.remove(&id);
                    }
                }
                deleted.push(id);
                continue;
            }
            let prev_consumer = g.pending.get(&id).unwrap().consumer.clone();
            let prev_count = g.pending.get(&id).unwrap().delivery_count;
            if prev_consumer != consumer {
                if let Some(c) = g.consumers.get_mut(&prev_consumer) {
                    c.pending.remove(&id);
                }
            }
            let delivery_count = if justid { prev_count } else { prev_count + 1 };
            g.pending.insert(
                id,
                PendingEntry {
                    consumer: consumer.to_string(),
                    delivery_time_ms: now_ms,
                    delivery_count,
                },
            );
            if let Some(c) = g.consumers.get_mut(consumer) {
                c.pending.insert(id);
                c.seen_time_ms = now_ms;
                c.active_time_ms = now_ms;
            }
            let fields = self.entries.get(&id).cloned().unwrap_or_default();
            claimed.push((id, fields));
        }
        Some(AutoclaimResult {
            cursor,
            claimed,
            deleted,
        })
    }

    /// `XINFO GROUPS`: one [`GroupInfo`] per group.
    pub fn group_infos(&self) -> Vec<GroupInfo> {
        self.groups
            .iter()
            .map(|(name, g)| {
                let lag = self
                    .entries
                    .range((Excluded(g.last_delivered_id), Unbounded))
                    .count() as u64;
                GroupInfo {
                    name: name.clone(),
                    consumers: g.consumers.len(),
                    pending: g.pending.len(),
                    last_delivered_id: g.last_delivered_id,
                    entries_read: g.entries_read,
                    lag,
                }
            })
            .collect()
    }

    /// `XINFO CONSUMERS key group`: one [`ConsumerInfo`] per consumer. `None` if
    /// the group is absent.
    pub fn consumer_infos(&self, group: &str, now_ms: u64) -> Option<Vec<ConsumerInfo>> {
        let g = self.groups.get(group)?;
        Some(
            g.consumers
                .iter()
                .map(|(name, c)| ConsumerInfo {
                    name: name.clone(),
                    pending: c.pending.len(),
                    idle_ms: now_ms.saturating_sub(c.seen_time_ms),
                    inactive_ms: now_ms.saturating_sub(c.active_time_ms),
                })
                .collect(),
        )
    }
}

/// A pending (delivered-but-unacknowledged) entry in a group's PEL.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingEntry {
    /// Name of the consumer that currently owns the entry.
    pub consumer: String,
    /// Wall-clock millisecond of the last delivery/claim.
    pub delivery_time_ms: u64,
    /// Number of times the entry has been delivered.
    pub delivery_count: u64,
}

/// A single consumer within a group.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Consumer {
    /// Ids this consumer currently owns in the group PEL.
    pending: HashSet<StreamId>,
    /// Last time the consumer interacted with the group in any way.
    seen_time_ms: u64,
    /// Last time the consumer successfully read or claimed an entry.
    active_time_ms: u64,
}

/// A consumer group over a single stream.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConsumerGroup {
    /// Highest id ever delivered to any consumer via `>` reads.
    last_delivered_id: StreamId,
    /// The group Pending Entries List (delivered-but-unacked), ordered by id.
    #[serde(with = "serde_pel")]
    pending: BTreeMap<StreamId, PendingEntry>,
    /// Consumers in the group, keyed by name.
    consumers: HashMap<String, Consumer>,
    /// Number of entries read by the group via `>` reads.
    entries_read: u64,
}

/// Options controlling [`StreamData::claim`] (`XCLAIM`).
#[derive(Clone, Debug, Default)]
pub struct ClaimOptions {
    /// Minimum idle time (ms) an entry must have to be claimable.
    pub min_idle_ms: u64,
    /// `IDLE ms`: set the claimed entry's idle time (new delivery_time = now-idle).
    pub set_idle_ms: Option<u64>,
    /// `TIME ms`: set the claimed entry's delivery_time directly (unix ms).
    pub set_time_ms: Option<u64>,
    /// `RETRYCOUNT n`: set delivery_count to exactly `n`.
    pub set_retry: Option<u64>,
    /// `FORCE`: create a PEL entry even when the id is not already pending.
    pub force: bool,
    /// `JUSTID`: do not bump delivery_count, and the caller returns only ids.
    pub justid: bool,
}

/// Result of [`StreamData::autoclaim`].
#[derive(Clone, Debug)]
pub struct AutoclaimResult {
    /// The next id to resume from (`0-0` when the scan is exhausted).
    pub cursor: StreamId,
    /// The successfully claimed `(id, fields)` pairs.
    pub claimed: Vec<StreamEntry>,
    /// Ids whose underlying entries were deleted (and purged from the PEL).
    pub deleted: Vec<StreamId>,
}

/// Summary form of `XPENDING`.
#[derive(Clone, Debug)]
pub struct PendingSummary {
    /// Total pending entries in the group.
    pub count: usize,
    /// Smallest pending id, if any.
    pub min_id: Option<StreamId>,
    /// Largest pending id, if any.
    pub max_id: Option<StreamId>,
    /// `(consumer, pending_count)` pairs, sorted by consumer name.
    pub consumers: Vec<(String, usize)>,
}

/// One row of the extended `XPENDING` form.
#[derive(Clone, Debug)]
pub struct PendingDetail {
    /// The pending entry id.
    pub id: StreamId,
    /// The owning consumer.
    pub consumer: String,
    /// Milliseconds since last delivery.
    pub idle_ms: u64,
    /// Times delivered.
    pub delivery_count: u64,
}

/// One row of `XINFO GROUPS`.
#[derive(Clone, Debug)]
pub struct GroupInfo {
    /// Group name.
    pub name: String,
    /// Number of consumers.
    pub consumers: usize,
    /// Number of pending entries.
    pub pending: usize,
    /// Highest delivered id.
    pub last_delivered_id: StreamId,
    /// Entries read by the group.
    pub entries_read: u64,
    /// Entries added to the stream after `last_delivered_id` (group lag).
    pub lag: u64,
}

/// One row of `XINFO CONSUMERS`.
#[derive(Clone, Debug)]
pub struct ConsumerInfo {
    /// Consumer name.
    pub name: String,
    /// Pending entries owned by the consumer.
    pub pending: usize,
    /// Milliseconds since the consumer last interacted with the group.
    pub idle_ms: u64,
    /// Milliseconds since the consumer last read/claimed.
    pub inactive_ms: u64,
}

/// Helper: collect an iterator of `(id, fields)` honouring an optional `count`
/// limit. Centralises the `take`/no-`take` branch so the call sites stay tidy.
fn collect_limited<I>(iter: I, count: Option<usize>) -> Vec<StreamEntry>
where
    I: Iterator<Item = StreamEntry>,
{
    match count {
        Some(n) => iter.take(n).collect(),
        None => iter.collect(),
    }
}

/// Serialize the `entries` map as a sequence of `(id, fields)` pairs so the
/// [`StreamId`] struct keys survive serializers that only accept string map keys
/// (e.g. JSON), then rebuild the [`BTreeMap`] on load.
mod serde_entries {
    use super::{StreamField, StreamId};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S>(
        map: &BTreeMap<StreamId, Vec<StreamField>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let pairs: Vec<(StreamId, Vec<StreamField>)> = map
            .iter()
            .map(|(id, fields)| (*id, fields.clone()))
            .collect();
        pairs.serialize(serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<BTreeMap<StreamId, Vec<StreamField>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let pairs = Vec::<(StreamId, Vec<StreamField>)>::deserialize(deserializer)?;
        Ok(pairs.into_iter().collect())
    }
}

/// Serialize a group PEL (`BTreeMap<StreamId, PendingEntry>`) as a sequence of
/// `(id, entry)` pairs, for the same JSON-map-key reason as [`serde_entries`].
mod serde_pel {
    use super::{PendingEntry, StreamId};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S>(
        map: &BTreeMap<StreamId, PendingEntry>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let pairs: Vec<(StreamId, PendingEntry)> =
            map.iter().map(|(id, p)| (*id, p.clone())).collect();
        pairs.serialize(serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<BTreeMap<StreamId, PendingEntry>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let pairs = Vec::<(StreamId, PendingEntry)>::deserialize(deserializer)?;
        Ok(pairs.into_iter().collect())
    }
}

/// Errors returned by [`StreamData::add`].
#[derive(Debug)]
pub enum StreamError {
    /// The supplied id is `<=` the current `last_id` (ids must strictly
    /// increase).
    IdTooSmall,
    /// The supplied id is the reserved zero id `0-0`.
    ZeroId,
}

/// Parse an explicit entry id token.
///
/// Accepts `"ms-seq"` (both parts decimal `u64`) or `"ms"` alone (the sequence
/// then defaults to `default_seq`). Returns `None` on malformed input.
///
/// The special command tokens `"*"`, `"$"`, `"-"` and `"+"` are *not* handled
/// here — they depend on stream/command state and are resolved by the caller.
pub fn parse_id(s: &str, default_seq: u64) -> Option<StreamId> {
    match s.split_once('-') {
        Some((ms, seq)) => {
            let ms = ms.parse::<u64>().ok()?;
            let seq = seq.parse::<u64>().ok()?;
            Some(StreamId { ms, seq })
        }
        None => {
            let ms = s.parse::<u64>().ok()?;
            Some(StreamId {
                ms,
                seq: default_seq,
            })
        }
    }
}

/// Parse a range **START** token.
///
/// * `"-"` => [`StreamId::MIN`] (the smallest possible id),
/// * `"ms"` => `(ms, 0)` (the start of that millisecond),
/// * `"ms-seq"` => that exact id.
///
/// Returns `None` on malformed input.
pub fn parse_range_start(s: &str) -> Option<StreamId> {
    if s == "-" {
        return Some(StreamId::MIN);
    }
    parse_id(s, 0)
}

/// Parse a range **END** token.
///
/// * `"+"` => [`StreamId::MAX`] (the largest possible id),
/// * `"ms"` => `(ms, u64::MAX)` (the end of that millisecond),
/// * `"ms-seq"` => that exact id.
///
/// Returns `None` on malformed input.
pub fn parse_range_end(s: &str) -> Option<StreamId> {
    if s == "+" {
        return Some(StreamId::MAX);
    }
    parse_id(s, u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fv(pairs: &[(&str, &str)]) -> Vec<(Vec<u8>, Vec<u8>)> {
        pairs
            .iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect()
    }

    fn id(ms: u64, seq: u64) -> StreamId {
        StreamId { ms, seq }
    }

    #[test]
    fn add_explicit_ids_ascending() {
        let mut s = StreamData::new();
        assert!(s.is_empty());
        assert_eq!(s.last_id(), StreamId::MIN);

        assert_eq!(s.add(id(1, 0), fv(&[("a", "1")])).unwrap(), id(1, 0));
        assert_eq!(s.add(id(1, 1), fv(&[("b", "2")])).unwrap(), id(1, 1));
        assert_eq!(s.add(id(2, 0), fv(&[("c", "3")])).unwrap(), id(2, 0));

        assert_eq!(s.len(), 3);
        assert!(!s.is_empty());
        assert_eq!(s.last_id(), id(2, 0));
    }

    #[test]
    fn add_rejects_out_of_order_and_equal() {
        let mut s = StreamData::new();
        s.add(id(5, 5), fv(&[("a", "1")])).unwrap();

        // strictly smaller ms
        assert!(matches!(
            s.add(id(4, 0), fv(&[])),
            Err(StreamError::IdTooSmall)
        ));
        // same ms, smaller seq
        assert!(matches!(
            s.add(id(5, 4), fv(&[])),
            Err(StreamError::IdTooSmall)
        ));
        // exactly equal to last_id
        assert!(matches!(
            s.add(id(5, 5), fv(&[])),
            Err(StreamError::IdTooSmall)
        ));
        // last_id unchanged after failures
        assert_eq!(s.last_id(), id(5, 5));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn add_rejects_zero_id() {
        let mut s = StreamData::new();
        assert!(matches!(s.add(id(0, 0), fv(&[])), Err(StreamError::ZeroId)));
        assert_eq!(s.len(), 0);
        assert_eq!(s.last_id(), StreamId::MIN);
    }

    #[test]
    fn next_auto_id_same_ms_bumps_seq() {
        let mut s = StreamData::new();
        s.add(id(100, 0), fv(&[])).unwrap();
        // now_ms == last_id.ms -> bump seq
        assert_eq!(s.next_auto_id(100), id(100, 1));
        // now_ms < last_id.ms (clock went backwards) -> still bump seq
        assert_eq!(s.next_auto_id(50), id(100, 1));

        s.add(id(100, 7), fv(&[])).unwrap();
        assert_eq!(s.next_auto_id(100), id(100, 8));
    }

    #[test]
    fn next_auto_id_advancing_ms_resets_seq() {
        let mut s = StreamData::new();
        s.add(id(100, 5), fv(&[])).unwrap();
        // now_ms > last_id.ms -> new ms, seq resets to 0
        assert_eq!(s.next_auto_id(200), id(200, 0));
    }

    #[test]
    fn next_auto_id_on_empty_stream() {
        let s = StreamData::new();
        // last_id is 0-0, so any now_ms > 0 advances ms.
        assert_eq!(s.next_auto_id(123), id(123, 0));
        // now_ms == 0 == last_id.ms -> bump seq from 0 to 1.
        assert_eq!(s.next_auto_id(0), id(0, 1));
    }

    fn seeded() -> StreamData {
        let mut s = StreamData::new();
        for i in 1..=5u64 {
            s.add(id(i, 0), fv(&[("k", "v")])).unwrap();
        }
        s
    }

    #[test]
    fn range_inclusive() {
        let s = seeded();
        let out = s.range(id(2, 0), id(4, 0), false, None);
        let ids: Vec<StreamId> = out.iter().map(|(i, _)| *i).collect();
        assert_eq!(ids, vec![id(2, 0), id(3, 0), id(4, 0)]);
    }

    #[test]
    fn range_full_with_min_max() {
        let s = seeded();
        let out = s.range(StreamId::MIN, StreamId::MAX, false, None);
        assert_eq!(out.len(), 5);
        assert_eq!(out[0].0, id(1, 0));
        assert_eq!(out[4].0, id(5, 0));
    }

    #[test]
    fn range_reversed() {
        let s = seeded();
        let out = s.range(id(2, 0), id(4, 0), true, None);
        let ids: Vec<StreamId> = out.iter().map(|(i, _)| *i).collect();
        assert_eq!(ids, vec![id(4, 0), id(3, 0), id(2, 0)]);
    }

    #[test]
    fn range_count_limits_after_ordering() {
        let s = seeded();
        // ascending, limited to 2 -> smallest two in range
        let asc = s.range(StreamId::MIN, StreamId::MAX, false, Some(2));
        let asc_ids: Vec<StreamId> = asc.iter().map(|(i, _)| *i).collect();
        assert_eq!(asc_ids, vec![id(1, 0), id(2, 0)]);

        // descending, limited to 2 -> largest two in range
        let desc = s.range(StreamId::MIN, StreamId::MAX, true, Some(2));
        let desc_ids: Vec<StreamId> = desc.iter().map(|(i, _)| *i).collect();
        assert_eq!(desc_ids, vec![id(5, 0), id(4, 0)]);
    }

    #[test]
    fn range_empty_when_start_after_end() {
        let s = seeded();
        assert!(s.range(id(4, 0), id(2, 0), false, None).is_empty());
    }

    #[test]
    fn read_after_strictly_greater() {
        let s = seeded();
        let out = s.read_after(id(3, 0), None);
        let ids: Vec<StreamId> = out.iter().map(|(i, _)| *i).collect();
        assert_eq!(ids, vec![id(4, 0), id(5, 0)]);

        // MIN reads everything
        assert_eq!(s.read_after(StreamId::MIN, None).len(), 5);
        // last id reads nothing
        assert!(s.read_after(id(5, 0), None).is_empty());
    }

    #[test]
    fn read_after_count_limited() {
        let s = seeded();
        let out = s.read_after(StreamId::MIN, Some(3));
        let ids: Vec<StreamId> = out.iter().map(|(i, _)| *i).collect();
        assert_eq!(ids, vec![id(1, 0), id(2, 0), id(3, 0)]);
    }

    #[test]
    fn delete_counts_existing_and_hides_ids() {
        let mut s = seeded();
        // one existing, one missing
        let removed = s.delete(&[id(2, 0), id(99, 0)]);
        assert_eq!(removed, 1);
        assert_eq!(s.len(), 4);

        // deleted id no longer appears in a range/read
        let all = s.range(StreamId::MIN, StreamId::MAX, false, None);
        assert!(!all.iter().any(|(i, _)| *i == id(2, 0)));
        assert!(s
            .read_after(id(1, 0), None)
            .iter()
            .all(|(i, _)| *i != id(2, 0)));

        // last_id unaffected by deletion
        assert_eq!(s.last_id(), id(5, 0));

        // deleting again returns 0
        assert_eq!(s.delete(&[id(2, 0)]), 0);
    }

    #[test]
    fn trim_maxlen_removes_oldest() {
        let mut s = seeded(); // ids 1..=5
        let removed = s.trim_maxlen(2);
        assert_eq!(removed, 3);
        assert_eq!(s.len(), 2);

        let ids: Vec<StreamId> = s
            .range(StreamId::MIN, StreamId::MAX, false, None)
            .iter()
            .map(|(i, _)| *i)
            .collect();
        // newest two survive
        assert_eq!(ids, vec![id(4, 0), id(5, 0)]);

        // last_id unaffected by trimming
        assert_eq!(s.last_id(), id(5, 0));
    }

    #[test]
    fn trim_maxlen_noop_when_short_enough() {
        let mut s = seeded();
        assert_eq!(s.trim_maxlen(5), 0);
        assert_eq!(s.trim_maxlen(10), 0);
        assert_eq!(s.len(), 5);
    }

    #[test]
    fn trim_maxlen_zero_clears() {
        let mut s = seeded();
        assert_eq!(s.trim_maxlen(0), 5);
        assert!(s.is_empty());
        // last_id still records the highest id ever added
        assert_eq!(s.last_id(), id(5, 0));
    }

    #[test]
    fn parse_id_variants() {
        assert_eq!(parse_id("5", 0), Some(id(5, 0)));
        assert_eq!(parse_id("5", 9), Some(id(5, 9)));
        assert_eq!(parse_id("5-3", 0), Some(id(5, 3)));
        assert_eq!(parse_id("0-0", 0), Some(StreamId::MIN));

        // malformed
        assert_eq!(parse_id("", 0), None);
        assert_eq!(parse_id("abc", 0), None);
        assert_eq!(parse_id("5-x", 0), None);
        assert_eq!(parse_id("5-", 0), None);
        assert_eq!(parse_id("-3", 0), None);
        assert_eq!(parse_id("5-3-1", 0), None);
    }

    #[test]
    fn parse_range_start_tokens() {
        assert_eq!(parse_range_start("-"), Some(StreamId::MIN));
        assert_eq!(parse_range_start("5"), Some(id(5, 0)));
        assert_eq!(parse_range_start("5-2"), Some(id(5, 2)));
        assert_eq!(parse_range_start("bad"), None);
    }

    #[test]
    fn parse_range_end_tokens() {
        assert_eq!(parse_range_end("+"), Some(StreamId::MAX));
        assert_eq!(parse_range_end("5"), Some(id(5, u64::MAX)));
        assert_eq!(parse_range_end("5-2"), Some(id(5, 2)));
        assert_eq!(parse_range_end("bad"), None);
    }

    #[test]
    fn to_redis_string_format() {
        assert_eq!(id(5, 3).to_redis_string(), "5-3");
        assert_eq!(StreamId::MIN.to_redis_string(), "0-0");
        assert_eq!(
            StreamId::MAX.to_redis_string(),
            format!("{}-{}", u64::MAX, u64::MAX)
        );
    }

    #[test]
    fn stream_id_ordering() {
        assert!(id(1, 0) < id(1, 1));
        assert!(id(1, 999) < id(2, 0));
        assert!(StreamId::MIN < StreamId::MAX);
    }

    #[test]
    fn serde_round_trip() {
        let mut s = StreamData::new();
        s.add(id(1, 0), fv(&[("field", "value"), ("bin", "\u{0}\u{1}")]))
            .unwrap();
        s.add(id(2, 5), fv(&[("x", "y")])).unwrap();
        s.delete(&[id(1, 0)]);

        let json = serde_json::to_string(&s).expect("serialize");
        let back: StreamData = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.len(), s.len());
        assert_eq!(back.last_id(), s.last_id());
        let orig = s.range(StreamId::MIN, StreamId::MAX, false, None);
        let round = back.range(StreamId::MIN, StreamId::MAX, false, None);
        assert_eq!(orig, round);
    }

    #[test]
    fn stream_id_serde_round_trip() {
        let original = id(42, 7);
        let json = serde_json::to_string(&original).expect("serialize");
        let back: StreamId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, back);
    }

    // ---- Consumer groups (Phase 8b) ----

    fn seeded_stream() -> StreamData {
        let mut s = StreamData::new();
        s.add(id(1, 0), fv(&[("v", "1")])).unwrap();
        s.add(id(2, 0), fv(&[("v", "2")])).unwrap();
        s.add(id(3, 0), fv(&[("v", "3")])).unwrap();
        s
    }

    #[test]
    fn create_group_and_duplicate() {
        let mut s = seeded_stream();
        assert!(s.create_group("g1", StreamId::MIN));
        assert!(!s.create_group("g1", StreamId::MIN), "duplicate -> false");
        assert!(s.group_exists("g1"));
        assert_eq!(s.groups_count(), 1);
        assert!(s.destroy_group("g1"));
        assert!(!s.destroy_group("g1"));
    }

    #[test]
    fn group_read_new_delivers_and_pels() {
        let mut s = seeded_stream();
        s.create_group("g", StreamId::MIN);
        let got = s.group_read_new("g", "c1", Some(2), 1000, false).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, id(1, 0));
        assert_eq!(got[1].0, id(2, 0));

        // Both are now pending under c1.
        let summary = s.pending_summary("g").unwrap();
        assert_eq!(summary.count, 2);
        assert_eq!(summary.min_id, Some(id(1, 0)));
        assert_eq!(summary.max_id, Some(id(2, 0)));
        assert_eq!(summary.consumers, vec![("c1".to_string(), 2)]);

        // A second `>` read picks up only what's after last_delivered.
        let more = s.group_read_new("g", "c1", None, 1001, false).unwrap();
        assert_eq!(more.len(), 1);
        assert_eq!(more[0].0, id(3, 0));
    }

    #[test]
    fn ack_removes_from_pel() {
        let mut s = seeded_stream();
        s.create_group("g", StreamId::MIN);
        s.group_read_new("g", "c1", None, 1000, false).unwrap();
        assert_eq!(s.pending_summary("g").unwrap().count, 3);

        let acked = s.ack("g", &[id(1, 0), id(2, 0)]).unwrap();
        assert_eq!(acked, 2);
        assert_eq!(s.pending_summary("g").unwrap().count, 1);
        // Re-acking an already-acked id is a no-op.
        assert_eq!(s.ack("g", &[id(1, 0)]).unwrap(), 0);
    }

    #[test]
    fn noack_does_not_pel() {
        let mut s = seeded_stream();
        s.create_group("g", StreamId::MIN);
        let got = s.group_read_new("g", "c1", None, 1000, true).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(s.pending_summary("g").unwrap().count, 0);
    }

    #[test]
    fn claim_transfers_ownership_after_idle() {
        let mut s = seeded_stream();
        s.create_group("g", StreamId::MIN);
        // c1 reads everything at t=1000.
        s.group_read_new("g", "c1", None, 1000, false).unwrap();

        // At t=1500, c2 claims entries idle >= 400ms (idle is 500ms) -> success.
        let opt = ClaimOptions {
            min_idle_ms: 400,
            ..ClaimOptions::default()
        };
        let claimed = s
            .claim("g", "c2", &[id(1, 0), id(2, 0)], &opt, 1500)
            .unwrap();
        assert_eq!(claimed.len(), 2);

        // Now those entries belong to c2; delivery_count bumped to 2.
        let det = s
            .pending_extended("g", StreamId::MIN, StreamId::MAX, 10, None, None, 1500)
            .unwrap();
        let e1 = det.iter().find(|d| d.id == id(1, 0)).unwrap();
        assert_eq!(e1.consumer, "c2");
        assert_eq!(e1.delivery_count, 2);

        // A claim that doesn't meet min-idle is skipped.
        let strict = ClaimOptions {
            min_idle_ms: 10_000,
            ..ClaimOptions::default()
        };
        let none = s.claim("g", "c3", &[id(1, 0)], &strict, 1600).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn autoclaim_scans_and_purges_deleted() {
        let mut s = seeded_stream();
        s.create_group("g", StreamId::MIN);
        s.group_read_new("g", "c1", None, 1000, false).unwrap();
        // Delete the underlying entry for id 2-0; its PEL entry is now stale.
        s.delete(&[id(2, 0)]);

        let res = s
            .autoclaim("g", "c2", 0, StreamId::MIN, 10, 2000, false)
            .unwrap();
        // 1-0 and 3-0 claimed; 2-0 reported deleted and purged.
        assert_eq!(res.claimed.len(), 2);
        assert_eq!(res.deleted, vec![id(2, 0)]);
        assert_eq!(res.cursor, StreamId::MIN, "scan exhausted -> 0-0 cursor");
        // PEL no longer contains the deleted id.
        let summary = s.pending_summary("g").unwrap();
        assert_eq!(summary.count, 2);
    }

    #[test]
    fn del_consumer_drops_its_pending() {
        let mut s = seeded_stream();
        s.create_group("g", StreamId::MIN);
        s.group_read_new("g", "c1", None, 1000, false).unwrap();
        let removed = s.del_consumer("g", "c1").unwrap();
        assert_eq!(removed, 3);
        assert_eq!(s.pending_summary("g").unwrap().count, 0);
    }

    #[test]
    fn group_and_consumer_info() {
        let mut s = seeded_stream();
        s.create_group("g", StreamId::MIN);
        s.group_read_new("g", "c1", Some(2), 1000, false).unwrap();

        let gi = s.group_infos();
        assert_eq!(gi.len(), 1);
        assert_eq!(gi[0].name, "g");
        assert_eq!(gi[0].consumers, 1);
        assert_eq!(gi[0].pending, 2);
        assert_eq!(gi[0].last_delivered_id, id(2, 0));
        assert_eq!(gi[0].lag, 1, "one entry (3-0) after last_delivered");

        let ci = s.consumer_infos("g", 1500).unwrap();
        assert_eq!(ci.len(), 1);
        assert_eq!(ci[0].name, "c1");
        assert_eq!(ci[0].pending, 2);
        assert_eq!(ci[0].idle_ms, 500);
    }

    #[test]
    fn group_state_serde_round_trip() {
        let mut s = seeded_stream();
        s.create_group("g", StreamId::MIN);
        s.group_read_new("g", "c1", None, 1000, false).unwrap();

        let json = serde_json::to_string(&s).expect("serialize");
        let back: StreamData = serde_json::from_str(&json).expect("deserialize");
        let summary = back.pending_summary("g").unwrap();
        assert_eq!(summary.count, 3);
        assert_eq!(back.group_infos()[0].consumers, 1);
    }
}
