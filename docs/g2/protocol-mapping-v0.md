# G2 · EchoMuse 链路 ↔ 网关协议 v0 映射草案（2026-11-07）

状态：**草案**（G2 设计交付，未实现）。上游事实来自 [取证文档](echomuse-protocol-forensics.md)（DOT@324d431 等）与 [实机观察](dot2d-observation-20261107.md)；北侧合同＝主仓 `docs/contracts-v0.md`（权威）＋本仓 `crates/gateway/src/protocol.rs`。协议 v0 冻结治理：字段变更走 contracts-v0.md 的版本流程（预计触发 **v0.2**，见 §5 缺口）。

## 0. 架构位形

网关内新增 **dot2 适配器**（厂商协议归属网关，符合 device-bridge-boundary §2）：

```
dot2-d (EchoMuse 固件, 外拨 WSS 客户端)
   │  wss://gw:9014  /control (JSON) ＋ /data (二进制)   ← 网关＝控制器角色（服务端）
   ▼
gateway :: dot2-adapter（重采样 16k→48k、响度/限幅〔可选先直通〕、帧切块、音量映射）
   │  TCP LDJSON 协议 v0（网关为客户端，外拨核心 :8932）
   ▼
hm-core（DeviceHub/语音管线/ASR/TTS/任务）
```

南向状态机归属适配器：register→ack→(config)→mic_start 的握手序、双平面连接配对（control 在而 data 未到＝不可播放）、重拨周期（设备 60s）期间的「设备离线」表示。北向只上报事实事件，不把南向细节漏给核心。

## 1. 帧级映射表（EchoMuse → 协议 v0）

| # | EchoMuse（南向） | 协议 v0（北向） | 映射说明 | 缺口 |
|---|---|---|---|---|
| 1 | `register{device_id,version,capabilities,ip,base_os}` | `Hello{stable_id,model,firmware,capabilities,audio_profile}`（网关聚合后发） | stable_id＝device_id（`dot2-d`）；model＝`AEOBC`；firmware＝version＋slot；audio_profile＝`fireos_builtin_speaker`；能力映射见 §2 | ip/base_os 无处放→detail/日志级 |
| 2 | `ack{features}`（南向握手应答） | `HelloAck{protocol,generation}`（北向，网关↔核心层） | 两层握手解耦：南向 register↔ack 在适配器内闭环；北向 hello↔hello_ack 与设备无关 | — |
| 3 | `/control`＋`/data` 配对与断线重拨 | DeviceHub 连接代（generation） | **北向连接不受南向断线影响**（网关↔核心保持，设备掉线只是设备事件） | v0 无 per-device 在线状态帧 → G6 |
| 4 | mic `0x01` 块 16k mono / 2ch[mic\|ref] 80ms | — | **v0 无 audio_in 通道**（Utterance 只收文本） | **G1（首要）：v0.2 增上行音频帧＋mic 会话控制** |
| 5 | 轮次哨兵 0x04/0x05（仅 lock_mic 轮次） | —（HM 首期连续流＋核心侧分段） | lock_mic:false 时不产生哨兵；若后续用受限轮次再映射 | 随 G1 一并 |
| 6 | `oww_wake{…}` | `Wake{device,ts,seq}` | 直接映射；ts 用控制器收帧时刻 | — |
| 7 | `oww_shadow_cross` | —（诊断计数） | 影子交叉不是唤醒，只进网关统计/日志 | 可选 v0.2 diagnostics |
| 8 | `button{clickType,muted}` | `Button{device,ts,seq,action}` | action 映射：113＝action button；其他类型接入时实测补表 | — |
| 9 | `mute_state{muted}` | `MuteState{device,ts,seq,muted,source}` | source＝`device_button` | — |
| 10 | `volume_state{level 0-127}` | `VolumeReceipt{effective,unit:linear,stage:device_confirmed}` | linear＝level/127；**raw level 入 detail**（0.5dB 刻度是网关侧知识，不给核心） | — |
| 11 | ← `VolumeSet{value 0..1 linear}` → `volume_set{level}` | 同上反向 | level＝round(value×127)；部署上限（HP 档/儿童房天花板）在**网关配置**钳制 | — |
| 12 | ← `Play{audio_b64 WAV,channel}` → `0x02` 帧流＋`0x03` | 适配器解 WAV→PCM、**重采样→48k**、按 4096B 整帧切块、结束发 EOS | TTS 输出采样率需实测（volc seed-tts-2.0）；16k→48k 重采样在适配器（协议取证 §3 决策 3：控制器负责重采样） | TTS 实际采样率 → G4 |
| 13 | `playback_stats{…bytesRecv…}` | `PlayFinished{played_ms}` | played_ms≈bytesRecv/(48k×2B)×1000；underruns/minDepth 入 detail（回执质量证据） | — |
| 14 | ← `Stop{flush:true}` → `speaker_flush` | flush 确认＝同连接带 id ping 等 pong＋playback_stats；`StopReceipt{stage}`：发出＝sent，pong/统计到＝device_confirmed，超时＝unknown | 与 M2「停止三态」合同一致 | — |
| 15 | `listen_state{state}` | —（诊断） | 输入通道观测，先只入网关日志 | 可选 v0.2 |
| 16 | `stats{aecRef: off\|hw\|sw}` | `ModeReport.aec{owner,reference}` | 现役布局＝hw 参考上行＋控制器 AEC → owner=`core`、reference=`available`（hw）；aecRef 实测值覆盖假设 | — |
| 17 | ← `ModeSet{duplex_requested}` | EchoMuse 传输层天然全双工（双平面独立、连续 mic＋下行 PCM 可并行）；半双工默认＝核心侧门控（播放中不收/打断停播），不是传输层限制 | 双工四层（capability/requested/negotiated/effective）在北向已建模；南向无需配置 | — |
| 18 | ← `config{oww*,aec*,agc*,bargeIn*,wakeMic…}` | v0 只有 ModeSet（双工） | 唤醒/AEC/AGC/打断阈值是设备侧旋钮，v0 无处表达 | **G2：v0.2 设备配置帧或网关侧默认值策略** |
| 19 | ← `mic_start/mic_stop` | 随 G1 上行音频会话帧一起设计 | 连续流默认 lock_mic:false | 随 G1 |
| 20 | ← `leds{…}` | v0 无 LED 帧 | 首期可不发 leds（设备保持固件默认）或按 §5 表做状态映射（纯网关行为，不涉核心） | 可选 v0.2 |
| 21 | music `0x04/0x05`（audio_mix 设备） | `Play{channel:media}` | dot2-d 是否宣告 audio_mix 未知；未宣告则音乐走 voice 通道 48k PCM（正确但单通道） | 实测 → G4 |
| 22 | `log{level,message}` | —（网关日志转发） | — | — |
| 23 | `refused`／close 1002（token 不符） | 网关侧连接拒绝计数＋诊断 | — | — |

## 2. 能力映射（register.capabilities → Hello.capabilities）

EchoMuse capabilities 字段实测值待接入时确认（观察快照 §5）。按取证事实预期映射：

| EchoMuse 事实 | v0 能力名 |
|---|---|
| mic 上行（16k 连续流） | `audio_in` |
| speaker 0x02/0x03 下行 | `audio_out`、`stop` |
| oww_wake / 影子 | `wake_event`（oww on-device 时） |
| volume_set/state | `volume` |
| 按键/静音 | （v0 Button/MuteState 事件，无需能力名） |

dot2-d 不具备：`text_in`（无屏无键盘）、图像类（v0 亦未定义）。**适配器不得伪造设备未宣告的能力**（合同红线）。

## 3. 音频路径与声学职责（对齐 device-bridge-boundary §4）

- **上行**：`dot2_hw_ref` 模式 2ch[mic|ref] 16k 直通北向（v0.2 音频帧带通道标记）；**AEC 所有者＝核心**（沿用现役「设备 AEC 关、控制器 AEC 开」布局，hw 参考＝ch8）。适配器不跑 AEC。
- **下行**：核心 TTS WAV → 适配器重采样 48k →（可选）响度 FIR＋限幅＋10ms 淡入淡出（DOT dsp.py 知识；**首版先直通**，响度按房间 profile 二期）→ 4096B 整帧＋EOS。
- **首帧延迟预算**：设备 prime ~1.1s＋句间重 prime 30–800ms（EOS 后新开流）；语音应答链路延迟以实测入账（G4 验收项）。
- **seq 头不是音频**：适配器解析 0x01 时必须剥 `[seq u16 BE]`（取证 §2.1 头注释的致命陷阱）。

## 4. 接入路径（两条，均已被用户授权范围覆盖）

### Path A（首选，G3）：保留 EchoMuse 固件，网关实现控制器角色

1. 网关新增 WSS 服务端（监听如 :9015，避免与本机他服务冲突；`/control`＋`/data`＋`X-EM-Token`＋TLS）。
2. TLS 信任：设备 `ca.pem` 现信任 RUI CA。三选一：**(a) 生成网关自签 CA＋重写设备 ca.pem**（纯本机写，已授权，备份现成模式）；(b) 复用 RUI 生产 CA/证书（涉旧系统读取——**需用户点头**）；(c) RUI 禁用状态下设备反正拒连，先插明文端口验证协议（**不可行**：设备只认 wss）。→ 取 (a)。
3. token：从设备读现值（adb root，授权内）或网关侧换新 token 并写设备（同 (a) 模式）。**不进 Git**。
4. `controller.json` 重指网关端点（备份 `.before-hm-20261107` 模式）；等待 ≤60s 重拨或重启 server 进程。
5. 回滚＝恢复 controller.json/ca.pem/token 三文件＋RUI API `enabled:true`（RUI 侧记录在案，rui-local@db2038d）。

**前置依赖**：缺口 G1（v0.2 上行音频）必须先落地，否则语音闭环缺 mic 侧。

### Path B（后置，用户方向）：服务上音箱本体

观察快照 §4.3 的可行性证据成立；依赖：Rust armv7-androideabi 交叉（cargo-zigbuild 路线复用）、ALSA/input 直占、A/B 槽监督脚本可整套复用或替换。属独立里程碑（建议 G5「上机研究」），**本轮只记录证据不做**。

## 5. 缺口清单（编号供引用；G3 前需决策/实现）

| # | 缺口 | 性质 | 处置建议 |
|---|---|---|---|
| G1 | **v0 无上行音频通道**：Utterance 只有 text；EchoMuse 语音闭环必须传 mic PCM | 协议演进（v0.2） | 设计 `audio_chunk`（gateway→core，inline base64 PCM＋meta/seq）＋`mic_session`（core→gateway start/stop）帧；走 contracts-v0.md 治理，主仓侧 DeviceHub/语音管线接 |
| G2 | 设备配置面（oww 模型/阈值、AEC/AGC/打断旋钮）v0 无表达 | 协议演进或网关策略 | 首版：网关内置保守默认（wakeWordEnabled 随 oww 决策、aecEnabled=false——沿用现役）；v0.2 再上 config 帧 |
| G3 | 唤醒词方案：HM 定「高达」；设备 oww 现模型 `hey_jarvis_v0.1` | 决策＋可能模型资产 | 两选：核心侧 KWS（连续流入核心，换词只改软件）／设备 oww 自定义模型（需训练资产，未评估）。首版建议核心侧 KWS（词表自由、与虚拟终端同路径），设备 oww 关 |
| G4 | TTS 输出采样率未实测（16k/24k?）→ 重采样档位未定 | 实测 | 接入前用现有 volc WAV 资产核对（主仓 asr 测试已有 39KB 样例可参照做法） |
| G5 | dot2-d `audio_mix` 能力未知（音乐通道） | 实测 | register.capabilities 接入时记录；未宣告则 music 走 voice 通道 |
| G6 | v0 无 per-device 在线/离线状态帧（设备 60s 重拨周期内核心无从得知） | 协议演进（v0.2 小项） | `device_status{device,online,reason}`；首版可先靠 Wake/回执超时兜底，但诊断页会瞎 |
| G7 | 响度/EQ（EQ_50…100、limiter、fade）未迁移 | 二期 | 首版直通；房间 profile（HP6/儿童房天花板）进网关配置模型 |
| G8 | LED 状态映射未接 | 可选 | 首版不发 leds；后按取证 §5 调色板映射（纯网关行为） |

## 6. 验收要点（G3 实现时逐项；此处只列标准）

1. 合同测试：握手（含 token 拒绝/首帧非 register 拒绝）、双平面配对、seq 剥头（2ch 布局不偏移）、哨兵、EOS/flush 三态、音量 0-127↔linear 往返、写停滞自愈（断 data 逼重拨）。
2. 实机：dot2-d register 字段实测、重连恢复（拔 controller.json 端点→恢复）、真实 mic 块到达北向、TTS 48k 实播、speaker_flush 后 0 残留（playback_stats periods 证据）。
3. 红线复验：一切设备写动作（ca.pem/token/controller.json）带备份＋回滚演练一次。
