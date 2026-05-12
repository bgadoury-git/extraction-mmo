// Compile-time guard: exactly one of `server` or `client` must be enabled.
#[cfg(all(feature = "server", feature = "client"))]
compile_error!("features `server` and `client` are mutually exclusive");

#[cfg(not(any(feature = "server", feature = "client")))]
compile_error!("one of features `server` or `client` must be enabled");

#[cfg(feature = "server")]
mod server;

#[cfg(feature = "client")]
mod client;

fn main() {
    #[cfg(feature = "server")]
    server::run();

    #[cfg(feature = "client")]
    client::run();
}
