# G2 · EchoMuse 设备线协议取证（2026-11-07）

用途：为网关实现 EchoMuse 控制器角色提供线协议事实。全部来自**只读**旧仓取证；引用均带提交号。证据等级：旧桥生产实现＋固件仓文档（现役事实，非本次实机重放验证——实机接入时逐项复测）。

## 0. 来源与提交号（取证时 HEAD，均干净工作树）

| 别名 | 仓库 | HEAD（2026-11-07 取证时） | 关键文件 |
|---|---|---|---|
| DOT | `01_Code/projects/rui-dot2-bridge` | `324d431`（与主仓迁移指南记录一致） | `docs/protocol-map.md`、`src/rui_dot2_bridge/frames.py`、`device_link.py`、`docs/findings.md` |
| FW | `01_Code/projects/echomuse-dot2-ref` | `178ba54`（＝基线记录的固件源提交） | `README.md`、`docs/ARRAY-20261105.md`（补丁仓；上游 EchoMuse v2.19.0-ea.1） |
| RUI | `01_Code/projects/rui-local` | `db2038d`（已前进；HANDOFF 记录的 `c4042f4` 为旧值） | `docs/HANDOFF.md`（dot2-d 禁用记录） |

上游 Go 源引用（`device/internal/client/data.go` 的 `sendFrame`、`vadOwwChunkBytes`）经 DOT `docs/findings.md` 转引，未直接读上游仓。

## 1. 传输与建链

- 设备是**纯客户端**：主动外拨 `wss://<controller>:<port>`（实机观察 2026-11-07：`Connecting to wss://192.168.1.10:9014`，refused 后 60s 退避重试，每端点 2 次/轮）。**仅 TLS**，无明文回退（DOT frames/错误表：证书缺失＝启动失败，不静默降级）。
- **两个 WebSocket 平面**：`/control`（JSON 文本）＋`/data`（二进制）。两平面独立连接；数据面断开重拨不重走 register（device_link.py:1328 注释）。
- **鉴权**：HTTP 头 `X-EM-Token`（两平面都查，device_link.py:927）；不匹配→`{"type":"refused"}`／close 1002。隔离开发开关 `DOT2_ALLOW_UNAUTHENTICATED=1`（不绕过已配置 token）。设备侧 token 为 64B 文件、ca.pem 554B（实机观察 §2.4）。
- **握手**（`/control`）：设备首帧必须 `{"type":"register","device_id","version","capabilities","ip","base_os"}`（10s 握手超时）；控制器回 `{"type":"ack","device_id","features":[…],"time_ms"}`。非 register 首帧＝拒（1002）。`ack.features` 是能力协商载体：现役特性 `dot2_hw_ref`（上行 2ch mic＋硬件参考，device_link.py:1470-1475）。
- **保活**：控制器每 15s `{"type":"ping","id"}`（PING_INTERVAL_S=15；keepalive 20s ping/40s 超时）；`/data` 静默看门狗 60s（`DOT2_DATA_SILENCE_S` 可调）。写超时：control 3s / data 1s；data 写停滞→close 1011 逼设备重拨；被替换旧套接字的迟到关闭按身份检查忽略。

## 2. 数据面帧格式（frames.py＝权威汇总，findings.md 带源码定位）

### 2.1 上行（设备→控制器）

| 帧 | 格式 | 语义 |
|---|---|---|
| `0x01` mic | `[0x01][seq u16 **BE**][PCM]` | **seq 不是音频**（当 PCM 用会逐块移位；2ch 模式下通道错位——frames.py 头注释明示致命）。80ms/块：mono 2560B 或 2ch `[mic\|ref]` 5120B（S16LE 16kHz；2ch 需控制器 ack `dot2_hw_ref`） |
| `0x01` 1B 载荷 | `payload ∈ {0x04, 0x05}` | 受限轮次哨兵：0x04＝VAD 结束、0x05＝无语音超时（仅 `lock_mic:true` 门控轮次；连续流不发） |
| `0x04`/`0x05` 独立帧 | 同上 | 同语义（device_link.py:1361 一并按哨兵处理） |

- 原始采集 9ch/24bit **不出设备**：固件内波束/融合（低频七麦平均、高频中心麦，FW ARRAY-20261105）后上行 mono 或 mic+ch8 参考。
- 控制器侧有界缓冲：64 块×80ms ≈ 5s，溢出丢最旧（`AUDIO_QUEUE_CHUNKS`，device_link.py:54/1446）。

### 2.2 下行（控制器→设备）

| 帧 | 格式 | 语义 |
|---|---|---|
| `0x02` speaker | `[0x02][PCM S16LE mono 48kHz ≤4096B]` | 42.7ms/块；**必须整帧发送**（设备靠整块保持自身节拍，frames.py split_playback） |
| `0x03` EOS | `[0x03]` | 流结束；**下一块音频到来时设备重新开流并重新 prime（30–800ms）**＝听感卡顿（findings.md:69）——句间连续性由此而来 |
| `0x04`/`0x05` music | 同构 | 仅 `audio_mix` 能力设备；首期不实现（protocol-map「不需要实现」清单） |

- 设备播放前 prime ~1.1s 缓冲（frames.py:87）。
- **停止＝`speaker_flush`（control 平面 JSON）**；flush 确认法＝同连接带 id ping，等 pong＋`playback_stats`（findings.md:105-110 实测：406/407/417ms 内 periods=4）。
- 播放结束遥测：`{"type":"playback_stats","stats":{periods,underruns,minDepth,primeWaitMs,recvSpanMs,maxGapMs,bytesRecv}}`（标识一条完成的流，非特定语音轮）。

## 3. 控制面消息全集（device_link.py 1109-1180 收发两侧）

**设备→控制器**：`register`、`pong{id}`、`playback_stats`、`volume_state{level}`、`mute_state{muted}`、`button{…,muted}`、`stats{…, aecRef: off|hw|sw}`、`log{level,message}`、`listen_state{state:"stream"|…}`、`oww_wake{…}`、`oww_shadow_cross{…}`（影子交叉＝设备在打分但未动作，非唤醒）。

**控制器→设备**：`ack`、`config`、`mic_start{lock_mic}`、`mic_stop`、`ping{id}`、`speaker_flush`、`volume_set{level}`、`leds{leds:[{id,r,g,b}],listening}`、`refused`。

`config` 可推送字段（device_link.py:1071-1099）：`deviceManagedVolume`、`startupVolume`、`owwOnDevice`、`owwModel`（缺省固件默认 `hey_jarvis_v0.1`——「碰巧正确」陷阱在注释中明示）、`owwThreshold`、`wakeWordEnabled`、`aecEnabled`、`micGainDb`、`aecTailMs`、`aecDelayMs`、`agcEnabled`、`bargeInEnabled`、`bargeInThreshold`、`wakeMic`、`aecRefSource`。

关键语义（protocol-map §三，生产决策）：

1. **连续流 vs 受限轮次**：默认 `mic_start lock_mic:false` 连续流（控制器侧 KWS 需要连续输入；设备 VAD 门控会让 KWS 只见句子片段）。设备自带唤醒设 off，避免双唤醒打架。
2. 帧大小：设备给 80ms，控制器内部再切 20ms（拷贝换可预测 KWS 延迟）。
3. 播放速率：**控制器负责重采样到 48k 再发**（`audio.playback.rate: 48000`），适配器内不做 DSP（响度/限幅在控制器侧，见 §4）。

## 4. 音量与响度链（volume.py/dsp.py@324d431）

- 设备音量级 0–127，codec 0.5dB/步，0＝-63.5dB；`volume_set{level}` / `volume_state{level}`。**设备自有持久化**（`178ba54`「persist and report device-owned speaker volume」；实机 state.json volume=114 吻合）。
- 控制器侧百分比＋部署上限（`DOT2_VOLUME_CAP` 硬顶）；上报高于上限时拉回（ceiling wins）。
- 控制器侧 DSP：按音量档 FIR 响度曲线（EQ_50…EQ_100 六档，EQ_50/60/70 字节相同）＋look-ahead limiter；10ms 淡入淡出@48k；`ladder_correct` 选项。
- 基线 HP 档位（dot2-d＝HP6）为硬件路径记录，映射到 HM 音量模型时按设备 profile 放网关配置。

## 5. LED 语义（protocol-map §五）

`{"type":"leds","leds":[…],"listening":bool}`；`listening` 规范绿 `#00A050` **独占**（保留色，覆盖值恰为它时回退默认）；wake `#787878`；thinking `#0028A0`；speaking `#006E96`；music `#64008C`；muted `#780000`；error `#AA0000`。仅映射状态，门控事实由控制器决定。

## 6. 现役生产配置事实（主仓 dot-device-baseline §3 快照＋本次对账）

16kHz 上行、mic=1/ref=1 双通道、48kHz 下行、AEC owner=controller（设备 Speex AEC 关）、continuous_enabled=false、speaker_gate=false。与 §2 帧格式一致（dot2_hw_ref 生效）。

## 7. 取证边界

- 以上为**旧桥生产实现与固件仓文档**记载的线协议；本次未在实机上重放验证（G2 只读）。网关实现后以 dot2-d 实机握手逐项复测（register 字段实测、audio_mix 有无、oww 现值）。
- `hermes-dot2-voice/docs/` 未在本轮重读（网关 HANDOFF §4 已登记为参考源）；如需 EchoMuse controller 端口/mDNS/adb 坑细节再定向查阅。
- 私人材料（录音/声纹/完整配置）未取用。
