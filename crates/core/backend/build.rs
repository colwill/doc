// `sqlx::migrate!` embeds the migrations when the crate compiles, so adding one alone must compile
// it again, or the backend starts without it.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
