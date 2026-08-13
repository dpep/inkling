fn main() {
    // The CLI arrives in M9; until then the library is where the work is.
    println!("inkling {}", env!("CARGO_PKG_VERSION"));
}
