//! Offline HTTP pages are exact loopback fixtures, never production exemptions.
use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(crate) struct Server {
    pub url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    pub async fn start(page: &'static str) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/documentation", listener.local_addr()?);
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = [0; 4096];
                let _ = socket.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    page.len(),
                    page
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        Ok(Self { url, task })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
