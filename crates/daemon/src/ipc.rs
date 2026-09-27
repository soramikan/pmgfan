//! Unix socket サーバ。`/run/pmgfand/control.sock` で JSON Lines を受け付ける。
//! docs/05-ipc.md 参照。

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use pmgfan_core::control::ControlParams;
use pmgfan_core::curve::Curve;
use pmgfan_core::protocol::{decode_request, encode, Request, Response};
use pmgfan_ipmi::backend::FanControlBackend;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, RwLock};
use tracing::warn;

use crate::daemon::{apply_mode, Shared};

const SOCKET_GROUP: &str = "pmgfan";
/// 1接続で受け付ける最大バイト数（プロトコルは1行リクエスト。
/// 誤動作クライアントの巨大入力でメモリを圧迫しないための上限）。
const MAX_REQUEST_BYTES: u64 = 16 * 1024;

/// 実行時ディレクトリを用意する。
///
/// リーフディレクトリを `create_dir` で1回だけ作成し、その成否で
/// 「自分が作ったか」を判定する（`exists()` や `create_dir_all` の
/// Ok では判定できず、競合・TOCTOU に弱い）。
/// 自分が作成したディレクトリにだけ 0750 + pmgfan グループを
/// 適用し、既存ディレクトリは一切触らない。
pub fn prepare_runtime_dir(dir: &Path) -> std::io::Result<()> {
    if dir.as_os_str().is_empty() {
        return Ok(());
    }
    // 親チェーンの作成は競合しても無害（権限はリーフだけに適用）
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::create_dir(dir) {
        Ok(()) => set_dir_permissions(dir),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// socket を bind し、権限を設定する。`READY=1` より先に完了させる
/// ため、serve の spawn 前に呼ぶ。
///
/// 親ディレクトリの権限・グループは、自ら作成した場合にのみ
/// 変更する（`--socket` で既存ディレクトリ（例: /tmp）を指されても、
/// その属性を勝手に変えない）。なおデーモン経路では
/// `acquire_instance_lock` が先に `prepare_runtime_dir` で
/// ディレクトリを用意しているため、ここでは既存扱いになる。
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        prepare_runtime_dir(dir)?;
    }
    let _ = std::fs::remove_file(path); // stale socket
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    chown_to_group(path);
    Ok(listener)
}

/// 接続ごとにタスクを立てて処理する。
pub async fn serve<B>(
    listener: UnixListener,
    backend: Arc<B>,
    shared: Arc<RwLock<Shared>>,
    curves: Arc<Vec<Curve>>,
    ctrl: Arc<Mutex<()>>,
    params: ControlParams,
) -> std::io::Result<()>
where
    B: FanControlBackend + Send + Sync + 'static,
{
    loop {
        let (conn, _) = listener.accept().await?;
        let backend = Arc::clone(&backend);
        let shared = Arc::clone(&shared);
        let curves = Arc::clone(&curves);
        let ctrl = Arc::clone(&ctrl);
        tokio::spawn(async move {
            if let Err(e) = handle(conn, &*backend, &shared, &curves, &ctrl, &params).await {
                warn!(error = %e, "ipc connection failed");
            }
        });
    }
}

/// この関数が自ら作成したディレクトリにだけ適用する権限設定。
fn set_dir_permissions(dir: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o750))?;
    // socket 同様に pmgfan グループで通過できるようにする
    chown_to_group(dir);
    Ok(())
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
    curves: &[Curve],
    ctrl: &Mutex<()>,
    params: &ControlParams,
) -> std::io::Result<()> {
    let (r, mut w) = conn.into_split();
    // 読み取り総量に上限を設け、巨大な1行でメモリを食われないようにする
    let mut lines = BufReader::new(r.take(MAX_REQUEST_BYTES)).lines();
    while let Some(line) = lines.next_line().await? {
        let resp = dispatch(&line, backend, shared, curves, ctrl, params).await;
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
    curves: &[Curve],
    ctrl: &Mutex<()>,
    params: &ControlParams,
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
        Request::SetMode { mode } => {
            match apply_mode(backend, shared, ctrl, params, curves, &mode).await {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error { error: e },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// 既存ディレクトリの権限を bind が勝手に変えないこと
    /// （`--socket /tmp/x.sock` で /tmp が 0750 になる事故の防止）。
    #[tokio::test]
    async fn bind_preserves_existing_dir_permissions() {
        let dir = std::env::temp_dir().join(format!("pmgfan-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let sock = dir.join("control.sock");
        let _listener = bind(&sock).unwrap();

        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o755,
            "bind must not chmod a pre-existing parent dir"
        );
        assert_eq!(
            std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777,
            0o660
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 自分が作成したディレクトリには権限を設定する
    #[tokio::test]
    async fn bind_sets_permissions_on_created_dir() {
        let dir = std::env::temp_dir().join(format!("pmgfan-test-new-{}", std::process::id()));
        let sock = dir.join("control.sock");
        let _listener = bind(&sock).unwrap();

        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o750
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
