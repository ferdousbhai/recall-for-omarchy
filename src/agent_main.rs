//! Native user CLI and desktop coordinator. The inference workers remain isolated processes.
mod desktop;
fn main() {
    let code = match desktop::run(std::env::args().skip(1).collect()) {
        Ok(code) => code,
        // Every failure leaves through here: one JSON object on stderr, exit status 1.
        Err(error) => desktop::report(&error),
    };
    std::process::exit(code);
}
