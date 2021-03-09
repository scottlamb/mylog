use log::info;

fn main() {
    let mut h = mylog::Builder::new()
        .set_spec(&std::env::var("RUST_LOG").unwrap_or("info".to_owned()))
        .build();
    h.clone().install().unwrap();
    let _a = h.async_scope();
    info!("Hello world.");
}
