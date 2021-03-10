use std::str::FromStr;

fn main() {
    let mut h = mylog::Builder::new()
        .set_spec(&std::env::var("RUST_LOG").unwrap_or("info".to_owned()))
        .set_format(
            ::std::env::var("MOONFIRE_FORMAT")
                .map_err(|_| ())
                .and_then(|s| mylog::Format::from_str(&s))
                .unwrap_or(mylog::Format::Google),
        )
        .set_color(
            ::std::env::var("MOONFIRE_COLOR")
                .map_err(|_| ())
                .and_then(|s| mylog::ColorMode::from_str(&s))
                .unwrap_or(mylog::ColorMode::Auto),
        )
        .build();
    h.clone().install().unwrap();
    let _a = h.async_scope();
    log::error!("error");
    log::warn!("warn");
    log::info!("info");
    log::debug!("debug");
    log::trace!("trace");
}
