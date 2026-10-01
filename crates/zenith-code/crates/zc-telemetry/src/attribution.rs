//! `resourceTelemetry/ResourceAttribution.ts`: logical I/O the server attributes to its own
//! components (the trace writer records `server-trace`/`append` on every flush), summed per
//! `(component, operation)`.

use std::sync::{Arc, Mutex};

use zc_contracts::{DateTimeUtc, ResourceAttributionEntry, ResourceAttributionSnapshot};

/// One record; absent numbers count as 0 (`count` as 1).
#[derive(Debug, Clone, Default)]
pub struct AttributionRecord {
    pub component: String,
    pub operation: String,
    pub logical_read_bytes: Option<f64>,
    pub logical_write_bytes: Option<f64>,
    pub count: Option<f64>,
    pub duration_ms: Option<f64>,
}

/// `ResourceAttribution`, cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct ResourceAttribution {
    entries: Arc<Mutex<Vec<ResourceAttributionEntry>>>,
}

fn non_negative_integer(value: Option<f64>, fallback: i64) -> i64 {
    match value {
        None => fallback,
        Some(v) if !v.is_finite() => 0,
        Some(v) => (v + 0.5).floor().max(0.0) as i64,
    }
}

impl ResourceAttribution {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, input: AttributionRecord) {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let index = entries
            .iter()
            .position(|entry| entry.component == input.component && entry.operation == input.operation);
        let read = non_negative_integer(input.logical_read_bytes, 0);
        let write = non_negative_integer(input.logical_write_bytes, 0);
        let count = non_negative_integer(input.count, 1);
        let duration = non_negative_integer(input.duration_ms, 0);
        match index {
            Some(index) => {
                let entry = &mut entries[index];
                entry.logical_read_bytes += read;
                entry.logical_write_bytes += write;
                entry.count += count;
                entry.duration_ms += duration;
            }
            None => entries.push(ResourceAttributionEntry {
                component: input.component,
                operation: input.operation,
                logical_read_bytes: read,
                logical_write_bytes: write,
                count,
                duration_ms: duration,
            }),
        }
    }

    /// Every entry, most bytes written first.
    pub fn snapshot(&self) -> ResourceAttributionSnapshot {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner()).clone();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.logical_write_bytes));
        ResourceAttributionSnapshot {
            read_at: DateTimeUtc::now(),
            entries,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_per_component_and_operation_and_sorts_by_bytes_written() {
        let attribution = ResourceAttribution::new();
        attribution.record(AttributionRecord {
            component: "server-trace".into(),
            operation: "append".into(),
            logical_write_bytes: Some(10.0),
            count: Some(2.0),
            duration_ms: Some(1.4),
            ..Default::default()
        });
        attribution.record(AttributionRecord {
            component: "provider-event-log".into(),
            operation: "append".into(),
            logical_write_bytes: Some(512.0),
            ..Default::default()
        });
        attribution.record(AttributionRecord {
            component: "server-trace".into(),
            operation: "append".into(),
            logical_write_bytes: Some(5.0),
            logical_read_bytes: Some(f64::NAN),
            ..Default::default()
        });
        let snapshot = attribution.snapshot();
        assert_eq!(snapshot.entries.len(), 2);
        assert_eq!(snapshot.entries[0].component, "provider-event-log");
        assert_eq!(snapshot.entries[0].count, 1);
        assert_eq!(snapshot.entries[1].logical_write_bytes, 15);
        assert_eq!(snapshot.entries[1].count, 3);
        assert_eq!(snapshot.entries[1].duration_ms, 1);
        assert_eq!(snapshot.entries[1].logical_read_bytes, 0);
    }
}
