use cpal::Sample;
use sendspin::audio::BufferPool;

#[test]
fn test_buffer_pool_creation() {
    let pool = BufferPool::new(10, 1024);
    assert_eq!(pool.capacity(), 1024);
}

#[test]
fn test_buffer_pool_get_and_return() {
    let pool = BufferPool::new(5, 1024);

    // Get a buffer
    let mut buf = pool.get();
    assert_eq!(buf.capacity(), 1024);

    // Modify it
    buf.extend_from_slice(&vec![i32::EQUILIBRIUM; 100]);
    assert_eq!(buf.len(), 100);

    // Return it
    pool.put(buf);

    // Get another - should be the same buffer (reused)
    let buf2 = pool.get();
    assert_eq!(buf2.capacity(), 1024);
    assert_eq!(buf2.len(), 0); // Should be cleared
}

#[test]
fn test_buffer_pool_fallback_allocation() {
    let pool = BufferPool::new(2, 1024);

    // Get all buffers from pool
    let _buf1 = pool.get();
    let _buf2 = pool.get();

    // This should allocate a new buffer
    let buf3 = pool.get();
    assert_eq!(buf3.capacity(), 1024);
}

#[test]
fn test_buffer_pool_put_actually_returns_buffer() {
    // `put()` must deposit the buffer back into the pool so a later `get()`
    // reuses it. A check on default-capacity buffers can't distinguish a
    // reused buffer from a fresh `Vec::with_capacity(pool.capacity())`, so
    // this test deposits a buffer whose capacity is *larger* than the pool's
    // default — if it comes back from `get()`, the pool really did store it.
    let pool = BufferPool::new(1, 1024);

    // Drain the pre-allocated buffer so the pool is empty.
    let _ = pool.get();

    // Put a buffer with a distinctive (larger) capacity.
    let oversized: Vec<i32> = Vec::with_capacity(4096);
    pool.put(oversized);

    // Get should return the oversized buffer we just put back, NOT a freshly
    // allocated 1024-capacity one.
    let reused = pool.get();
    assert!(
        reused.capacity() >= 4096,
        "expected put buffer to be returned (cap >= 4096), got {}",
        reused.capacity()
    );
}
