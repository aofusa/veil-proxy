//! Queue Notification System for WASM Module Callbacks
//!
//! Manages notifications for shared queue subscribers when messages are enqueued.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use once_cell::sync::Lazy;

use super::engine::FilterEngine;
use std::sync::Arc;

/// Global queue subscription registry
static QUEUE_SUBSCRIPTIONS: Lazy<RwLock<QueueSubscriptionRegistry>> =
    Lazy::new(|| RwLock::new(QueueSubscriptionRegistry::new()));

/// Registry of queue subscriptions
#[derive(Debug)]
struct QueueSubscriptionRegistry {
    /// Map from queue_id to set of subscribed module names
    subscriptions: HashMap<u32, HashSet<String>>,
    /// Map from queue_name to queue_id for resolution
    name_to_id: HashMap<String, u32>,
}

impl QueueSubscriptionRegistry {
    fn new() -> Self {
        Self {
            subscriptions: HashMap::new(),
            name_to_id: HashMap::new(),
        }
    }
}

/// Subscribe a module to a queue
/// 
/// Called when a module resolves a queue via proxy_resolve_shared_queue.
pub fn subscribe_to_queue(module_name: &str, queue_id: u32) {
    let mut registry = match QUEUE_SUBSCRIPTIONS.write() {
        Ok(r) => r,
        Err(_) => return,
    };
    
    registry
        .subscriptions
        .entry(queue_id)
        .or_default()
        .insert(module_name.to_string());
    
    ftlog::debug!(
        "[wasm:queue] Module '{}' subscribed to queue {}",
        module_name,
        queue_id
    );
}

/// Unsubscribe a module from a queue
pub fn unsubscribe_from_queue(module_name: &str, queue_id: u32) {
    let mut registry = match QUEUE_SUBSCRIPTIONS.write() {
        Ok(r) => r,
        Err(_) => return,
    };
    
    if let Some(subscribers) = registry.subscriptions.get_mut(&queue_id) {
        subscribers.remove(module_name);
    }
}

/// Unsubscribe a module from all queues
/// 
/// Called when a module context is destroyed.
pub fn unsubscribe_from_all_queues(module_name: &str) {
    let mut registry = match QUEUE_SUBSCRIPTIONS.write() {
        Ok(r) => r,
        Err(_) => return,
    };
    
    for subscribers in registry.subscriptions.values_mut() {
        subscribers.remove(module_name);
    }
}

/// Get all subscribers for a queue
pub fn get_queue_subscribers(queue_id: u32) -> Vec<String> {
    let registry = match QUEUE_SUBSCRIPTIONS.read() {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    
    registry
        .subscriptions
        .get(&queue_id)
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default()
}

/// Notify all subscribers of a queue that data is available
/// 
/// This should be called after proxy_enqueue_shared_queue adds data to a queue.
pub fn notify_queue_subscribers(engine: &Arc<FilterEngine>, queue_id: u32) {
    let subscribers = get_queue_subscribers(queue_id);
    
    for module_name in subscribers {
        engine.on_queue_ready(&module_name, queue_id);
    }
}

/// Register a queue name to ID mapping
pub fn register_queue_name(queue_name: &str, queue_id: u32) {
    let mut registry = match QUEUE_SUBSCRIPTIONS.write() {
        Ok(r) => r,
        Err(_) => return,
    };
    
    registry.name_to_id.insert(queue_name.to_string(), queue_id);
}

/// Resolve a queue name to its ID
pub fn resolve_queue_name(queue_name: &str) -> Option<u32> {
    let registry = match QUEUE_SUBSCRIPTIONS.read() {
        Ok(r) => r,
        Err(_) => return None,
    };
    
    registry.name_to_id.get(queue_name).copied()
}

/// Get statistics about queue subscriptions
pub fn get_queue_stats() -> QueueStats {
    let registry = match QUEUE_SUBSCRIPTIONS.read() {
        Ok(r) => r,
        Err(_) => return QueueStats::default(),
    };
    
    let total_subscriptions: usize = registry.subscriptions.values().map(|s| s.len()).sum();
    
    QueueStats {
        registered_queues: registry.name_to_id.len(),
        queues_with_subscribers: registry.subscriptions.len(),
        total_subscriptions,
    }
}

/// Statistics about queue subscriptions
#[derive(Debug, Default)]
pub struct QueueStats {
    /// Number of registered queues
    pub registered_queues: usize,
    /// Number of queues with at least one subscriber
    pub queues_with_subscribers: usize,
    /// Total number of subscriptions across all queues
    pub total_subscriptions: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_subscribe_to_queue() {
        subscribe_to_queue("test_module_sub", 100);
        
        let subscribers = get_queue_subscribers(100);
        assert!(subscribers.contains(&"test_module_sub".to_string()));
        
        // Cleanup
        unsubscribe_from_queue("test_module_sub", 100);
    }
    
    #[test]
    fn test_unsubscribe_from_all() {
        subscribe_to_queue("test_module_unsub", 200);
        subscribe_to_queue("test_module_unsub", 201);
        
        unsubscribe_from_all_queues("test_module_unsub");
        
        assert!(!get_queue_subscribers(200).contains(&"test_module_unsub".to_string()));
        assert!(!get_queue_subscribers(201).contains(&"test_module_unsub".to_string()));
    }
    
    #[test]
    fn test_queue_name_resolution() {
        register_queue_name("my.queue.name", 300);
        
        let id = resolve_queue_name("my.queue.name");
        assert_eq!(id, Some(300));
        
        let unknown = resolve_queue_name("unknown.queue");
        assert_eq!(unknown, None);
    }
    
    #[test]
    fn test_queue_stats() {
        subscribe_to_queue("stats_module_1", 400);
        subscribe_to_queue("stats_module_2", 400);
        register_queue_name("stats.queue", 400);
        
        let stats = get_queue_stats();
        assert!(stats.registered_queues >= 1);
        assert!(stats.queues_with_subscribers >= 1);
        
        // Cleanup
        unsubscribe_from_queue("stats_module_1", 400);
        unsubscribe_from_queue("stats_module_2", 400);
    }
}
