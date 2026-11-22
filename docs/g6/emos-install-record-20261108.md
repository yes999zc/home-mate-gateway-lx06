# G6 · emOS 安装记录（dot2-d，2026-11-08 02:13–02:17 UTC）

执行人：agent（预授权破坏性动作，前后说明已给用户）。前置条件全绿：
物理门（[gate record](physical-gate-record-20261108.md)）＋备份三重＋镜像
离线核验（[build record](../deploy/dot2d/emos/build-record-20261108.md)）＋
G6 固件 FireOS 实测（bring-up 21/21＋TTS device_confirmed）。

## 时间线（全部 UTC）

| 时刻 | 动作 | 证据 |
|---|---|---|
| 02:12 | /data 预置：G6 固件双槽（armv7 musl `905d6756…`，md5 `2a3dd459…` 双向核对）＋emOS 感知监督脚本＋arm64 诊断二进制 | supervisor.log `os=fireos` 行 |
| 02:13 | FireOS 上验证 G6 固件：`[bringup] 21 pre-PCM settings applied`、TTS `device_confirmed`（synth 665ms） | /tmp/hm-direct.log |
| 02:13:58 | `adb reboot recovery` → TWRP 3.7.0_9-bboe2 | getprop |
| 02:14 | dd 镜像 → p10（0.916s，16+0 records）；**首次尝试 `conv=fsync` 被 TWRP dd 拒绝，未写入**（读回仍 fa5e512c＝未污染），改朴素 dd＋sync | 读回 `4fa97518…`＝构建镜像逐字节 |
| 02:16:55 | `adb reboot` → emOS 首启 | — |
| ~02:17 | 串口 `/dev/cu.usbmodem31101`（iSerial=G090L91072732EXG）出横幅 `emOS emos-v0.10-hm.1-nocache` | emos-firstboot-serial-20261108.log |
| 02:18 | boot-trail 段 1–9 全达；WiFi 关联（wlan0 192.168.1.73，用 /data 既有 Android wpa 配置）；init 提升 known-good（`rollback promoted rc=0 bytes=6932480`） | 同上 |
| 02:19 | hm-direct 自起：`[bringup] controlC0: 239 controls`＋`21 pre-PCM applied`＋`amp up`＋`no /dev/uinput — direct button mode (emOS)`＋北向 `connected to core (generation 30)` | /run/hm-direct.log |
| 02:20 | 核心侧 `hm-firmware 0.0.1 (emos)` 在线；mic 14.0 chunk/s、噪底均值 −50.6dB（＝G4 基线）；TTS 探针 `device_confirmed`（synth 599ms/total 2053ms） | /api/v0/status、/mode |

## 已知事实与边界（如实）

- **nptd 对 192.168.1.1 超时**：设备时钟未同步。当前不阻塞（固件↔core 是
  IP 直连无 TLS；云 API 都在 Mac 侧走）。**C 阶段 core 上机前必须解决**
  （否则 TLS 全挂）——已记入 C 阶段任务。
- emOS 无 adbd：调试走 USB ACM 串口（本次 `/dev/cu.usbmodem31101`，
  raw 只读监视＋`serial_cmd.py` 命令注入）。串口不作常规升级通道。
- LED：init 启动环（用户可目验）→固件 dark-start 接管；emOS 上无
  ledcontroller，固件是唯一写者（设计如此，灯语验收见 acceptance §8/§10）。
- 回滚常备：物理门（Mute 按住 30s）→ TWRP → dd `boot_a_x.img`
  （Mac/NAS 均有副本）→ 2 分钟回 FireOS。AEC 落地后复验一次全流程。
