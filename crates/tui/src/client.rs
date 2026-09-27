//! pmgfand 制御 socket への JSON Lines クライアント。

use std::path::Path;

use anyhow::{bail, Context, Result};
use pmgfan_core::protocol::{decode_response, encode, Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

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
    let Some(resp_line) = lines.next_line().await? else {
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
