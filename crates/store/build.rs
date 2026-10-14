// `sqlx::migrate!` embeds the migrations at compile time; rebuild when a new
// file is added, not only when Rust sources change.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
