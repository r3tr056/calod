//! In-process publish/subscribe registry.
//!
//! Tracks channel and pattern subscriptions per connection and fans published
//! messages out to subscribers' out-of-band push channels (see
//! [`crate::cld_srv::connection::Connection::push_sender`]). Message frames are
//! built once per subscriber in that subscriber's negotiated protocol: a RESP2
//! subscriber receives a `*`-array, a RESP3 subscriber receives a `>` push.

use std::collections::{HashMap, HashSet};

use dashmap::DashMap;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::protocol::ProtocolVersion;
use crate::parser::value::Value;

/// Case-sensitive Redis glob match (`*` and `?`), matching the semantics used by
/// `KEYS`. Pub/sub channel names are case-sensitive, so this must not fold case.
fn glob_matches(pattern: &str, text: &str) -> bool {
    fn rec(p: &[u8], t: &[u8]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            b'*' => rec(&p[1..], t) || (!t.is_empty() && rec(p, &t[1..])),
            b'?' => !t.is_empty() && rec(&p[1..], &t[1..]),
            ch => !t.is_empty() && ch == t[0] && rec(&p[1..], &t[1..]),
        }
    }
    rec(pattern.as_bytes(), text.as_bytes())
}

#[derive(Clone)]
struct Subscriber {
    push_tx: mpsc::Sender<Vec<u8>>,
    proto: ProtocolVersion,
}

#[derive(Default)]
struct ClientSubs {
    channels: HashSet<String>,
    patterns: HashSet<String>,
}

/// Concurrent registry of channel/pattern subscriptions.
#[derive(Default)]
pub struct PubSubRegistry {
    channels: DashMap<String, HashMap<Uuid, Subscriber>>,
    patterns: DashMap<String, HashMap<Uuid, Subscriber>>,
    client_index: DashMap<Uuid, ClientSubs>,
}

impl PubSubRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Total subscriptions (channels + patterns) currently held by a client.
    pub fn total_for_client(&self, client: Uuid) -> usize {
        self.client_index
            .get(&client)
            .map(|c| c.channels.len() + c.patterns.len())
            .unwrap_or(0)
    }

    /// Subscribe a client to an exact channel; returns the client's new total.
    pub fn subscribe_channel(
        &self,
        client: Uuid,
        push_tx: &mpsc::Sender<Vec<u8>>,
        proto: ProtocolVersion,
        channel: &str,
    ) -> usize {
        self.channels
            .entry(channel.to_string())
            .or_default()
            .insert(
                client,
                Subscriber {
                    push_tx: push_tx.clone(),
                    proto,
                },
            );
        self.client_index
            .entry(client)
            .or_default()
            .channels
            .insert(channel.to_string());
        self.total_for_client(client)
    }

    /// Unsubscribe a client from an exact channel; returns the new total.
    pub fn unsubscribe_channel(&self, client: Uuid, channel: &str) -> usize {
        if let Some(mut subs) = self.channels.get_mut(channel) {
            subs.remove(&client);
            let empty = subs.is_empty();
            drop(subs);
            if empty {
                self.channels.remove(channel);
            }
        }
        if let Some(mut ci) = self.client_index.get_mut(&client) {
            ci.channels.remove(channel);
        }
        self.total_for_client(client)
    }

    /// Subscribe a client to a glob pattern; returns the client's new total.
    pub fn subscribe_pattern(
        &self,
        client: Uuid,
        push_tx: &mpsc::Sender<Vec<u8>>,
        proto: ProtocolVersion,
        pattern: &str,
    ) -> usize {
        self.patterns
            .entry(pattern.to_string())
            .or_default()
            .insert(
                client,
                Subscriber {
                    push_tx: push_tx.clone(),
                    proto,
                },
            );
        self.client_index
            .entry(client)
            .or_default()
            .patterns
            .insert(pattern.to_string());
        self.total_for_client(client)
    }

    /// Unsubscribe a client from a glob pattern; returns the new total.
    pub fn unsubscribe_pattern(&self, client: Uuid, pattern: &str) -> usize {
        if let Some(mut subs) = self.patterns.get_mut(pattern) {
            subs.remove(&client);
            let empty = subs.is_empty();
            drop(subs);
            if empty {
                self.patterns.remove(pattern);
            }
        }
        if let Some(mut ci) = self.client_index.get_mut(&client) {
            ci.patterns.remove(pattern);
        }
        self.total_for_client(client)
    }

    /// Channels a client is currently subscribed to (for argument-less UNSUBSCRIBE).
    pub fn client_channels(&self, client: Uuid) -> Vec<String> {
        self.client_index
            .get(&client)
            .map(|c| c.channels.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Patterns a client is currently subscribed to (for argument-less PUNSUBSCRIBE).
    pub fn client_patterns(&self, client: Uuid) -> Vec<String> {
        self.client_index
            .get(&client)
            .map(|c| c.patterns.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Remove all of a client's subscriptions (call on disconnect).
    pub fn cleanup_client(&self, client: Uuid) {
        if let Some((_, subs)) = self.client_index.remove(&client) {
            for ch in subs.channels {
                if let Some(mut s) = self.channels.get_mut(&ch) {
                    s.remove(&client);
                    let empty = s.is_empty();
                    drop(s);
                    if empty {
                        self.channels.remove(&ch);
                    }
                }
            }
            for p in subs.patterns {
                if let Some(mut s) = self.patterns.get_mut(&p) {
                    s.remove(&client);
                    let empty = s.is_empty();
                    drop(s);
                    if empty {
                        self.patterns.remove(&p);
                    }
                }
            }
        }
    }

    /// Publish `payload` on `channel`; returns the number of clients reached
    /// (exact subscribers plus pattern subscribers). Delivery is non-blocking;
    /// a full subscriber channel drops the message (the slow subscriber is not
    /// allowed to stall the publisher).
    pub fn publish(&self, channel: &str, payload: &[u8]) -> usize {
        let mut count = 0usize;

        if let Some(subs) = self.channels.get(channel) {
            for sub in subs.values() {
                let frame = message_frame(channel, payload, sub.proto);
                if sub.push_tx.try_send(frame).is_ok() {
                    count += 1;
                }
            }
        }

        for entry in self.patterns.iter() {
            let pattern = entry.key();
            if glob_matches(pattern, channel) {
                for sub in entry.value().values() {
                    let frame = pmessage_frame(pattern, channel, payload, sub.proto);
                    if sub.push_tx.try_send(frame).is_ok() {
                        count += 1;
                    }
                }
            }
        }

        count
    }

    /// PUBSUB CHANNELS [pattern]: active channels with ≥1 subscriber.
    pub fn channels_list(&self, pattern: Option<&str>) -> Vec<String> {
        self.channels
            .iter()
            .map(|e| e.key().clone())
            .filter(|c| pattern.is_none_or(|p| glob_matches(p, c)))
            .collect()
    }

    /// PUBSUB NUMSUB: subscriber counts for the given channels.
    pub fn numsub(&self, names: &[String]) -> Vec<(String, usize)> {
        names
            .iter()
            .map(|n| {
                (
                    n.clone(),
                    self.channels.get(n).map(|s| s.len()).unwrap_or(0),
                )
            })
            .collect()
    }

    /// PUBSUB NUMPAT: number of distinct patterns with ≥1 subscriber.
    pub fn numpat(&self) -> usize {
        self.patterns.len()
    }
}

fn message_frame(channel: &str, payload: &[u8], proto: ProtocolVersion) -> Vec<u8> {
    Value::Push(vec![
        Value::BulkString(b"message".to_vec()),
        Value::BulkString(channel.as_bytes().to_vec()),
        Value::BulkString(payload.to_vec()),
    ])
    .encode(proto)
}

fn pmessage_frame(pattern: &str, channel: &str, payload: &[u8], proto: ProtocolVersion) -> Vec<u8> {
    Value::Push(vec![
        Value::BulkString(b"pmessage".to_vec()),
        Value::BulkString(pattern.as_bytes().to_vec()),
        Value::BulkString(channel.as_bytes().to_vec()),
        Value::BulkString(payload.to_vec()),
    ])
    .encode(proto)
}
