# hearthd セキュリティ監査

- 対象: `hearth` commit `fbf85e8`
- 対象コンポーネント: `hearth-daemon`、daemon transport、daemon から到達可能な `hearth-tools` / `hearth-core` / `hearth-graph`
- 監査方式: ソース追跡、脅威モデリング、既存テスト/CI確認、Codex による独立観点の並列レビュー

## 1. 結論

現状の `hearthd` は、**Unix socket に接続できる主体を全面的に信頼する、ユーザー権限の任意コード実行・任意ファイル操作サービス**である。socket 接続後の認証・操作別認可、workspace 境界、sandbox はない。

したがって、次の条件をすべて満たす場合に限り、現在の設計は限定的に許容できる。

1. daemon を非特権ユーザーで実行する。
2. socket の親ディレクトリが当該ユーザーだけにアクセス可能である。
3. 同一 UID の全プロセスを信頼する。
4. 入力規模とクライアント数が善意で、可用性攻撃を想定しない。
5. daemon が保持する環境変数、OS 権限、TCC/entitlement 等がクライアントより強くない。

`hearthd` を別 UID、root、権限の強い launch agent/service、複数信頼ドメインで共有する構成は、現状では安全でない。また、同一 UID の侵害プロセスを防御対象にする場合、socket `0600` や peer UID 検査だけでは境界にならない。別サービス UID、OS sandbox、継承済み FD 等による能力分離が必要である。

最優先事項は以下である。

1. request送信後の自動fallbackを停止し、mutation二重実行とstreamed `Read`の重複出力を防ぐ。
2. root起動拒否、安全なstale socket処理、secure temp生成をリリース阻止項目として修正する。
3. endpoint の安全な生成と server/client 双方向の peer 検証。
4. 接続数、thread、frame、I/O 時間、operation 並列数、出力、常駐メモリへの hard limit。
5. `bash` とファイル操作の capability 分離、およびworkspace dirfdを起点にしたpath policy。

peer UID、private directory、socket `0600` は主に別UIDを防ぐ対策であり、same-UIDの悪意あるprocessには効かない。same-UID分離には別UID、OS sandbox、アクセス不能なbroker secret、継承済みFD等が必要である。

### 1.1 攻撃主体と判定基準

| 攻撃主体 | 到達条件 | daemon側のauthority差 | 判定 |
|---|---|---|---|
| 別UID process | socket DAC突破、unsafe custom path、または偽endpoint先取り | 通常あり | Critical。接続成功時はdaemon UIDの全authorityへ到達 |
| same-UID侵害process | 通常socketへ到達可能 | 通常なし。ただしdaemon固有の環境・TCC・別workspace stateがあれば差が生じる | 権限昇格とは限らないがHighのintegrity/availability/confused-deputy risk |
| buggy/悪意ある許可client | endpointへの正規アクセス | 配布されたcapability次第 | resource exhaustion、停止、cross-workspace破壊を評価 |
| passiveなmalicious repository | user/daemonがtreeを走査・解析 | active filesystem write権限なし | 巨大入力、静的symlink、native parser負荷が中心 |
| active filesystem attacker | target parentで並行rename/symlink/temp作成が可能 | daemonのwrite authorityを利用 | secure-temp/TOCTOU findingが成立 |

## 2. 脅威モデル

### 2.1 保護対象

- daemon 実行 UID が読書きできるファイル、repository、設定、credential
- daemon が継承した環境変数と file descriptor
- daemon が利用できる process 実行権限と network access
- CLI が daemon に送る command、編集内容、ファイル内容、stdout FD
- daemon、client、repository の可用性と整合性
- file/walk/graph cache の整合性とメモリ

### 2.2 信頼境界と entry point

| 境界 | Entry point |
|---|---|
| local process → daemon | Unix socket、length-prefixed MessagePack、`SCM_RIGHTS` |
| request → OS | `Read`、`Write`、`Edit`、`EditBatch`、`Bash`、`Grep`、`Graph` |
| control plane | `Invalidate`、`ClearCaches`、`Stats`、`Shutdown` |
| daemon → child process | 任意 shell program/args/command/cwd/env |
| daemon → filesystem | 任意 absolute/relative path、symlink、directory walk、watch |
| untrusted source → native parser | tree-sitter language parsers、resolver、grep/diff |
| CLI → server | 予測可能な socket path、server 検証なし、stdout FD 委譲 |

`Request` は全操作を一つの envelope で公開し（`crates/hearth-proto/src/lib.rs:1193-1213`）、daemon は資格情報や policy を確認せず `dispatch` する（`crates/hearth-daemon/src/main.rs:97-121`、`crates/hearth-tools/src/lib.rs:43-87`）。

### 2.3 攻撃者

1. **別 UID の local user**: unsafe な親ディレクトリ、緩い umask/custom socket、platform の socket DAC 次第で接続・先取りを狙う。
2. **同一 UID の侵害 process**: 通常の Unix DAC 上は同権限だが、daemon 停止、confused deputy、別 workspace の破壊、長寿命 process/credential の悪用を狙う。
3. **malicious/buggy client**: 独立したOS主体ではなく、接続を許可されたprocessの悪意・不具合を表す。巨大・partial・malformed request、FD、長時間command、unique cache keyを送る。
4. **passiveなmalicious repository/content**: 巨大tree/file、静的symlink、parser/watcher負荷、意図しないroot外参照を誘発する。
5. **active filesystem attacker**: target directoryでtemp作成、symlink swap、renameを並行実行する。HD-08/HD-10のrace成立にはこの能力が必要である。
6. **偽 daemon**: CLI request を取得し、偽 response を返し、渡された stdout FD を保持・書き込む。

同一 UID 攻撃は一般には「権限昇格」ではない。ただし daemon が root/別 UID、より強い sandbox entitlement、秘密を含む環境、別 workspace の state を持つ場合は、実質的な権限境界の破壊になる。

## 3. 優先リスク台帳

### HD-01 — 認証・認可のない高権限 request surface

**Severity: Critical（socket が低信頼主体から到達可能な場合） / High（same-UID availability と confused deputy）**
**状態: 確認済み**

- daemon は接続 peer の UID/GID/PID を検査しない（`crates/hearth-daemon/src/main.rs:85-121`）。
- `dispatch` は接続者に全 request を許可する（`crates/hearth-tools/src/lib.rs:43-87`）。
- `BashParams` は command、任意 cwd、追加 env、env 継承、任意 shell program/args を許す（`crates/hearth-proto/src/lib.rs:362-470`）。
- file tools は absolute path、`..`、daemon cwd 外を拒否しない。relative pathは `default_cwd` に単純 join される（`crates/hearth-tools/src/util.rs:13-20`）。
- `Shutdown` は response 後に process 全体を終了する（`crates/hearth-daemon/src/main.rs:114-121`）。

**影響**: daemon UID での任意 command 実行、任意ファイル読取/改変、credential の出力、network access、cache 攪乱、daemon 停止。root 起動時は root code execution となる。

**対策**:

- daemon を privilege boundary として扱わないことを明文化し、root 起動を原則拒否する。
- `Bash`、mutation、read-only、control plane を capability または別 socket/process に分離する。
- 開いたworkspace dirfdを起点にcomponent traversalを制約するoperation policyを導入する。`canonicalize` allowlistは存在しない作成先とrename/symlink raceを防げないため補助検査に限定する。
- same-UID 攻撃も防ぐ必要がある場合は、別 UID/service sandbox または caller に配布しない継承済み channel を使用する。

### HD-02 — endpoint の安全性が runtime directory と umask に依存

**Severity: Critical（別 UID が接続可能な構成） / High（偽 daemon・起動阻害）**
**状態: 確認済み。別 UID からの実 exploitability は OS、親 directory、umask に依存**

- default path は `$TMPDIR/hearth-$UID.sock`、長い場合は `/tmp/hearth-$UID.sock`（`crates/hearth-tools/src/transport.rs:20-31`）。名前は予測可能である。
- 親 directory の型、owner、mode を検証しない。
- socket mode を明示的に `0600` にしない。
- 起動時に既存 path を型・owner・live daemon の確認なしで `remove_file` し、失敗も無視する（`crates/hearth-daemon/src/main.rs:55-60`）。
- server は peer credential を、CLI は server credential を検証しない。

macOS の通常の `$TMPDIR` はユーザー専用であることが多い。現行Linux/macOSのpathname Unix socketはlive socketへのconnectにsocket DACを適用するため、`/tmp` fallbackだけで通常umaskのsocketへ別UIDが直ちに接続できるとは限らない。一方、world-writable parent内の予測可能な名前は、daemon停止中のpre-bind/fake-daemon、起動阻害、pathname lifecycleのnamespace integrity問題を残す。unsafeな`TMPDIR`、custom `--socket`、緩いumaskでsecurity propertyが変化する設計もfail-safeではない。

**影響**: endpoint 到達時は HD-01、socket 先取りによる起動 DoS、偽 response、request の窃取。二つ目の daemon は一つ目の live socket pathname を unlink でき、旧 daemon を到達不能のまま残す。

**対策**:

- Linux は owner/mode を検証した `$XDG_RUNTIME_DIR/hearth/`、macOS は owner-only の runtime directory を使用し、directory `0700` を保証する。
- bind 前に `lstat` し、owner-owned socket だけを stale 判定する。live endpoint があれば削除せず起動を拒否する。
- private directory 内で bind し、socket `0600` を保証する。
- Linux `SO_PEERCRED`、macOS/BSD `getpeereid` 相当で server 側が期待 UID を検証し、CLI も接続先 UID を検証する。
- cleanup は bind 時 inode/device と一致する pathname のみ unlink する。

### HD-03 — 無制限 thread-per-connection と blocking I/O

**Severity: High（低信頼clientへ公開した場合はCritical級、authority差のないtrusted same-UID構成ではMediumのavailability risk）**
**状態: 確認済み**

- accept ごとに無制限に `std::thread::spawn` する（`crates/hearth-daemon/src/main.rs:85-90`）。free `thread::spawn` は resource exhaustion 時に panic し、accept loop 自体を終了させ得る。
- connection 数、queue、header/body read timeout、response write timeout、idle timeout がない。
- clientは無送信のまま接続を保持するか、完全な4-byte headerの後でbodyを途中まで送り、threadとFDを無期限保持できる（`crates/hearth-tools/src/transport.rs:104-158`）。1–3 byteだけ送った場合、現実装は最初の`recvmsg`後にshort-frame errorで切断するため保持攻撃にはならない。ただし正当なheader fragmentationも拒否する。
- client-supplied FD への `libc::write` は blocking である。満杯 pipe 等を渡すと handler が無期限停止し得る（`crates/hearth-daemon/src/main.rs:127-156`）。
- daemon は一接続で何 request でも処理するため、idle connection も thread を占有し続ける。

**影響**: thread/FD/virtual memory exhaustion、OOM、accept loop panic、daemon 全停止。

**対策**: bounded worker pool、接続 semaphore、bounded accept queue、header/body/idle/write deadline、一接続一requestまたは明示的 session limit、spawn error の通常処理、全 resource の計測と hard ceiling。

### HD-04 — frame 上限が大きく、request/response の allocation が非対称

**Severity: High**
**状態: 確認済み**

- inbound frame 上限は 256 MiB（`crates/hearth-tools/src/transport.rs:34-35,143-158`）。並列 request では容易にメモリを枯渇させる。
- `recv_request` は `body` を `Vec::with_capacity(len)` した上で、不足分を別のzero-filled `rest` に確保してからcopyするため、最大frameでは一接続で一時的に約2倍のbody領域を持ち得る（同: `150-157`）。request decode 後もoperation固有clone/cache/result allocationが重なる。
- `write_msg` と `send_request_with_fd` は `u32` 変換しか行わず `MAX_FRAME` と比較しない（同: `38-46,71-98`）。daemon は巨大 response を serialization した後、client 側 `read_msg` の 256 MiB 上限で拒否され得る。
- malformed MessagePack による memory-safety 欠陥は確認していないが、最大 256 MiB の nested/large vector/string decode は可用性リスクである。

**対策**: control frame を実用的な値（例 1–16 MiB）へ縮小し、operation 別 payload limit、incremental decode、response size check、全接続の aggregate in-flight byte budget を導入する。

### HD-05 — operation の work/output と常駐 state に総量上限がない

**Severity: High**
**状態: 確認済み**

- `Read` は任意サイズのファイルを cache し、String response または任意 FD に全量出力する（`crates/hearth-tools/src/read.rs:34-161`）。さらに改行ごとの `u32` を持つ line index 等の lazy derived allocation は file cache の byte accounting に含まれない（`crates/hearth-core/src/cache/file.rs:20-31,48-51,158-177`、`crates/hearth-core/src/line_index.rs:13-32`）。
- `Bash` は timeout を caller が上書きでき、stdout/stderr を既定で全量保持する。reader channel も unbounded である（`crates/hearth-tools/src/bash.rs:74-110,259-400`）。正常に shell が終了した場合、pipe を保持する background descendant は100 msのidle grace後もkillされず、reader threadと共に存続し得る（同: `330-357`、`crates/hearth-tools/tests/contract_bash.rs:205-230`）。したがって request timeout は descendant の寿命上限ではない。
- timeoutは任意`u64`で、fresh pathはchild spawn後、warm pathはscript dispatch後に`Instant + timeout`を計算する（`crates/hearth-tools/src/bash.rs:290-324`、`crates/hearth-tools/src/shell.rs:398-413`）。対象macOSでは`u64::MAX`加算panicを再現できなかったが、platform表現範囲を超えるとpanicし、実行開始済みcommandがtimeout管理外になる可能性がある。dispatch前validationと`checked_add`が必要である。
- `env_clear`はfresh pathでのみ適用され、warm-shell modeでは仕様上無視される（`crates/hearth-proto/src/lib.rs:445-449`、`crates/hearth-tools/src/bash.rs:88-99,283-288`）。秘密を含むdaemon環境を除去したつもりのclient commandへ継承するconfidentiality riskがある。
- `Grep` content/context result、`EditBatch` の diff/full original/full result、graph traversal/basis は request ごとの総出力上限がない。
- grep matcher cache は unique regex/glob ごとに無制限に増える（`crates/hearth-tools/src/grep.rs:277-320`）。`ClearCaches` は file/walk/graph state だけを消し、この matcher cache や watcher root は解放しない（`crates/hearth-tools/src/lib.rs:75-80`）。したがって protocol 上の回復手段がない。
- walk cache は root ごとの file list を無制限に保持する（`crates/hearth-core/src/cache/walk.rs:35-103`）。
- `--watch` 時、任意 root が watcher の path state と OS watch resource を増やし得る（`crates/hearth-core/src/engine.rs:225-247`、`crates/hearth-core/src/watch.rs:512-533`）。追加済みrootを削除する protocol/API はない。
- graph は root 数を通常16に制限するが、root ごとの byte budget はなく、busy root では一時 overshoot も許す（`crates/hearth-tools/src/graph.rs:39-43,145-183`）。
- file cache の eviction は optimizer の周期処理であり瞬間 overshoot が可能。`--no-optimizer` では byte/entry cap の enforcement 自体が停止する（`crates/hearth-core/src/engine.rs:128-158,410-463`）。
- concurrent `Bash` 数は warm pool の free capacity と無関係に無制限である。

**影響**: RSS/CPU/disk I/O/process/watch descriptor exhaustion、他 client の latency 悪化、OOM abort。

**対策**: operation semaphore、最大 command timeout、最大 stdout/stderr/result/diff/matches/files、cache ごとの LRU byte budget、watch root 上限、graph aggregate budget、optimizer 無効時も働く hard cap、client 切断連動 cancellation。

### HD-06 — CLI が偽 daemon を検証せず stdout FD を渡す

**Severity: High（unsafe endpoint で conditional）**
**状態: 確認済み**

CLI は path に接続できれば server identity を確認せず request を送り、通常の `read` では stdout FD も `SCM_RIGHTS` で渡す（`crates/hearth-cli/src/main.rs:368-395`）。

**影響**:

- command、path、write/edit 内容の窃取。
- forged response/output。
- 偽 server が stdout FD を CLI 終了後も保持し、後から書き込む。redirect 先や terminal への出力 integrity が失われる。
- daemon impersonation による操作結果の偽装。

渡された FD の access mode を超える権限は得られないため、常に任意 read/write 権限が増えるわけではない。

**対策**: private runtime directoryとserver peer UID検証はdifferent-UIDを、認証付きworkspace/instance handshakeは誤接続を防ぐ。未認証のhandshake/instance identityはsame-UIDの偽daemonを防がない。same-UID防御にはアクセス不能なbroker secret、継承済みchannel、別service UID、OS sandboxのいずれかを必須とする。stdout FD fast pathはこの認証後にのみ許可し、必要なら明示opt-inにする。

### HD-07 — transport 失敗後に mutating request を inline 再実行する

**Severity: High（`Bash` と非冪等 edit）**
**状態: 確認済み**

CLI は connect 後に request を送信しても、response の write/read/decode が失敗すると同じ request を inline engine で実行する（`crates/hearth-cli/src/main.rs:378-401`）。daemon が operation を完了した後に接続が切れた場合、`Bash`、`Edit`、`EditBatch` 等が二重実行され得る。

warm shell 内部は at-most-once を意識しているが、この end-to-end fallback はその保証を破る。protocol version 不一致で response decode に失敗する場合も同様である。

**対策**:

- request送信後は自動fallbackしない。mutating operationは二重作用し、stdout FDを使う`Read`もdaemonが一部/全部を書いた後のinline fallbackで重複・混在出力を生み得る。
- request ID、daemon-side deduplication、acknowledged dispatch stateを導入する。
- operation種別にかかわらず「daemonへ1 byteも到達していないことが証明できる場合」だけfallbackする。

### HD-08 — atomic write の一時ファイルが予測可能かつ安全に生成されない

**Severity: High（attacker-writable target directory で conditional）**
**状態: 確認済み**

一時名は `.{name}.hearth.{pid}.{seq}.tmp` で予測可能であり、`File::create` を使う（`crates/hearth-tools/src/util.rs:97-135`）。`O_EXCL`、`O_NOFOLLOW`、random nonce、dirfd 固定がない。

**攻撃/不具合**:

- attacker が temp path に symlink を置くと、daemon はその target を truncate/write し得る。
- 既存 private file の mode を temp にコピーする前、temp は process umask 由来の mode で可視になる。親 directory を別 UID が探索でき、umask が `022` 等なら秘密内容が一時的に読める可能性がある。
- parent directory の差し替えや symlink race で、確認時と commit 時の対象が変わり得る。
- 既存targetのpermission copyで`set_permissions`の失敗を無視する（同: `125-129`）。失敗時はumask由来modeのtempがそのままtargetになり得る。
- `flush`のみでfile/directory `fsync`がなく、atomic visibilityはあるがcrash durabilityは保証しない。これはattack resistanceとは別のdurability契約である。

**セキュリティ対策**: owner-only mode `0600` でrandomなsibling tempを`O_CREAT|O_EXCL|O_NOFOLLOW`生成し、dirfd-relative operation、target/parent再検証、permission設定失敗のfatal化、inode確認付きcleanupを行う。

**durability対策**: 必要なdurability levelを明示し、契約に応じてfile `fsync`、rename、directory `fsync`を適用する。性能影響があるためsecurity修正とは分離する。

### HD-09 — workspace/cwd の binding がなく、per-UID default socket が workspace を混同する

**Severity: Medium–High**
**状態: 確認済み**

- default socket は UID ごとに一つで workspace identity を含まない（`crates/hearth-tools/src/transport.rs:20-31`）。
- CLI の read/write/edit/grep path と bash は client cwd を protocol に固定せず、relative path/cwd は daemon の `default_cwd` で解決される（`crates/hearth-tools/src/util.rs:13-20`、`crates/hearth-tools/src/bash.rs:74-88`）。
- `--cwd` は default であり boundary ではない。

repo A 用 daemon が動く状態で repo B から同じ default socket を使うと、relative operation が A に作用し得る。malicious client は absolute path や `..` で任意場所を指定できる。

**対策**: workspaceごとのendpoint/instance identity、client cwdの明示、root外operationの拒否。path認可は開いたworkspace dirfdを起点にcomponentごとに行い、FDに対して検証・I/O・renameする。canonical root handshakeは誤接続検知には使えるが、認証されなければsame-UID security boundaryにはならない。rootが異なる場合はfail closedし、暗黙fallback/接続を避ける。

### HD-10 — symlink/TOCTOU と `follow_symlinks=false` の不一致

**Severity: Medium（privileged daemon または attacker-writable tree では High）**
**状態: 一部確認済み、一部 race 条件依存**

- symlink は `symlink_metadata` と `canonicalize/read_link` で解決した後に別 syscall で書くため race window がある（`crates/hearth-tools/src/util.rs:29-55`）。
- mutation lock は process 内の Hearth writer だけを直列化し、外部 attacker の rename/symlink swap は防がない（`crates/hearth-core/src/pathlock.rs:24-45`）。
- `WriteMode::InPlace` は `OpenOptions::open` を使う（`crates/hearth-tools/src/util.rs:148-174`）。`follow_symlinks=false` で元 path を返しても in-place open は OS 上 symlink を follow するため、「link を置換する」という契約にならない。Atomic mode とは挙動が異なる。
- read/cache は metadata と read が別 syscall で、取得 bytes と記録 metadata が異なる版になる race がある（`crates/hearth-core/src/cache/file.rs:234-283`）。

**対策**: workspace dirfd から `openat`/`openat2` 相当で component policy を適用し、`O_NOFOLLOW`、inode 再検証、FD に対する I/O と rename を使用する。InPlace + no-follow の contract test を追加する。

### HD-11 — client disconnect が operation cancellation にならない

**Severity: Medium–High**
**状態: 確認済み**

Daemon dispatch は non-cancellable API を使用する（`crates/hearth-tools/src/lib.rs:43-87`）。client が切断しても bash/grep/graph/edit は完了まで動き、response write 時に初めて切断を知る。`Bash` timeout は caller が非常に大きな値へ上書きできる。

**影響**: abandoned work、child process、CPU/memory/disk I/O の継続。攻撃者は request 送信直後に切断し、connection FD を保持せず負荷を残せる。

**対策**: per-request cancel token、socket hangup monitor、operation deadline、disconnect 時の process-group kill/reap。graph/grep workerも join してから resource を解放する。

### HD-12 — `Shutdown` は graceful ではなく、child cleanup を保証しない

**Severity: Medium**
**状態: 確認済み**

`Shutdown` は handler thread から `std::process::exit(0)` を呼ぶ（`crates/hearth-daemon/src/main.rs:114-121`）。Rust destructor は実行されないため、Engine drop に依存する warm shell cleanup（`crates/hearth-tools/src/shell.rs:118-125`）は走らない。他 connection の実行中 child も drain/cancel/reap されない。

**影響**: orphan shell/command、途中の mutation、socket cleanup race、response 後ただちに全 connection が切断。protocol comment の「gracefully」と一致しない。

**対策**: main loop へ shutdown signal を送り、listener close → new request 停止 → active request cancel/drain → child kill/reap → cache/shell drop → inode-checked socket cleanup の順に終了する。SIGTERM/SIGINT も同じ path を通す。

### HD-13 — socket unlink/cleanup が pathname の再利用を考慮しない

**Severity: Medium**
**状態: 確認済み**

起動時と shutdown 時に pathname をそのまま unlink する（`crates/hearth-daemon/src/main.rs:59-60,119-121`）。二重起動、外部 unlink/rebind、custom writable parent で、現在の listener と異なる socket/fileを削除し得る。

**対策**: active daemon probe/lock、bind 後 inode/device 記録、cleanup 時 `lstat` 一致確認、RAII cleanup guard、unsafe existing file は削除せず明示 error。

### HD-14 — SCM_RIGHTS/framing の異常系が十分に harden されていない

**Severity: High（FD継承） / Medium（その他の availability/integrity）**
**状態: 確認済み。ただし memory-safety exploit は未確認**

- `recv_request` の ancillary buffer は FD 1個分で、`MSG_CTRUNC` を検査しない（`crates/hearth-tools/src/transport.rs:104-136`）。余剰 FD の OS ごとの処理を明示的に検証していない。
- `recvmsg` は `MsgFlags::empty()` で、受信FDに `FD_CLOEXEC` を設定しない（同: `104-127`）。非Read requestに付けられたFDも拒否せず `dispatch` 中まで保持するため、`Bash` がspawnするshellへ継承され得る（`crates/hearth-daemon/src/main.rs:104-116`、`crates/hearth-tools/src/bash.rs:269-295`）。現行の悪意あるclientは既に任意bashを実行できるので追加の権限昇格とは限らないが、FD capabilityと寿命の意図しない拡大になる。
- `send_request_with_fd` は一回の `sendmsg` が frame 全体を送ると仮定し、返された byte 数を確認しない（同: `71-98`）。Unix stream の short write は API 上可能である。
- `recv_request` は一回の `recvmsg` が次 frame まで over-read した場合、余剰 bytes を保持しない。現在の lockstep client は回避するが parser 自体は一般的な stream framing になっていない。
- Read+FD の write が `0` を返すと残 bytes があっても success 扱いし、metadata は全長を書いたように返す（`crates/hearth-daemon/src/main.rs:141-156`）。
- FD の型、blocking 性、出力上限を検査しない。

**対策**: buffered frame decoder、short send/write loop、`MSG_CTRUNC`/unexpected cmsg 拒否、FD数とrequest kindの厳密対応、受信直後の`FD_CLOEXEC`、deadline/nonblocking strategy、actual bytesの検証。

### HD-15 — `trust_cache` と `Bash` mutation がcache整合性を失わせる

**Severity: Medium（`--trust-cache` 使用時）**
**状態: 仕様上確認済み**

`trust_cache` は warm hit の `stat` を省略する（`crates/hearth-core/src/engine.rs:36-45,252-256`）。Hearth外の書換えだけでなく、daemonから到達可能な`Bash`も実行後にcwdをinvalidateしないため（`crates/hearth-tools/src/bash.rs:62-113`）、同じEngineのread/grep/graphがstale stateを返し得る。通常modeでもBashが作成・削除したfileはcached walkに反映されない場合がある。

**対策**: `Bash`成功・timeout・indeterminate後にcanonical cwd配下を保守的にinvalidateするか、Bashと`trust_cache`の併用を拒否する。multi-writer workspaceでは`trust_cache`を無効化し、security-sensitive readは常にFD/metadataを再検証する。

### HD-16 — request 値による handler panic と panic-safe でない coordination

**Severity: Low–Medium（hardening。debug handler panicは再現済み）**
**状態: handler panic は再現済み。single-flight wedge はコード上確認済みだが leader panic の到達入力は未実証**

`Read` の line window は `start_line + n - 1` を通常加算しており（`crates/hearth-tools/src/read.rs:103-108`）、たとえば `offset=2, limit=u64::MAX` で debug build の handler が整数 overflow panic することを確認した。daemon の connection thread 自体は通常 process 全体を即終了させないが、response は返らず、request値がpanic境界を越えている。

加えて file cache の `SingleFlight::run` は leader が loader から値を返すことを前提にする。loader がpanicすると result publish、notify、map remove が実行されず、同じ key の follower は Condvar で永久待機し、その key は以後 wedged する（`crates/hearth-core/src/singleflight.rs:39-69`）。現在の file loader は主に `std::fs::read` とallocationで、通常I/O errorは `Result` に変換されるため、この leader panic をnetwork requestだけで確実に起こす経路は確認していない。

**対策**: 全 request 算術を `checked_*` / `saturating_*` と validation へ変更する。handler top-level をpanic isolationし、single-flightには leader cleanup guard、`catch_unwind` によるerror publish、全pathのnotify/removeを実装する。境界値property testとpanic injection testを追加する。

### HD-17 — security observability と dependency advisory gate がない

**Severity: Low–Medium**
**状態: 確認済み**

- 接続 peer、拒否、request 種別、resource limit、shutdown caller を記録する security audit log/metric がない。一方、認可なしの `Stats` はprocess-global profilerとcache状態を返すため（`crates/hearth-core/src/engine.rs:347-366`、`crates/hearth-core/src/profiler/mod.rs:21-31,62-100`）、低信頼client間ではworkload/size/timingのside-channelにもなる。
- dependency は `Cargo.lock` と exact version で固定され、Dependabot は weekly で設定されている（`.github/dependabot.yml`）。GitHub Actions も commit SHA 固定、CI 権限は least privilege である（`.github/workflows/ci.yml`）。
- 一方、CI に RustSec `cargo audit` / `cargo deny` 等の advisory gate はない。監査環境にも両 tool は未導入だったため、既知 CVE がないことは本監査では確認できていない。

**対策**: advisory DB を固定/更新する CI、license/source policy、SBOM、release artifact provenance、security event metric を追加する。

## 4. 確認できた既存の緩和策

| 緩和策 | Evidence | 残余リスク |
|---|---|---|
| inbound frame cap | `transport.rs:34-61,143-149` | 256 MiB は大きく、並列/response cap はない |
| 余分に解析できた FD は `OwnedFd` drop | `transport.rs:120-133` | `MSG_CTRUNC` 未検査、buffer は1 FD分 |
| bash default timeout、process group kill | `bash.rs:74-92,310-377,425-429` | caller が timeout を拡大可能、並列/出力 cap と disconnect cancel はない |
| file cache byte/entry LRU | `engine.rs:410-463` | 周期 enforcement、瞬間 overshoot、optimizer off で停止 |
| graph 1 file 2 MiB、root 16件 | `graph.rs:39-43,145-183` | root ごとの総メモリ/総 file 数は無制限 |
| invalidation log 4096件 | `invalidation.rs:9-92` | availability/security boundary ではない |
| process 内 mutation lock | `pathlock.rs:24-110` | 外部 writer と filesystem race は防がない |
| warm shell free pool 2–8 | `shell.rs:81-95,361-370` | concurrent active shell 数は無制限 |
| exact dependency pins / Dependabot / pinned Actions | `Cargo.toml`, `.github/dependabot.yml`, `.github/workflows/ci.yml` | advisory gate は未導入 |

## 5. Unsafe / serialization / native dependency 所見

- daemon の direct FD write、`OwnedFd::from_raw_fd`、UTF-8 unchecked fast path、shell pipe/process-group syscall を確認した。通常 path で即時の Rust memory-safety violation は確認できなかった。
- `SCM_RIGHTS` の ownership は受信 FD を `OwnedFd` 化しており、解析できた FD は drop される。ただし HD-14 の truncation/short-write test が必要である。
- `rmp-serde` decode は strongly typed `Request` に入るため、任意 type confusion は確認していない。allocation/recursion/fuzz 耐性は未検証である。
- tree-sitter grammar と resolver は untrusted repository content を処理する native-heavy dependency surface である。graph には 2 MiB/file cap と symbol cap があるが、parser dependency の脆弱性は advisory scan/fuzz/sanitizer の対象にすべきである。
- 本監査では destructive stress、multi-user OS matrix、fuzzer、ASan/LSan/Miri、RustSec advisory DB 照合を実施していない。

## 6. セキュリティテスト行列

### P0: endpoint / authorization

- private runtime dir が owner、mode `0700` でない場合は起動拒否。
- socket mode `0600` を Linux/macOS で確認。
- foreign-owned socket、regular file、symlink、FIFO を stale socket として削除しない。
- live daemon がいる状態の二重起動は既存 endpoint を壊さず拒否。
- serverは異なるUID peerを拒否し、CLIは異なるUIDの偽serverを拒否。別途、same-UIDの偽serverを防ぐ構成ではbroker secretまたは継承済みchannelを検証。
- shutdown/control/read/write/bash の capability matrix を全組合せで検証。
- root起動を無条件に拒否する。

### P0: resource exhaustion

- silent connectionと完全header後のpartial bodyがdeadline後に解放される。1–3 byteに分割された正当なheaderはbufferして継続受信するか、仕様として即時拒否することを明示する。
- `MAX_FRAME-1`、`MAX_FRAME`、`MAX_FRAME+1`、zero frame、malformed MessagePack、deep nesting。
- 全 `u64`/`u32` parameter の `0`、`MAX`、加算境界をproperty testし、requestがhandlerをpanicさせないことを保証。
- N個の half-open connection でも thread/FD/RSS が hard limit 内。
- 満杯 pipe FD、読まれない response socket、closed pipe、regular file、terminal FD。
- infinite stdout/stderr、巨大 single line、改行密度の高い巨大 read、巨大 grep context、巨大 edit diff、graph full basis が output/cache cap で停止。
- command timeoutをdispatch前に最大値clampし、`checked_add`を使用。fresh/warm両経路の`u64::MAX`を検証。
- warm modeの`env_clear`でdaemon環境のsentinel secretがcommandから見えない。
- 同時bash/grep/graph semaphore、disconnect・timeout・正常shell終了のすべてでbackground descendantとreader threadが消滅。
- unique regex/glob/root/files/watch request を大量投入しても全 cache が byte budget 内。
- `ClearCaches` または管理APIで matcher/watcherを含む全resident stateが既知のbaselineへ戻る。
- optimizer disabled 時も hard cap が機能する。

### P0: request replay / output integrity

- response欠落、response decode失敗、protocol version mismatch後にmutationを二重実行しない。
- streamed `Read`でresponse欠落・decode失敗後もstdoutへ重複・混在出力しない。
- daemonへ1 byteも届かなかったことを証明できる場合だけfallbackする。

### P0: filesystem

- 予測 temp 名への symlink/hardlink pre-creation が失敗し、第三の file を変更しない。
- private target rewrite 中、temp file が他 UID から読める瞬間がない。
- parent directory rename/symlink swap、target symlink swap、dangling/multi-hop symlink。
- `Atomic` / `InPlace` × `follow_symlinks` true/false の全4通り。
- absolute path、`..`、symlink、bind mount 等による workspace escape を拒否。
- rename 前後の inode/owner/mode/xattr/hardlink semantics を契約化。

### P1: protocol / lifecycle

- FDなしRead、非ReadへのFD、複数FD、ancillary truncation、unexpected cmsg。受信FDがspawn/exec先に存在しないことも検証。
- injected short `sendmsg`/write と、複数 frame を一度に送る stream。
- response 上限超過時に serialization 前後で bounded failure。
- old/new protocol field、unknown operationの互換性と、version negotiation成功時の挙動。
- graceful shutdown が listener を閉じ、active operation を cancel/drainし、全 shell/child を reap してから socket を消す。
- SIGINT/SIGTERM/crash recovery と、pathname が別 inode に置換された場合の cleanup。

### P1: fuzz / sanitizer

- `read_msg` / `recv_request` の length+MessagePack stateful fuzz。
- ancillary data と frame boundary を組み合わせた Unix socket pair fuzz。
- `Request` parameter validation、path policy、edit normalization/diff、grep/graph parser corpus fuzz。
- Linux/macOS の ASan/LSan、可能な pure Rust 部分の Miri、long-running FD/thread/process leak test。

## 7. 推奨実装順序

### Phase 0: 緊急の出荷ゲート

- request送信を試みた後の自動fallbackを止める。完全なat-most-onceは後続のrequest ID/dedupで補う。
- root起動を拒否し、「同一UIDの全clientを信頼する」「別権限service/multi-tenantで使わない」をREADMEへ明記する。
- unsafe existing socketを削除せずfail closedするsafe stale-path処理を入れる。
- secure temp生成（`0600`、random、exclusive、nofollow）とpermission error処理を入れる。
- `SCM_RIGHTS`をCLOEXECで受信し、request種別とFD数を厳密検証する。
- 完了条件: P0 endpoint/authorization、request replay/output integrity、filesystem試験をすべて通す。

### Phase 1: endpoint と bounded server

- private runtime directory、socket/peer credential、client-side verification。これらは主にdifferent-UID対策である。
- bounded worker pool、connection/frame/I/O timeout、aggregate in-flight budget。
- bash/graph/grep concurrency、timeout、output cap、disconnect cancellation。
- file/matcher/walk/watch/graph stateのhard byte/count ceilingをoptimizer非依存で同期的に強制する。
- 完了条件: P0 resource exhaustion試験をすべて通す。

### Phase 2: authority とpath認可の縮小

- read-only daemon を default にし、mutation/bash/control は opt-in capability とする。socket分割だけではsame-UID境界にならない。
- workspace-scoped instanceと、workspace dirfd起点のcomponent traversal/path policyを導入する。
- daemon credential/environment sanitization、OS sandbox、必要なら別service UIDを使う。
- 完了条件: capability matrix、same-UID/different-UID境界、workspace escape試験を通す。

### Phase 3: 完全なprotocol/filesystem/lifecycle保証

- end-to-end request ID、acknowledgement、daemon-side dedupによるat-most-once。
- dirfd-relative filesystem APIと、必要なdurability levelに応じたfsync契約。
- graceful lifecycle と child registry。
- matcher/watcher/warm shellを含むresident state reset API。hard ceiling自体はPhase 1で完了させる。
- 完了条件: P1 protocol/lifecycle試験を通す。

### Phase 4: 継続保証

- P1 fuzz/sanitizer、RustSec advisory gate、security metrics、長時間のleak/stress監視。

## 8. 監査時の検証結果

次を対象 commit 上で実行した。test と Clippy は成功し、境界値試験ではpanicを再現した。

```text
cargo test -p hearth-daemon -p hearth-tools --no-fail-fast
  hearth-daemon unit tests: 0
  hearth-tools unit/integration/contract tests: 148 passed

cargo clippy -p hearth-daemon -p hearth-tools --all-targets -- -D warnings
  passed

cargo run -q -p hearth-cli -- --no-daemon read Cargo.toml \
  --offset 2 --limit 18446744073709551615
  debug buildでread.rs:107のinteger-overflow panicを再現
```

既存の tool contract test は file/cache/bash/grep/graph の機能・並行性を広く検証している。一方、`hearth-daemon/src/main.rs` と `hearth-tools/src/transport.rs` には専用 test module がなく、socket permission、peer credential、malformed/partial frame、FD 異常系、unauthorized shutdown、connection/resource ceiling、fake daemon、end-to-end replay は未検証である。

## 9. 最終判定

- **単一ユーザー・全 same-UID process trusted・非特権・善意 workload**: 条件付きで利用可能。ただし accidental DoS、cross-workspace、二重実行、socket lifecycle の改善が必要。
- **別 UID/低信頼 process から endpoint 到達可能**: 利用不可。任意 command/file/control capability へ直結する。
- **same-UID compromised process も防御対象**: 現行 Unix socket/DAC モデルでは利用不可。OS identity/capability の再設計が必要。
- **root/権限の強い daemon**: 利用不可。
- **multi-tenant/shared service**: 利用不可。
