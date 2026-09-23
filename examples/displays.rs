//! Hardware smoke test: what the daemon sees. `cargo run --example displays`

fn main() {
    let _app = aerospace_responsive::macos::shared_app();
    for s in aerospace_responsive::macos::screens() {
        println!("{s:?}");
    }
    println!("geometry: {:?}", aerospace_responsive::macos::geometry_fingerprint());
}
