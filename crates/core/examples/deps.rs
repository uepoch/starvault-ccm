//! Dump a container's dependency list: `cargo run -p svccm-core --example deps -- <file>`
fn main() {
    let path = std::env::args().nth(1).expect("usage: deps <container>");
    match svccm_core::package::container::read_container_dependencies(std::path::Path::new(&path)) {
        Ok(c) => {
            for d in c.dependencies() {
                println!("{d}");
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
