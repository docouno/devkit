use std::io;

use devkit_api::router;
use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
};

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let host = match std::env::var("HOST") {
        Ok(host) => host,
        Err(std::env::VarError::NotPresent) => "0.0.0.0".into(),
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "HOST must be UTF-8",
            ));
        }
    };
    if host.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "HOST must not be empty",
        ));
    }
    let port = match std::env::var("PORT") {
        Ok(raw) if !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) => raw
            .parse::<u16>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "PORT must be 0..65535"))?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PORT must be 0..65535",
            ));
        }
        Err(std::env::VarError::NotPresent) => 8080,
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PORT must be UTF-8",
            ));
        }
    };
    let listener = TcpListener::bind((host.as_str(), port)).await?;
    let name = env!("CARGO_PKG_NAME");
    let address = listener.local_addr()?;
    println!("{name} listening on http://{address}");

    axum::serve(listener, router())
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = terminate.recv() => {},
            }
        })
        .await
}
