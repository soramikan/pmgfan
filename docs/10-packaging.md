# 10. RPM パッケージ

AlmaLinux / RHEL 系向けに `.rpm` で配布・導入できる。

## ビルド

対象ホスト（または同 OS のビルド環境）で:

```bash
sudo dnf install rpm-build systemd-rpm-macros
./packaging/build-rpm.sh
```

`~/rpmbuild/RPMS/<arch>/pmgfan-<ver>-<rel>.<arch>.rpm` が生成される。
`cargo` は rustup 等でもよい（`~/.cargo/bin` を自動で PATH に足す）。

## インストール

```bash
sudo dnf install ./pmgfan-0.1.0-1.el10.x86_64.rpm
sudo systemctl enable --now pmgfand
```

パッケージの内容:

| パス | 内容 |
|---|---|
| `/usr/sbin/pmgfand` | デーモン（root で実行） |
| `/usr/bin/pmgfanctl` | CLI/TUI クライアント |
| `/usr/lib/systemd/system/pmgfand.service` | unit（watchdog・RuntimeDirectory 付き） |
| `/etc/pmgfand/config.toml` | 設定（`noreplace`。`root:pmgfan 0640`） |

`%pre` で `pmgfan` グループを作成する。非 root から
`pmgfanctl` を使うユーザーはこのグループに追加する:

```bash
sudo usermod -aG pmgfan <user>
```

ランタイムディレクトリ `/run/pmgfand`（socket 置き場）は
unit の `RuntimeDirectory=` が `root:pmgfan 0750` で用意する。

## アンインストール

```bash
sudo dnf remove pmgfan
```

systemd scriptlet がサービスを停止・無効化する。
`/etc/pmgfand/config.toml` を編集済みなら
`config.toml.rpmsave` として残る。
