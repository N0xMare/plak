//! Entry point for the product-internal persistence benchmark.
fn main() {
    let directory = std::env::var_os("PLAK_BENCH_DIR").map(std::path::PathBuf::from);
    println!("{}", plak::benchmark_product_store(directory.as_deref()));
}
