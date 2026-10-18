//! Scan a path and print stats without writing a snapshot: `cargo run --release --example scan_bench -- /`
fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| "/".into());
    let (ix, st) = ferret_core::scan::scan(std::path::Path::new(&root)).expect("scan");
    println!(
        "{} nodes: {} files, {} dirs, {} symlinks, {} denied in {:.2?}",
        ix.len(),
        st.files,
        st.dirs,
        st.symlinks,
        st.permission_denied,
        st.elapsed
    );
}
