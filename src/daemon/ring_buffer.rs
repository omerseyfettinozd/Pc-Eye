use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

/// Monotonik sıra numaralı (seq) ve milisaniye zaman damgalı halka bellek öğesi.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeqItem<T> {
    pub seq: u64,
    pub timestamp_ms: u64,
    pub data: T,
}

/// Persentil istatistikleri (Rowl Eye FrameRingBuffer OS karşılığı).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PercentileStats {
    pub samples: usize,
    pub latest: f64,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub spike_detected: bool,
}

impl PercentileStats {
    pub fn from_slice(values: &[f64], spike_threshold_multiplier: f64) -> Self {
        if values.is_empty() {
            return Self::default();
        }
        let samples = values.len();
        let latest = *values.last().unwrap_or(&0.0);
        let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
        if sorted.is_empty() {
            return Self::default();
        }
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let min = sorted[0];
        let max = *sorted.last().unwrap();
        let sum: f64 = sorted.iter().sum();
        let mean = sum / (sorted.len() as f64);

        let percentile = |p: f64| -> f64 {
            let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
            sorted[idx.min(sorted.len() - 1)]
        };

        let p50 = percentile(0.50);
        let p95 = percentile(0.95);
        let p99 = percentile(0.99);

        // Eğer p99 veya son değer medyanın (p50) belirlenen katından yüksekse ve anlamlı bir taban varsa spike işaretle
        let baseline = p50.max(1.0);
        let spike_detected = max > baseline * spike_threshold_multiplier && (max - p50) > 5.0;

        Self {
            samples,
            latest: round2(latest),
            min: round2(min),
            max: round2(max),
            mean: round2(mean),
            p50: round2(p50),
            p95: round2(p95),
            p99: round2(p99),
            spike_detected,
        }
    }
}

fn round2(val: f64) -> f64 {
    (val * 100.0).round() / 100.0
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Sabit kapasiteli, monotonik `seq` üreten dairesel tampon (Ring Buffer).
#[derive(Debug, Clone)]
pub struct SeqRingBuffer<T> {
    capacity: usize,
    next_seq: u64,
    buf: VecDeque<SeqItem<T>>,
}

impl<T: Clone> SeqRingBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            capacity: cap,
            next_seq: 1,
            buf: VecDeque::with_capacity(cap),
        }
    }

    pub fn push(&mut self, data: T) -> u64 {
        self.push_at(data, now_ms())
    }

    pub fn push_at(&mut self, data: T, timestamp_ms: u64) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        if self.buf.len() == self.capacity {
            self.buf.pop_front();
        }
        self.buf.push_back(SeqItem {
            seq,
            timestamp_ms,
            data,
        });
        seq
    }

    pub fn latest_seq(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn latest(&self) -> Option<&SeqItem<T>> {
        self.buf.back()
    }

    /// `seq_since` değerinden büyük olan tüm öğeleri (Delta/Diff) döndürür.
    /// Ayrıca istemcinin çok geride kalıp kalmadığını (`dropped_count`) bildirir.
    pub fn since_seq(&self, seq_since: u64, limit: usize) -> (Vec<SeqItem<T>>, u64) {
        let first_available = self.buf.front().map(|i| i.seq).unwrap_or(self.next_seq);
        let dropped = if seq_since > 0 && first_available > seq_since + 1 {
            first_available - (seq_since + 1)
        } else {
            0
        };

        let items: Vec<SeqItem<T>> = self
            .buf
            .iter()
            .filter(|item| item.seq > seq_since)
            .rev()
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        (items, dropped)
    }

    pub fn iter(&self) -> impl Iterator<Item = &SeqItem<T>> {
        self.buf.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_buffer_wrap_and_delta() {
        let mut rb = SeqRingBuffer::new(3);
        assert_eq!(rb.push_at("a", 100), 1);
        assert_eq!(rb.push_at("b", 200), 2);
        assert_eq!(rb.push_at("c", 300), 3);
        assert_eq!(rb.push_at("d", 400), 4); // "a" (seq 1) düştü

        assert_eq!(rb.len(), 3);
        assert_eq!(rb.latest_seq(), 4);

        // seq_since = 2 -> [3 ("c"), 4 ("d")], dropped = 0
        let (delta, dropped) = rb.since_seq(2, 10);
        assert_eq!(dropped, 0);
        assert_eq!(delta.len(), 2);
        assert_eq!(delta[0].data, "c");
        assert_eq!(delta[1].data, "d");

        // seq_since = 0 -> tüm mevcutlar [2, 3, 4]
        let (all, dropped_zero) = rb.since_seq(0, 10);
        assert_eq!(dropped_zero, 0);
        assert_eq!(all.len(), 3);

        // seq_since = 0 değil de 0'dan sonra 1 verilseydi: ilk mevcut seq=2 olduğu için dropped=0,
        // ama bir eleman daha eklersek (seq=5 -> mevcut [3,4,5]), seq_since=1 için seq=2 düşmüş olur (dropped=1)
        rb.push_at("e", 500);
        let (delta2, dropped2) = rb.since_seq(1, 10);
        assert_eq!(dropped2, 1);
        assert_eq!(delta2.len(), 3);
        assert_eq!(delta2[0].seq, 3);
    }

    #[test]
    fn test_percentile_stats_and_spike_detection() {
        let mut samples = vec![10.0; 95];
        samples.extend_from_slice(&[12.0, 14.0, 15.0, 85.0, 95.0]);
        let stats = PercentileStats::from_slice(&samples, 3.0);
        assert_eq!(stats.samples, 100);
        assert_eq!(stats.min, 10.0);
        assert_eq!(stats.max, 95.0);
        assert_eq!(stats.p50, 10.0);
        assert!(stats.p99 >= 85.0);
        assert!(stats.spike_detected);
    }
}
