//! Unix socket サーバ。`/run/pmgfand/control.sock` で JSON Lines を受け付ける。
//! docs/05-ipc.md 参照。

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use pmgfan_core::protocol::{decode_request, encode, Request, Response};
use pmgfan_ipmi::backend::FanControlBackend;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::RwLock;
use tracing::warn;

use crate::daemon::{apply_mode, Shared};

const SOCKET_GROUP: &str = "pmgfan";

/// socket を bind し、接続ごとにタスクを立てて処理する。
pub async fn serve<B>(path: &Path, backend: Arc<B>, shared: Arc<RwLock<Shared>>) -> std::io::Result<()>
where
    B: FanControlBackend + Send + Sync + 'static,
{
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o750))?;
        // socket 同様に pmgfan グループで通過できるようにする
        chown_to_group(dir);
    }
    let _ = std::fs::remove_file(path); // stale socket
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    chown_to_group(path);

    loop {
        let (conn, _) = listener.accept().await?;
        let backend = Arc::clone(&backend);
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            if let Err(e) = handle(conn, &*backend, &shared).await {
                warn!(error = %e, "ipc connection failed");
            }
        });
    }
}

/// socket のグループを `pmgfan` にする。グループが無ければ警告のみ。
fn chown_to_group(path: &Path) {
    let Ok(group) = CString::new(SOCKET_GROUP) else { return };
    unsafe {
        let grp = libc::getgrnam(group.as_ptr());
        if grp.is_null() {
            warn!("group '{SOCKET_GROUP}' not found; socket is root-only");
            return;
        }
        let Ok(cpath) = CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        // uid = -1 で owner 維持、gid のみ変更
        libc::chown(cpath.as_ptr(), u32::MAX, (*grp).gr_gid);
    }
}

async fn handle<B: FanControlBackend>(
    conn: UnixStream,
    backend: &B,
    shared: &RwLock<Shared>,
) -> std::io::Result<()> {
    let (r, mut w) = conn.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        let resp = dispatch(&line, backend, shared).await;
        let mut out = encode(&resp).unwrap_or_else(|_| {
            r#"{"version":1,"type":"error","error":"encode failed"}"#.to_string()
        });
        out.push('\n');
        w.write_all(out.as_bytes()).await?;
    }
    Ok(())
}

async fn dispatch<B: FanControlBackend>(
    line: &str,
    backend: &B,
    shared: &RwLock<Shared>,
) -> Response {
    let req = match decode_request(line) {
        Ok(r) => r,
        Err(e) => return Response::Error {
            error: format!("bad request: {e}"),
        },
    };
    match req {
        Request::GetStatus => {
            let s = shared.read().await;
            Response::Status {
                state: s.state,
                mode: s.mode.clone(),
                pwm: s.pwm,
                fans: s.fans.clone(),
                temperatures: s.temps.clone(),
                uptime_secs: s.started.elapsed().as_secs_f64(),
            }
        }
        Request::SetMode { mode } => match apply_mode(backend, shared, &mode).await {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error { error: e },
        },
    }
}
