# リリース手順

配布物はリリース zip 一本。

## 切り方

1. ゲート: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
2. `Cargo.toml` の `version` を上げてコミット
   - バージョン番号は build.rs が exe のバージョンリソースに自動反映（要手動同期なし）
3. `git tag v<version> && git push origin <既定ブランチ> v<version>`
   （リモート公開前はタグだけ打っておき、公開時に push すれば workflows が走る）
4. `.github/workflows/release.yml` が windows-latest で `ef.exe` / `efd.exe` /
   `ef-index.exe` をビルドし、`everyfind-x86_64-pc-windows-msvc-v<version>.zip` +
   `SHA256SUMS` を GitHub Release に添付する
5. 初回リリース後: `packaging/scoop/everyfind.json` の `hash` を SHA256SUMS の値で
   埋めて push（以後は autoupdate が SHA256SUMS を読む）

## ウイルス誤検知（重要。公開前に必ず）

署名なしの新品 Rust exe は、Defender の ML ヒューリスティック（`!ml` 系。実際に
開発機で `Trojan:Win32/Bearfoos.B!ml` を踏んだ）に誤検知されやすい。Everyfind は
レジストリ書き込み（右クリック統合）・デタッチ子プロセス・サービス制御・名前付き
パイプと、ML が嫌う挙動が揃うため特に出やすい。対策:

1. **バージョンリソース埋め込み**（実装済み・build.rs）: メタデータ無しの exe は
   疑い度が上がる主因。`ef.exe`/`efd.exe`/`ef-index.exe` に会社名・製品名・
   バージョン・説明を焼き込んである（右クリック→プロパティ→詳細で確認可）。
2. **Microsoft へ誤検知報告**（リリース毎・無料・数日で反映）:
   <https://www.microsoft.com/en-us/wdsi/filesubmission> に「Software developer」
   として zip 内の3 exe を提出。承認されると ML モデル側が学習し検知が消える。
   **これはリリース手順の一部**（タグ push → zip 生成 → 提出、まで1セット）。
3. 恒久策は **コード署名証明書**だが有償なので当面は買わない（README にも明記）。
   自己署名は SmartScreen には効かないので不要。

## zip からの動作確認（受け入れ手順・利用者手順と同一）

1. zip を展開し、`ef.exe` と `efd.exe` を PATH の通った場所に置く
   （`ef-index.exe` は開発用エンジン CLI。配布はするが利用者は触らない）
2. **SmartScreen**: 署名証明書は買わない方針。初回実行で「WindowsによってPCが
   保護されました」が出たら **「詳細情報」→「実行」**。README にも同じ注記がある
3. 管理者ターミナルで一度だけ: `ef service install` → `ef service start`
4. 通常ターミナルで: `ef <なにか>` が即答すること、`ef status` が entries を返すこと
5. アンインストール: `ef service stop` → `ef service uninstall` → exe を削除
   （状態は `C:\ProgramData\everyfind`。消してよい）

## 配布チャネル

**README には、実際に動くものだけ載せる。** 動かない導線を書いておくと、読んだ人が
そのとおり打って失敗する。増えたらそのつど README に足す。

| チャネル | README 掲載 | 有効にするために必要なこと |
| --- | --- | --- |
| GitHub Releases | **済** | リポジトリ公開 + `v*` タグ。release.yml が zip と SHA256SUMS を作る |
| cargo install --git | **済** | リポジトリ公開のみ |
| scoop | 未 | ① 初回リリースの SHA256 を `packaging/scoop/everyfind.json` の `hash` に入れる（今は `REPLACE-WITH-...` のまま）② README に書く raw URL のブランチ名を既定ブランチに合わせる（`main` 決め打ちになっている） |
| cargo binstall | 未 | crates.io publish が要る。その前に `everyfind-ipc` を publish して、`Cargo.toml` の path 依存を `{ version = "…", path = "ipc" }` に変える（path のみの依存は publish できない） |
| winget | 未 | Releases 公開後に microsoft/winget-pkgs へ manifest を PR。審査あり |
