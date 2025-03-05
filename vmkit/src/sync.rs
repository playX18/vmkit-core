//! Synchronization primitives for VMKit.
//! 
//! 
//! Provides synchronization primitives which are friendly to our thread system. Most of the types
//! provide `*_with_handshake` and `*_no_handshake` methods. The main difference between these two
//! methods is that the former will notify the scheduler that thread is blocked, while the latter
//! will not. Most of the times your code should use the `*_with_handshake` methods, so that GC
//! or other tasks can be scheduled while the thread is blocked. `*_no_handshake` methods should be
//! used when the thread is accessing GC data structures, or when the thread is not allowed to be
//! blocked.
pub mod semaphore;
pub mod monitor;


pub use monitor::*;

pub use super::threading::parked_scope;