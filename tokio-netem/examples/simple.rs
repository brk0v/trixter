use std::time::Duration;

use bytes::Bytes;
use tokio::{
    io::{self, AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, oneshot},
};
use tokio_netem::{
    corrupter::Corrupter,
    injector::ReadInjector,
    io::{NetEmReadExt, NetEmWriteExt},
    shutdowner::Shutdowner,
    terminator::Terminator,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let stream = TcpStream::connect("localhost:8080").await?;
    //let stream = tokio::io::BufStream::new(stream);

    let (_inject_tx, inject_rx) = mpsc::channel(12);

    let stream = Terminator::new(stream, 0.1);
    let stream = Corrupter::new(stream, 0.1);
    let _stream = stream
        .inject_read(inject_rx)
        //.delay_reads(Duration::from_secs(1))
        .slice_writes(12)
        .delay_writes(Duration::from_secs(1));

    todo!()
}

async fn _example1() -> io::Result<()> {
    // Create throttled stream
    let mut stream = TcpStream::connect("localhost:80")
        .await?
        .throttle_writes(32 * 1024) // 32 KB/s
        .slice_writes(16); // flush writes every 16B

    stream.write_all(b"ping").await?;

    // I/O fails with 1% probability
    let mut stream = Terminator::new(stream, 0.01);

    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).await?;

    // Kill communication with provided error
    let (tx_err, rx_err) = oneshot::channel();
    let stream = Shutdowner::new(stream, rx_err);
    tx_err
        .send(io::Error::new(io::ErrorKind::Other, "unexpected").into())
        .unwrap();

    // Corrupt data with 0.5% probability
    let stream = Corrupter::new(stream, 0.005);

    // Inject data on read
    let (tx_data, rx_data) = mpsc::channel(1);

    let mut _stream = ReadInjector::new(stream, rx_data);
    tx_data
        .send(Bytes::from_static(b"@*(@*!(#*!("))
        .await
        .unwrap();

    todo!()
}
