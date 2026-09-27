//! pmgfand 制御 socket への JSON Lines クライアント。

use std::path::Path;

use anyhow::{bail, Context, Result};
use pmgfan_core::protocol::{decode_response, encode, Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// ソケット通信の上限。デーモンが詰まっているときに
/// クライアント側が無制限に待たないための保証。
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 1リクエスト → 1レスポンスを送受信する。
pub async fn request(socket: &Path, req: &Request) -> Result<Response> {
    let stream = UnixStream::connect(socket).await.with_context(|| {
        format!(
            "cannot connect to {} (is pmgfand running? do you belong to the 'pmgfan' group?)",
            socket.display()
        )
    })?;
    let (r, mut w) = stream.into_split();

    let mut line = encode(req)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.shutdown().await?;

    let mut lines = BufReader::new(r).lines();
    let resp_line = tokio::time::timeout(REQUEST_TIMEOUT, lines.next_line())
        .await
        .context("timed out waiting for pmgfand response")??;
    let Some(resp_line) = resp_line else {
        bail!("pmgfand closed connection without a response");
    };
    decode_response(&resp_line).with_context(|| format!("bad response: {resp_line}"))
}

/// レスポンスを検査し、Error なら Err にする。
pub fn expect_ok(resp: Response) -> Result<()> {
    match resp {
        Response::Ok => Ok(()),
        Response::Error { error } => bail!("pmgfand: {error}"),
        _ => bail!("unexpected response"),
    }
}
