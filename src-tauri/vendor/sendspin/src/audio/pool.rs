// ABOUTME: Buffer pool for reusing audio sample buffers
// ABOUTME: Eliminates allocations in the audio hot path

use cpal::Sample;
use crossbeam::queue::ArrayQueue;
use std::sync::Arc;

/// Buffer pool for reusing audio sample buffers
pub struct BufferPool {
    pool: Arc<ArrayQueue<Vec<i32>>>,
    capacity: usize,
}

impl BufferPool {
    /// Create a new buffer pool
    ///
    /// # Arguments
    /// * `pool_size` - Number of buffers to pre-allocate
    /// * `buffer_capacity` - Capacity of each buffer in samples
    pub fn new(pool_size: usize, buffer_capacity: usize) -> Self {
        let pool = Arc::new(ArrayQueue::new(pool_size));

        // Pre-allocate buffers
        for _ in 0..pool_size {
            let mut buf = Vec::with_capacity(buffer_capacity);
            buf.resize(buffer_capacity, i32::EQUILIBRIUM);
            buf.clear(); // Clear so len() is 0 but capacity is preserved
            let _ = pool.push(buf);
        }

        Self {
            pool,
            capacity: buffer_capacity,
        }
    }

    /// Get a buffer from the pool (or allocate if pool is empty)
    pub fn get(&self) -> Vec<i32> {
        self.pool.pop().unwrap_or_else(|| {
            log::warn!(
                "Buffer pool exhausted (capacity={} samples); falling back to heap allocation",
                self.capacity
            );
            Vec::with_capacity(self.capacity)
        })
    }

    /// Return a buffer to the pool
    pub fn put(&self, mut buf: Vec<i32>) {
        buf.clear();
        let _ = self.pool.push(buf); // Ignore if pool is full
    }

    /// Get the buffer capacity
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}
