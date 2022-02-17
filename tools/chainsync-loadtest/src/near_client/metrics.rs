use near_metrics::{
    try_create_histogram, try_create_int_counter, try_create_int_gauge, Histogram, IntCounter,
    IntGauge,
};
use once_cell::sync::Lazy;

pub static BLOCK_PRODUCED_TOTAL: Lazy<IntCounter> = Lazy::new(|| {
    try_create_int_counter(
        "near_block_produced_total",
        "Total number of blocks produced since starting this node",
    )
    .unwrap()
});
pub static CHUNK_PRODUCED_TOTAL: Lazy<IntCounter> = Lazy::new(|| {
    try_create_int_counter(
        "near_chunk_produced_total",
        "Total number of chunks produced since starting this node",
    )
    .unwrap()
});
pub static GC_TIME: Lazy<Histogram> = Lazy::new(|| {
    try_create_histogram("near_gc_time", "Time taken to do garbage collection").unwrap()
});
pub static CHUNKS_RECEIVING_DELAY_US: Lazy<IntGauge> = Lazy::new(|| {
    try_create_int_gauge(
        "near_chunks_receiving_delay_us",
        "Max delay between receiving a block and its chunks for several most recent blocks",
    )
    .unwrap()
});
pub static BLOCKS_AHEAD_OF_HEAD: Lazy<IntGauge> = Lazy::new(|| {
    try_create_int_gauge(
        "near_blocks_ahead_of_head",
        "Height difference between the current head and the newest block or chunk received",
    )
    .unwrap()
});
