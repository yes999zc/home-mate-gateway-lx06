# G5 · 服务上音箱——形态提案（2026-11-07 深夜，自主会话）

用户指令：形态先提案再动手（替换 EchoMuse 客户端 vs 共存；EchoMuse 只是 root 工具、可干掉，破坏性动作已预授权）。本文件＝实测证据＋提案＋本里程碑执行范围。

## 1. 设备实测事实（2026-11-07 夜，adb USB 存活时探得）

| 项 | 事实 | 证据 |
|---|---|---|
| CPU/ABI | **32 位 ARM**：`ro.product.cpu.abilist=armeabi-v7a,armeabi`；EchoMuse server ELF 头 `01 01 … 03 00 28 00`＝ELF32 + EM_ARM(40) + PIE | adb getprop + dd 头 20 字节 |
| 内存 | 482,956 kB 总量，298,468 kB 可用 | /proc/meminfo |
| 声卡 | ALSA `mtsndcard`（MediaTek）就位，/dev/snd 下 40+ PCM 设备（pcmC0D0p…pcmC0D25c，c/p 混布） | /proc/asound/cards + ls /dev/snd |
| 音频链初始化 | `start_server.sh` 用 tinymix：I2S1/HP DAC/MFP Gpio/PCM Playback 100；四 ADC（A/B/C/D）Digital 88 + MICPGA 40——**这些就是直占所需的 mixer 序列** | /data/local/bin/start_server.sh |
| LED | 启动脚本 `kill ledcontroller`（原厂 LED 服务被让位）；LED 由 EchoMuse server 按 `leds` 控制帧驱动 | 同上 |
| 监督 | start_server.sh＝A/B 槽监督：`server`→symlink→server_a/server_b；<15s 快退连 3 次自动回滚另一槽并退出交 Android init 重启；term 信号转发＋amp_off 兜底 | 同上 |
| 唤醒锁 | `echo EchoMuse > /sys/power/wake_lock` 防 WiFi 休眠——**任何自研组件必须同样持有** | 同上 |
| root | Magisk su -c 可用；`/data/adb/service.d/` 存在（空）＝Magisk 开机服务入口 | ls |
| AEC 资产 | `/data/local/etc/echomuse/aec_echo_path.v2.bin.mics`＝对方 AEC 的回声路径标定数据（对我们不可直接复用，但证明回声路径可测） | ls |

## 2. 形态选项

### A. 共存（G5-1，本里程碑执行）
**dot2 适配器（本仓 controller 角色）交叉编译为 armv7 静态二进制，跑进音箱本体**；EchoMuse 固件 client 不动，`controller.json` 指到 **127.0.0.1:9015**（环回）；适配器北向拨 Mac/NAS 核心。监督＝Magisk `service.d` 脚本（复刻 start_server.sh 的快退重试＋日志截断思想，单槽——回滚＝恢复 controller.json 指回 Mac 适配器）。

- 价值：算力与 TLS 终点上机；armv7 工具链、开机自启、7×24 稳定性首次实测；回滚演练简单（改回 controller.json）。
- 边界（如实）：音频仍由 EchoMuse 固件经 ALSA 直占——**不是**"替换"；音箱离了 Mac 核心（LLM/ASR/TTS 都在 API 服务）仍不能独立对话。
- 风险：低。EchoMuse client 环回拨号＝G3 已验证的拨号逻辑换地址；证书 SAN 已含 IP:127.0.0.1（G3 生成时写入）。

### B. 替换（G5-2，提案——后续里程碑，动手前与用户过一遍）
自研 Rust 音频组件直接 ALSA 直占（mic 采集＋speaker 播放），**干掉 EchoMuse server**（已预授权）。要点：
- 二进制路线同 G5-1（armv7 musl 静态；纯 Rust ALSA 绑定或 tinyalsa 风格 /dev/snd ioctl）。
- mixer 初始化序列照抄 §1 表（已到手）；speaker PCM 与 mic PCM 的具体设备号需只读探测（tinymix/tinypcminfo 逐个试）。
- 必须持有 wake_lock；启动时同样 kill ledcontroller（LED 归我们后其实无需 kill——LED 由我们自己的组件驱动）。
- AEC：无对方二进制后，回声路径需自测（hw_ref 2ch 采集在自研组件里天然可行，比 Path A 更直接）；v0.2 核心侧 AEC 评估见主仓 G4 记录。
- 回滚：EchoMuse 三件套（server_a/b＋start_server.sh＋init 服务）完整保留在 /data/local/bin，恢复＝停自研服务＋还原 init 指向。
- 风险：高（无人值守期静音风险、未知 PCM 拓扑、喇叭功放时序）；这正是提案先行、不由本里程碑夜航执行的原因。

### C. 全核上机（G5-3，远期评估）
hm-core 整体上音箱（SQLite+axum+reqwest musl 静态）。482MB RAM 对核心本体可行，但三服务仍在 API 云端，收益有限；等 B 稳定后再按 plan §三评估。

## 3. 本里程碑（G4 合并版）执行范围

**A（G5-1）今晚执行**：交叉编译 → 上机环回 → Magisk 监督 → 同套验收回归（mic 流/唤醒自回环/播放探针/音量/LED）→ 写入物存档 `deploy/dot2d/` → 回滚演练。
**B 只做只读探测**（PCM 拓扑、tinymix 快照）作为提案证据；实现留给下一里程碑与用户确认后。

## 4. hw_ref 2ch 与核心侧 AEC 评估（G4 记录，2026-11-08）

**现成输入**：设备 register 宣告 `aec_hw_ref`＋`ref_uplink`；适配器已有 `--hw-ref` 开关（mic_ref 2ch [mic|ref] 上行，v0.2 布局字段已定）；固件侧 AEC 资产（`aec_echo_path.v2.bin.mics`）证明该硬件回声路径稳定可测。

**评估结论（本轮不实现）**：
- 核心侧 AEC 的价值场景＝播放中免打断继续收音（当前半双工靠诚实丢块门控）。收益真实但被两件事压着：①唤醒/跟随链路的瓶颈在 ASR＋LLM，不在门控；②夜间实测电平（噪底 p95 −47.6、语音 −27～−40 dBFS）显示门控策略在当前房间足够可靠（误唤醒 0/6）。
- 实现路线候选（若做）：WebRTC AEC3（`webrtc-audio-processing` 绑定，BSD-3 → 商用需按 docs/commercial-licensing.md 过一遍）或 speexdsp（MIT，效果弱于 AEC3）。均为 C 依赖＋交叉编译负担（armv7 musl），且金标准＝真机回声路径数据（G5-2 替换形态下可自采）。
- **顺序建议**：AEC 随 G5-2（替换 EchoMuse 后自持 2ch 采集）一起做，在 Path A 共存形态上叠加只得到"半套"（ref 通道存在但回放时钟不归我们）。
