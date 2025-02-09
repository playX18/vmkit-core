use std::sync::LazyLock;

use clap::Parser;

#[derive(Parser)]
pub struct Options {
    #[cfg(feature = "cooperative")]
    #[clap(long, default_value_t = true)]
    pub conservative_stacks: bool,

    /// Maximum amount of bytes to scan for interior pointers. 
    #[clap(long, default_value_t = 1024)]
    pub interior_pointer_max_bytes: usize,

    #[clap(long, default_value_t = false)]
    pub aslr: bool,

    #[clap(long, default_value_t = true)]
    pub compressed_pointers: bool,
    /// If true, the VM will guarantee two bits for the tag of compressed pointers.
    /// 
    /// Requires MIN_ALIGNMENT to be 16.
    #[clap(long, default_value_t = true)]
    pub tag_compressed_pointers: bool,

    #[clap(long, default_value_t = 256 * 1024 * 1024)]
    pub max_heap_size: usize,

    #[clap(long, default_value_t = 64 * 1024 * 1024)]
    pub min_heap_size: usize,
}

pub static OPTIONS: LazyLock<Options> = LazyLock::new(Options::parse);
