# 導入の詳細

`tadoru setup` で足りる場合は [README](../../README.md) だけ読めばよい。
ここは手作業で設定したい場合と、うまく動かない場合の資料。

## setup が書く内容

目印で囲んだ 1 ブロックを、シェルの起動ファイルに追記する。

```text
# >>> tadoru >>>
$tadoruEncoding = [Console]::OutputEncoding
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
Invoke-Expression (& C:\path\to\tadoru.exe init powershell | Out-String)
[Console]::OutputEncoding = $tadoruEncoding
Remove-Variable tadoruEncoding
# <<< tadoru <<<
```

再実行しても増えない。内容が変わっていれば差し替える。既に同じなら何もしない。
やめるときは目印から目印までを消す。書き込みは一時ファイルを経由するので、
途中で中断しても起動ファイルが壊れた状態で残らない。
既存の起動ファイルが UTF-8 として読めない場合は、上書きせず中止して理由を表示する。

シェルは環境から判定する。明示したいときは `tadoru setup powershell` のように指定する。

## 文字コードについて

`.ps1` に書き込むときは UTF-8 の BOM を付ける。Windows PowerShell 5.1 は BOM のない
`.ps1` をシステムのコードページとして読むため、日本語を含むパスが別の文字列になり、
存在しない実行ファイルを指すようになる。`init powershell --out` が作る `.ps1` も同じ。

ブロックの中で `[Console]::OutputEncoding` を切り替えているのも同じ理由。5.1 は外部
プログラムの出力もコードページで復号するので、`init` が出す初期化コードに含まれる
実行ファイルのパスが壊れる。PowerShell 7 はどちらも最初から UTF-8 なので、7 では
この 2 つは何も変えない。

`tadoru setup` が書き込むのは PowerShell 7 のプロファイル
(`ドキュメント\PowerShell\Microsoft.PowerShell_profile.ps1`)。5.1 を使う場合は
`ドキュメント\WindowsPowerShell\Microsoft.PowerShell_profile.ps1` に同じブロックを
自分で書く。保存は BOM 付き UTF-8 にする。

## 手動で設定する

dotfiles をバージョン管理している、共有マシンで設定を書き換えられない、
zoxide との読み込み順を自分で決めたい。そういう場合は自分で書く。

`z` と `zi` は zoxide がエイリアスとして定義する。エイリアスは関数より優先されるため、
tadoru の行は **zoxide init より後**に置く必要がある。

| シェル | 書くファイル | 書く内容 |
|---|---|---|
| PowerShell | `$PROFILE` | 上の「setup が書く内容」の 5 行。BOM 付き UTF-8 で保存する |
| bash / zsh | `~/.bashrc` `~/.zshrc` | `eval "$(tadoru init bash)"` |
| cmd.exe | なし | `tadoru init cmd --out`（exe と同じフォルダに書く） |

`tadoru init <shell>` は初期化コードを標準出力に書くだけで、ファイルには触らない。
どのファイルに、どの順序で入れるかは利用者が決めることなので、`init` は判断しない。

自分でシムを書く場合は `tadoru pick --mode <mode>` を使う。選ばれたパスを標準出力に
1 行で書くので、それを `cd` に渡す。標準出力はこの 1 行専用で、案内や警告は標準エラーに出る。

| 終了コード | 意味 |
|---|---|
| 0 | パスを 1 行出力した |
| 1 | 選ばずに終了した（Esc / Ctrl-C） |
| 2 | エラー。理由は標準エラーに出る |

`0` 以外のときは移動しない。`1` と `2` を区別しないと、エラーを黙って握りつぶす。

## PowerShell の .ps1 を PATH に置く方法

プロファイルを触らずに済ませたい場合、`c.ps1` などを PATH に置く手もある。

```text
tadoru init powershell --out
```

`--out` の後ろに何も書かなければ exe と同じフォルダに出す。生成物が 1 か所に集まり、
PATH に足すフォルダも 1 つで済む。別の場所に出したいときだけ `--out C:\path\on\PATH`
のように指定する。

制約が 2 つある。zoxide を使っていると `z` と `zi` はエイリアスが優先されるので、
この方法では置き換えられない。実行ポリシーが Restricted だと .ps1 は動かない。
zip から展開した直後はブロック属性が付くことがあるので、
そのフォルダで `Unblock-File *.ps1` を実行する。

## 更新したとき

本体を差し替えただけならそのまま動く。シムは自分と同じフォルダの実行ファイルを先に探す。
シェル連携の内容そのものが変わった場合は `tadoru setup` か `tadoru init` を実行し直し、
新しいシェルを開くか初期化を読み直す。

## コマンド名を変える

既定の名前は `c`（ディレクトリ）、`cf`（ファイル）、`z` と `zi`（zoxide の履歴）。
1 文字の名前は他のスクリプトとぶつかりやすく、`cf` は Cloud Foundry の CLI の名前でもある。

`--cmd` で頭の名前を変えられる。ファイル用はその名前に `f` を付けたものになる。
`setup` にも `init` にも付けられ、`setup` は付けた名前をプロファイルの行にも書き込む。

```text
tadoru setup --cmd j            # j と jf
tadoru init cmd --out --cmd j   # j.cmd と jf.cmd
```

`setup` と `init --out` は、同じ名前のコマンドが PATH 上にすでにあれば場所を表示する。
cmd.exe と、PATH に置いた `.ps1` は PATH の先にあるほうが見つかるので、どちらが先かも表示する。
PowerShell と bash の関数はそのシェルでは優先されるが、他のシェルでは元のコマンドが動く。

## zoxide との関係

履歴を使う `z`・`zi`・recent は zoxide の記録を読む。無い環境では、
入れ方と代わりの手段を画面に表示する。`c`・`cf`・browse・favorites は本体だけで動く。

履歴を自前で持たないのは、zoxide がシェルの cd フックですべての移動を記録しているため。
tadoru が自前で持つと tadoru 経由の移動しか残らず、既存の履歴も捨てさせることになる。
tadoru 経由で移動したフォルダは、zoxide が使える場合に `zoxide add` で記録する。

tadoru は zoxide の `z` と `zi` を自分の定義で置き換える。`z` で移動しても `c -` で戻れるように
移動元を記録するためで、`zi` は tadoru の一覧で選ぶ。zoxide の元の `z` と `zi` を使いたい場合は
`--no-z` を付ける。その場合 recent は `c` の画面で Shift-Tab を 2 回押すか、上枠の recent をクリックして開く。
