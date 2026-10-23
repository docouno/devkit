use std::io;

use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
};

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let listener = TcpListener::bind(("0.0.0.0", 8080)).await?;
    println!("devkit-api listening on http://0.0.0.0:8080");

    axum::serve(listener, devkit_api::router())
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = terminate.recv() => {},
            }
        })
        .await
}
